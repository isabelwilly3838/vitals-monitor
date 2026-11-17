pub mod auth;
pub mod fhir;
pub mod ops;
pub mod ws;

/// Client IP: behind our nginx the real address arrives in X-Real-IP (set by the proxy).
pub fn client_ip(req: &actix_web::HttpRequest) -> String {
    req.headers()
        .get("x-real-ip")
        .and_then(|h| h.to_str().ok())
        .map(String::from)
        .or_else(|| req.peer_addr().map(|a| a.ip().to_string()))
        .unwrap_or_else(|| "-".into())
}
