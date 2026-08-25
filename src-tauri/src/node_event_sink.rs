// src-tauri/src/node_event_sink.rs
//
// ZEB-445: mode-agnostic event emission. The object-safe trait, the
// serialize-then-emit helper, and the fan-out combinator now live in
// `harmony-foundation` (ZEB-548 Stage 1) so feature crates can emit without a
// back-dependency on `harmony-app` or Tauri; they are re-exported here so the
// ~20 in-crate `crate::node_event_sink::*` call sites resolve unchanged.
//
// What stays in the binary are the two sinks that need the live Tauri/API
// runtime: `AppHandleSink` (webview emission — an orphan-rule newtype over the
// foreign `AppHandle` type) and the `NodeEventSink` impl on the API firehose
// (`api::events::ApiEventSink`).

pub use harmony_foundation::node_event_sink::{emit_ser, FanoutSink, NodeEventSink};

// The recorder is a foundation `test-fixtures` export (its sink impl is on
// `Arc<RecordingSink>`, only legal where the trait is local). `pub use` — not
// `pub(crate)` — so the feature-on non-test lib target does not trip
// `unused_imports` under `clippy --all-targets --features test-fixtures`.
#[cfg(any(test, feature = "test-fixtures"))]
pub use harmony_foundation::node_event_sink::RecordingSink;

/// ZEB-445 / ZEB-452: the webview event sink. A newtype over `AppHandle`
/// because `NodeEventSink` now lives in `harmony-foundation`: the impl for the
/// foreign `AppHandle` type cannot live here without a local wrapper (orphan
/// rule). Wrap an `AppHandle` in this to use it as an `Arc<dyn NodeEventSink>`.
pub(crate) struct AppHandleSink<R: tauri::Runtime>(pub tauri::AppHandle<R>);

impl<R: tauri::Runtime> NodeEventSink for AppHandleSink<R> {
    fn emit(&self, event: &str, payload: serde_json::Value) {
        // ZEB-452: GUI-mode API parity. When this process also hosts the
        // localhost API (HARMONY_API_PORT), every GUI-bound event is
        // mirrored onto the WS broadcast here — at the sink, not per
        // wrapper — so no emission site (current or future) can miss the
        // stream. ApiHost is managed in both modes; `events` is None when
        // the API is off, and try_state covers early emissions before
        // setup manages it.
        if let Some(host) = tauri::Manager::try_state::<crate::api::gui_host::ApiHost>(&self.0) {
            if let Some(events) = &host.events {
                // `events` is `Arc<ApiEventSink>`; the sink impl is on the
                // bare `ApiEventSink`, so this method call auto-derefs.
                events.emit(event, payload.clone());
            }
        }
        // Fully-qualified call into tauri's Emitter trait — NOT a recursive
        // call into NodeEventSink::emit.
        if let Err(e) = tauri::Emitter::emit(&self.0, event, payload) {
            tracing::warn!(event, error = %e, "tauri emit failed");
        }
    }
}
