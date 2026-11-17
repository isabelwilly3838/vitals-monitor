use chrono::Utc;
use serde_json::Value;
use sqlx::Row;
use vitals_monitor::fhir::builders as b;

use crate::helpers::spawn_app;

#[tokio::test]
async fn health_endpoints() {
    let app = spawn_app().await;
    let live: Value = app.get("/health/live", None).await.json().await.unwrap();
    assert_eq!(live["status"], "ok");
    let ready: Value = app.get("/health/ready", None).await.json().await.unwrap();
    assert_eq!(ready["status"], "ok");
    assert_eq!(ready["checks"]["storage"]["breaker"], "closed");
}

#[tokio::test]
async fn key_rotation_reencrypts_every_row() {
    let app = spawn_app().await;
    let t = app.token("clinician").await;
    app.post_obs(&b::body_temperature("x", "P001", "D", Utc::now(), 37.0), &t)
        .await;
    let before: Vec<Vec<u8>> = sqlx::query("SELECT data FROM resources ORDER BY id")
        .fetch_all(&app.db)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get("data"))
        .collect();

    assert_eq!(
        app.post_empty("/admin/keys/rotate", &t).await.status(),
        403,
        "clinician may not rotate"
    );
    let r = app
        .post_empty("/admin/keys/rotate", &app.token("admin").await)
        .await;
    assert_eq!(r.status(), 200);
    let n = r.json::<Value>().await.unwrap()["reencrypted_rows"]
        .as_u64()
        .unwrap();
    assert!(n as usize >= before.len());

    let after: Vec<Vec<u8>> = sqlx::query("SELECT data FROM resources ORDER BY id")
        .fetch_all(&app.db)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get("data"))
        .collect();
    assert!(before.iter().zip(&after).all(|(a, b)| a != b));
    assert_eq!(
        app.get("/fhir/Patient/P001", Some(&t)).await.status(),
        200,
        "still readable"
    );
}

#[tokio::test]
async fn storage_outage_degrades_gracefully_and_recovers() {
    let app = spawn_app().await;
    let (t, admin) = (app.token("clinician").await, app.token("admin").await);
    app.post_empty("/admin/chaos/storage?fail=true", &admin)
        .await;

    // writes are still accepted (buffered) and the stream keeps flowing; reads are 503
    for _ in 0..4 {
        assert_eq!(
            app.post_obs(&b::body_temperature("x", "P001", "D", Utc::now(), 37.0), &t)
                .await
                .status(),
            201
        );
    }
    assert_eq!(app.get("/fhir/Patient", Some(&t)).await.status(), 503);
    let ready: Value = app.get("/health/ready", None).await.json().await.unwrap();
    assert_eq!(ready["status"], "degraded");
    assert_eq!(ready["checks"]["storage"]["breaker"], "open");
    assert_eq!(ready["checks"]["storage"]["buffered"], 4);

    // recovery: breaker half-opens after 200 ms (test config), drain persists the buffer
    app.post_empty("/admin/chaos/storage?fail=false", &admin)
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(app.state.store.drain().await, 4);
    let ready: Value = app.get("/health/ready", None).await.json().await.unwrap();
    assert_eq!(
        (
            ready["status"].as_str(),
            ready["checks"]["storage"]["buffered"].as_u64()
        ),
        (Some("ok"), Some(0))
    );
    let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM resources WHERE rtype='Observation'")
        .fetch_one(&app.db)
        .await
        .unwrap()
        .get("n");
    assert_eq!(n, 4, "no data lost");
}
