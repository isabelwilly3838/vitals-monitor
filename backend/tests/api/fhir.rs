use chrono::Utc;
use serde_json::{Value, json};
use sqlx::Row;
use vitals_monitor::fhir::builders as b;

use crate::helpers::{FHIR, spawn_app};

fn temp(v: f64) -> Value {
    b::body_temperature("t1", "P001", "D001", Utc::now(), v)
}

// ------------------------------------------------------------------ public / headers

#[tokio::test]
async fn capability_statement_is_public_and_schema_valid() {
    let app = spawn_app().await;
    let r = app.get("/fhir/metadata", None).await;
    assert_eq!(r.status(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with(FHIR)
    );
    let body: Value = r.json().await.unwrap();
    assert_eq!(b::validate_schema(&body), vec![]);
}

#[tokio::test]
async fn security_headers_and_request_id_on_every_response() {
    let app = spawn_app().await;
    let r = app.get("/fhir/metadata", None).await;
    let h = r.headers();
    assert!(!h["x-request-id"].is_empty());
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["x-frame-options"], "DENY");
    assert_eq!(h["cache-control"], "no-store");
    assert!(
        h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
}

#[tokio::test]
async fn unknown_endpoint_is_operation_outcome_404() {
    let app = spawn_app().await;
    let r = app.get("/fhir/Nope", None).await;
    assert_eq!(r.status(), 404);
    assert_eq!(
        r.json::<Value>().await.unwrap()["issue"][0]["code"],
        "not-found"
    );
}

// ------------------------------------------------------------------ RBAC

#[tokio::test]
async fn patient_sees_only_own_record() {
    let app = spawn_app().await;
    let t = app.token("patient1").await;
    let bundle: Value = app
        .get("/fhir/Patient", Some(&t))
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = bundle["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["resource"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["P001"]);
    assert_eq!(app.get("/fhir/Patient/P002", Some(&t)).await.status(), 403);
    assert_eq!(
        app.get("/fhir/Observation?patient=P002", Some(&t))
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn clinician_sees_all_patients() {
    let app = spawn_app().await;
    let t = app.token("clinician").await;
    let bundle: Value = app
        .get("/fhir/Patient", Some(&t))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(bundle["total"], 4); // P001-P003 simulated + P004 real Arduino sensor
    assert_eq!(b::validate_schema(&bundle), vec![]);
}

#[tokio::test]
async fn device_can_write_but_not_read() {
    let app = spawn_app().await;
    let t = app.token("gateway").await;
    assert_eq!(app.post_obs(&temp(37.0), &t).await.status(), 201);
    assert_eq!(app.get("/fhir/Observation", Some(&t)).await.status(), 403);
}

#[tokio::test]
async fn audit_log_is_admin_only() {
    let app = spawn_app().await;
    assert_eq!(
        app.get("/fhir/AuditEvent", Some(&app.token("clinician").await))
            .await
            .status(),
        403
    );
    assert_eq!(
        app.get("/fhir/AuditEvent", Some(&app.token("admin").await))
            .await
            .status(),
        200
    );
}

// ------------------------------------------------------------------ FHIR REST

#[tokio::test]
async fn create_then_read_and_search() {
    let app = spawn_app().await;
    let t = app.token("clinician").await;
    let r = app
        .post_obs(
            &b::blood_pressure("x", "P001", "D001", Utc::now(), 150.0, 95.0),
            &t,
        )
        .await;
    assert_eq!(r.status(), 201);
    let location = r.headers()["location"].to_str().unwrap().to_string();
    let created: Value = r.json().await.unwrap();
    let id = created["id"].as_str().unwrap();
    assert!(location.ends_with(&format!("/Observation/{id}")));
    assert_eq!(created["interpretation"][0]["coding"][0]["code"], "H");

    let read: Value = app
        .get(&format!("/fhir/Observation/{id}"), Some(&t))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(read, created);

    let bundle: Value = app
        .get_query(
            "/fhir/Observation",
            &[
                ("patient", "Patient/P001"),
                ("code", "http://loinc.org|85354-9"),
            ],
            &t,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        (bundle["type"].as_str(), bundle["total"].as_i64()),
        (Some("searchset"), Some(1))
    );
    assert_eq!(b::validate_schema(&bundle), vec![]);
}

#[tokio::test]
async fn search_by_exact_instant_and_by_day() {
    let app = spawn_app().await;
    let t = app.token("clinician").await;
    let ts = "2026-03-01T10:00:00.123Z";
    let mut o = temp(37.0);
    o["effectiveDateTime"] = json!(ts);
    assert_eq!(app.post_obs(&o, &t).await.status(), 201);
    let total = |q: Vec<(&'static str, String)>| {
        let app = &app;
        let t = t.clone();
        async move {
            let q: Vec<(&str, &str)> = q.iter().map(|(k, v)| (*k, v.as_str())).collect();
            app.get_query("/fhir/Observation", &q, &t)
                .await
                .json::<Value>()
                .await
                .unwrap()["total"]
                .as_i64()
                .unwrap()
        }
    };
    assert_eq!(total(vec![("date", format!("eq{ts}"))]).await, 1);
    assert_eq!(total(vec![("date", "2026-03-01".into())]).await, 1);
    assert_eq!(total(vec![("date", "ge2026-03-02".into())]).await, 0);
    assert_eq!(
        total(vec![
            ("date", "ge2026-02-28".into()),
            ("date", "le2026-03-01".into())
        ])
        .await,
        1
    );
}

#[tokio::test]
async fn invalid_observation_is_422_operation_outcome() {
    let app = spawn_app().await;
    let r = app
        .post_obs(&temp(99.0), &app.token("clinician").await)
        .await;
    assert_eq!(r.status(), 422);
    let oo: Value = r.json().await.unwrap();
    assert_eq!(oo["resourceType"], "OperationOutcome");
    assert_eq!(
        oo["issue"][0]["expression"],
        json!(["Observation.valueQuantity"])
    );
}

#[tokio::test]
async fn validate_operation_dry_run() {
    let app = spawn_app().await;
    let t = app.token("clinician").await;
    let ok: Value = app
        .post_raw(
            "/fhir/Observation/$validate",
            temp(37.0).to_string(),
            FHIR,
            &t,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ok["issue"][0]["severity"], "information");
    let (_, total) = app
        .state
        .store
        .store
        .search_observations(&Default::default())
        .await
        .map(|(r, _)| ((), r.len()))
        .unwrap();
    assert_eq!(total, 0, "$validate must not store anything");
}

#[tokio::test]
async fn bad_content_type_or_json_rejected() {
    let app = spawn_app().await;
    let t = app.token("clinician").await;
    assert_eq!(
        app.post_raw("/fhir/Observation", "{}".into(), "text/plain", &t)
            .await
            .status(),
        422
    );
    assert_eq!(
        app.post_raw("/fhir/Observation", "{not json".into(), FHIR, &t)
            .await
            .status(),
        422
    );
}

#[tokio::test]
async fn oversize_body_rejected() {
    let app = spawn_app().await;
    let r = app
        .post_raw(
            "/fhir/Observation",
            "x".repeat(70_000),
            FHIR,
            &app.token("clinician").await,
        )
        .await;
    assert_eq!(r.status(), 413);
}

#[tokio::test]
async fn injection_attempts_rejected() {
    let app = spawn_app().await;
    let t = app.token("clinician").await;
    for (k, v) in [
        ("patient", "P001' OR '1'='1"),
        ("patient", "P001;DROP TABLE resources"),
        ("date", "ge2026-01-01' --"),
        ("code", "1 OR 1=1"),
        ("_count", "99999"),
        ("_count", "abc"),
    ] {
        assert_eq!(
            app.get_query("/fhir/Observation", &[(k, v)], &t)
                .await
                .status(),
            422,
            "{k}={v}"
        );
    }
    // the table is still there
    assert_eq!(app.get("/fhir/Patient", Some(&t)).await.status(), 200);
}

#[tokio::test]
async fn reads_and_denials_are_audited() {
    let app = spawn_app().await;
    app.get("/fhir/Patient/P001", Some(&app.token("clinician").await))
        .await;
    app.get("/fhir/Patient/P002", Some(&app.token("patient1").await))
        .await;
    let rows = sqlx::query("SELECT action, outcome FROM audit_events")
        .fetch_all(&app.db)
        .await
        .unwrap();
    let pairs: Vec<(String, String)> = rows
        .iter()
        .map(|r| (r.get("action"), r.get("outcome")))
        .collect();
    assert!(pairs.contains(&("R".into(), "0".into())));
    assert!(pairs.contains(&("R".into(), "4".into())));
}

#[tokio::test]
async fn stored_data_is_encrypted_at_rest() {
    let app = spawn_app().await;
    app.post_obs(&temp(39.1), &app.token("clinician").await)
        .await;
    let row = sqlx::query("SELECT data FROM resources WHERE rtype = 'Observation'")
        .fetch_one(&app.db)
        .await
        .unwrap();
    let blob: Vec<u8> = row.get("data");
    let as_text = String::from_utf8_lossy(&blob);
    assert!(!as_text.contains("39.1") && !as_text.contains("Observation"));
}
