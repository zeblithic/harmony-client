// src-tauri/src/node_event_sink.rs
//
// ZEB-445: mode-agnostic event emission. The GUI emits to the webview via
// AppHandle; serve mode emits to the WS broadcast; a GUI instance with the
// API enabled fans out to both. Payloads are serde_json::Value at the trait
// boundary so the trait stays object-safe.

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

impl<R: tauri::Runtime> NodeEventSink for tauri::AppHandle<R> {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        // ZEB-452: GUI-mode API parity. When this process also hosts the
        // localhost API (HARMONY_API_PORT), every GUI-bound event is
        // mirrored onto the WS broadcast here — at the sink, not per
        // wrapper — so no emission site (current or future) can miss the
        // stream. ApiHost is managed in both modes; `events` is None when
        // the API is off, and try_state covers early emissions before
        // setup manages it.
        if let Some(host) = tauri::Manager::try_state::<crate::api::gui_host::ApiHost>(self) {
            if let Some(events) = &host.events {
                NodeEventSink::emit(events, event, payload.clone());
            }
        }
        // Fully-qualified call into tauri's Emitter trait — NOT a recursive
        // call into NodeEventSink::emit.
        if let Err(e) = tauri::Emitter::emit(self, event, payload) {
            tracing::warn!(event, error = %e, "tauri emit failed");
        }
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

/// Test helper: records every emitted frame for later assertions. Kept at
/// module scope (not inside `mod tests`) so later tasks' tests can use it.
#[cfg(test)]
pub(crate) struct RecordingSink {
    frames: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

#[cfg(test)]
impl RecordingSink {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            frames: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn frames(&self) -> Vec<(String, serde_json::Value)> {
        self.frames
            .lock()
            .expect("RecordingSink mutex poisoned")
            .clone()
    }
}

#[cfg(test)]
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
