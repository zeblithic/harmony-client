//! ZEB-373: dynamic mid-session iroh dial driver. Consumes `DialHint`s from the
//! resolver notify seam, dedups by node-id (bounded, FIFO-evicting), and dials each
//! newly-learned peer once through a `PeerDialer` with bounded backoff. Re-dialing a
//! peer whose dial failed, or whose transport later drops, is out of scope — that is
//! liveness/reconnection (ZEB-321 Phase 3).
use std::collections::{HashSet, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::network_health::DialTelemetry;
use crate::reachability_resolver::DialHint;

use zenoh::internal::runtime::Runtime;
use zenoh_protocol::core::{Locator, ZenohIdProto};

/// Capacity of the resolver→driver dial-hint channel. Bounded so reachability
/// discovery cannot become unbounded heap growth if a peer floods unique node-ids
/// faster than the driver drains (CodeRabbit). Generously sized to absorb a normal
/// burst (a community's worth of first-learns at once); the driver drains in O(µs)
/// per hint, so this only backs up under a genuine flood, where dropping excess
/// hints (`try_send`) is the intended back-pressure.
pub const DIAL_HINT_CHANNEL_CAP: usize = 1024;

/// Cap on the "already dialed this session" dedup set. Bounds memory under a
/// long-lived session or adversarial churn of unique node-ids (CodeAnt). A node
/// realistically dials at most its community-membership's worth of peers, well
/// under this; the oldest entries evict (FIFO) past the cap.
const DIALED_SET_CAP: usize = 4096;

/// Max dial attempts per hint (initial try + retries). A persistently-unreachable
/// peer is left for ZEB-321 Phase 3 (liveness), not retried across the session.
const MAX_DIAL_ATTEMPTS: u32 = 3;

/// Bounded dedup of node-ids already dialed this session. Membership is a `HashSet`;
/// insertion order is tracked in a `VecDeque` so the oldest node-id evicts once the
/// cap is exceeded, keeping memory bounded regardless of how many distinct peers are
/// seen.
///
/// Eviction is PURELY a memory bound — it does NOT create a re-dial path. The
/// resolver emits a `DialHint` only on first-learn of a `(owner, node_id)`, so an
/// already-known peer that re-announces does not re-emit, and an evicted node-id is
/// not re-dialed in practice (a left-then-rejoined member that we should re-dial is
/// re-dial-after-drop → ZEB-321 Phase 3, out of scope here). (Greptile, PR #190.)
struct DialedSet {
    seen: HashSet<[u8; 32]>,
    order: VecDeque<[u8; 32]>,
}

impl DialedSet {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Claim a node-id for dialing. Returns true if newly claimed (caller should
    /// dial), false if already present (caller skips it as a duplicate).
    fn claim(&mut self, id: [u8; 32]) -> bool {
        if !self.seen.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > DIALED_SET_CAP {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

/// Abstraction over "dial this iroh peer". Production wraps a zenoh `Runtime`
/// (`connect_peer`); tests use a mock. `locator` is `iroh/<hex>`.
#[async_trait::async_trait]
pub trait PeerDialer: Send + Sync {
    async fn dial(&self, node_id: [u8; 32], locator: String) -> bool;
}

fn iroh_locator(node_id: &[u8; 32]) -> String {
    format!("iroh/{}", hex::encode(node_id))
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

/// Run the dial driver until the hint channel closes (node stop drops the resolver's
/// sender). `backoff_base` is the first retry delay (doubles each retry); tests pass
/// ZERO.
pub async fn run_dial_driver(
    mut rx: tokio::sync::mpsc::Receiver<DialHint>,
    dialer: Arc<dyn PeerDialer>,
    telemetry: Arc<DialTelemetry>,
    self_node_id: [u8; 32],
    backoff_base: Duration,
) {
    let dialed = Arc::new(Mutex::new(DialedSet::new()));
    while let Some(hint) = rx.recv().await {
        if hint.node_id == self_node_id {
            tracing::debug!("ZEB-373: skip dial to self");
            continue;
        }
        if !dialed.lock().expect("dialed set lock").claim(hint.node_id) {
            telemetry.record_skipped_duplicate();
            continue;
        }
        let dialer = Arc::clone(&dialer);
        let telemetry = Arc::clone(&telemetry);
        tokio::spawn(async move {
            let loc = iroh_locator(&hint.node_id);
            // Count EVERY actual dial() call (incl. retries) so the attempts metric
            // matches real network operations, not hints handled (CodeAnt).
            telemetry.record_attempt();
            let mut ok = dialer.dial(hint.node_id, loc.clone()).await;
            let mut attempts = 1u32;
            let mut delay = backoff_base;
            while !ok && attempts < MAX_DIAL_ATTEMPTS {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                delay = delay.saturating_mul(2);
                telemetry.record_attempt();
                ok = dialer.dial(hint.node_id, loc.clone()).await;
                attempts += 1;
            }
            if ok {
                telemetry.record_succeeded(hint.node_id, hint.owner.0);
                tracing::info!(
                    "ZEB-373: dialed iroh peer {}",
                    hex::encode(&hint.node_id[..4])
                );
            } else {
                // Terminal for this session: the node-id stays claimed (no re-dial).
                // A persistently-unreachable peer is reconnected by ZEB-321 Phase 3
                // (liveness/rebinding), not retried here — ZEB-373 stays dial-once.
                telemetry.record_failed(hint.node_id, hint.owner.0);
                tracing::warn!(
                    "ZEB-373: dial failed ({MAX_DIAL_ATTEMPTS} attempts) for {}",
                    hex::encode(&hint.node_id[..4])
                );
            }
        });
    }
    tracing::debug!("ZEB-373: dial driver stopping (hint channel closed)");
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
    use crate::owner_state_types::OwnerAddr;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockDialer {
        calls: AtomicU32,
        fail_first_n: u32,
    }
    #[async_trait::async_trait]
    impl PeerDialer for MockDialer {
        async fn dial(&self, _node_id: [u8; 32], _locator: String) -> bool {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            n >= self.fail_first_n
        }
    }

    fn hint(node_id: u8) -> DialHint {
        DialHint {
            node_id: [node_id; 32],
            owner: OwnerAddr([0xAA; 16]),
        }
    }

    #[test]
    fn dialed_set_evicts_oldest_past_cap() {
        let mut d = DialedSet::new();
        // Fill to capacity with distinct node-ids.
        for i in 0..DIALED_SET_CAP {
            let mut id = [0u8; 32];
            id[0] = (i & 0xff) as u8;
            id[1] = (i >> 8) as u8;
            assert!(d.claim(id), "first claim is new");
        }
        assert_eq!(d.seen.len(), DIALED_SET_CAP);
        let first = {
            let mut id = [0u8; 32];
            id[0] = 0;
            id[1] = 0;
            id
        };
        assert!(!d.claim(first), "still present before overflow");
        // One more distinct id overflows the cap and evicts the oldest.
        let overflow = [0xFFu8; 32];
        assert!(d.claim(overflow), "overflow id is new");
        assert_eq!(d.seen.len(), DIALED_SET_CAP, "size stays capped");
        // The oldest (first) was evicted, so it can be claimed afresh at the set
        // level. (This is the memory-bound contract only — in production the
        // resolver's first-learn guard means a known peer won't re-emit a hint, so
        // eviction does not re-dial; see the DialedSet doc comment.)
        assert!(d.claim(first), "oldest evicted → reclaimable");
    }

    #[tokio::test]
    async fn dials_new_peer_once_and_skips_self_and_duplicates() {
        let dialer = Arc::new(MockDialer {
            calls: AtomicU32::new(0),
            fail_first_n: 0,
        });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let self_id = [0xEE; 32];
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            self_id,
            Duration::ZERO,
        ));
        tx.send(hint(0x11)).await.unwrap();
        tx.send(hint(0x11)).await.unwrap();
        tx.send(DialHint {
            node_id: self_id,
            owner: OwnerAddr([0xAA; 16]),
        })
        .await
        .unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.attempts, 1, "one real dial attempt");
        assert_eq!(s.succeeded, 1);
        assert_eq!(s.skipped_duplicate, 1);
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_then_succeeds_within_three_attempts() {
        let dialer = Arc::new(MockDialer {
            calls: AtomicU32::new(0),
            fail_first_n: 2,
        });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            [0xEE; 32],
            Duration::ZERO,
        ));
        tx.send(hint(0x22)).await.unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.succeeded, 1);
        // attempts now counts each dial() call, including the 2 retries (CodeAnt).
        assert_eq!(s.attempts, 3, "3 dial attempts recorded");
        assert_eq!(
            dialer.calls.load(Ordering::SeqCst),
            3,
            "3 dial calls: fail,fail,succeed"
        );
    }

    #[tokio::test]
    async fn exhausted_failure_is_terminal_no_redial() {
        let dialer = Arc::new(MockDialer {
            calls: AtomicU32::new(0),
            fail_first_n: 100,
        });
        let telemetry = Arc::new(DialTelemetry::new());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let driver = tokio::spawn(run_dial_driver(
            rx,
            dialer.clone(),
            telemetry.clone(),
            [0xEE; 32],
            Duration::ZERO,
        ));
        tx.send(hint(0x33)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A second hint for the SAME node-id is skipped (still claimed): a failed
        // dial is terminal for the session, not re-armed (cross-refresh retry is
        // ZEB-321 Phase 3).
        tx.send(hint(0x33)).await.unwrap();
        drop(tx);
        driver.await.unwrap();
        let s = telemetry.summary();
        assert_eq!(s.failed, 1, "one terminal failure");
        assert_eq!(s.skipped_duplicate, 1, "second hint skipped as duplicate");
        assert_eq!(s.attempts, 3, "3 dial attempts from the single round");
        assert_eq!(dialer.calls.load(Ordering::SeqCst), 3, "no re-dial");
    }

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
