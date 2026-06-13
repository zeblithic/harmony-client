//! `/v1/events` WebSocket subscriber.

use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, Deserialize)]
pub struct EventFrame {
    pub seq: Option<u64>,
    pub event: String,
    pub payload: Value,
}

/// Connect to `ws://127.0.0.1:<port>/v1/events` with bearer auth and forward
/// each parsed frame to an mpsc receiver. The background task ends when the
/// receiver is dropped or the socket closes; the returned `JoinHandle` is for
/// the caller to keep alive (dropping it merely detaches — it does NOT abort).
pub async fn subscribe(
    port: u16,
    token: &str,
) -> anyhow::Result<(
    mpsc::UnboundedReceiver<EventFrame>,
    tokio::task::JoinHandle<()>,
)> {
    let url = format!("ws://127.0.0.1:{port}/v1/events");
    let mut req = url.into_client_request().context("ws request")?;
    req.headers_mut()
        .insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .context("ws connect")?;

    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        // Move the whole stream in (no split — we never send, and splitting risks
        // dropping the write half early on some impls).
        let mut ws = ws;
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(txt) = msg {
                if let Ok(frame) = serde_json::from_str::<EventFrame>(&txt) {
                    if tx.send(frame).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok((rx, task))
}

/// Drain the receiver until `pred` matches a frame or `timeout` elapses.
pub async fn await_event(
    rx: &mut mpsc::UnboundedReceiver<EventFrame>,
    timeout: Duration,
    pred: impl Fn(&EventFrame) -> bool,
) -> anyhow::Result<EventFrame> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("await_event timed out after {timeout:?}");
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(frame)) if pred(&frame) => return Ok(frame),
            Ok(Some(_)) => continue,
            Ok(None) => anyhow::bail!("event stream closed"),
            Err(_) => anyhow::bail!("await_event timed out after {timeout:?}"),
        }
    }
}
