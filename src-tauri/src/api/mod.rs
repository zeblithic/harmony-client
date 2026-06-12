// src-tauri/src/api/ — ZEB-445 localhost control surface.
//
// Mode-agnostic: hosted by `harmony-app serve` (windowless) today and by the
// GUI process (opt-in) in a follow-up. Binds 127.0.0.1 only; bearer-token
// auth on every endpoint (see auth.rs for the trust-boundary rationale).
pub mod auth;
pub mod events;
pub mod lock;
pub mod rpc;
