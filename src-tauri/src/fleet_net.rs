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

/// Map the fleet-net snapshot to the advertised butler set (max
/// [`crate::butler_deposit::BUTLER_SET_MAX_ENTRIES`]). `self_entry` is the
/// publishing device's own pre-built entry (always available even when the
/// snapshot is cold); `vk_lookup` resolves a device-id to its ed25519 verify
/// key (owner_device cache in prod — devices without a resolvable vk are
/// skipped with a debug log by the CALLER side; here just skip on `None`).
///
/// Guarantees: self_entry's device appears exactly once (the snapshot row
/// for self is replaced by self_entry's fresher transport data); pinned-first
/// ordering via [`butler_set_order`]; falls back to `vec![self_entry]` when
/// the snapshot yields nothing.
///
/// Each produced entry's `pinned` flag comes from `doc.pinned`. The wire
/// `device_id` is the 16-byte signing-identity hash derived from the vk the
/// same way `start_node` derives `this_device_id_hash` from the device key
/// (`PubKeyBundle::classical_identity_hash`); fleet-net keys are the SP1
/// 64-hex device ids (hex of the 32-byte ed25519 vk), so the hash is
/// re-derived here via `vk_lookup`'s resolved key.
///
/// Skips that don't resolve do NOT consume cap slots — the next-ordered
/// fresh device is considered instead.
pub fn build_butler_set(
    doc: &FleetNetDoc,
    self_device_id: &str,
    self_entry: crate::reachability_record::ButlerSetEntry,
    vk_lookup: &dyn Fn(&str) -> Option<[u8; 32]>,
    stale_before_ms: u64,
) -> Vec<crate::reachability_record::ButlerSetEntry> {
    use crate::butler_deposit::BUTLER_SET_MAX_ENTRIES;
    use harmony_owner::pubkey_bundle::PubKeyBundle;

    let pinned_id = doc.pinned.as_deref();
    let mut out: Vec<crate::reachability_record::ButlerSetEntry> = Vec::new();
    for (dev_id, row) in butler_set_order(doc, stale_before_ms) {
        if out.len() >= BUTLER_SET_MAX_ENTRIES {
            break;
        }
        let pinned = pinned_id == Some(dev_id.as_str());
        if dev_id == self_device_id {
            // Self appears exactly once: the snapshot row for self is
            // replaced by self_entry's fresher transport data (live iroh
            // endpoint + relay snapshotted at blob-build time); only the
            // pinned flag comes from the doc.
            let mut e = self_entry.clone();
            e.pinned = pinned;
            out.push(e);
            continue;
        }
        let Some(vk) = vk_lookup(&dev_id) else {
            // Unresolvable vk: skip without consuming a cap slot (caller
            // side logs at debug level inside its `vk_lookup`).
            continue;
        };
        out.push(crate::reachability_record::ButlerSetEntry {
            device_id: PubKeyBundle::classical_identity_hash(&vk),
            iroh_endpoint_id: row.iroh_endpoint_id,
            device_ed25519_verify: vk,
            home_relay: row.home_relay.clone(),
            pinned,
        });
    }
    if out.is_empty() {
        // Cold/empty snapshot (or every row stale/unresolvable): degrade to
        // the P1 self-only advertisement. The pinned flag still reflects the
        // doc — the pin LWW pair can survive a row wipe.
        let mut e = self_entry;
        e.pinned = pinned_id == Some(self_device_id);
        return vec![e];
    }
    out
}

/// Selection-relevant projection of the fleet-net doc (ZEB-418 P2, D16):
/// the advertised prefix's (device-id, endpoint, relay, pinned) tuples.
/// Deliberately EXCLUDES `seen_at` — stamp-only refreshes (the periodic
/// re-stamp every BUTLER_SET_REFRESH_MS) must not look like fleet changes,
/// otherwise every sibling heartbeat would schedule a debounced republish.
pub fn selection_view(
    doc: &FleetNetDoc,
    stale_before_ms: u64,
) -> Vec<(String, [u8; 32], String, bool)> {
    butler_set_order(doc, stale_before_ms)
        .into_iter()
        .take(crate::butler_deposit::BUTLER_SET_MAX_ENTRIES)
        .map(|(id, row)| {
            let pinned = doc.pinned.as_deref() == Some(id.as_str());
            (id, row.iroh_endpoint_id, row.home_relay, pinned)
        })
        .collect()
}

/// Snapshot the SP1-device-id → ed25519-vk map that `build_butler_set`'s
/// prod `vk_lookup` reads, from the owner_device_cache (spec §5: "`vk` comes
/// from `owner_device_cache`"). Fleet-net keys are hex(ed25519 vk); the cache
/// stores 64-byte `X25519_pub(32) || Ed25519_pub(32)` identity pubs, so the
/// map keys on hex of each cached pub's Ed25519 half. Only the SELF owner's
/// entry matters (the butler set advertises our own fleet). The publishing
/// device itself is always inserted (`self_device_id_hex` → `self_vk`), so
/// self resolves even on a cold cache.
pub(crate) fn vk_map_from_device_cache(
    cache: &crate::owner_state_types::OwnerDeviceCache,
    self_owner: &crate::owner_state_types::OwnerAddr,
    self_device_id_hex: &str,
    self_vk: [u8; 32],
) -> BTreeMap<String, [u8; 32]> {
    let mut map: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    if let Some(entry) = cache.devices.get(self_owner) {
        for pub64 in entry.device_identity_pubs.iter().flatten() {
            let mut ed: [u8; 32] = [0u8; 32];
            ed.copy_from_slice(&pub64[32..64]);
            map.insert(hex::encode(ed), ed);
        }
    }
    map.insert(self_device_id_hex.to_string(), self_vk);
    map
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

    // ── build_butler_set tests (ZEB-418 P2 Task 7) ────────────────────────────

    use crate::reachability_record::ButlerSetEntry;

    /// 64-hex SP1 device ids + matching test vks. The hex↔vk relation is
    /// arbitrary here (prod derives the map from owner_device_cache); the
    /// fn under test only sees `vk_lookup`.
    fn self_id() -> String {
        "aa".repeat(32)
    }
    fn sib1_id() -> String {
        "bb".repeat(32)
    }
    fn sib2_id() -> String {
        "cc".repeat(32)
    }

    fn test_vk_lookup(dev_id: &str) -> Option<[u8; 32]> {
        if dev_id == sib1_id() {
            Some([0xBB; 32])
        } else if dev_id == sib2_id() {
            Some([0xCC; 32])
        } else {
            None
        }
    }

    /// The publishing device's pre-built self entry (what the lib.rs blob
    /// builder snapshots from the live iroh endpoint).
    fn self_entry() -> ButlerSetEntry {
        ButlerSetEntry {
            device_id: [0x5E; 16],
            iroh_endpoint_id: [0x0E; 32],
            device_ed25519_verify: [0xAA; 32],
            home_relay: "relay.live.example".into(),
            pinned: false,
        }
    }

    #[test]
    fn build_set_emits_self_only_on_cold_snapshot() {
        let doc = FleetNetDoc::default();
        let set = build_butler_set(&doc, &self_id(), self_entry(), &test_vk_lookup, 0);
        assert_eq!(
            set,
            vec![self_entry()],
            "cold snapshot must degrade to P1 self-only"
        );
    }

    #[test]
    fn build_set_adds_sibling_secondary() {
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert(self_id(), row(0x01, "relay.self.snap", hlc(2000, "self")));
        doc.devices
            .insert(sib1_id(), row(0x02, "relay.sib1", hlc(1000, "sib1")));

        let set = build_butler_set(&doc, &self_id(), self_entry(), &test_vk_lookup, 0);
        assert_eq!(set.len(), 2);
        // Self first (most recent), then the sibling secondary.
        assert_eq!(set[0].device_id, self_entry().device_id);
        let sib = &set[1];
        assert_eq!(sib.device_ed25519_verify, [0xBB; 32]);
        assert_eq!(
            sib.device_id,
            harmony_owner::pubkey_bundle::PubKeyBundle::classical_identity_hash(&[0xBB; 32]),
            "wire device_id must be the identity hash derived from the resolved vk"
        );
        assert_eq!(sib.iroh_endpoint_id, [0x02; 32]);
        assert_eq!(sib.home_relay, "relay.sib1");
        assert!(!sib.pinned);
    }

    #[test]
    fn build_set_self_appears_exactly_once_with_own_transport_data() {
        // The snapshot's self row carries STALE transport data; the produced
        // set must contain self exactly once, with self_entry's live data.
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert(self_id(), row(0x01, "relay.self.stale", hlc(2000, "self")));
        doc.devices
            .insert(sib1_id(), row(0x02, "relay.sib1", hlc(1000, "sib1")));

        let set = build_butler_set(&doc, &self_id(), self_entry(), &test_vk_lookup, 0);
        let selves: Vec<_> = set
            .iter()
            .filter(|e| e.device_id == self_entry().device_id)
            .collect();
        assert_eq!(selves.len(), 1, "self must appear exactly once");
        assert_eq!(selves[0].iroh_endpoint_id, self_entry().iroh_endpoint_id);
        assert_eq!(selves[0].home_relay, self_entry().home_relay);
    }

    #[test]
    fn build_set_pinned_leads() {
        // sib1 is older than self but pinned → leads the set, pinned flag set.
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert(self_id(), row(0x01, "relay.self.snap", hlc(2000, "self")));
        doc.devices
            .insert(sib1_id(), row(0x02, "relay.sib1", hlc(1000, "sib1")));
        doc.pinned = Some(sib1_id());
        doc.pinned_at = hlc(3000, "owner");

        let set = build_butler_set(&doc, &self_id(), self_entry(), &test_vk_lookup, 0);
        assert_eq!(set.len(), 2);
        assert_eq!(
            set[0].device_ed25519_verify, [0xBB; 32],
            "pinned sibling leads"
        );
        assert!(set[0].pinned);
        assert_eq!(set[1].device_id, self_entry().device_id);
        assert!(!set[1].pinned);
    }

    #[test]
    fn build_set_skips_unresolvable_vk() {
        // sib-x is the MOST recent but has no resolvable vk → skipped without
        // consuming a cap slot; the set still fills to two entries.
        let sibx_id = "dd".repeat(32); // test_vk_lookup → None
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert(sibx_id, row(0x0D, "relay.sibx", hlc(3000, "sibx")));
        doc.devices
            .insert(self_id(), row(0x01, "relay.self.snap", hlc(2000, "self")));
        doc.devices
            .insert(sib2_id(), row(0x03, "relay.sib2", hlc(1000, "sib2")));

        let set = build_butler_set(&doc, &self_id(), self_entry(), &test_vk_lookup, 0);
        assert_eq!(set.len(), 2, "skip must not consume a cap slot");
        assert_eq!(set[0].device_id, self_entry().device_id);
        assert_eq!(set[1].device_ed25519_verify, [0xCC; 32]);
    }

    #[test]
    fn build_set_caps_at_max_entries() {
        // Three fresh, resolvable devices → capped to the two most recent.
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert(self_id(), row(0x01, "relay.self.snap", hlc(3000, "self")));
        doc.devices
            .insert(sib1_id(), row(0x02, "relay.sib1", hlc(2000, "sib1")));
        doc.devices
            .insert(sib2_id(), row(0x03, "relay.sib2", hlc(1000, "sib2")));

        let set = build_butler_set(&doc, &self_id(), self_entry(), &test_vk_lookup, 0);
        assert_eq!(set.len(), crate::butler_deposit::BUTLER_SET_MAX_ENTRIES);
        assert_eq!(set[0].device_id, self_entry().device_id);
        assert_eq!(set[1].device_ed25519_verify, [0xBB; 32]);
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
    /// `publish_seen: true`, lookup tag `b"fleet-net-v1"`; `on_applied`
    /// stays `None` here — production wires Task 7's snapshot-refresh
    /// nudge, which is exercised separately) must emit an outbound
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
