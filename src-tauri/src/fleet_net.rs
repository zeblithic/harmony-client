//! fleet-net-v1: per-device network info (iroh endpoint + home relay) and
//! owner-level pinned-butler setting, fleet-replicated via FleetSyncEngine
//! (ZEB-418 P2). Feeds the butler-set advertisement in the owner's pkarr
//! routing record. See spec §5–§6.

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Skip-serializing sentinel: true when `h` was never set (zero wall_ms,
/// zero logical, empty device_id). Used to omit `pinned_at` from the
/// canonical CBOR map when the owner has never pinned a device.
pub fn hlc_is_unset(h: &Hlc) -> bool {
    h.wall_ms == 0 && h.logical == 0 && h.device_id.is_empty()
}

/// Serde default constructor for `Hlc`: returns the zero/never-set sentinel.
/// Used by `#[serde(default = "default_hlc")]` on `FleetNetDoc::pinned_at`.
fn default_hlc() -> Hlc {
    Hlc {
        wall_ms: 0,
        logical: 0,
        device_id: String::new(),
    }
}

/// One device's current network coordinates. Keyed in `FleetNetDoc::devices`
/// by the SP1 64-hex device ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNetRow {
    /// iroh EndpointID (transport key — NOT the identity key).
    #[serde(rename = "ep")]
    pub iroh_endpoint_id: [u8; 32],
    #[serde(rename = "hr")]
    pub home_relay: String,
    /// LWW stamp for this row; also the staleness clock.
    #[serde(rename = "sa")]
    pub seen_at: Hlc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNetDoc {
    /// Keyed by SP1 64-hex device id (same form as DmInboxEntry.deposited_by).
    #[serde(rename = "dv")]
    pub devices: BTreeMap<String, FleetNetRow>,
    /// Owner-level pinned butler device (64-hex), LWW by `pinned_at`.
    #[serde(rename = "pn", default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
    /// LWW stamp for `pinned`. Zero/default when never set.
    #[serde(
        rename = "pa",
        default = "default_hlc",
        skip_serializing_if = "hlc_is_unset"
    )]
    pub pinned_at: Hlc,
}

impl Default for FleetNetDoc {
    fn default() -> Self {
        FleetNetDoc {
            devices: BTreeMap::new(),
            pinned: None,
            pinned_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: String::new(),
            },
        }
    }
}

// Manual CanonicalPayload registration: the `impl_canonical!` macro in
// owner_state_types.rs is module-private, so we register these types with the
// two impls the macro expands to (mirroring `dm_outhold` and `dm_inbox_crdt`).
impl CanonicalPayloadSealed for FleetNetRow {}
impl CanonicalPayload for FleetNetRow {}
impl CanonicalPayloadSealed for FleetNetDoc {}
impl CanonicalPayload for FleetNetDoc {}

impl FleetNetDoc {
    /// LWW row merge: a remote row replaces the local row only when
    /// `remote.seen_at.is_strictly_newer_than(&local.seen_at)`.
    /// Absent keys are inserted. `pinned`+`pinned_at` merge as an LWW PAIR
    /// by `pinned_at` (remote strictly newer → take both fields).
    /// Returns `changed = true` on any insert, replacement, or pin change.
    pub fn merge_from(&mut self, remote: FleetNetDoc) -> MergeOutcome {
        let mut changed = false;

        // Device rows: per-row LWW by seen_at.
        for (device_id, remote_row) in remote.devices {
            match self.devices.get(&device_id) {
                None => {
                    // Absent key: insert unconditionally.
                    self.devices.insert(device_id, remote_row);
                    changed = true;
                }
                Some(local_row) => {
                    if remote_row
                        .seen_at
                        .is_strictly_newer_than(&local_row.seen_at)
                    {
                        self.devices.insert(device_id, remote_row);
                        changed = true;
                    }
                    // Otherwise: local is equal or newer — keep local.
                }
            }
        }

        // Pin LWW pair: remote strictly newer by pinned_at → take both fields.
        if remote.pinned_at.is_strictly_newer_than(&self.pinned_at) {
            let pin_changed = self.pinned != remote.pinned;
            self.pinned = remote.pinned;
            self.pinned_at = remote.pinned_at;
            // Flag changed only if the pin value actually changed — an LWW
            // stamp update carrying the same pin device is a no-op for callers.
            if pin_changed {
                changed = true;
            }
        }

        MergeOutcome { changed }
    }
}

/// Ordered butler-set candidates: pinned first (if its row is fresh), then by
/// most-recent seen_at (ties broken by device-id ordering for determinism),
/// self included wherever it falls. Rows with seen_at.wall_ms < stale_before_ms
/// are excluded entirely.
///
/// This is the heart of the fleet-net-v1 contribution: it maps the
/// replicated `FleetNetDoc` to an ordered advisory butler-set for the
/// pkarr advertisement.
pub fn butler_set_order(doc: &FleetNetDoc, stale_before_ms: u64) -> Vec<(String, FleetNetRow)> {
    // Collect fresh rows only (stale = wall_ms < stale_before_ms).
    let mut fresh: Vec<(String, FleetNetRow)> = doc
        .devices
        .iter()
        .filter(|(_, row)| row.seen_at.wall_ms >= stale_before_ms)
        .map(|(id, row)| (id.clone(), row.clone()))
        .collect();

    // Sort by descending seen_at, ties broken by ascending device-id
    // (device-id tiebreak is deterministic across the fleet since all
    // devices share the same CRDT state).
    fresh.sort_by(|(id_a, row_a), (id_b, row_b)| {
        // Primary: descending wall_ms
        let w = row_b.seen_at.wall_ms.cmp(&row_a.seen_at.wall_ms);
        if w != std::cmp::Ordering::Equal {
            return w;
        }
        // Secondary: descending logical
        let l = row_b.seen_at.logical.cmp(&row_a.seen_at.logical);
        if l != std::cmp::Ordering::Equal {
            return l;
        }
        // Tertiary: ascending device_id (deterministic tiebreak)
        id_a.cmp(id_b)
    });

    // Promote pinned device to front (if it exists in the fresh set).
    if let Some(ref pin_id) = doc.pinned {
        if let Some(pos) = fresh.iter().position(|(id, _)| id == pin_id) {
            if pos != 0 {
                let pinned_entry = fresh.remove(pos);
                fresh.insert(0, pinned_entry);
            }
        }
        // If the pinned device has a stale/absent row, it was already excluded
        // from `fresh` — ordering falls back to recency (spec §6, D17).
    }

    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;

    fn hlc(wall_ms: u64, device_id: &str) -> Hlc {
        Hlc {
            wall_ms,
            logical: 0,
            device_id: device_id.into(),
        }
    }

    fn row(ep_byte: u8, relay: &str, seen_at: Hlc) -> FleetNetRow {
        FleetNetRow {
            iroh_endpoint_id: [ep_byte; 32],
            home_relay: relay.into(),
            seen_at,
        }
    }

    // ── Row LWW tests ─────────────────────────────────────────────────────────

    #[test]
    fn row_lww_keeps_strictly_newer() {
        // Remote row is strictly newer → replaces local.
        let mut local = FleetNetDoc::default();
        local.devices.insert(
            "dev-a".into(),
            row(0x01, "relay.example.com", hlc(10, "dev-a")),
        );

        let mut remote = FleetNetDoc::default();
        remote.devices.insert(
            "dev-a".into(),
            row(0x02, "relay.newer.com", hlc(20, "dev-a")),
        );

        let out = local.merge_from(remote);
        assert!(
            out.changed,
            "strictly newer remote must replace and flag changed"
        );
        assert_eq!(local.devices["dev-a"].seen_at.wall_ms, 20);
        assert_eq!(local.devices["dev-a"].home_relay, "relay.newer.com");
    }

    #[test]
    fn row_lww_ignores_older_remote() {
        // Remote row is older → local kept; changed = false.
        let mut local = FleetNetDoc::default();
        local.devices.insert(
            "dev-a".into(),
            row(0x01, "relay.local.com", hlc(20, "dev-a")),
        );

        let mut remote = FleetNetDoc::default();
        remote
            .devices
            .insert("dev-a".into(), row(0x02, "relay.old.com", hlc(10, "dev-a")));

        let out = local.merge_from(remote);
        assert!(!out.changed, "older remote must be ignored; no change");
        assert_eq!(local.devices["dev-a"].seen_at.wall_ms, 20);
        assert_eq!(local.devices["dev-a"].home_relay, "relay.local.com");
    }

    #[test]
    fn row_absent_key_inserts_unconditionally() {
        let mut local = FleetNetDoc::default();

        let mut remote = FleetNetDoc::default();
        remote
            .devices
            .insert("dev-b".into(), row(0x03, "relay.b.com", hlc(5, "dev-b")));

        let out = local.merge_from(remote);
        assert!(out.changed);
        assert!(local.devices.contains_key("dev-b"));
    }

    // ── Pin LWW pair tests ────────────────────────────────────────────────────

    #[test]
    fn pin_lww_pair_merges() {
        // Remote has a strictly newer pinned_at → both pinned + pinned_at taken.
        let mut local = FleetNetDoc::default();
        local.pinned = Some("dev-old".into());
        local.pinned_at = hlc(5, "dev-x");

        let mut remote = FleetNetDoc::default();
        remote.pinned = Some("dev-new".into());
        remote.pinned_at = hlc(10, "dev-x");

        let out = local.merge_from(remote);
        assert!(out.changed, "newer remote pin must flag changed");
        assert_eq!(local.pinned.as_deref(), Some("dev-new"));
        assert_eq!(local.pinned_at.wall_ms, 10);
    }

    #[test]
    fn pin_lww_older_remote_ignored() {
        let mut local = FleetNetDoc::default();
        local.pinned = Some("dev-keep".into());
        local.pinned_at = hlc(20, "dev-x");

        let mut remote = FleetNetDoc::default();
        remote.pinned = Some("dev-discard".into());
        remote.pinned_at = hlc(5, "dev-x");

        let out = local.merge_from(remote);
        assert!(!out.changed);
        assert_eq!(local.pinned.as_deref(), Some("dev-keep"));
    }

    #[test]
    fn pin_lww_same_stamp_local_wins() {
        // Equal pinned_at → is_strictly_newer_than is false → local kept.
        let mut local = FleetNetDoc::default();
        local.pinned = Some("dev-local".into());
        local.pinned_at = hlc(10, "dev-x");

        let mut remote = FleetNetDoc::default();
        remote.pinned = Some("dev-remote".into());
        remote.pinned_at = hlc(10, "dev-x");

        local.merge_from(remote);
        assert_eq!(local.pinned.as_deref(), Some("dev-local"));
    }

    // ── Butler set ordering tests ─────────────────────────────────────────────

    #[test]
    fn order_pinned_first_then_recency() {
        let stale_before = 100u64;
        let mut doc = FleetNetDoc::default();

        // dev-old: most recent seen_at but NOT pinned
        doc.devices.insert(
            "dev-old".into(),
            row(0xAA, "relay.old", hlc(500, "dev-old")),
        );
        // dev-pin: pinned, slightly older
        doc.devices.insert(
            "dev-pin".into(),
            row(0xBB, "relay.pin", hlc(300, "dev-pin")),
        );
        // dev-mid: in the middle by recency
        doc.devices.insert(
            "dev-mid".into(),
            row(0xCC, "relay.mid", hlc(400, "dev-mid")),
        );

        doc.pinned = Some("dev-pin".into());
        doc.pinned_at = hlc(1000, "owner");

        let order = butler_set_order(&doc, stale_before);
        assert_eq!(order.len(), 3);
        assert_eq!(order[0].0, "dev-pin", "pinned must be first");
        assert_eq!(order[1].0, "dev-old", "most recent non-pinned second");
        assert_eq!(order[2].0, "dev-mid", "least recent non-pinned last");
    }

    #[test]
    fn stale_rows_excluded() {
        let stale_before = 200u64;
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert("dev-fresh".into(), row(0x01, "relay.fresh", hlc(300, "d1")));
        doc.devices.insert(
            "dev-stale".into(),
            row(0x02, "relay.stale", hlc(100, "d2")), // wall_ms < stale_before
        );

        let order = butler_set_order(&doc, stale_before);
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].0, "dev-fresh");
    }

    #[test]
    fn pinned_but_stale_falls_back_to_recency() {
        // Pinned device's row is stale → excluded entirely;
        // ordering is purely by recency.
        let stale_before = 200u64;
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert("dev-fresh".into(), row(0x01, "relay.fresh", hlc(300, "d1")));
        doc.devices.insert(
            "dev-pinned-stale".into(),
            row(0x02, "relay.stale", hlc(50, "d2")), // stale
        );

        doc.pinned = Some("dev-pinned-stale".into());
        doc.pinned_at = hlc(999, "owner");

        let order = butler_set_order(&doc, stale_before);
        assert_eq!(order.len(), 1, "stale pinned row must be excluded");
        assert_eq!(order[0].0, "dev-fresh");
    }

    #[test]
    fn empty_doc_returns_empty_order() {
        let doc = FleetNetDoc::default();
        let order = butler_set_order(&doc, 0);
        assert!(order.is_empty());
    }

    #[test]
    fn order_deterministic_on_tie() {
        // Two rows with equal wall_ms: lower device-id should come first.
        let stale_before = 0u64;
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert("dev-z".into(), row(0x01, "relay.z", hlc(100, "dev-z")));
        doc.devices
            .insert("dev-a".into(), row(0x02, "relay.a", hlc(100, "dev-a")));

        let order = butler_set_order(&doc, stale_before);
        assert_eq!(order.len(), 2);
        assert_eq!(
            order[0].0, "dev-a",
            "lower device-id wins tiebreak (deterministic)"
        );
        assert_eq!(order[1].0, "dev-z");
    }

    // ── Wire-format pin fixture ───────────────────────────────────────────────

    /// Pins the fleet-net-v1 wire format. NEVER regenerate — any change to
    /// this hex means the on-disk/over-the-wire encoding changed and old peers
    /// would break.
    #[test]
    fn fleet_net_doc_canonical_cbor_pinned() {
        use ciborium::into_writer;

        // Fixed deterministic fixture values.
        let dev_a_id = "aaaa".repeat(16); // 64-char hex device ID
        let dev_b_id = "bbbb".repeat(16); // 64-char hex device ID
        let pin_dev_id = dev_a_id.clone();

        let row_a = FleetNetRow {
            iroh_endpoint_id: [0xAA; 32],
            home_relay: "relay.alpha.com".into(),
            seen_at: Hlc {
                wall_ms: 1000,
                logical: 0,
                device_id: "dev-a".into(),
            },
        };
        let row_b = FleetNetRow {
            iroh_endpoint_id: [0xBB; 32],
            home_relay: "relay.beta.com".into(),
            seen_at: Hlc {
                wall_ms: 2000,
                logical: 0,
                device_id: "dev-b".into(),
            },
        };

        let mut doc = FleetNetDoc::default();
        doc.devices.insert(dev_a_id.clone(), row_a);
        doc.devices.insert(dev_b_id.clone(), row_b);
        doc.pinned = Some(pin_dev_id.clone());
        doc.pinned_at = Hlc {
            wall_ms: 3000,
            logical: 0,
            device_id: "dev-owner".into(),
        };

        let mut buf = Vec::new();
        into_writer(&doc, &mut buf).expect("encode");
        let actual = hex::encode(&buf);

        // Pins the fleet-net-v1 wire format; NEVER regenerate.
        // See EXPECTED_OUTHOLD_DOC_HEX in dm_outhold.rs for the pattern.
        const EXPECTED_FLEET_NET_DOC_HEX: &str = "a3626476a2784061616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161a3626570982018aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa6268726f72656c61792e616c7068612e636f6d627361a361771903e8616c006164656465762d61784062626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262a3626570982018bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb6268726e72656c61792e626574612e636f6d627361a361771907d0616c006164656465762d6262706e784061616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161627061a36177190bb8616c006164696465762d6f776e6572";
        assert_eq!(
            actual, EXPECTED_FLEET_NET_DOC_HEX,
            "FleetNetDoc wire encoding drifted from pinned fixture.\nactual hex: {actual}"
        );
    }

    // ── Engine-wiring proof ───────────────────────────────────────────────────

    /// End-to-end engine-wiring proof (ZEB-418 P2 Task 6): a real
    /// `FleetSyncEngine<FleetNetDoc>` configured exactly as `start_node`
    /// configures it (FleetNetPersist sink, `merge_from` merger,
    /// `publish_seen: true`, lookup tag `b"fleet-net-v1"`, `on_applied:
    /// None` — Task 7 adds the snapshot refresh) must emit an outbound
    /// wire frame on the publisher channel when a local self-row upsert is
    /// followed by `notify_dirty` + `flush_now`. Mirrors
    /// `dm_inbox_ingest::dm_inbox_engine_publishes_on_local_write`
    /// site-for-site.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fleet_net_engine_publishes_on_local_write() {
        use crate::content_store::{ContentStore, InMemoryStub};
        use crate::fleet_net_persist::FleetNetPersist;
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine, Merger, DEFAULT_DEBOUNCE_MS};
        use crate::owner_state_crypto::KeyTree;
        use std::collections::BTreeMap;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::{mpsc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x66u8; 32]).expect("derive kt"));
        let doc = Arc::new(Mutex::new(FleetNetDoc::default()));
        let tracker = Arc::new(Mutex::new(BTreeMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let merger: Merger<FleetNetDoc> = Arc::new(|local, remote| local.merge_from(remote));

        let engine = FleetSyncEngine::<FleetNetDoc>::new(FleetSyncConfig {
            kt,
            device_id: "dev-A".to_string(),
            state: Arc::clone(&doc),
            merger,
            replay_tracker: Arc::clone(&tracker),
            content_store: cas,
            publisher_tx: out_tx,
            subscriber_rx: in_rx,
            persist: Arc::new(FleetNetPersist {
                doc_path: dir.path().join("fleet_net.cbor"),
                replay_path: dir.path().join("fleet_net_replay.cbor"),
            }),
            lookup_key_tag: b"fleet-net-v1",
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            publish_seen: true,
            on_applied: None,
            sibling_acks: Arc::new(Mutex::new(BTreeMap::new())),
        });

        // A local self-row upsert (what start_node's post-bind upsert
        // does under the doc lock), then force the engine to publish.
        {
            let mut guard = doc.lock().await;
            guard.devices.insert(
                "aaaa".repeat(16),
                row(0x01, "relay.example.com", hlc(1000, "dev-A")),
            );
        }
        engine.notify_dirty();
        engine.flush_now().await.unwrap();

        // The local write must have driven a (non-empty) publish frame
        // onto the outbound channel.
        let frame = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("publish frame produced within 5s")
            .expect("publisher channel yielded Some(frame)");
        assert!(!frame.is_empty(), "published wire frame must be non-empty");

        let _ = engine.shutdown().await;
    }
}
