//! harmony-mail — ZEB-548 Stage 1 (PR #6).
//!
//! The mail store ([`mail`]) + the mail-sync engine ([`mail_sync`]), extracted
//! from `harmony-app`. A leaf: it depends only downward on `harmony-foundation`
//! (`node_event_sink`), `harmony-identity-crypto` (`device_dataset_file` /
//! `recoverable_load`), `harmony-runtime-ipc` (`FetchRequest`), and
//! `harmony-mailbox` (message wire types). No Tauri, no back-reference to
//! `harmony-app`.
//!
//! The module bodies are unchanged from their `harmony-app` form: the leaf
//! dependencies are re-exported below under the same `crate::…` paths the
//! modules already use internally, mirroring how `harmony-app` itself re-exports
//! them. `harmony-app` in turn re-exports `mail` / `mail_sync` so its
//! `crate::mail::*` / `crate::mail_sync::*` call sites resolve unchanged.

pub use harmony_foundation::node_event_sink;
pub use harmony_identity_crypto::{device_dataset_file, recoverable_load};

/// `harmony-app` re-exports `FetchRequest` from its own `event_loop` module;
/// mirror that path here so `mail_sync`'s `crate::event_loop::FetchRequest`
/// resolves to the same `harmony-runtime-ipc` type without editing its body.
pub mod event_loop {
    pub use harmony_runtime_ipc::FetchRequest;
}

pub mod mail;
pub mod mail_sync;
