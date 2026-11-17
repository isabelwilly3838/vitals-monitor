//! HTTP middleware: request-id header, security headers, and a token-bucket rate limiter.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use actix_web::HttpMessage;
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header::{HeaderName, HeaderValue};
use actix_web::middleware::Next;

pub const SECURITY_HEADERS: [(&str, &str); 6] = [
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "no-referrer"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=()",
    ),
    ("cross-origin-opener-policy", "same-origin"),
    (
        "content-security-policy",
        "default-src 'self'; script-src 'self' https://cdn.jsdelivr.net; style-src 'self'; \
         img-src 'self' data:; connect-src 'self' ws: wss:; frame-ancestors 'none'; \
         base-uri 'none'; form-action 'self'",
    ),
];

/// Adds `X-Request-ID` (the id of the tracing root span) and security headers to every
/// response; API responses are additionally marked `Cache-Control: no-store`.
pub async fn security_headers(
    req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
    let request_id = req
        .extensions()
        .get::<tracing_actix_web::RequestId>()
        .map(|r| r.to_string());
    let api = ["/fhir", "/auth", "/admin"]
        .iter()
        .any(|p| req.path().starts_with(p));
    let mut res = next.call(req).await?;
    let h = res.headers_mut();
    if let Some(id) = request_id.and_then(|id| HeaderValue::from_str(&id).ok()) {
        h.insert(HeaderName::from_static("x-request-id"), id);
    }
    for (k, v) in SECURITY_HEADERS {
        h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
    }
    if api {
        h.insert(
            HeaderName::from_static("cache-control"),
            HeaderValue::from_static("no-store"),
        );
    }
    Ok(res)
}

/// Token bucket per key (client IP). Single process; use Redis when running >1 replica.
pub struct RateLimiter {
    capacity: f64,
    per_sec: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        let capacity = per_minute.max(1) as f64;
        Self {
            capacity,
            per_sec: capacity / 60.0,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();
        let (tokens, last) = buckets.get(key).copied().unwrap_or((self.capacity, now));
        let tokens =
            (tokens + now.duration_since(last).as_secs_f64() * self.per_sec).min(self.capacity);
        if tokens < 1.0 {
            buckets.insert(key.to_string(), (tokens, now));
            return false;
        }
        buckets.insert(key.to_string(), (tokens - 1.0, now));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::RateLimiter;

    #[test]
    fn bucket_blocks_after_capacity_and_is_per_key() {
        let rl = RateLimiter::new(2);
        assert!(rl.allow("a") && rl.allow("a"));
        assert!(!rl.allow("a"));
        assert!(rl.allow("b"));
    }
}
