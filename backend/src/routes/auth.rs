use actix_web::{HttpRequest, HttpResponse, web};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;

use super::client_ip;
use crate::error::ApiError;
use crate::security::auth::{Authenticated, Principal, issue_token};
use crate::services::state::AppState;

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: SecretString,
}

#[tracing::instrument(name = "login", skip(form, state, req), fields(username = %form.username))]
pub async fn login(
    req: HttpRequest,
    form: web::Form<LoginForm>,
    state: web::Data<AppState>,
) -> Result<HttpResponse, ApiError> {
    let ip = client_ip(&req);
    if !state.login_limiter.allow(&ip) {
        return Err(ApiError::RateLimited);
    }
    let form = form.into_inner();
    if form.username.len() > 64 || form.password.expose_secret().len() > 128 {
        return Err(ApiError::Auth("Invalid credentials".into()));
    }
    // bcrypt is CPU-bound: keep it off the async executor
    let st = state.clone();
    let (username, password) = (form.username.clone(), form.password);
    let principal = crate::telemetry::spawn_blocking_with_tracing(move || {
        st.users.authenticate(&username, &password)
    })
    .await
    .map_err(|e| anyhow::anyhow!(e))?;
    let Some(p) = principal else {
        let attempted = Principal {
            username: form.username.chars().take(64).collect(),
            roles: vec![],
            patient_id: None,
        };
        state
            .audit("login", false, Some(&attempted), &ip, None, None, "login")
            .await;
        return Err(ApiError::Auth("Invalid credentials".into()));
    };
    let (token, ttl) = issue_token(&p, &state.settings.security)?;
    state
        .audit("login", true, Some(&p), &ip, None, None, "login")
        .await;
    Ok(HttpResponse::Ok().json(json!({
        "access_token": token, "token_type": "bearer", "expires_in": ttl,
        "roles": p.role_names(), "patient_id": p.patient_id,
    })))
}

pub async fn me(Authenticated(p): Authenticated) -> HttpResponse {
    HttpResponse::Ok()
        .json(json!({"username": p.username, "roles": p.role_names(), "patient_id": p.patient_id}))
}
