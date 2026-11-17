//! Health probes and admin operations.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::Ordering;

use super::client_ip;
use crate::configuration::Environment;
use crate::error::ApiError;
use crate::security::auth::{Authenticated, Perm};
use crate::services::state::AppState;

pub async fn live() -> HttpResponse {
    HttpResponse::Ok().json(json!({"status": "ok"}))
}

/// Readiness: reports breaker/buffer state. A degraded store still serves the live stream,
/// so this stays 200 with `status=degraded` (the load balancer should keep routing to us).
pub async fn ready(state: web::Data<AppState>) -> HttpResponse {
    let ping = state.store.store.ping().await.is_ok();
    let breaker = state.store.breaker.state();
    let degraded = !ping || breaker.as_str() != "closed";
    HttpResponse::Ok().json(json!({
        "status": if degraded { "degraded" } else { "ok" },
        "checks": {
            "storage": {"ping": if ping { "ok" } else { "fail" }, "breaker": breaker.as_str(),
                        "buffered": state.store.buffered(), "dropped": state.store.dropped()},
            "stream": {"subscribers": state.hub.subscriber_count()},
        }
    }))
}

pub async fn rotate_keys(
    req: HttpRequest,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::AdminKeys)?;
    let n = state.store.store.rotate_keys().await?;
    state
        .audit(
            "execute",
            true,
            Some(&p),
            &client_ip(&req),
            None,
            Some("keys/rotate"),
            "operation",
        )
        .await;
    tracing::warn!(
        rows = n,
        keys = state.store.store.keys.key_count(),
        "encryption keys rotated"
    );
    Ok(HttpResponse::Ok()
        .json(json!({"reencrypted_rows": n, "active_keys": state.store.store.keys.key_count()})))
}

#[derive(Deserialize)]
pub struct Chaos {
    fail: bool,
}

/// Fault injection to demonstrate circuit breaker + buffering. Disabled in prod.
pub async fn chaos_storage(
    q: web::Query<Chaos>,
    state: web::Data<AppState>,
    Authenticated(p): Authenticated,
) -> Result<HttpResponse, ApiError> {
    p.require(Perm::AdminKeys)?;
    if state.settings.environment == Environment::Prod {
        return Err(ApiError::Forbidden(
            "Chaos endpoints are disabled in production".into(),
        ));
    }
    state.store.store.fail_mode.store(q.fail, Ordering::Relaxed);
    tracing::warn!(fail = q.fail, "chaos storage toggled");
    Ok(HttpResponse::Ok().json(json!({
        "storage_fail_mode": q.fail, "breaker": state.store.breaker.state().as_str(), "buffered": state.store.buffered()
    })))
}
