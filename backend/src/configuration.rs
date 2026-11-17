//! Layered, environment-aware configuration (Zero To Production style).
//!
//! Load order (later wins):
//!   1. `config/base.yaml`
//!   2. `config/<APP_ENVIRONMENT>.yaml`        (dev | test | prod, default dev)
//!   3. env vars `APP_<SECTION>__<KEY>`        e.g. `APP_DATABASE__PORT=5432`
//!   4. env vars `APP_<SECTION>__<KEY>_FILE`   Docker/K8s secrets mounted as files
//!
//! `Settings::validate` refuses to start production with weak or missing secrets.

use std::path::PathBuf;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_aux::field_attributes::deserialize_number_from_string;
use sqlx::postgres::{PgConnectOptions, PgSslMode};

pub const DEV_PLACEHOLDER: &str = "dev-only-insecure";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Dev,
    Test,
    Prod,
}

impl Environment {
    pub fn as_str(&self) -> &'static str {
        match self {
            Environment::Dev => "dev",
            Environment::Test => "test",
            Environment::Prod => "prod",
        }
    }
}

impl TryFrom<String> for Environment {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.to_lowercase().as_str() {
            "dev" | "local" => Ok(Self::Dev),
            "test" => Ok(Self::Test),
            "prod" | "production" => Ok(Self::Prod),
            other => Err(format!(
                "{other} is not a supported environment (dev|test|prod)"
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    #[serde(skip_deserializing, default = "default_env")]
    pub environment: Environment,
    pub application: ApplicationSettings,
    pub database: DatabaseSettings,
    pub security: SecuritySettings,
    pub simulator: SimulatorSettings,
    pub resilience: ResilienceSettings,
}

fn default_env() -> Environment {
    Environment::Dev
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationSettings {
    pub host: String,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub port: u16,
    pub serve_frontend: bool,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub max_body_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseSettings {
    pub host: String,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub port: u16,
    pub username: String,
    pub password: SecretString,
    pub database_name: String,
    pub require_ssl: bool,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub max_connections: u32,
}

impl DatabaseSettings {
    pub fn without_db(&self) -> PgConnectOptions {
        let ssl = if self.require_ssl {
            PgSslMode::Require
        } else {
            PgSslMode::Prefer
        };
        PgConnectOptions::new()
            .host(&self.host)
            .port(self.port)
            .username(&self.username)
            .password(self.password.expose_secret())
            .ssl_mode(ssl)
    }

    pub fn with_db(&self) -> PgConnectOptions {
        self.without_db().database(&self.database_name)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecuritySettings {
    /// Comma separated. First signs, all verify (zero-downtime rotation).
    pub jwt_secrets: SecretString,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub jwt_ttl_minutes: i64,
    pub jwt_issuer: String,
    /// Comma separated base64 AES-256 keys. First encrypts, all decrypt.
    pub encryption_keys: SecretString,
    /// JSON list of users: [{"username","password_hash","roles":[..],"patient_id":?}]
    pub users_json: SecretString,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub login_rate_per_minute: u32,
}

impl SecuritySettings {
    pub fn jwt_secret_list(&self) -> Vec<String> {
        split_list(self.jwt_secrets.expose_secret())
    }
    pub fn encryption_key_list(&self) -> Vec<String> {
        split_list(self.encryption_keys.expose_secret())
    }
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimulatorSettings {
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub hz: f64,
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResilienceSettings {
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub store_retry_attempts: u32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub breaker_failure_threshold: u32,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub breaker_reset_timeout_ms: u64,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub fallback_buffer_size: usize,
    #[serde(deserialize_with = "deserialize_number_from_string")]
    pub ws_auth_timeout_ms: u64,
}

/// Directory containing `base.yaml`: `$APP_CONFIG_DIR`, else `<repo>/config`.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("APP_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [manifest.join("../config"), PathBuf::from("config")];
    candidates
        .into_iter()
        .find(|p| p.join("base.yaml").exists())
        .unwrap_or_else(|| "config".into())
}

/// Turn `APP_X__Y_FILE=/run/secrets/y` into `APP_X__Y=<file contents>` (if not already set).
/// A `_FILE` that cannot be read is a hard error naming the variable - never silently
/// ignored (that would surface later as a confusing "secret missing" failure).
fn read_secret_files() -> Result<(), String> {
    for (key, value) in std::env::vars() {
        let Some(target) = key.strip_suffix("_FILE") else {
            continue;
        };
        if !target.starts_with("APP_") || std::env::var(target).is_ok() {
            continue;
        }
        let contents = std::fs::read_to_string(&value)
            .map_err(|e| format!("cannot read secret file for {key} ({value}): {e}"))?;
        // SAFETY: called once at startup before any threads are spawned.
        unsafe { std::env::set_var(target, contents.trim()) };
    }
    Ok(())
}

pub fn get_configuration() -> Result<Settings, config::ConfigError> {
    read_secret_files().map_err(config::ConfigError::Message)?;
    let environment: Environment = std::env::var("APP_ENVIRONMENT")
        .unwrap_or_else(|_| "dev".into())
        .try_into()
        .map_err(config::ConfigError::Message)?;
    get_configuration_for(environment)
}

/// Load settings for an explicit environment (used by the integration tests).
pub fn get_configuration_for(environment: Environment) -> Result<Settings, config::ConfigError> {
    let dir = config_dir();
    let mut settings: Settings = config::Config::builder()
        .add_source(config::File::from(dir.join("base.yaml")))
        .add_source(config::File::from(
            dir.join(format!("{}.yaml", environment.as_str())),
        ))
        .add_source(
            config::Environment::with_prefix("APP")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        )
        .build()?
        .try_deserialize()?;
    settings.environment = environment;
    settings.validate().map_err(config::ConfigError::Message)?;
    Ok(settings)
}

impl Settings {
    /// Production hardening: refuse to boot with insecure configuration.
    pub fn validate(&self) -> Result<(), String> {
        if self.simulator.hz <= 0.0 || self.simulator.hz > 50.0 {
            return Err("simulator.hz must be in (0, 50]".into());
        }
        if self.security.jwt_ttl_minutes < 1 || self.security.jwt_ttl_minutes > 240 {
            return Err("security.jwt_ttl_minutes must be in [1, 240]".into());
        }
        if self.environment != Environment::Prod {
            return Ok(());
        }
        let mut problems = vec![];
        let jwt = self.security.jwt_secret_list();
        if jwt.is_empty()
            || jwt
                .iter()
                .any(|s| s.len() < 32 || s.contains(DEV_PLACEHOLDER))
        {
            problems.push("JWT secrets must be set, >= 32 chars and not the dev default");
        }
        if self.security.encryption_key_list().is_empty() {
            problems.push("encryption keys must be set");
        }
        if self.security.users_json.expose_secret().is_empty() {
            problems.push("users_json must be provided (no seeded dev users in prod)");
        }
        if self.database.password.expose_secret().is_empty() {
            problems.push("database password must be set");
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!("Unsafe production config: {}", problems.join("; ")))
        }
    }
}
