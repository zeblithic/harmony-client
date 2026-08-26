//! Wire-protocol versioning primitives (ZEB-623).
//!
//! Two independent evolution mechanisms live here — an ALPN *generation* bump
//! (rare, wire-incompatible) and an in-protocol *hello* frame (common, additive
//! feature negotiation) — plus the fleet `MIN_SUPPORTED_*` policy constants and
//! a registry that surfaces peer incompatibility loudly instead of failing a
//! connect silently. The design rationale, the N/N-1 fleet rule, the additive
//! payload conventions, and the tunnel-v2 exemplar are written up in
//! `docs/specs/2026-07-03-zeb-623-protocol-versioning-design.md`.

use std::collections::HashMap;
use std::sync::Mutex;

/// Current tunnel ALPN *generation* the dialer prefers (matches the numeric
/// suffix of [`crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V2`]). Bumping this
/// mints a new `/vN` ALPN string; see the module doc and spec for when that is
/// warranted (wire-incompatible framing only).
pub const TUNNEL_ALPN_GENERATION: u16 = 2;

/// Oldest tunnel ALPN generation this build still *accepts* on the bind list.
/// N/N-1 fleet rule: while `MIN < CURRENT`, both `/v{MIN..=CURRENT}` ALPNs stay
/// registered so a one-generation-behind peer can still connect during the
/// deprecation window.
pub const MIN_SUPPORTED_TUNNEL_ALPN_GENERATION: u16 = 1;

/// Version carried in the tunnel [`TunnelHello`] frame — the *feature* rate of
/// change, orthogonal to the ALPN generation. New optional capabilities bump
/// this without minting a new ALPN.
pub const TUNNEL_PROTOCOL_VERSION: u16 = 1;

/// Oldest tunnel hello `protocol_version` this build interoperates with. A
/// hello below this is reported (loudly, via [`ProtocolCompatRegistry`]) rather
/// than silently dropped.
pub const MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION: u16 = 1;

/// Hard cap on an encoded [`TunnelHello`] frame. [`decode_hello`] refuses any
/// input above this before parsing, so a hostile peer can't force an unbounded
/// allocation on the first frame.
pub const TUNNEL_HELLO_MAX: usize = 1024;

/// First frame each side sends on a freshly opened tunnel stream. `capabilities`
/// is an additive bitmap: unknown bits are ignored, so a newer peer's extra
/// features never break an older peer. New fields MUST be `#[serde(default)]`
/// (see the spec's additive-payload rule) — never `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TunnelHello {
    /// Feature version of the tunnel protocol the sender speaks.
    pub protocol_version: u16,
    /// Additive capability bitmap; unknown bits are ignored by the receiver.
    #[serde(default)]
    pub capabilities: u64,
}

impl TunnelHello {
    /// The hello this build advertises: [`TUNNEL_PROTOCOL_VERSION`] with no
    /// optional capabilities set yet.
    pub fn current() -> Self {
        Self {
            protocol_version: TUNNEL_PROTOCOL_VERSION,
            capabilities: 0,
        }
    }
}

/// Encode a [`TunnelHello`] to CBOR bytes (no length prefix; the caller frames
/// it). Mirrors the `ciborium::into_writer` idiom in `iroh_friend_acceptor`.
pub fn encode_hello(h: &TunnelHello) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    ciborium::into_writer(h, &mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Decode a [`TunnelHello`] from CBOR bytes, bounding the input at
/// [`TUNNEL_HELLO_MAX`] before parsing. Unknown fields are tolerated and a
/// missing `capabilities` defaults to `0`, so a v-next hello still decodes.
pub fn decode_hello(bytes: &[u8]) -> Result<TunnelHello, String> {
    if bytes.len() > TUNNEL_HELLO_MAX {
        return Err(format!(
            "tunnel hello {} bytes exceeds max {}",
            bytes.len(),
            TUNNEL_HELLO_MAX
        ));
    }
    ciborium::from_reader(bytes).map_err(|e| e.to_string())
}

/// Compatibility gate for a received hello. `Err(reason)` when the peer's
/// `protocol_version` is below [`MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION`]; a
/// version newer than ours is always compatible (unknown capability bits are
/// ignored). The returned reason is what the caller records in
/// [`ProtocolCompatRegistry`] and surfaces in Network Health.
pub fn check_hello_compatible(h: &TunnelHello) -> Result<(), String> {
    if h.protocol_version < MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION {
        return Err(format!(
            "tunnel hello v{} < min supported v{}",
            h.protocol_version, MIN_SUPPORTED_TUNNEL_PROTOCOL_VERSION
        ));
    }
    Ok(())
}

/// Per-peer record of protocol incompatibility so the failure is *visible*
/// (Network Health) rather than a silent connect drop. Keyed by the peer's
/// 32-byte IROH EndpointId (the ed25519 endpoint key Network Health joins on) —
/// NOT the tunnel node id (`blake3(ML-DSA pubkey)`); the initiator records under
/// this key so the reader in `network_health.rs` (which looks up by
/// `record.iroh_node_id`) finds the entry. An entry present means "we could not
/// speak a compatible protocol with this peer, for this reason".
#[derive(Default)]
pub struct ProtocolCompatRegistry {
    inner: Mutex<HashMap<[u8; 32], String>>,
}

impl ProtocolCompatRegistry {
    /// Record (and loudly log) that `node_id` is protocol-incompatible. This is
    /// the LOUD path the N/N-1 policy mandates — the reason is retained for
    /// Network Health to display.
    pub fn note_incompatible(&self, node_id: [u8; 32], reason: String) {
        tracing::warn!(
            node_id = %hex::encode(node_id),
            reason = %reason,
            "peer speaks an incompatible protocol generation; surfacing in network health"
        );
        self.inner
            .lock()
            .expect("protocol compat registry mutex poisoned")
            .insert(node_id, reason);
    }

    /// Clear any incompatibility record for `node_id` (e.g. after a successful
    /// compatible handshake on a re-dial).
    pub fn note_compatible(&self, node_id: [u8; 32]) {
        self.inner
            .lock()
            .expect("protocol compat registry mutex poisoned")
            .remove(&node_id);
    }

    /// The recorded incompatibility reason for `node_id`, if any.
    pub fn incompat_reason(&self, node_id: &[u8; 32]) -> Option<String> {
        self.inner
            .lock()
            .expect("protocol compat registry mutex poisoned")
            .get(node_id)
            .cloned()
    }
}

/// ZEB-739 Seam B: bridge the crate-owned `CompatSink` the tunnel driver reports
/// through onto this concrete registry (the Network-Health read side). The driver
/// emits exactly one [`HandshakeOutcome`](crate::tunnel_manager::HandshakeOutcome)
/// per peer — keyed by the peer's IROH EndpointId (the Network Health join key) —
/// at the two sites the client formerly called `note_incompatible` /
/// `note_compatible` directly (incompatible-hello rejection; successful
/// handshake, which clears).
impl crate::tunnel_manager::CompatSink for ProtocolCompatRegistry {
    fn record_handshake_outcome(
        &self,
        peer: [u8; 32],
        outcome: crate::tunnel_manager::HandshakeOutcome,
    ) {
        match outcome {
            crate::tunnel_manager::HandshakeOutcome::Compatible => self.note_compatible(peer),
            crate::tunnel_manager::HandshakeOutcome::Incompatible { reason } => {
                self.note_incompatible(peer, reason)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrips_via_cbor() {
        let h = TunnelHello {
            protocol_version: 1,
            capabilities: 0b101,
        };
        let bytes = encode_hello(&h).unwrap();
        assert!(bytes.len() < TUNNEL_HELLO_MAX);
        assert_eq!(decode_hello(&bytes).unwrap(), h);
    }

    #[test]
    fn hello_decode_tolerates_unknown_fields_and_missing_capabilities() {
        // Future-proofing: a v-next hello with extra fields decodes; capabilities defaults.
        let mut extended = std::collections::BTreeMap::new();
        extended.insert(
            "protocol_version".to_string(),
            ciborium::Value::Integer(7.into()),
        );
        extended.insert(
            "some_future_field".to_string(),
            ciborium::Value::Text("x".into()),
        );
        let mut bytes = Vec::new();
        ciborium::into_writer(
            &ciborium::Value::Map(
                extended
                    .into_iter()
                    .map(|(k, v)| (ciborium::Value::Text(k), v))
                    .collect(),
            ),
            &mut bytes,
        )
        .unwrap();
        let h = decode_hello(&bytes).unwrap();
        assert_eq!(h.protocol_version, 7);
        assert_eq!(h.capabilities, 0);
    }

    #[test]
    fn decode_hello_rejects_oversized_frame() {
        assert!(decode_hello(&vec![0u8; TUNNEL_HELLO_MAX + 1]).is_err());
    }

    #[test]
    fn check_hello_rejects_below_min_supported() {
        assert!(check_hello_compatible(&TunnelHello {
            protocol_version: 0,
            capabilities: 0
        })
        .is_err());
        assert!(check_hello_compatible(&TunnelHello::current()).is_ok());
        // A NEWER version than ours is compatible (unknown capability bits ignored).
        assert!(check_hello_compatible(&TunnelHello {
            protocol_version: u16::MAX,
            capabilities: u64::MAX
        })
        .is_ok());
    }

    #[test]
    fn registry_note_and_clear() {
        let r = ProtocolCompatRegistry::default();
        let id = [7u8; 32];
        assert_eq!(r.incompat_reason(&id), None);
        r.note_incompatible(id, "tunnel hello v0 < min 1".into());
        assert_eq!(
            r.incompat_reason(&id).as_deref(),
            Some("tunnel hello v0 < min 1")
        );
        r.note_compatible(id);
        assert_eq!(r.incompat_reason(&id), None);
    }

    #[test]
    fn tunnel_alpn_generations_cover_n_minus_1() {
        // N/N-1 pin: both generations remain registered while MIN < CURRENT.
        assert_eq!(
            crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V1,
            b"harmony/tunnel/v1"
        );
        assert_eq!(
            crate::iroh_endpoint::alpn::HARMONY_TUNNEL_V2,
            b"harmony/tunnel/v2"
        );
        const { assert!(MIN_SUPPORTED_TUNNEL_ALPN_GENERATION <= TUNNEL_ALPN_GENERATION) };
    }
}
