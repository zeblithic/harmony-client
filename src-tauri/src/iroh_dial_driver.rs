//! ZEB-373 / ZEB-620: iroh dial primitives.
//!
//! This module once housed the dial-once `run_dial_driver` (ZEB-373): a driver
//! that consumed resolver `DialHint`s, deduped by node-id, and dialed each
//! newly-learned peer a fixed number of times before giving up for the session.
//! ZEB-620 replaced that driver with the [`crate::reconnect_supervisor`], a
//! per-peer reconnect state machine that owns *all* scheduling (first-learn,
//! record change, drop, presence sweep) with a jittered backoff ladder. The
//! resolver now kicks the supervisor directly; the `DialHint` mpsc is gone.
//!
//! What remains here is the dial *mechanism* the supervisor drives:
//! - [`PeerDialer`] — the "dial this iroh peer" abstraction (mocked in tests).
//! - [`RuntimePeerDialer`] — the production dialer, over a live zenoh `Runtime`'s
//!   `connect_peer`.
//! - [`deterministic_zid_hex`] — the ZEB-390/ZEB-455 node-id→zenoh-zid derivation
//!   shared by a node's own `config["id"]` and every dial target, so
//!   `connect_peer`'s post-handshake transport lookup matches.

use std::str::FromStr;

use zenoh::internal::runtime::Runtime;
use zenoh_protocol::core::{Locator, ZenohIdProto};

/// Abstraction over "dial this iroh peer". Production wraps a zenoh `Runtime`
/// (`connect_peer`); tests use a mock. `locator` is `iroh/<hex>`.
#[async_trait::async_trait]
pub trait PeerDialer: Send + Sync {
    async fn dial(&self, node_id: [u8; 32], locator: String) -> bool;
}

/// ZEB-390: derive the deterministic zenoh `ZenohIdProto` **hex string** for a
/// node from its 32-byte iroh `EndpointId`. A `ZenohId` is at most 16 bytes
/// ([`ZenohIdProto::MAX_SIZE`]), so we take the first 16 bytes of the node-id —
/// an Ed25519 public key, uniformly random, so a 16-byte-prefix collision across
/// a community is ~2^-128.
///
/// Both a node's OWN zenoh session id (`config["id"]`, set in `event_loop::run`
/// before `zenoh::open`) and the dialer's `connect_peer` target zid (below) are
/// derived through THIS function and parsed via the SAME `ZenohIdProto::from_str`
/// (zenoh's `config::ZenohId::from_str` delegates straight to it), so the two
/// sides are byte-identical regardless of zenoh's internal id endianness. That
/// equality is what makes `connect_peer`'s post-handshake
/// `get_transport_unicast(zid)` lookup actually find the peer.
pub fn deterministic_zid_hex(node_id: &[u8; 32]) -> String {
    // ZEB-455: zenoh's `ZenohId` is a VALUE, not a fixed-width byte string — its
    // canonical hex (what `ZenohIdProto::from_str` accepts and `session.zid()`
    // reports) has NO leading zeros ("Leading 0s are not valid"). `hex::encode`
    // emits fixed-width 32-char hex, so a 16-byte prefix beginning with a zero
    // nibble (~1/16 of identities) would be REJECTED by `zenoh::open` — killing
    // transport for that node entirely. Strip leading-zero nibbles to the
    // canonical form. Both consumers (`config["id"]` and the dialer's
    // `connect_peer` target) go through this one function, so they stay equal —
    // and now also equal `session.zid()`, which zenoh always reports stripped.
    let hex = hex::encode(&node_id[..16]);
    let stripped = hex.trim_start_matches('0');
    // All-zero 16-byte prefix is unreachable for a real Ed25519 key (~2^-128);
    // keep one nibble so the id is never an empty string.
    if stripped.is_empty() {
        "0".to_string()
    } else {
        stripped.to_string()
    }
}

/// Production `PeerDialer`: dials through the live zenoh `Runtime`'s
/// `connect_peer`. ZEB-390: the target zid is DETERMINISTIC — derived from the
/// peer's iroh node-id via [`deterministic_zid_hex`] — not a random placeholder.
/// `connect_peer` reports success by looking up a transport under the zid we pass
/// AFTER the link handshake (zenoh registers the transport under the peer's
/// wire-negotiated zid), so the zid we pass MUST equal the zid the peer set for
/// itself (every node sets `config["id"]` from its own node-id; see
/// `event_loop::run`). The previous `ZenohIdProto::rand()` placeholder never
/// matched the wire zid, so `connect_peer` always returned `false` — the dial
/// was reported as failed even when the iroh link opened cleanly.
pub struct RuntimePeerDialer {
    runtime: Runtime,
}
impl RuntimePeerDialer {
    pub fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }
}
#[async_trait::async_trait]
impl PeerDialer for RuntimePeerDialer {
    async fn dial(&self, node_id: [u8; 32], locator: String) -> bool {
        let loc = match locator.parse::<Locator>() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("ZEB-373: bad iroh locator {locator}: {e}");
                return false;
            }
        };
        // ZEB-390: target the peer's DETERMINISTIC zid (derived from its iroh
        // node-id), not a random placeholder — see `deterministic_zid_hex`.
        let zid_hex = deterministic_zid_hex(&node_id);
        let zid = match ZenohIdProto::from_str(&zid_hex) {
            Ok(z) => z,
            Err(e) => {
                tracing::warn!("ZEB-390: bad derived zid {zid_hex}: {e}");
                return false;
            }
        };
        self.runtime.connect_peer(&zid, &[loc]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ZEB-390: the derived zid hex is deterministic (stable per node-id),
    /// exactly 16 bytes wide, and distinct for node-ids that differ within their
    /// first 16 bytes.
    #[test]
    fn deterministic_zid_hex_is_stable_and_distinct() {
        let a = [0x11u8; 32];
        let mut b = [0x11u8; 32];
        b[15] = 0x22; // differs within the first 16 bytes
        let mut tail_only = [0x11u8; 32];
        tail_only[16] = 0x99; // differs ONLY past byte 16 → same 16-byte prefix

        assert_eq!(
            deterministic_zid_hex(&a),
            deterministic_zid_hex(&a),
            "stable for the same node-id"
        );
        assert_eq!(
            deterministic_zid_hex(&a).len(),
            32,
            "16 bytes -> 32 hex chars (ZenohIdProto::MAX_SIZE)"
        );
        assert_ne!(
            deterministic_zid_hex(&a),
            deterministic_zid_hex(&b),
            "differing 16-byte prefixes must yield distinct zids"
        );
        assert_eq!(
            deterministic_zid_hex(&a),
            deterministic_zid_hex(&tail_only),
            "only the first 16 bytes are significant"
        );
    }

    /// ZEB-390 load-bearing invariant: the zid the dialer passes to
    /// `connect_peer` (via `ZenohIdProto::from_str`) must equal the zid a node
    /// derives for its own `config["id"]` (via `zenoh::config::ZenohId::from_str`,
    /// which delegates to the same `ZenohIdProto::from_str`). If these ever
    /// diverged, `connect_peer`'s post-handshake transport lookup would miss and
    /// every dynamic dial would be reported as failed — the original ZEB-390 bug.
    #[test]
    fn config_id_and_dialer_derive_equal_zids() {
        let node_id = [0xABu8; 32];
        let hex = deterministic_zid_hex(&node_id);

        // What the dialer passes to connect_peer:
        let dialer_zid = ZenohIdProto::from_str(&hex).expect("dialer zid parses");

        // What zenoh derives from config["id"] = "<hex>":
        let config_zid: ZenohIdProto = zenoh::config::ZenohId::from_str(&hex)
            .expect("config zid parses")
            .into();

        assert_eq!(
            dialer_zid, config_zid,
            "config-derived zid must equal the dialer's connect_peer target"
        );
    }

    /// ZEB-455: zenoh's `ZenohIdProto::from_str` REJECTS leading-zero hex
    /// ("Leading 0s are not valid"), and `session.zid()` reports the stripped
    /// canonical form. A node whose 16-byte iroh-id prefix starts with a zero
    /// nibble (`node_id[0] < 0x10`, ~1/16 of identities) must STILL derive a zid
    /// zenoh accepts — otherwise `config.insert_json5("id", …)` →
    /// `zenoh::open` fails and the node has no transport at all (and its dial
    /// target mis-parses on every peer).
    #[test]
    fn deterministic_zid_hex_strips_leading_zeros_for_zenoh() {
        let mut node_id = [0x11u8; 32];
        node_id[0] = 0x0a; // -> hex begins "0a…"
        let hex = deterministic_zid_hex(&node_id);
        assert!(
            !hex.starts_with('0'),
            "leading-zero nibble must be stripped to match zenoh's canonical id: {hex}"
        );
        // The load-bearing assertion: zenoh must accept it. This is what both
        // `config["id"]` and the dialer's `connect_peer` target parse through.
        ZenohIdProto::from_str(&hex).expect("zenoh must accept the derived zid");
        assert_eq!(hex, deterministic_zid_hex(&node_id), "still deterministic");
        // config-derived == dialer-derived still holds for a leading-zero id.
        let config_zid: ZenohIdProto = zenoh::config::ZenohId::from_str(&hex)
            .expect("config zid parses")
            .into();
        assert_eq!(
            ZenohIdProto::from_str(&hex).unwrap(),
            config_zid,
            "config-derived zid equals the dialer target for a leading-zero node-id too"
        );
    }

    /// ZEB-455: distinct node-ids that differ only in a leading-zero nibble must
    /// still yield DISTINCT zids after stripping (stripping is on the value, not
    /// a lossy truncation).
    #[test]
    fn leading_zero_stripping_preserves_distinctness() {
        let mut a = [0x11u8; 32];
        a[0] = 0x0a;
        let mut b = [0x11u8; 32];
        b[0] = 0xa0; // same nibbles, different byte → different value
        assert_ne!(
            deterministic_zid_hex(&a),
            deterministic_zid_hex(&b),
            "0a… and a0… must not collide after leading-zero stripping"
        );
    }
}
