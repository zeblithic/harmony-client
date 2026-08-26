//! ZEB-473 (DM-over-iroh, Move 1a): per-connection async driver bridging an
//! iroh QUIC bi-stream to a `harmony_tunnel::TunnelSession`.
//!
//! ZEB-739 (iroh-tier extraction): the driver — the initiator/responder
//! handshake, the read/write/keepalive loop, the 4-byte big-endian framing, and
//! the `TunnelHello` (v2) negotiation — now lives in the reusable
//! [`harmony_tunnel_iroh`] crate. This module is a thin re-export so
//! `crate::tunnel_task::{run_tunnel_initiator, run_tunnel_responder}` keeps
//! resolving for the client's tunnel acceptor (`iroh_tunnel_acceptor`) and the
//! re-exported `TunnelManager`'s dial paths. The wire framing is preserved
//! byte-for-byte in the core crate (it was seeded from this module).

pub use harmony_tunnel_iroh::{run_tunnel_initiator, run_tunnel_responder};
