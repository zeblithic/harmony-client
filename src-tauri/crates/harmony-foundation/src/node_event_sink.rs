//! Mode-agnostic event emission (ZEB-445), extracted to the foundation tier so
//! feature crates can emit UI/API events without a back-dependency on
//! `harmony-app` or Tauri. This crate owns only the object-safe trait, the
//! serialize-then-emit helper, and the fan-out combinator; the concrete sinks
//! that need the live runtime (the webview `AppHandle` sink, the WS-firehose
//! `ApiEventSink`) stay in `harmony-app`.
//!
//! Payloads are `serde_json::Value` at the trait boundary so the trait stays
//! object-safe.

use serde::Serialize;

pub trait NodeEventSink: Send + Sync + 'static {
    fn emit(&self, event: &str, payload: serde_json::Value);
}

/// Serialize-then-emit helper. Serialization failure is logged and dropped —
/// emission is fire-and-forget everywhere today (`let _ = app.emit(...)`).
pub fn emit_ser<T: Serialize>(sink: &dyn NodeEventSink, event: &str, payload: &T) {
    match serde_json::to_value(payload) {
        Ok(v) => sink.emit(event, v),
        Err(e) => tracing::warn!(event, error = %e, "event payload serialization failed"),
    }
}

/// Fan-out to several sinks (GUI + API simultaneously).
pub struct FanoutSink(pub Vec<std::sync::Arc<dyn NodeEventSink>>);

impl NodeEventSink for FanoutSink {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        for s in &self.0 {
            s.emit(event, payload.clone());
        }
    }
}

/// Test helper: records every emitted frame for later assertions. Exposed under
/// the `test-fixtures` feature so `harmony-app` (whose ~60 in-crate test sites
/// share it) can reach it across the crate boundary. The impl is on
/// `Arc<RecordingSink>` so a handle is both recordable (`.frames()`) and —
/// wrapped once more in `Arc` — usable as a `dyn NodeEventSink`.
#[cfg(any(test, feature = "test-fixtures"))]
pub struct RecordingSink {
    frames: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

#[cfg(any(test, feature = "test-fixtures"))]
impl RecordingSink {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            frames: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn frames(&self) -> Vec<(String, serde_json::Value)> {
        self.frames
            .lock()
            .expect("RecordingSink mutex poisoned")
            .clone()
    }
}

#[cfg(any(test, feature = "test-fixtures"))]
impl NodeEventSink for std::sync::Arc<RecordingSink> {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        self.frames
            .lock()
            .expect("RecordingSink mutex poisoned")
            .push((event.to_string(), payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn emit_ser_preserves_camel_case_dto_shape() {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct P {
            some_field: u32,
        }

        let rec = RecordingSink::new();
        emit_ser(&rec, "x", &P { some_field: 7 });

        let frames = rec.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, "x");
        assert_eq!(frames[0].1["someField"], 7);
    }

    #[test]
    fn fanout_delivers_to_all_sinks_in_order() {
        let a = RecordingSink::new();
        let b = RecordingSink::new();
        let fan = FanoutSink(vec![
            Arc::new(a.clone()) as Arc<dyn NodeEventSink>,
            Arc::new(b.clone()) as Arc<dyn NodeEventSink>,
        ]);

        fan.emit("ev", serde_json::json!({"k": 1}));

        for rec in [&a, &b] {
            let frames = rec.frames();
            assert_eq!(frames.len(), 1, "each sink receives exactly one frame");
            assert_eq!(frames[0].0, "ev");
            assert_eq!(frames[0].1, serde_json::json!({"k": 1}));
        }
    }
}
