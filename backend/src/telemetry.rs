//! Tracing (not just logging): every request is a span carrying a request id; sub-tasks
//! (validation, storage, audit) are child spans, so one request id ties together the whole
//! processing tree. Output is Bunyan-style JSON with ISO-8601 timestamps and levels,
//! ready for a central log store (Loki / ELK).
//!
//! PHI policy: secrets are `SecretString` (Debug prints `[REDACTED]`), key material is
//! `KeyRing` (Debug prints `[REDACTED]`), and spans use `skip(...)` so measured values,
//! request bodies and tokens are never recorded - only identifiers (patient id, observation
//! id, LOINC code). See the `phi_never_reaches_logs` test.

use tracing::Subscriber;
use tracing::subscriber::set_global_default;
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_log::LogTracer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::{EnvFilter, Registry, layer::SubscriberExt};

pub fn get_subscriber<Sink>(
    name: String,
    env_filter: String,
    sink: Sink,
) -> impl Subscriber + Send + Sync
where
    Sink: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(env_filter));
    Registry::default()
        .with(env_filter)
        .with(JsonStorageLayer)
        .with(BunyanFormattingLayer::new(name, sink))
}

pub fn init_subscriber(subscriber: impl Subscriber + Send + Sync) {
    LogTracer::init().ok();
    set_global_default(subscriber).ok();
}

/// Run blocking work on a dedicated thread while keeping the current span.
pub fn spawn_blocking_with_tracing<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || span.in_scope(f))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use secrecy::SecretString;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn logs_are_json_with_time_level_and_phi_never_reaches_logs() {
        let buf = Buf::default();
        let sink = buf.clone();
        let sub = super::get_subscriber("t".into(), "info".into(), move || sink.clone());
        tracing::subscriber::with_default(sub, || {
            let password = SecretString::from("hunter2-secret".to_string());
            let keys =
                crate::security::crypto::KeyRing::new(&[crate::security::crypto::generate_key()])
                    .unwrap();
            tracing::info!(user = "clinician", ?password, ?keys, "login attempt");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line: serde_json::Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
        assert_eq!(line["msg"], "login attempt");
        assert_eq!(line["level"], 30);
        assert!(line["time"].as_str().unwrap().ends_with('Z'));
        assert!(!out.contains("hunter2-secret"));
        assert!(out.contains("REDACTED"));
    }
}
