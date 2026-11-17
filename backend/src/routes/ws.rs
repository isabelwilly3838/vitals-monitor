//! Real-time WebSocket stream (`/ws/vitals`).
//!
//! Protocol:
//!   client -> {"token": "<jwt>", "encoding": "binary"|"fhir", "patients": ["P001"]?}
//!   server -> {"type":"hello", "patients":[...], "allowed":[...], "frame_bytes":28}
//!   server -> binary frames (services/stream.rs) | {"type":"observations", ...} text
//! The token travels in the first message, not the URL, so it never lands in proxy logs.

use std::collections::HashSet;
use std::time::Duration;

use actix_web::{HttpRequest, HttpResponse, rt, web};
use actix_ws::{CloseCode, CloseReason, Message};
use chrono::DateTime;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::fhir::builders as b;
use crate::security::auth::{Perm, decode_token};
use crate::services::state::AppState;
use crate::services::stream::{FRAME_BYTES, Sample, encode};

#[derive(Deserialize)]
struct Hello {
    token: String,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    patients: Option<Vec<String>>,
}

pub async fn vitals_stream(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, mut session, mut msgs) = actix_ws::handle(&req, body)?;
    let timeout = Duration::from_millis(state.settings.resilience.ws_auth_timeout_ms);

    rt::spawn(async move {
        // --- authenticate with the first text message
        let first = tokio::time::timeout(timeout, msgs.next()).await;
        let hello = match first {
            Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Hello>(&t).ok(),
            _ => None,
        };
        let principal = hello
            .as_ref()
            .and_then(|h| decode_token(&h.token, &state.settings.security).ok())
            .filter(|p| p.has(Perm::Stream));
        let (Some(hello), Some(p)) = (hello, principal) else {
            tracing::warn!("websocket authentication failed");
            let _ = session
                .close(Some(CloseReason {
                    code: CloseCode::Other(4401),
                    description: Some("unauthorized".into()),
                }))
                .await;
            return;
        };

        let mut allowed: Vec<String> = state
            .patient_ids
            .iter()
            .filter(|id| p.can_access_patient(id))
            .cloned()
            .collect();
        if let Some(wanted) = &hello.patients {
            allowed.retain(|id| wanted.contains(id));
        }
        let filter: HashSet<String> = allowed.iter().cloned().collect();
        let fhir_mode = hello.encoding.as_deref() == Some("fhir");
        let mut rx = state.hub.subscribe();
        tracing::info!(user = %p.username, patients = ?allowed, fhir_mode, "websocket subscribed");

        let hello_msg = json!({"type": "hello", "encoding": if fhir_mode {"fhir"} else {"binary"},
                               "patients": state.patient_ids, "allowed": allowed, "frame_bytes": FRAME_BYTES});
        if session.text(hello_msg.to_string()).await.is_err() {
            return;
        }

        let mut dropped = 0u64;
        loop {
            tokio::select! {
                sample = rx.recv() => match sample {
                    Ok(s) if filter.contains(&s.patient_id) => {
                        let sent = if fhir_mode {
                            session.text(fhir_message(&s).to_string()).await
                        } else {
                            session.binary(encode(&s).to_vec()).await
                        };
                        if sent.is_err() { break; }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => dropped += n, // slow client: skip ahead
                    Err(RecvError::Closed) => break,
                },
                msg = msgs.next() => match msg {
                    Some(Ok(Message::Ping(b))) => { if session.pong(&b).await.is_err() { break; } }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
        tracing::info!(user = %p.username, dropped, "websocket closed");
        let _ = session.close(None).await;
    });
    Ok(response)
}

fn fhir_message(s: &Sample) -> serde_json::Value {
    let ts = DateTime::from_timestamp_millis(s.ts_ms as i64).unwrap_or_default();
    let mut out = vec![];
    if !s.temp_c.is_nan() {
        out.push(b::body_temperature(
            &format!("{}-t-{}", s.patient_id, s.seq),
            &s.patient_id,
            "stream",
            ts,
            s.temp_c as f64,
        ));
    }
    if !s.systolic.is_nan() {
        out.push(b::blood_pressure(
            &format!("{}-bp-{}", s.patient_id, s.seq),
            &s.patient_id,
            "stream",
            ts,
            s.systolic as f64,
            s.diastolic as f64,
        ));
    }
    json!({"type": "observations", "alerts": s.alerts, "resources": out})
}
