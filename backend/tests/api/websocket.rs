use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use vitals_monitor::services::stream::decode;

use crate::helpers::{TestApp, spawn_app};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(app: &TestApp, hello: Value) -> Ws {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws/vitals", app.port))
            .await
            .unwrap();
    ws.send(Message::Text(hello.to_string().into()))
        .await
        .unwrap();
    ws
}

async fn next(ws: &mut Ws) -> Message {
    tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .expect("timed out")
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn bad_token_is_closed_with_4401() {
    let app = spawn_app().await;
    let mut ws = connect(&app, json!({"token": "bad"})).await;
    match next(&mut ws).await {
        Message::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 4401),
        other => panic!("expected close, got {other:?}"),
    }
}

#[tokio::test]
async fn device_role_may_not_subscribe() {
    let app = spawn_app().await;
    let mut ws = connect(&app, json!({"token": app.token("gateway").await})).await;
    assert!(matches!(next(&mut ws).await, Message::Close(_)));
}

#[tokio::test]
async fn streams_28_byte_binary_frames_with_alert_flags() {
    let app = spawn_app().await;
    let mut ws = connect(&app, json!({"token": app.token("clinician").await})).await;
    let hello: Value = serde_json::from_str(next(&mut ws).await.to_text().unwrap()).unwrap();
    assert_eq!(
        (hello["type"].as_str(), hello["frame_bytes"].as_u64()),
        (Some("hello"), Some(28))
    );
    let ids: Vec<String> = serde_json::from_value(hello["patients"].clone()).unwrap();

    app.state
        .ingest_sample(
            "P002",
            Utc::now(),
            Some(38.5),
            Some(120.0),
            Some(80.0),
            "D002",
        )
        .await
        .unwrap();
    let Message::Binary(frame) = next(&mut ws).await else {
        panic!("expected binary frame")
    };
    assert_eq!(frame.len(), 28);
    let s = decode(&frame, &ids).unwrap();
    assert_eq!(s.patient_id, "P002");
    assert!(s.alerts & 1 != 0, "fever flag");
    assert!((s.temp_c - 38.5).abs() < 1e-4);
}

#[tokio::test]
async fn patient_stream_is_scoped_to_own_data() {
    let app = spawn_app().await;
    let mut ws = connect(&app, json!({"token": app.token("patient1").await})).await;
    let hello: Value = serde_json::from_str(next(&mut ws).await.to_text().unwrap()).unwrap();
    assert_eq!(hello["allowed"], json!(["P001"]));
    let now = Utc::now();
    app.state
        .ingest_sample("P002", now, Some(37.0), Some(120.0), Some(80.0), "D")
        .await
        .unwrap();
    app.state
        .ingest_sample("P001", now, Some(37.0), Some(120.0), Some(80.0), "D")
        .await
        .unwrap();
    let Message::Binary(frame) = next(&mut ws).await else {
        panic!()
    };
    assert_eq!(
        frame[1], 0,
        "first frame received is P001 (index 0), P002 was filtered"
    );
}

#[tokio::test]
async fn fhir_encoding_streams_observations() {
    let app = spawn_app().await;
    let mut ws = connect(
        &app,
        json!({"token": app.token("clinician").await, "encoding": "fhir"}),
    )
    .await;
    next(&mut ws).await; // hello
    app.state
        .ingest_sample("P001", Utc::now(), Some(37.0), Some(120.0), Some(80.0), "D")
        .await
        .unwrap();
    let msg: Value = serde_json::from_str(next(&mut ws).await.to_text().unwrap()).unwrap();
    let codes: Vec<&str> = msg["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["code"]["coding"][0]["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, vec!["8310-5", "85354-9"]);
}
