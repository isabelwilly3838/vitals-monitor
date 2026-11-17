//! Application wiring (Zero To Production style): build everything from `Settings`,
//! bind a listener (port 0 = random, used by the integration tests), run migrations,
//! seed reference data, start supervised background tasks, and return a runnable server.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix_web::body::MessageBody;
use actix_web::dev::{Server, ServiceRequest, ServiceResponse};
use actix_web::middleware::from_fn;
use actix_web::{App, HttpServer, web};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tracing::Span;
use tracing_actix_web::{DefaultRootSpanBuilder, RootSpanBuilder, TracingLogger};

use crate::configuration::{DatabaseSettings, Environment, Settings};
use crate::error::{form_error_handler, not_found, query_error_handler};
use crate::routes::{auth, fhir, ops, ws};
use crate::security::crypto::{KeyRing, generate_key};
use crate::security::middleware::security_headers;
use crate::services::resilience::supervise;
use crate::services::state::AppState;
use crate::services::store::PgStore;

pub struct Application {
    port: u16,
    server: Server,
    pub state: Arc<AppState>,
}

impl Application {
    pub async fn build(settings: Settings) -> anyhow::Result<Self> {
        let pool = get_connection_pool(&settings.database);
        sqlx::migrate!("./migrations").run(&pool).await?;

        let keys = KeyRing::new(&encryption_keys(&settings)?)?;
        let store = Arc::new(PgStore::new(pool, keys));
        let state = Arc::new(AppState::new(settings.clone(), store)?);
        state.seed_reference_data().await?;

        let st = state.clone();
        tokio::spawn(supervise("store-drain", move || st.clone().drain_forever()));
        if settings.simulator.enabled {
            let st = state.clone();
            tokio::spawn(supervise("simulator", move || st.clone().run_simulator()));
        }

        let listener = TcpListener::bind(format!(
            "{}:{}",
            settings.application.host, settings.application.port
        ))?;
        let port = listener.local_addr()?.port();
        let server = run(listener, state.clone(), &settings)?;
        tracing::info!(
            env = settings.environment.as_str(),
            port,
            simulator = settings.simulator.enabled,
            "startup"
        );
        Ok(Self {
            port,
            server,
            state,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub async fn run_until_stopped(self) -> std::io::Result<()> {
        self.server.await
    }
}

pub fn get_connection_pool(db: &DatabaseSettings) -> PgPool {
    PgPoolOptions::new()
        .max_connections(db.max_connections)
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy_with(db.with_db())
}

/// Production uses configured keys only. Dev persists a local key file so data survives
/// restarts; tests use an ephemeral key.
fn encryption_keys(settings: &Settings) -> anyhow::Result<Vec<String>> {
    let keys = settings.security.encryption_key_list();
    if !keys.is_empty() {
        return Ok(keys);
    }
    match settings.environment {
        Environment::Prod => anyhow::bail!("encryption keys are required in prod"),
        Environment::Test => Ok(vec![generate_key()]),
        Environment::Dev => {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../data/dev.key");
            if let Ok(k) = std::fs::read_to_string(&path) {
                return Ok(vec![k.trim().to_string()]);
            }
            std::fs::create_dir_all(path.parent().unwrap())?;
            let k = generate_key();
            std::fs::write(&path, &k)?;
            tracing::warn!(path = %path.display(), "created local dev encryption key");
            Ok(vec![k])
        }
    }
}

/// Root span with an (initially empty) `user` field, filled in by the auth extractor, so
/// every log line of a request carries request_id + user.
pub struct VitalsRootSpan;

impl RootSpanBuilder for VitalsRootSpan {
    fn on_request_start(request: &ServiceRequest) -> Span {
        tracing_actix_web::root_span!(request, user = tracing::field::Empty)
    }
    fn on_request_end<B: MessageBody>(
        span: Span,
        outcome: &Result<ServiceResponse<B>, actix_web::Error>,
    ) {
        DefaultRootSpanBuilder::on_request_end(span, outcome);
    }
}

fn frontend_dir() -> Option<PathBuf> {
    let dir = std::env::var("APP_FRONTEND_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../frontend"));
    dir.join("index.html").exists().then_some(dir)
}

fn run(listener: TcpListener, state: Arc<AppState>, settings: &Settings) -> anyhow::Result<Server> {
    let data = web::Data::from(state);
    let serve_frontend = settings.application.serve_frontend;
    let max_body = settings.application.max_body_bytes;
    let server = HttpServer::new(move || {
        let mut app = App::new()
            .wrap(from_fn(security_headers))
            .wrap(TracingLogger::<VitalsRootSpan>::new())
            .app_data(data.clone())
            .app_data(web::QueryConfig::default().error_handler(query_error_handler))
            .app_data(
                web::FormConfig::default()
                    .limit(4096)
                    .error_handler(form_error_handler),
            )
            .app_data(web::PayloadConfig::new(max_body))
            .route("/health/live", web::get().to(ops::live))
            .route("/health/ready", web::get().to(ops::ready))
            .service(
                web::scope("/auth")
                    .route("/token", web::post().to(auth::login))
                    .route("/me", web::get().to(auth::me)),
            )
            .service(
                web::scope("/fhir")
                    .route("/metadata", web::get().to(fhir::capability_statement))
                    .route("/Patient", web::get().to(fhir::search_patients))
                    .route("/Patient/{id}", web::get().to(fhir::read_patient))
                    .route("/Device/{id}", web::get().to(fhir::read_device))
                    .route("/Observation", web::get().to(fhir::search_observations))
                    .route("/Observation", web::post().to(fhir::create_observation))
                    .route(
                        "/Observation/$validate",
                        web::post().to(fhir::validate_only),
                    )
                    .route("/Observation/{id}", web::get().to(fhir::read_observation))
                    .route("/AuditEvent", web::get().to(fhir::search_audit))
                    .default_service(web::to(not_found)),
            )
            .service(
                web::scope("/admin")
                    .route("/keys/rotate", web::post().to(ops::rotate_keys))
                    .route("/chaos/storage", web::post().to(ops::chaos_storage)),
            )
            .route("/ws/vitals", web::get().to(ws::vitals_stream));
        app = match (serve_frontend, frontend_dir()) {
            (true, Some(dir)) => app.service(
                actix_files::Files::new("/", dir)
                    .index_file("index.html")
                    .default_handler(web::to(not_found)),
            ),
            _ => app.default_service(web::to(not_found)),
        };
        app
    })
    .shutdown_timeout(10)
    .listen(listener)?
    .run();
    Ok(server)
}
