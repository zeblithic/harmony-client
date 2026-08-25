// src-tauri/src/api/events.rs — ZEB-445 WS event firehose.
//
// Every NodeEventSink::emit on the API sink becomes one JSON text frame
// {seq, event, payload}. seq is monotonic per server lifetime so clients
// detect gaps; a lagging client gets an explicit `_lagged` marker frame
// (bounded broadcast) — drops are loud, never silent. No replay: agents
// connect before acting.

use crate::node_event_sink::NodeEventSink;

#[derive(Clone, Debug, serde::Serialize)]
pub struct EventFrame {
    pub seq: u64,
    pub event: String,
    pub payload: serde_json::Value,
}

/// Bounded: a slow WS client lags and gets an explicit `_lagged` frame
/// rather than stalling the node or silently dropping.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

pub struct ApiEventSink {
    tx: tokio::sync::broadcast::Sender<EventFrame>,
    // Mutex (not AtomicU64): assignment and send must be one critical
    // section so delivered frames are seq-ordered — see emit().
    seq: std::sync::Mutex<u64>,
}

impl ApiEventSink {
    pub fn new() -> std::sync::Arc<Self> {
        let (tx, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAPACITY);
        std::sync::Arc::new(Self {
            tx,
            seq: std::sync::Mutex::new(0),
        })
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<EventFrame> {
        self.tx.subscribe()
    }

    /// Live WS subscriber count — lets tests wait for a subscription
    /// condition-style instead of sleeping a fixed interval.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

// Impl on the bare type (not `Arc<ApiEventSink>`): `NodeEventSink` now lives in
// `harmony-foundation`, and a foreign trait cannot be impl'd for the
// non-fundamental `Arc<local>` (orphan rule). `Arc<ApiEventSink>` still coerces
// straight to `Arc<dyn NodeEventSink>` because the pointee impls the trait.
impl NodeEventSink for ApiEventSink {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        // seq assignment and the broadcast send happen under one lock:
        // with an atomic counter alone, two concurrent emitters could send
        // out of seq order, breaking the gap-detection contract clients rely
        // on. `broadcast::Sender::send` is non-blocking, so holding the std
        // mutex across it is fine.
        let mut seq = match self.seq.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let frame = EventFrame {
            seq: *seq,
            event: event.to_string(),
            payload,
        };
        *seq += 1;
        // No subscribers is fine (send returns Err) — fire-and-forget.
        let _ = self.tx.send(frame);
    }
}

/// Forward frames to one WS connection until it closes. On lag, emit the
/// explicit `_lagged` marker frame and continue from the live edge.
pub async fn forward_events(
    mut rx: tokio::sync::broadcast::Receiver<EventFrame>,
    mut ws: axum::extract::ws::WebSocket,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(frame) => {
                let txt = match serde_json::to_string(&frame) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ws
                    .send(axum::extract::ws::Message::Text(txt.into()))
                    .await
                    .is_err()
                {
                    return; // client gone
                }
            }
            Err(RecvError::Lagged(missed)) => {
                let marker = serde_json::json!({
                    "seq": serde_json::Value::Null,
                    "event": "_lagged",
                    "payload": { "missed": missed },
                });
                if ws
                    .send(axum::extract::ws::Message::Text(marker.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::RecvError;

    #[tokio::test]
    async fn seq_is_monotonic_from_zero() {
        let sink = ApiEventSink::new();
        let mut rx = sink.subscribe();

        sink.emit("first", serde_json::json!({"a": 1}));
        sink.emit("second", serde_json::json!({"b": 2}));

        let f0 = rx.recv().await.expect("first frame");
        assert_eq!(f0.seq, 0);
        assert_eq!(f0.event, "first");
        assert_eq!(f0.payload, serde_json::json!({"a": 1}));

        let f1 = rx.recv().await.expect("second frame");
        assert_eq!(f1.seq, 1);
        assert_eq!(f1.event, "second");
        assert_eq!(f1.payload, serde_json::json!({"b": 2}));
    }

    #[tokio::test]
    async fn lag_is_reported_not_silent() {
        // Validates the mechanism forward_events relies on: a bounded
        // broadcast reports overwritten frames as RecvError::Lagged.
        let (tx, mut rx) = tokio::sync::broadcast::channel::<EventFrame>(1);

        tx.send(EventFrame {
            seq: 0,
            event: "a".to_string(),
            payload: serde_json::Value::Null,
        })
        .expect("send first");
        tx.send(EventFrame {
            seq: 1,
            event: "b".to_string(),
            payload: serde_json::Value::Null,
        })
        .expect("send second");

        match rx.recv().await {
            Err(RecvError::Lagged(missed)) => assert_eq!(missed, 1),
            other => panic!("expected Lagged(1), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_does_not_panic() {
        let sink = ApiEventSink::new();
        sink.emit("nobody-listening", serde_json::json!({"x": true}));
    }

    #[tokio::test]
    async fn seq_continues_across_subscribers() {
        let sink = ApiEventSink::new();

        // seq 0 emitted with no subscriber — dropped, but the counter advances.
        sink.emit("dropped", serde_json::json!(null));

        let mut rx = sink.subscribe();
        sink.emit("seen", serde_json::json!({"k": "v"}));

        let frame = rx.recv().await.expect("frame after subscribe");
        assert_eq!(
            frame.seq, 1,
            "seq is process-lifetime monotonic, not per-connection"
        );
        assert_eq!(frame.event, "seen");
    }
}
