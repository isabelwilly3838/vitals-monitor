use serde_json::Value;

use crate::helpers::{spawn_app, spawn_app_with};

#[tokio::test]
async fn wrong_password_returns_operation_outcome_401() {
    let app = spawn_app().await;
    let r = app.login("admin", "nope").await;
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["www-authenticate"], "Bearer");
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["resourceType"], "OperationOutcome");
}

#[tokio::test]
async fn unknown_user_gets_same_error_as_wrong_password() {
    let app = spawn_app().await;
    let a: Value = app.login("ghost", "x").await.json().await.unwrap();
    let b: Value = app.login("admin", "x").await.json().await.unwrap();
    assert_eq!(a["issue"][0]["diagnostics"], b["issue"][0]["diagnostics"]);
}

#[tokio::test]
async fn protected_endpoints_require_a_valid_token() {
    let app = spawn_app().await;
    for token in [None, Some("garbage"), Some("a.b.c")] {
        assert_eq!(
            app.get("/fhir/Patient", token).await.status(),
            401,
            "{token:?}"
        );
    }
}

#[tokio::test]
async fn me_returns_roles_and_patient_scope() {
    let app = spawn_app().await;
    let t = app.token("patient1").await;
    let me: Value = app.get("/auth/me", Some(&t)).await.json().await.unwrap();
    assert_eq!(
        me,
        serde_json::json!({"username": "patient1", "roles": ["patient"], "patient_id": "P001"})
    );
}

#[tokio::test]
async fn login_is_rate_limited_per_client() {
    let app = spawn_app_with(|s| s.security.login_rate_per_minute = 2).await;
    assert_eq!(app.login("x", "y").await.status(), 401);
    assert_eq!(app.login("x", "y").await.status(), 401);
    let r = app.login("x", "y").await;
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "60");
}

#[tokio::test]
async fn login_events_are_audited() {
    let app = spawn_app().await;
    app.login("admin", "bad").await;
    let t = app.token("admin").await;
    let bundle: Value = app
        .get("/fhir/AuditEvent", Some(&t))
        .await
        .json()
        .await
        .unwrap();
    let logins: Vec<&Value> = bundle["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["resource"])
        .filter(|r| r["subtype"][0]["code"] == "login")
        .collect();
    assert!(logins.iter().any(|r| r["outcome"] == "4"));
    assert!(logins.iter().any(|r| r["outcome"] == "0"));
}
