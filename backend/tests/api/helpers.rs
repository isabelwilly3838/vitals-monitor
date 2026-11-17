//! Test harness (Zero To Production style): every test gets its own app instance on a
//! random port and its own freshly-migrated PostgreSQL database -> full test isolation,
//! tests run in parallel, re-runs never see stale data.

use std::sync::Arc;

use once_cell::sync::Lazy;
use reqwest::Response;
use serde_json::Value;
use sqlx::{Connection, Executor, PgConnection, PgPool};
use uuid::Uuid;
use vitals_monitor::configuration::{
    DatabaseSettings, Environment, Settings, get_configuration_for,
};
use vitals_monitor::services::state::AppState;
use vitals_monitor::startup::{Application, get_connection_pool};
use vitals_monitor::telemetry::{get_subscriber, init_subscriber};

/// `TEST_LOG=1 cargo test` prints the JSON trace output; silent by default.
static TRACING: Lazy<()> = Lazy::new(|| {
    if std::env::var("TEST_LOG").is_ok() {
        init_subscriber(get_subscriber(
            "test".into(),
            "debug".into(),
            std::io::stdout,
        ));
    } else {
        init_subscriber(get_subscriber("test".into(), "debug".into(), std::io::sink));
    }
});

pub const FHIR: &str = "application/fhir+json";

pub fn password(user: &str) -> &'static str {
    match user {
        "admin" => "admin-dev-pass",
        "clinician" => "clinician-dev-pass",
        "patient1" => "patient-dev-pass",
        "gateway" => "device-dev-pass",
        _ => "wrong",
    }
}

pub struct TestApp {
    pub address: String,
    pub port: u16,
    pub db: PgPool,
    pub state: Arc<AppState>,
    pub client: reqwest::Client,
}

impl TestApp {
    pub async fn token(&self, user: &str) -> String {
        let r = self.login(user, password(user)).await;
        assert_eq!(r.status(), 200, "login failed for {user}");
        r.json::<Value>().await.unwrap()["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    pub async fn login(&self, user: &str, pw: &str) -> Response {
        self.client
            .post(format!("{}/auth/token", self.address))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("username={user}&password={pw}"))
            .send()
            .await
            .unwrap()
    }

    pub async fn get(&self, path: &str, token: Option<&str>) -> Response {
        let mut req = self.client.get(format!("{}{path}", self.address));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap()
    }

    pub async fn get_query(&self, path: &str, query: &[(&str, &str)], token: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.address))
            .query(query)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    pub async fn post_obs(&self, obs: &Value, token: &str) -> Response {
        self.post_raw("/fhir/Observation", obs.to_string(), FHIR, token)
            .await
    }

    pub async fn post_raw(&self, path: &str, body: String, ctype: &str, token: &str) -> Response {
        self.client
            .post(format!("{}{path}", self.address))
            .header("Content-Type", ctype)
            .bearer_auth(token)
            .body(body)
            .send()
            .await
            .unwrap()
    }

    pub async fn post_empty(&self, path: &str, token: &str) -> Response {
        self.client
            .post(format!("{}{path}", self.address))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }
}

pub async fn spawn_app() -> TestApp {
    spawn_app_with(|_| {}).await
}

/// Spawn with test settings tweaked by `customise` (e.g. a tiny rate-limit budget).
pub async fn spawn_app_with(customise: impl FnOnce(&mut Settings)) -> TestApp {
    Lazy::force(&TRACING);
    let mut settings =
        get_configuration_for(Environment::Test).expect("failed to read test configuration");
    customise(&mut settings);
    settings.database.database_name = format!("test_{}", Uuid::new_v4().simple());
    let db = configure_database(&settings.database).await;

    let app = Application::build(settings)
        .await
        .expect("failed to build application");
    let port = app.port();
    let state = app.state.clone();
    tokio::spawn(app.run_until_stopped());
    TestApp {
        address: format!("http://127.0.0.1:{port}"),
        port,
        db,
        state,
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    }
}

async fn configure_database(cfg: &DatabaseSettings) -> PgPool {
    let mut conn = PgConnection::connect_with(&cfg.without_db())
        .await
        .expect("Postgres not reachable - start it with `make db`");
    conn.execute(format!(r#"CREATE DATABASE "{}";"#, cfg.database_name).as_str())
        .await
        .expect("failed to create test database");
    get_connection_pool(cfg)
}
