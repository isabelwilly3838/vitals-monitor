//! Centralised error handling.
//!
//! Every error leaves the API as a FHIR `OperationOutcome` with a *safe* message.
//! Internal details (sqlx errors, stack context) are only recorded in the tracing span.

use actix_web::http::StatusCode;
use actix_web::http::header::{self, HeaderValue};
use actix_web::{HttpResponse, ResponseError};

use crate::fhir::builders::{Issue, operation_outcome};

pub const FHIR_JSON: &str = "application/fhir+json";

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("validation failed")]
    Validation(Vec<Issue>),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("too many requests")]
    RateLimited,
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("service unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Unexpected(#[from] anyhow::Error),
}

impl ApiError {
    pub fn invalid(code: &'static str, msg: impl Into<String>, expr: &str) -> Self {
        Self::Validation(vec![Issue::error(code, msg, Some(expr))])
    }

    fn issues(&self) -> Vec<Issue> {
        let one = |code, msg: &str| vec![Issue::error(code, msg, None)];
        match self {
            Self::Validation(i) => i.clone(),
            Self::BadRequest(m) => one("invalid", m),
            Self::Auth(m) => one("login", m),
            Self::Forbidden(m) => one("forbidden", m),
            Self::NotFound(m) => one("not-found", m),
            Self::RateLimited => one("throttled", "Too many requests, try again later"),
            Self::PayloadTooLarge => one("too-costly", "Request body too large"),
            Self::Unavailable(m) => one("transient", m),
            Self::Unexpected(_) => vec![Issue {
                severity: "fatal",
                code: "exception",
                diagnostics: "Internal server error. Quote the X-Request-ID header to support."
                    .into(),
                expression: None,
            }],
        }
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::Validation(_) | Self::BadRequest(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Auth(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Unexpected(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        let status = self.status_code();
        if status.is_server_error() {
            tracing::error!(error.cause_chain = ?self, error.message = %self, "request failed");
        } else {
            tracing::warn!(status = status.as_u16(), error.message = %self, "request rejected");
        }
        let mut resp = HttpResponse::build(status);
        resp.content_type(FHIR_JSON);
        match self {
            Self::Auth(_) => {
                resp.insert_header((header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer")));
            }
            Self::RateLimited => {
                resp.insert_header((header::RETRY_AFTER, HeaderValue::from_static("60")));
            }
            _ => {}
        }
        resp.body(operation_outcome(&self.issues()).to_string())
    }
}

/// Map extractor errors (bad query string, bad path, bad form) to OperationOutcome.
pub fn query_error_handler(
    err: actix_web::error::QueryPayloadError,
    _req: &actix_web::HttpRequest,
) -> actix_web::Error {
    ApiError::BadRequest(format!("Invalid query parameters: {err}")).into()
}

pub fn form_error_handler(
    err: actix_web::error::UrlencodedError,
    _req: &actix_web::HttpRequest,
) -> actix_web::Error {
    ApiError::BadRequest(format!("Invalid form body: {err}")).into()
}

pub async fn not_found() -> Result<HttpResponse, ApiError> {
    Err(ApiError::NotFound("Resource or endpoint not found".into()))
}
