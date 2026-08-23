//! fleet-net-v1: per-device network info (iroh endpoint + home relay) and
//! owner-level pinned-butler setting, fleet-replicated via FleetSyncEngine
//! (ZEB-418 P2). Feeds the butler-set advertisement in the owner's pkarr
//! routing record. See spec §5–§6.

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Inbound zenoh size cap for fleet-net-v1 full-doc sync frames (ZEB-418
/// P2, PR #222 round 1). Rows are ~100 bytes/device; 64 KiB covers hundreds
/// of devices with margin — anything larger is a malformed or hostile peer
/// frame and is dropped before allocation.
pub const FLEET_NET_DATASET_MAX_BYTES: usize = 64 * 1024;

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
    /// ZEB-678 S2 (§3.5): this device's active `FeedAuthorityRecord` (its full
    /// binding, no revocation), self-stamped on first migrated vine publish, as
    /// the record's canonical `serde_json` string. Stored as an opaque String —
    /// NOT a nested struct — because `FeedAuthorityRecord` is camelCase JSON
    /// with mixed-length field names, which would violate the canonical-CBOR
    /// same-length-key contract if nested in this row (see
    /// `owner_state_crypto::canonical_cbor_encode`). On master-revoke the
    /// seed-holder parses this, appends a `RevocationCert`, bumps `updated_at`
    /// (both excluded from `n_sig`, so the binding stays valid), and republishes
    /// to `harmony/vines/{N}/authority` (§6). Additive: absent on the wire when
    /// the device has not migrated a feed, so pre-migration peers are unaffected.
    #[serde(rename = "fb", default, skip_serializing_if = "Option::is_none")]
    pub feed_binding: Option<String>,
}

/// A fleet-synced device petname (ZEB-668 S4). Assigned by ANY of the
/// owner's devices ABOUT any device — deliberately outside `FleetNetRow`
/// (rows are self-stamped by their subject device; petnames are not).
/// `name: ""` means "cleared" (kept as an LWW value so a clear replicates;
/// entry removal would not converge).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNetPetname {
    #[serde(rename = "n")]
    pub name: String,
    /// LWW stamp; strictly-newer wins, ties keep local.
    #[serde(rename = "st")]
    pub set_at: Hlc,
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
    /// Fleet-synced device petnames (ZEB-668 S4), keyed like `devices` by SP1
    /// 64-hex device id. Per-key LWW by `set_at`. Additive: absent on the
    /// wire when empty, so pre-S4 payloads and peers are unaffected.
    #[serde(rename = "pt", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub petnames: BTreeMap<String, FleetNetPetname>,
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
            petnames: BTreeMap::new(),
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
impl CanonicalPayloadSealed for FleetNetPetname {}
impl CanonicalPayload for FleetNetPetname {}

impl FleetNetDoc {
    /// LWW row merge: a remote row replaces the local row only when
    /// `remote.seen_at.is_strictly_newer_than(&local.seen_at)`.
    /// Absent keys are inserted. `pinned`+`pinned_at` merge as an LWW PAIR
    /// by `pinned_at` (remote strictly newer → take both fields).
    /// Returns `changed = true` on any insert, replacement, or pin change.
    ///
    /// ZEB-852 (D2-MERGE): device rows are a STORED replicated register LWW'd by
    /// the sibling's self-stamped `seen_at`, so a future-dated `seen_at` would
    /// win the LWW and FREEZE the row (no honest later stamp could ever be
    /// `is_strictly_newer_than` it), pinning butler deposits onto a dead device.
    /// A stored register must be REJECTED (never clamped — a clamped stored value
    /// is receiver-dependent and would diverge across peers), so incoming rows
    /// are bounded against the receiver's OWN control-tier ceiling. Sampled once
    /// via [`crate::clock_trust::receiver_now_ms`]; `None` (unreadable clock) ⇒
    /// apply-all so a bad LOCAL clock never drops honest sibling rows. Delegated
    /// to [`Self::merge_from_bounded`] so the reject / adopt / apply-all branches
    /// are unit-testable without the real system clock.
    pub fn merge_from(&mut self, remote: FleetNetDoc) -> MergeOutcome {
        self.merge_from_bounded(remote, crate::clock_trust::receiver_now_ms())
    }

    /// Clock-injected core of [`Self::merge_from`]. `receiver_now` is the
    /// receiver's own wall clock (`None` ⇒ unreadable ⇒ apply-all). The
    /// forward-skew reject guards every stored replicated stamp merged here: the
    /// self-stamped device-row `seen_at` (ZEB-852 D2) and — since ZEB-856 R3 —
    /// the owner-stamped `pinned_at` pair and each petname `set_at`.
    fn merge_from_bounded(
        &mut self,
        remote: FleetNetDoc,
        receiver_now: Option<u64>,
    ) -> MergeOutcome {
        let mut changed = false;

        // Device rows: per-row LWW by seen_at.
        for (device_id, remote_row) in remote.devices {
            // ZEB-852 (D2-MERGE): drop a row whose self-stamped seen_at is
            // implausibly far in the receiver's future (control tier). Gates
            // BOTH the absent-key insert and the LWW replace — either path would
            // otherwise store a poison stamp that freezes the register.
            if crate::clock_trust::wall_exceeds_forward_skew_logged(
                remote_row.seen_at.wall_ms,
                receiver_now,
                "fleet_net.device.seen_at",
            ) {
                continue;
            }
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
        // ZEB-856 (R3): reject a future-dated pin stamp before the LWW, mirroring
        // the seen_at reject above. `pinned_at` is a STORED replicated register, so
        // a stamp implausibly ahead of the receiver clock would win the LWW and
        // FREEZE the pin (no honest later pinned_at could be strictly-newer),
        // pinning butler routing permanently. Reject — never clamp: a clamped
        // stored value is receiver-dependent and would diverge across peers.
        // Control tier; `receiver_now == None` ⇒ apply-all (a bad LOCAL clock must
        // never drop an honest owner pin). `pinned_at` is owner-stamped (vs
        // `seen_at` self-stamped by the subject sibling) — same freeze hazard.
        if !crate::clock_trust::wall_exceeds_forward_skew_logged(
            remote.pinned_at.wall_ms,
            receiver_now,
            "fleet_net.pin.pinned_at",
        ) && remote.pinned_at.is_strictly_newer_than(&self.pinned_at)
        {
            let pin_changed = self.pinned != remote.pinned;
            self.pinned = remote.pinned;
            self.pinned_at = remote.pinned_at;
            // Flag changed only if the pin value actually changed — an LWW
            // stamp update carrying the same pin device is a no-op for callers.
            if pin_changed {
                changed = true;
            }
        }

        // Petnames (ZEB-668 S4): per-key LWW by set_at — same shape as the
        // device rows above.
        for (device_id, remote_pn) in remote.petnames {
            // ZEB-856 (R3): drop a petname whose set_at is implausibly future —
            // same stored-register freeze hazard as the pin and seen_at. (Petnames
            // are owner-assigned about a device, not self-stamped by the subject —
            // still a stored LWW register a future stamp can freeze.)
            // Reject-not-clamp; `receiver_now == None` ⇒ apply-all.
            if crate::clock_trust::wall_exceeds_forward_skew_logged(
                remote_pn.set_at.wall_ms,
                receiver_now,
                "fleet_net.petname.set_at",
            ) {
                continue;
            }
            match self.petnames.get(&device_id) {
                None => {
                    self.petnames.insert(device_id, remote_pn);
                    changed = true;
                }
                Some(local_pn) => {
                    if remote_pn.set_at.is_strictly_newer_than(&local_pn.set_at) {
                        self.petnames.insert(device_id, remote_pn);
                        changed = true;
                    }
                }
            }
        }

        MergeOutcome { changed }
    }
}

/// Ordered butler-set candidates: pinned first (if its row is fresh), then by
/// most-recent seen_at — the ranking key is CLAMPED to the receiver `now`, then
/// ties broken by device-id ordering for determinism — self included wherever
/// it falls. Rows are kept only inside the freshness window: `seen_at.wall_ms`
/// below `stale_before_ms` (stale) **or** more than one `BUTLER_SET_FRESHNESS_MS`
/// window past `now` (implausibly future-dated) are excluded entirely (ZEB-852 D2).
///
/// **Caller contract (load-bearing):** `stale_before_ms` MUST equal
/// `now - BUTLER_SET_FRESHNESS_MS`. The receiver `now` used for the upper freshness
/// bound and the sort clamp is recovered here as
/// `stale_before_ms + BUTLER_SET_FRESHNESS_MS`, so a caller passing any other cutoff
/// (e.g. `0` for "no lower bound") would mis-bound the upper filter and the clamp and
/// could silently drop or mis-rank valid rows. All current callers — the lib.rs pkarr
/// blob builder and `selection_view` — satisfy this.
///
/// **Accepted residual (ZEB-856 R1 — near-future clamp-to-top).** The ranking
/// key is peer-self-stamped and this function has no independent liveness
/// signal, so a sibling that stamps `seen_at.wall_ms = now` leads honest
/// siblings sitting at `now − Δ` (their stamp ages up to one
/// `BUTLER_SET_REFRESH_MS` between refreshes). Left UNFIXED by decision:
/// `wall = now` is indistinguishable from an honestly-just-refreshed device,
/// and any structural demotion is fail-open — it could push a mildly
/// clock-skewed honest device below a stale one and route butler deposits to a
/// dead device. The exposure is bounded (the clamp caps inflation at `now`, R2
/// removed the `logical` axis, `device_id` is fixed) and the sanctioned
/// override is the owner's PIN, which ZEB-856 R3 makes un-freezable. Pinned by
/// the `butler_rank_r1_now_stamper_leads_then_pin_overrides` canary test.
///
/// This is the heart of the fleet-net-v1 contribution: it maps the
/// replicated `FleetNetDoc` to an ordered advisory butler-set for the
/// pkarr advertisement.
pub fn butler_set_order(doc: &FleetNetDoc, stale_before_ms: u64) -> Vec<(String, FleetNetRow)> {
    // ZEB-852 (D2): recover the caller's receiver `now`. Every caller derives
    // `stale_before_ms = now - BUTLER_SET_FRESHNESS_MS` (the lib.rs pkarr blob
    // builder and `selection_view`), so this inversion is exact and keeps the
    // public signature unchanged.
    let now = stale_before_ms.saturating_add(crate::butler_deposit::BUTLER_SET_FRESHNESS_MS);

    // Collect fresh rows only. LOWER bound: stale = wall_ms < stale_before_ms.
    // UPPER bound (ZEB-852): a maliciously fast-clocked sibling self-stamps its
    // row in the future; with a descending sort that row ranks slot 0 and is
    // published to other owners, who then route butler DEPOSITS onto a dead
    // device. Drop rows more than one freshness window ahead of `now`, mirroring
    // `fresh_butler_set`'s both-sided `bs_at` bound (reachability_record.rs).
    // Shape B (transient sort/filter, nothing persisted) → CLAMP, not reject.
    let mut fresh: Vec<(String, FleetNetRow)> = doc
        .devices
        .iter()
        .filter(|(_, row)| {
            row.seen_at.wall_ms >= stale_before_ms
                && row.seen_at.wall_ms
                    <= now.saturating_add(crate::butler_deposit::BUTLER_SET_FRESHNESS_MS)
        })
        .map(|(id, row)| (id.clone(), row.clone()))
        .collect();

    // Sort by descending seen_at, ties broken by ascending device-id
    // (device-id tiebreak is deterministic across the fleet since all
    // devices share the same CRDT state). The primary key is CLAMPED to `now`
    // (ZEB-852): a still-in-window but future-dated sibling must not out-rank an
    // honest present row purely by its inflated wall_ms — `min(wall_ms, now)`
    // collapses any future stamp to `now`, after which the device-id tiebreak
    // decides (ZEB-856 R2 removed the peer-inflatable `logical` axis here).
    let clamp = |wall_ms: u64| wall_ms.min(now);
    fresh.sort_by(|(id_a, row_a), (id_b, row_b)| {
        // Primary: descending wall_ms (clamped to now)
        let w = clamp(row_b.seen_at.wall_ms).cmp(&clamp(row_a.seen_at.wall_ms));
        if w != std::cmp::Ordering::Equal {
            return w;
        }
        // Final tiebreak: ascending device_id.
        // ZEB-856 (R2): the descending-`logical` secondary was REMOVED here. In this
        // cross-device ranking `logical` is a per-device HLC counter with no
        // cross-device meaning, and it is peer-self-stamped (a sibling could set
        // `logical = u32::MAX` to win a clamped-wall tie for butler slot 0). The
        // remaining key `(clamp(wall_ms), device_id)` is fully bounded/fixed:
        // clamped-wall is receiver-capped (ZEB-852) and `device_id` is the
        // identity-bound device id (the fleet-net map key = hex of the device's
        // ed25519 verifying key), which a peer cannot cheaply set to an arbitrary
        // value, and is unique per row → a strict total order, so determinism is
        // preserved. `logical` stays in
        // `Hlc::is_strictly_newer_than` for the merge LWW, where same-device
        // causality legitimately needs it. `selection_view` delegates here, so it
        // inherits this policy (there is exactly one sort site).
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
/// Guarantees: self_entry's device appears exactly once, enforced
/// UNCONDITIONALLY. When self's snapshot row is fresh it is replaced
/// in-order by self_entry's fresher transport data; when self's row is
/// stale or missing (cold snapshot, or fresh siblings filled the cap)
/// self_entry is force-inserted at the front — after a pinned sibling, if
/// any — evicting the lowest-priority entry if the set is full. Rationale:
/// the publisher is by definition online at blob-build time (it is
/// publishing), so its live transport data is fresher than any seen_at
/// row. Pinned-first ordering via [`butler_set_order`].
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
    let mut saw_self = false;
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
            saw_self = true;
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
    if !saw_self {
        // Self's snapshot row is stale or missing (cold snapshot, or fresh
        // siblings filled the cap). The publisher is by definition online
        // NOW — it is publishing this very record — so force-include
        // self_entry's live transport data, evicting the lowest-priority
        // sibling if the set is full. The pinned flag still reflects the
        // doc — the pin LWW pair can survive a row wipe.
        let mut e = self_entry;
        e.pinned = pinned_id == Some(self_device_id);
        if out.len() >= BUTLER_SET_MAX_ENTRIES {
            out.pop();
        }
        // A pinned sibling keeps slot 0 (pinned-first contract); otherwise
        // self leads.
        let idx = if !e.pinned && out.first().is_some_and(|f| f.pinned) {
            1
        } else {
            0
        };
        out.insert(idx.min(out.len()), e);
    }
    out
}

/// Aggregate the creator's active devices into a vine relay set (max
/// [`crate::pkarr_vines::VINE_RELAY_SET_MAX`]). The vines analogue of
/// [`build_butler_set`], minus the `vk_lookup` layer: a `VineRelayEntry`
/// carries only `iroh_endpoint_id` + `home_relay`, both present directly in
/// `FleetNetRow`, so no per-device verify-key resolution is needed.
///
/// `self_entry` is the publishing device's own live transport data (it is
/// online by definition at publish time) and appears exactly once: when self's
/// snapshot row is in the fresh ordering it is replaced by `self_entry`'s
/// fresher data; when self's row is stale/missing or fresh siblings filled the
/// cap, `self_entry` is force-inserted at the front, evicting the
/// lowest-priority entry if the set is full.
///
/// Sibling ordering, staleness filtering, and the ZEB-852/856 peer-inflation
/// hardening all come from reusing [`butler_set_order`]. `now_ms` is the
/// receiver clock; the freshness window is `BUTLER_SET_FRESHNESS_MS`, and the
/// `stale_before_ms` inversion `butler_set_order` expects is computed HERE so
/// no caller can get it wrong.
///
/// Pin promotion is inherited from `butler_set_order`, which leads with the
/// owner's pinned device. `VineRelayEntry` carries no `pinned` field, so no pin
/// metadata is transmitted — but the pin is still observable in effect: it sets
/// the serialized ORDER (a dialing-preference hint) and, when more fresh devices
/// exist than the cap, WHICH devices make the set. When self must be
/// force-included (below), a fresh pinned sibling keeps slot 0, mirroring
/// `build_butler_set`.
pub fn build_vine_relay_set(
    doc: &FleetNetDoc,
    self_device_id: &str,
    self_entry: crate::pkarr_vines::VineRelayEntry,
    now_ms: u64,
) -> Vec<crate::pkarr_vines::VineRelayEntry> {
    use crate::pkarr_vines::{VineRelayEntry, VINE_RELAY_SET_MAX};

    let stale_before_ms = now_ms.saturating_sub(crate::butler_deposit::BUTLER_SET_FRESHNESS_MS);
    let self_is_pinned = doc.pinned.as_deref() == Some(self_device_id);

    let mut out: Vec<VineRelayEntry> = Vec::new();
    let mut saw_self = false;
    // Whether out[0] is the owner's pinned device (a sibling, not self):
    // `butler_set_order` promotes a fresh pin to the front, so it lands first.
    let mut leading_is_pinned_sibling = false;
    for (dev_id, row) in butler_set_order(doc, stale_before_ms) {
        if out.len() >= VINE_RELAY_SET_MAX {
            break;
        }
        if dev_id == self_device_id {
            // Self appears once: replace its snapshot row with the fresher live
            // transport data captured at blob-build time.
            saw_self = true;
            out.push(self_entry.clone());
            continue;
        }
        if out.is_empty() && doc.pinned.as_deref() == Some(dev_id.as_str()) {
            leading_is_pinned_sibling = true;
        }
        out.push(VineRelayEntry {
            iroh_endpoint_id: row.iroh_endpoint_id,
            home_relay: row.home_relay.clone(),
        });
    }
    if !saw_self {
        // Self's row is stale/missing, or fresh siblings filled the cap. The
        // publisher is online NOW (it is publishing this record), so force its
        // live entry in, evicting the lowest-priority sibling if the set is full.
        if out.len() >= VINE_RELAY_SET_MAX {
            out.pop();
        }
        // Keep a fresh pinned sibling at slot 0 (pinned-first, mirroring
        // build_butler_set); otherwise self leads.
        let idx = if !self_is_pinned && leading_is_pinned_sibling {
            1
        } else {
            0
        };
        out.insert(idx.min(out.len()), self_entry);
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

/// The vines analogue of [`selection_view`] (ZEB-820): the up-to-
/// `VINE_RELAY_SET_MAX` prefix that `build_vine_relay_set` would publish,
/// projected to (device-id, endpoint, relay, pinned) and EXCLUDING `seen_at`
/// for the same reason — so the fleet-change task can debounce a vine
/// re-publish on a change to the advertised set (a sibling joining, aging out,
/// or changing its relay) without every heartbeat's stamp churn triggering one.
/// Self's live entry is constant across ticks, so this sibling/pin prefix is the
/// change signal that matters. Wider than [`selection_view`] (cap 4 vs 2), so a
/// device entering only the vine set — not the butler set — is still caught.
pub fn vine_selection_view(
    doc: &FleetNetDoc,
    stale_before_ms: u64,
) -> Vec<(String, [u8; 32], String, bool)> {
    butler_set_order(doc, stale_before_ms)
        .into_iter()
        .take(crate::pkarr_vines::VINE_RELAY_SET_MAX)
        .map(|(id, row)| {
            let pinned = doc.pinned.as_deref() == Some(id.as_str());
            (id, row.iroh_endpoint_id, row.home_relay, pinned)
        })
        .collect()
}

/// ZEB-510: project a durable fleet-net device row into a dial-target
/// reachability payload for the [`crate::reachability_resolver::ReachabilityResolver`].
///
/// The row's `iroh_endpoint_id` becomes the payload's `iroh_node_id` (the
/// resolver keys on it). The payload is **verification-exempt**: `identity_
/// signature` is zero-filled because the trust boundary for a fleet row is
/// fleet-net's symmetric-key decrypt (only an enrolled sibling holding the
/// owner's fleet KeyTree produces a decryptable row), not a per-record
/// identity signature. `butler_set`/`bs_at` are empty — a sibling is a dial
/// target here, not advertising its own butlers — and `direct_addresses` is
/// empty because node-id-based dialing holepunches/relays (fleet rows carry no
/// direct addrs).
pub fn sibling_reachability_payload(
    row: &FleetNetRow,
) -> crate::reachability_record::ReachabilityAnnouncePayload {
    crate::reachability_record::ReachabilityAnnouncePayload {
        iroh_node_id: row.iroh_endpoint_id,
        home_relay_url: row.home_relay.clone(),
        direct_addresses: Vec::new(),
        announced_at_ms: row.seen_at.wall_ms,
        identity_signature: [0u8; 64],
        butler_set: Vec::new(),
        bs_at: 0,
    }
}

/// ZEB-510: every device row in `doc` EXCEPT `self_device_id`, as owned clones.
///
/// Owned clones (not borrows) so callers can drop the `FleetNetDoc` lock before
/// feeding the resolver. The self row is excluded so P never dials itself.
pub fn sibling_rows(doc: &FleetNetDoc, self_device_id: &str) -> Vec<(String, FleetNetRow)> {
    doc.devices
        .iter()
        .filter(|(id, _)| id.as_str() != self_device_id)
        .map(|(id, row)| (id.clone(), row.clone()))
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

/// ZEB-510 step 4: record a freshly-paired same-owner sibling in
/// `owner_device_cache` so `build_butler_set`'s `vk_lookup` resolves it and the
/// published butler-set advert can dial it. `add_enrollment` records the sibling
/// in the (separate) harmony-owner enrollment state, but the advert reads THIS
/// CRDT projection — which, co-located (no community device-intro, no CRDT
/// convergence between the owner's own devices), never learns the sibling.
/// Without this the advert SKIPS the sibling ("vk_lookup unresolved") even with
/// a perfect FleetNetDoc endpoint row (the ZEB-510 s7 failure: the depositor
/// fell back to the owner's own self-entry and never reached the butler).
///
/// Routed through the canonical [`crate::owner_state_crdt::OwnerState::apply_owner_device_update`]
/// so the sort/dedup, LWW, and pub-preserve invariants hold. That method
/// REPLACES the device list with the payload, so we UNION the sibling into the
/// owner's existing self-owner entry rather than clobbering already-known
/// devices, under a strictly-newer HLC (the LWW guard rejects stale). Idempotent:
/// a re-pair or a genuine later self-announcement supersedes this with the
/// identical identity pub (no conflict). Returns `true` iff the cache was
/// updated (the caller then persists + republishes the advert).
pub(crate) fn seed_sibling_device_cache(
    state: &mut crate::owner_state_crdt::OwnerState,
    self_owner: crate::owner_state_types::OwnerAddr,
    sibling_ed25519_verify: [u8; 32],
    wall_ms: u64,
) -> bool {
    let x_pub = match crate::dm_signing::ed25519_pub_to_x25519(&sibling_ed25519_verify) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(error = %e, "ZEB-510 step 4: sibling ed25519->x25519 failed; owner_device_cache seed skipped");
            return false;
        }
    };
    let mut identity_pub = [0u8; 64];
    identity_pub[..32].copy_from_slice(&x_pub);
    identity_pub[32..].copy_from_slice(&sibling_ed25519_verify);
    let Some(hash) = crate::dm_signing::derive_device_hash_from_identity_pub(&identity_pub) else {
        tracing::warn!("ZEB-510 step 4: sibling device-hash derivation failed; owner_device_cache seed skipped");
        return false;
    };

    // UNION with the existing self-owner entry: apply_owner_device_update takes
    // the payload AS the new device list (pub-preserving overlaps), so passing
    // only the sibling would drop already-known devices.
    let (mut devices, mut pubs, mut contacts, base_hlc) =
        match state.owner_device_cache.devices.get(&self_owner) {
            Some(e) => (
                e.devices.clone(),
                e.device_identity_pubs.clone(),
                e.device_tunnel_contacts.clone(),
                Some(e.learned_at.clone()),
            ),
            None => (Vec::new(), Vec::new(), Vec::new(), None),
        };
    // Idempotent only when the sibling is already present WITH its identity pub.
    // If the hash exists but its aligned pub is `None` (a Path-B "known by hash,
    // pub not yet propagated" state), fall through and re-add: `vk_lookup` only
    // resolves devices carrying `Some(pub)`, and `apply_owner_device_update`'s
    // Some-over-None dedup then fills the pub without duplicating the device.
    if let Some(idx) = devices.iter().position(|d| *d == hash) {
        if pubs.get(idx).is_some_and(|p| p.is_some()) {
            return false; // present with pub — true no-op
        }
    }
    devices.push(hash);
    pubs.push(Some(identity_pub));
    contacts.push(None);

    let device_id = hex::encode(self_owner.0);
    let learned_at = match base_hlc {
        Some(prev) if prev.wall_ms >= wall_ms => crate::owner_state_types::Hlc {
            wall_ms: prev.wall_ms,
            logical: prev.logical.saturating_add(1),
            device_id,
        },
        _ => crate::owner_state_types::Hlc {
            wall_ms,
            logical: 0,
            device_id,
        },
    };

    match state.apply_owner_device_update(self_owner, devices, pubs, contacts, learned_at) {
        crate::owner_state_crdt::ApplyOutcome::Rejected(r) => {
            tracing::warn!(reason = ?r, "ZEB-510 step 4: owner_device_cache seed rejected by LWW/invariant guard");
            false
        }
        _ => {
            tracing::info!(
                sibling = %hex::encode(sibling_ed25519_verify),
                "ZEB-510 step 4: seeded paired sibling into owner_device_cache for butler-set vk_lookup"
            );
            true
        }
    }
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
            feed_binding: None,
        }
    }

    fn petname(name: &str, set_at: Hlc) -> FleetNetPetname {
        FleetNetPetname {
            name: name.into(),
            set_at,
        }
    }

    // ZEB-510 step 4: seeding a paired sibling into owner_device_cache makes the
    // butler-set `vk_lookup` (vk_map_from_device_cache) resolve it, without which
    // build_butler_set skips the sibling and the depositor can't dial the butler.
    #[test]
    fn seed_sibling_device_cache_makes_vk_lookup_resolve() {
        use crate::owner_state_crdt::OwnerState;
        use crate::owner_state_types::OwnerAddr;

        let self_owner = OwnerAddr([0x11; 16]);
        let self_vk = [0x99u8; 32];
        // Real ed25519 device identities (a real verifying key always converts
        // to x25519, unlike an arbitrary 32-byte blob).
        let sib_ed: [u8; 32] = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])
            .verifying_key()
            .to_bytes();
        let sib2_ed: [u8; 32] = ed25519_dalek::SigningKey::from_bytes(&[0x43; 32])
            .verifying_key()
            .to_bytes();

        let mut state = OwnerState::default();
        let vk_map = |st: &OwnerState| {
            vk_map_from_device_cache(&st.owner_device_cache, &self_owner, "self-dev", self_vk)
        };

        // Before: the sibling does not resolve in the advert's vk map.
        assert!(!vk_map(&state).contains_key(&hex::encode(sib_ed)));

        // Seed it → vk_lookup now resolves the sibling to its ed25519 vk.
        assert!(seed_sibling_device_cache(
            &mut state, self_owner, sib_ed, 1_000
        ));
        assert_eq!(vk_map(&state).get(&hex::encode(sib_ed)), Some(&sib_ed));

        // Idempotent: a second seed of the same sibling is a no-op.
        assert!(!seed_sibling_device_cache(
            &mut state, self_owner, sib_ed, 2_000
        ));

        // A DIFFERENT sibling unions in without dropping the first.
        assert!(seed_sibling_device_cache(
            &mut state, self_owner, sib2_ed, 3_000
        ));
        let after = vk_map(&state);
        assert_eq!(after.get(&hex::encode(sib_ed)), Some(&sib_ed));
        assert_eq!(after.get(&hex::encode(sib2_ed)), Some(&sib2_ed));

        // LWW logical-bump branch: seed a THIRD sibling with a wall_ms (1_000)
        // <= the existing entry's learned_at.wall_ms (3_000 from sib2). The seed
        // MUST still construct a strictly-newer HLC (bumping `logical`), else
        // apply_owner_device_update rejects it as StaleHlc and the sibling
        // silently fails to resolve. Pins the `prev.wall_ms >= wall_ms` arm.
        let sib3_ed: [u8; 32] = ed25519_dalek::SigningKey::from_bytes(&[0x44; 32])
            .verifying_key()
            .to_bytes();
        assert!(seed_sibling_device_cache(
            &mut state, self_owner, sib3_ed, 1_000
        ));
        let after3 = vk_map(&state);
        assert_eq!(after3.get(&hex::encode(sib3_ed)), Some(&sib3_ed));
        // ...and the earlier siblings survive the strictly-newer re-write.
        assert_eq!(after3.get(&hex::encode(sib_ed)), Some(&sib_ed));
        assert_eq!(after3.get(&hex::encode(sib2_ed)), Some(&sib2_ed));
    }

    // ZEB-690 (item 5): pins the converge-fix fall-through — when the self-owner
    // entry already holds the sibling's device HASH but with its aligned pub
    // `None` (a Path-B "known by hash, pub not yet propagated" state), seeding
    // must fill the pub (so vk_lookup resolves) and NOT duplicate the hash.
    #[test]
    fn seed_sibling_fills_pub_when_hash_present_without_pub() {
        use crate::owner_state_crdt::{ApplyOutcome, OwnerState};
        use crate::owner_state_types::OwnerAddr;

        let self_owner = OwnerAddr([0x11; 16]);
        let self_vk = [0x99u8; 32];
        let sib_ed: [u8; 32] = ed25519_dalek::SigningKey::from_bytes(&[0x42; 32])
            .verifying_key()
            .to_bytes();
        // Reconstruct the sibling's device hash exactly as the seed does.
        let x_pub = crate::dm_signing::ed25519_pub_to_x25519(&sib_ed).unwrap();
        let mut identity_pub = [0u8; 64];
        identity_pub[..32].copy_from_slice(&x_pub);
        identity_pub[32..].copy_from_slice(&sib_ed);
        let hash = crate::dm_signing::derive_device_hash_from_identity_pub(&identity_pub).unwrap();

        let mut state = OwnerState::default();
        // Pre-seed: hash present, pub None (Path-B; older HLC than the seed below).
        // Assert acceptance so the precondition can't be silently vacuous.
        let pre = state.apply_owner_device_update(
            self_owner,
            vec![hash],
            vec![None],
            vec![None],
            hlc(500, "pre"),
        );
        assert!(
            !matches!(pre, ApplyOutcome::Rejected(_)),
            "pre-seed must be accepted, else the setup is vacuous: {pre:?}"
        );
        // Precondition: the entry holds the sibling's hash with its aligned pub None.
        {
            let e = state.owner_device_cache.devices.get(&self_owner).unwrap();
            let idx = e
                .devices
                .iter()
                .position(|d| *d == hash)
                .expect("pre-seeded hash present");
            assert!(
                e.device_identity_pubs[idx].is_none(),
                "precondition: aligned pub must be None"
            );
        }

        let vk_map = |st: &OwnerState| {
            vk_map_from_device_cache(&st.owner_device_cache, &self_owner, "self-dev", self_vk)
        };
        // vk_lookup does NOT resolve yet — pub is None.
        assert!(!vk_map(&state).contains_key(&hex::encode(sib_ed)));

        // Seed → falls through the idempotency guard and fills the pub.
        assert!(seed_sibling_device_cache(
            &mut state, self_owner, sib_ed, 1_000
        ));
        assert_eq!(vk_map(&state).get(&hex::encode(sib_ed)), Some(&sib_ed));

        // Postcondition: the SAME hash is retained (no duplicate) with its aligned
        // pub now Some — the fall-through filled it in place.
        let entry = state.owner_device_cache.devices.get(&self_owner).unwrap();
        assert_eq!(entry.devices.len(), 1, "device hash must not be duplicated");
        let idx = entry
            .devices
            .iter()
            .position(|d| *d == hash)
            .expect("seeded hash retained");
        assert!(
            entry.device_identity_pubs[idx].is_some(),
            "seed must fill the previously-None pub"
        );
    }

    // Pins the fleet-net-v1 wire format; NEVER regenerate. Mod-level so the
    // additive-decode test can prove pre-S4 bytes still parse (ZEB-668 S4).
    // See EXPECTED_OUTHOLD_DOC_HEX in dm_outhold.rs for the pattern.
    const EXPECTED_FLEET_NET_DOC_HEX: &str = "a3626476a2784061616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161a3626570982018aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa18aa6268726f72656c61792e616c7068612e636f6d627361a361771903e8616c006164656465762d61784062626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262626262a3626570982018bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb18bb6268726e72656c61792e626574612e636f6d627361a361771907d0616c006164656465762d6262706e784061616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161627061a36177190bb8616c006164696465762d6f776e6572";

    // ── Sibling-dial mapper + helper tests (ZEB-510) ─────────────────────────

    #[test]
    fn sibling_reachability_payload_maps_fields_and_is_unsigned() {
        let r = row(0xB2, "https://relay.example/", hlc(4242, "dev"));
        let p = sibling_reachability_payload(&r);
        assert_eq!(p.iroh_node_id, [0xB2; 32]);
        assert_eq!(p.home_relay_url, "https://relay.example/");
        assert_eq!(p.announced_at_ms, 4242);
        assert!(p.direct_addresses.is_empty());
        assert_eq!(p.identity_signature, [0u8; 64]); // verification-exempt
        assert!(p.butler_set.is_empty());
        assert_eq!(p.bs_at, 0);
    }

    #[test]
    fn sibling_rows_excludes_self_and_returns_the_rest() {
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert("self-id".into(), row(0x01, "a", hlc(10, "dev")));
        doc.devices
            .insert("sib-b2".into(), row(0x02, "b", hlc(20, "dev")));
        doc.devices
            .insert("sib-b3".into(), row(0x03, "c", hlc(30, "dev")));

        let out = sibling_rows(&doc, "self-id");
        let ids: std::collections::BTreeSet<&str> = out.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(out.len(), 2);
        assert!(ids.contains("sib-b2"));
        assert!(ids.contains("sib-b3"));
        assert!(!ids.contains("self-id"), "self row must be excluded");
    }

    #[test]
    fn sibling_rows_empty_when_only_self_present() {
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert("self-id".into(), row(0x01, "a", hlc(10, "dev")));
        assert!(sibling_rows(&doc, "self-id").is_empty());
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
    fn feed_binding_cbor_roundtrips_and_omits_when_none() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let mut r = row(0x01, "relay.example.com", hlc(5, "dev-a"));
        assert!(r.feed_binding.is_none());
        let none_bytes = canonical_cbor_encode(&r).expect("encode none");
        // Additive: the `fb` key is omitted when None, so pre-migration rows
        // stay byte-identical to what old builds produce.
        assert!(
            !none_bytes.windows(2).any(|w| w == b"fb"),
            "fb key omitted when feed_binding is None"
        );
        assert_eq!(
            canonical_cbor_decode::<FleetNetRow>(&none_bytes).unwrap(),
            r
        );

        // With a binding (an opaque JSON string — NOT a nested map, so the
        // canonical-CBOR same-length-key contract is unaffected) it round-trips
        // deterministically.
        r.feed_binding = Some(r#"{"feedId":"aa","ownerId":"bb"}"#.to_string());
        let bytes = canonical_cbor_encode(&r).expect("encode some");
        assert_eq!(canonical_cbor_decode::<FleetNetRow>(&bytes).unwrap(), r);
        assert_eq!(
            canonical_cbor_encode(&r).unwrap(),
            bytes,
            "same value encodes byte-identically"
        );
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

    /// ZEB-852 (D2-MERGE): device rows are a STORED replicated register LWW'd by
    /// the sibling's self-stamped `seen_at`. A future-dated `seen_at` would win
    /// the LWW and freeze the row forever — so it must be REJECTED against the
    /// receiver's control-tier ceiling. All three branches:
    ///   (1) a year-ahead row does NOT replace an honest current row, and does
    ///       NOT insert on an absent key (poison-insert also blocked);
    ///   (2) an honest NEWER in-range row IS adopted (guard doesn't over-reject);
    ///   (3) an unreadable receiver clock (`None`) ⇒ apply-all: even a far-future
    ///       row is adopted (a bad LOCAL clock must never drop honest rows).
    /// (1) and (2) run through the public `merge_from` (real `receiver_now_ms()`
    /// clock seam); (3) uses the injected `merge_from_bounded` seam `merge_from`
    /// delegates to, the only way to force the `None` branch deterministically.
    #[test]
    fn merge_from_rejects_future_seen_at() {
        let now = crate::clock_trust::receiver_now_ms().expect("host clock is post-epoch");
        let one_year: u64 = 365 * 24 * 60 * 60 * 1000;

        // (1a) Future-dated remote row must NOT replace an honest current row.
        let mut local = FleetNetDoc::default();
        local
            .devices
            .insert("dev-a".into(), row(0x01, "relay.honest", hlc(now, "dev-a")));
        let mut remote = FleetNetDoc::default();
        remote.devices.insert(
            "dev-a".into(),
            row(0x02, "relay.future", hlc(now + one_year, "dev-a")),
        );
        let out = local.merge_from(remote);
        assert!(!out.changed, "future-dated remote row must be rejected");
        assert_eq!(local.devices["dev-a"].home_relay, "relay.honest");
        assert_eq!(local.devices["dev-a"].seen_at.wall_ms, now);

        // (1b) Future-dated remote row on an ABSENT key must also be rejected
        // (a poison-insert freezes the register just as a poison-replace does).
        let mut local_abs = FleetNetDoc::default();
        let mut remote_abs = FleetNetDoc::default();
        remote_abs.devices.insert(
            "dev-new".into(),
            row(0x03, "relay.future", hlc(now + one_year, "dev-new")),
        );
        let out_abs = local_abs.merge_from(remote_abs);
        assert!(
            !out_abs.changed,
            "future-dated absent-key insert must be rejected"
        );
        assert!(!local_abs.devices.contains_key("dev-new"));

        // (2) An honest NEWER in-range row IS adopted (well within the 5-min
        // control tier), proving the guard doesn't over-reject.
        let mut local_ok = FleetNetDoc::default();
        local_ok.devices.insert(
            "dev-a".into(),
            row(0x01, "relay.old", hlc(now - 10_000, "dev-a")),
        );
        let mut remote_ok = FleetNetDoc::default();
        remote_ok.devices.insert(
            "dev-a".into(),
            row(0x02, "relay.newer", hlc(now + 1_000, "dev-a")),
        );
        let out_ok = local_ok.merge_from(remote_ok);
        assert!(out_ok.changed, "in-range newer row must be adopted");
        assert_eq!(local_ok.devices["dev-a"].home_relay, "relay.newer");

        // (3) Unreadable receiver clock (None) ⇒ apply-all: even a far-future row
        // is adopted, so a bad LOCAL clock never drops honest sibling rows.
        let mut local_none = FleetNetDoc::default();
        local_none
            .devices
            .insert("dev-a".into(), row(0x01, "relay.honest", hlc(now, "dev-a")));
        let mut remote_none = FleetNetDoc::default();
        remote_none.devices.insert(
            "dev-a".into(),
            row(0x02, "relay.future", hlc(now + one_year, "dev-a")),
        );
        let out_none = local_none.merge_from_bounded(remote_none, None);
        assert!(
            out_none.changed,
            "None receiver clock ⇒ apply-all (adopt), never drop honest rows"
        );
        assert_eq!(local_none.devices["dev-a"].home_relay, "relay.future");
    }

    // ── Pin LWW pair tests ────────────────────────────────────────────────────

    #[test]
    fn pin_lww_pair_merges() {
        // Remote has a strictly newer pinned_at → both pinned + pinned_at taken.
        let mut local = FleetNetDoc {
            pinned: Some("dev-old".into()),
            pinned_at: hlc(5, "dev-x"),
            ..Default::default()
        };

        let remote = FleetNetDoc {
            pinned: Some("dev-new".into()),
            pinned_at: hlc(10, "dev-x"),
            ..Default::default()
        };

        let out = local.merge_from(remote);
        assert!(out.changed, "newer remote pin must flag changed");
        assert_eq!(local.pinned.as_deref(), Some("dev-new"));
        assert_eq!(local.pinned_at.wall_ms, 10);
    }

    #[test]
    fn pin_lww_older_remote_ignored() {
        let mut local = FleetNetDoc {
            pinned: Some("dev-keep".into()),
            pinned_at: hlc(20, "dev-x"),
            ..Default::default()
        };

        let remote = FleetNetDoc {
            pinned: Some("dev-discard".into()),
            pinned_at: hlc(5, "dev-x"),
            ..Default::default()
        };

        let out = local.merge_from(remote);
        assert!(!out.changed);
        assert_eq!(local.pinned.as_deref(), Some("dev-keep"));
    }

    #[test]
    fn pin_lww_same_stamp_local_wins() {
        // Equal pinned_at → is_strictly_newer_than is false → local kept.
        let mut local = FleetNetDoc {
            pinned: Some("dev-local".into()),
            pinned_at: hlc(10, "dev-x"),
            ..Default::default()
        };

        let remote = FleetNetDoc {
            pinned: Some("dev-remote".into()),
            pinned_at: hlc(10, "dev-x"),
            ..Default::default()
        };

        local.merge_from(remote);
        assert_eq!(local.pinned.as_deref(), Some("dev-local"));
    }

    // ── Petname LWW tests (ZEB-668 S4) ───────────────────────────────────────

    #[test]
    fn petname_lww_newer_remote_wins() {
        let mut local = FleetNetDoc::default();
        local
            .petnames
            .insert("dev-a".into(), petname("old", hlc(5, "dev-x")));
        let mut remote = FleetNetDoc::default();
        remote
            .petnames
            .insert("dev-a".into(), petname("new", hlc(10, "dev-y")));

        let out = local.merge_from(remote);
        assert!(out.changed);
        assert_eq!(local.petnames["dev-a"].name, "new");
        assert_eq!(local.petnames["dev-a"].set_at.wall_ms, 10);
    }

    #[test]
    fn petname_lww_older_remote_ignored_and_tie_keeps_local() {
        let mut local = FleetNetDoc::default();
        local
            .petnames
            .insert("dev-a".into(), petname("keep", hlc(10, "dev-x")));

        let mut older = FleetNetDoc::default();
        older
            .petnames
            .insert("dev-a".into(), petname("stale", hlc(5, "dev-x")));
        assert!(!local.merge_from(older).changed);
        assert_eq!(local.petnames["dev-a"].name, "keep");

        let mut tie = FleetNetDoc::default();
        tie.petnames
            .insert("dev-a".into(), petname("tie", hlc(10, "dev-x")));
        assert!(!local.merge_from(tie).changed);
        assert_eq!(local.petnames["dev-a"].name, "keep");
    }

    #[test]
    fn petname_absent_key_inserts_unconditionally() {
        let mut local = FleetNetDoc::default();
        let mut remote = FleetNetDoc::default();
        remote
            .petnames
            .insert("dev-b".into(), petname("KRILE", hlc(1, "dev-b")));
        assert!(local.merge_from(remote).changed);
        assert_eq!(local.petnames["dev-b"].name, "KRILE");
    }

    #[test]
    fn empty_petnames_map_is_omitted_from_wire_encoding() {
        // Additive wire compat: a doc with no petnames must encode WITHOUT the
        // "pt" key — byte-identical to the pre-S4 shape. (The pinned
        // EXPECTED_FLEET_NET_DOC_HEX fixture is the cross-check: it must keep
        // passing untouched.)
        let mut doc = FleetNetDoc::default();
        doc.devices.insert(
            "dev-a".into(),
            row(0x01, "relay.example.com", hlc(1, "dev-a")),
        );
        let mut buf = Vec::new();
        ciborium::into_writer(&doc, &mut buf).expect("encode");
        let val: ciborium::Value = ciborium::from_reader(buf.as_slice()).expect("decode");
        let map = val.as_map().expect("top-level map");
        assert!(
            !map.iter().any(|(k, _)| k.as_text() == Some("pt")),
            "empty petnames must be skip-serialized"
        );
    }

    #[test]
    fn petnames_round_trip_and_old_bytes_decode_to_empty_map() {
        // Round-trip with an entry present.
        let mut doc = FleetNetDoc::default();
        doc.petnames
            .insert("dev-a".into(), petname("KRILE", hlc(7, "dev-b")));
        let mut buf = Vec::new();
        ciborium::into_writer(&doc, &mut buf).expect("encode");
        let back: FleetNetDoc = ciborium::from_reader(buf.as_slice()).expect("decode");
        assert_eq!(back, doc);

        // Pre-S4 bytes (the pinned fixture hex) decode with petnames defaulted.
        let old = hex::decode(EXPECTED_FLEET_NET_DOC_HEX).expect("fixture hex");
        let decoded: FleetNetDoc = ciborium::from_reader(old.as_slice()).expect("decode old");
        assert!(decoded.petnames.is_empty());
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

    /// ZEB-852 (D2 read-side): a fast-clocked sibling must not win the butler-set
    /// order. Two defenses, both exercised here:
    ///   (a) UPPER-BOUND FILTER — a row stamped a full year ahead of `now` is
    ///       dropped entirely (mirrors `fresh_butler_set`'s upper `bs_at` bound).
    ///   (b) SORT CLAMP — a still-in-window but future-dated row (its stamp is
    ///       ≤ `now + BUTLER_SET_FRESHNESS_MS`, so it survives the filter) must
    ///       NOT out-rank an honest present row purely by its inflated wall_ms;
    ///       `min(wall_ms, now)` collapses it to `now` and the device-id tiebreak
    ///       decides.
    /// In-range ordering is preserved: an honest fresh row still leads an honest
    /// stale one.
    #[test]
    fn butler_set_order_sweeps_and_deranks_future_sibling() {
        let window = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
        let now: u64 = 2_000_000_000_000; // realistic present-day wall (ms)
        let stale_before = now - window; // exactly how every caller derives it
        let one_year: u64 = 365 * 24 * 60 * 60 * 1000;

        let mut doc = FleetNetDoc::default();
        // Honest present row (device-id sorts FIRST among the clamp-tied pair).
        doc.devices.insert(
            "dev-a-honest-fresh".into(),
            row(0x01, "relay.fresh", hlc(now, "d")),
        );
        // Honest older-but-in-window row → ranks behind the present rows.
        doc.devices.insert(
            "dev-b-honest-stale".into(),
            row(0x02, "relay.stale", hlc(now - 100_000, "d")),
        );
        // Future-but-in-window sibling (≤ now + window): SURVIVES the filter, so
        // it exercises the sort clamp. Its device-id sorts AFTER the honest one,
        // so if the clamp works it must land behind dev-a-honest-fresh.
        doc.devices.insert(
            "dev-z-future-inwindow".into(),
            row(0x03, "relay.future", hlc(now + window / 2, "d")),
        );
        // Far-future sibling (a year ahead): must be FILTERED OUT by the upper bound.
        doc.devices.insert(
            "dev-future-1yr".into(),
            row(0x04, "relay.evil", hlc(now + one_year, "d")),
        );

        let order = butler_set_order(&doc, stale_before);

        // (a) Upper bound: the year-ahead row is gone; the other three remain.
        assert_eq!(order.len(), 3, "far-future row must be filtered out");
        assert!(
            !order.iter().any(|(id, _)| id == "dev-future-1yr"),
            "far-future sibling must not appear at all"
        );
        // (b) Sort clamp: the in-window future sibling clamps to `now`, tying with
        // the honest present row, so the device-id tiebreak puts the honest row at
        // slot 0 — the future sibling does NOT rank ahead by its inflated stamp.
        assert_eq!(
            order[0].0, "dev-a-honest-fresh",
            "future-dated in-window sibling must not out-rank the honest present row"
        );
        // In-range ordering preserved: honest fresh ahead of honest stale.
        let pos_fresh = order.iter().position(|(id, _)| id == "dev-a-honest-fresh");
        let pos_stale = order.iter().position(|(id, _)| id == "dev-b-honest-stale");
        assert!(
            pos_fresh < pos_stale,
            "honest fresh row must rank ahead of the honest stale row"
        );
    }

    #[test]
    fn butler_rank_logical_inflation_does_not_win_slot_zero() {
        let window = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
        let now: u64 = 2_000_000_000_000;
        let stale_before = now - window;

        let mut doc = FleetNetDoc::default();
        // Honest present row: wall = now, logical = 0, LOWER device-id key.
        doc.devices.insert(
            "dev-honest".into(),
            row(0x01, "relay.honest", hlc(now, "d")),
        );
        // Inflation attempt: SAME clamped wall (now), logical = u32::MAX, HIGHER
        // device-id key. Pre-fix the descending-`logical` secondary ranked this at
        // slot 0; post-fix (logical dropped) the device-id tiebreak keeps honest ahead.
        doc.devices.insert(
            "dev-zzz-evil".into(),
            row(
                0x02,
                "relay.evil",
                Hlc {
                    wall_ms: now,
                    logical: u32::MAX,
                    device_id: "evil".into(),
                },
            ),
        );

        let order = butler_set_order(&doc, stale_before);
        assert_eq!(
            order[0].0, "dev-honest",
            "a self-inflated `logical` must NOT win butler slot 0 over an honest present row"
        );
    }

    #[test]
    fn butler_rank_clamped_wall_tie_orders_by_device_id_deterministically() {
        let window = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
        let now: u64 = 2_000_000_000_000;
        let stale_before = now - window;

        let mut doc = FleetNetDoc::default();
        // Identical clamped wall and logical → only device-id decides.
        for key in ["dev-ccc", "dev-aaa", "dev-bbb"] {
            doc.devices
                .insert(key.into(), row(0x01, "relay", hlc(now, "d")));
        }
        let keys: Vec<String> = butler_set_order(&doc, stale_before)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            keys,
            vec![
                "dev-aaa".to_string(),
                "dev-bbb".to_string(),
                "dev-ccc".to_string()
            ],
            "clamped-wall ties must order by ascending device-id, deterministically"
        );
    }

    #[test]
    fn merge_rejects_future_pinned_at_then_honest_later_pin_wins() {
        let now: u64 = 2_000_000_000_000;
        let six_min = 6 * 60 * 1000;

        let mut local = FleetNetDoc {
            pinned: Some("dev-p0".into()),
            pinned_at: hlc(now, "owner"),
            ..Default::default()
        };

        // Far-future (> 5-min control tier) stamp: must be rejected, not win the LWW.
        let poison = FleetNetDoc {
            pinned: Some("dev-evil".into()),
            pinned_at: hlc(now + six_min, "owner"),
            ..Default::default()
        };
        local.merge_from_bounded(poison, Some(now));
        assert_eq!(
            local.pinned.as_deref(),
            Some("dev-p0"),
            "a future-dated pinned_at must be REJECTED, not win the LWW"
        );

        // Register must remain LIVE (not frozen): an honest later pin still wins.
        let honest = FleetNetDoc {
            pinned: Some("dev-p2".into()),
            pinned_at: hlc(now + 1000, "owner"),
            ..Default::default()
        };
        local.merge_from_bounded(honest, Some(now + 2000));
        assert_eq!(
            local.pinned.as_deref(),
            Some("dev-p2"),
            "the pin register must stay live after rejecting a poison stamp (not frozen)"
        );
    }

    #[test]
    fn merge_accepts_in_tolerance_future_pinned_at() {
        let now: u64 = 2_000_000_000_000;
        let four_min = 4 * 60 * 1000;

        let mut local = FleetNetDoc {
            pinned: Some("dev-p0".into()),
            pinned_at: hlc(now, "owner"),
            ..Default::default()
        };

        let near = FleetNetDoc {
            pinned: Some("dev-p1".into()),
            pinned_at: hlc(now + four_min, "owner"),
            ..Default::default()
        };
        local.merge_from_bounded(near, Some(now));
        assert_eq!(
            local.pinned.as_deref(),
            Some("dev-p1"),
            "a pin within the 5-min control tier must still be applied (reject is > tier only)"
        );
    }

    #[test]
    fn merge_rejects_future_petname_set_at_then_honest_later_wins() {
        let now: u64 = 2_000_000_000_000;
        let six_min = 6 * 60 * 1000;
        let key = "dev-x".to_string();

        let mut local = FleetNetDoc::default();
        local
            .petnames
            .insert(key.clone(), petname("orig", hlc(now, "owner")));

        let mut poison = FleetNetDoc::default();
        poison
            .petnames
            .insert(key.clone(), petname("evil", hlc(now + six_min, "owner")));
        local.merge_from_bounded(poison, Some(now));
        assert_eq!(
            local.petnames.get(&key).map(|p| p.name.as_str()),
            Some("orig"),
            "a future-dated petname set_at must be REJECTED"
        );

        let mut honest = FleetNetDoc::default();
        honest
            .petnames
            .insert(key.clone(), petname("real", hlc(now + 1000, "owner")));
        local.merge_from_bounded(honest, Some(now + 2000));
        assert_eq!(
            local.petnames.get(&key).map(|p| p.name.as_str()),
            Some("real"),
            "the petname register must stay live after rejecting a poison stamp"
        );
    }

    #[test]
    fn merge_none_clock_applies_future_pin_and_petname() {
        let now: u64 = 2_000_000_000_000;
        let one_year: u64 = 365 * 24 * 60 * 60 * 1000;
        let key = "dev-x".to_string();

        let mut local = FleetNetDoc {
            pinned: Some("dev-p0".into()),
            pinned_at: hlc(now, "owner"),
            ..Default::default()
        };
        local
            .petnames
            .insert(key.clone(), petname("orig", hlc(now, "owner")));

        // Unreadable local clock (None) ⇒ apply-all: a bad LOCAL clock must never
        // drop honest pin/petname updates, even far-future ones.
        let mut remote = FleetNetDoc {
            pinned: Some("dev-far".into()),
            pinned_at: hlc(now + one_year, "owner"),
            ..Default::default()
        };
        remote
            .petnames
            .insert(key.clone(), petname("far", hlc(now + one_year, "owner")));
        local.merge_from_bounded(remote, None);
        assert_eq!(
            local.pinned.as_deref(),
            Some("dev-far"),
            "None clock ⇒ apply-all for the pin"
        );
        assert_eq!(
            local.petnames.get(&key).map(|p| p.name.as_str()),
            Some("far"),
            "None clock ⇒ apply-all for the petname"
        );
    }

    #[test]
    fn butler_rank_r1_now_stamper_leads_then_pin_overrides() {
        let window = crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
        let refresh = crate::butler_deposit::BUTLER_SET_REFRESH_MS;
        let now: u64 = 2_000_000_000_000;
        let stale_before = now - window;

        let mut doc = FleetNetDoc::default();
        // A "now"-stamper (a sibling always claiming maximum freshness).
        doc.devices.insert(
            "dev-nowstamper".into(),
            row(0x01, "relay.ns", hlc(now, "d")),
        );
        // An honest device, one refresh interval stale.
        doc.devices.insert(
            "dev-honest".into(),
            row(0x02, "relay.h", hlc(now - refresh, "d")),
        );

        // (a) ACCEPTED RESIDUAL (ZEB-856 R1): the fresher self-stamp leads. This is
        // deliberately UNFIXED — a demotion here would be fail-open (could route
        // deposits to a dead device). Canary: if a future change alters ranking so
        // the now-stamper no longer leads, this trips and forces a fresh decision.
        let order = butler_set_order(&doc, stale_before);
        assert_eq!(
            order[0].0, "dev-nowstamper",
            "R1: the freshest self-stamp leads (accepted residual)"
        );

        // (b) MITIGATION: the owner's pin overrides freshness ranking (and R3 keeps
        // the pin un-freezable). Pinning the honest device puts it at slot 0.
        doc.pinned = Some("dev-honest".into());
        let order = butler_set_order(&doc, stale_before);
        assert_eq!(
            order[0].0, "dev-honest",
            "R1 mitigation: the owner pin overrides freshness ranking"
        );
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
    fn build_set_force_includes_self_when_own_row_stale() {
        // Self's row is STALE while two fresh siblings would fill the cap.
        // Self must still be force-included (the publisher is online at
        // blob-build time), exactly once, at the front, evicting the
        // lowest-priority sibling.
        let stale_before = 1500u64;
        let mut doc = FleetNetDoc::default();
        doc.devices
            .insert(self_id(), row(0x01, "relay.self.stale", hlc(1000, "self")));
        doc.devices
            .insert(sib1_id(), row(0x02, "relay.sib1", hlc(3000, "sib1")));
        doc.devices
            .insert(sib2_id(), row(0x03, "relay.sib2", hlc(2000, "sib2")));

        let set = build_butler_set(
            &doc,
            &self_id(),
            self_entry(),
            &test_vk_lookup,
            stale_before,
        );
        let selves: Vec<_> = set
            .iter()
            .filter(|e| e.device_id == self_entry().device_id)
            .collect();
        assert_eq!(selves.len(), 1, "self must appear exactly once");
        assert_eq!(
            set[0].device_id,
            self_entry().device_id,
            "force-inserted self leads the set"
        );
        assert_eq!(
            set.len(),
            crate::butler_deposit::BUTLER_SET_MAX_ENTRIES,
            "cap still respected (lowest-priority sibling evicted)"
        );
    }

    #[test]
    fn build_set_self_stale_pinned_sibling_keeps_slot_zero() {
        // Self's row is stale; a fresh PINNED sibling exists. The pinned
        // sibling keeps slot 0 (pinned-first contract); self is inserted
        // right behind it.
        let stale_before = 1500u64;
        let mut doc = FleetNetDoc {
            pinned: Some(sib1_id()),
            pinned_at: hlc(5000, "owner"),
            ..Default::default()
        };
        doc.devices
            .insert(self_id(), row(0x01, "relay.self.stale", hlc(1000, "self")));
        doc.devices
            .insert(sib1_id(), row(0x02, "relay.sib1", hlc(3000, "sib1")));

        let set = build_butler_set(
            &doc,
            &self_id(),
            self_entry(),
            &test_vk_lookup,
            stale_before,
        );
        assert_eq!(set.len(), 2);
        assert_eq!(
            set[0].device_ed25519_verify, [0xBB; 32],
            "pinned sibling keeps slot 0"
        );
        assert!(set[0].pinned);
        assert_eq!(
            set[1].device_id,
            self_entry().device_id,
            "force-inserted self goes behind the pinned sibling"
        );
        assert!(!set[1].pinned);
    }

    #[test]
    fn build_set_self_row_missing_but_pinned_self_leads() {
        // Self has NO row at all (e.g. wiped doc) but doc.pinned == self:
        // the force-inserted self leads with pinned=true (the pin LWW pair
        // can survive a row wipe).
        let mut doc = FleetNetDoc {
            pinned: Some(self_id()),
            pinned_at: hlc(5000, "owner"),
            ..Default::default()
        };
        doc.devices
            .insert(sib1_id(), row(0x02, "relay.sib1", hlc(3000, "sib1")));

        let set = build_butler_set(&doc, &self_id(), self_entry(), &test_vk_lookup, 0);
        assert_eq!(set.len(), 2);
        assert_eq!(
            set[0].device_id,
            self_entry().device_id,
            "pinned self leads despite missing row"
        );
        assert!(set[0].pinned, "pinned flag from the doc's pin LWW pair");
        assert_eq!(set[1].device_ed25519_verify, [0xBB; 32]);
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
            feed_binding: None,
        };
        let row_b = FleetNetRow {
            iroh_endpoint_id: [0xBB; 32],
            home_relay: "relay.beta.com".into(),
            seen_at: Hlc {
                wall_ms: 2000,
                logical: 0,
                device_id: "dev-b".into(),
            },
            feed_binding: None,
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
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::{mpsc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x66u8; 32]).expect("derive kt"));
        let doc = Arc::new(Mutex::new(FleetNetDoc::default()));
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".to_string(),
        )));
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
        let cas: Arc<dyn ContentStore> = Arc::new(InMemoryStub::default());
        let merger: Merger<FleetNetDoc> = Arc::new(|local, remote| local.merge_from(remote));

        let engine = FleetSyncEngine::<FleetNetDoc>::new(FleetSyncConfig {
            keys: Some(crate::owner_state_crypto::FleetKeySet::new(kt)),
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
                cipher: crate::fleet_dataset_file::test_cipher(),
            }),
            lookup_key_tag: b"fleet-net-v1",
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            publish_seen: true,
            on_applied: None,
            sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
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

#[cfg(test)]
mod vine_relay_set_tests {
    use super::*;
    use crate::butler_deposit::BUTLER_SET_FRESHNESS_MS;
    use crate::owner_state_types::Hlc;
    use crate::pkarr_vines::{VineRelayEntry, VINE_RELAY_SET_MAX};

    const SELF_ID: &str = "self-device";
    const SELF_EP: [u8; 32] = [0xEE; 32];

    fn row(ep: u8, relay: &str, wall_ms: u64) -> FleetNetRow {
        FleetNetRow {
            iroh_endpoint_id: [ep; 32],
            home_relay: relay.to_string(),
            seen_at: Hlc {
                wall_ms,
                logical: 0,
                device_id: String::new(),
            },
            feed_binding: None,
        }
    }

    fn self_entry() -> VineRelayEntry {
        VineRelayEntry {
            iroh_endpoint_id: SELF_EP,
            home_relay: "https://self.example".to_string(),
        }
    }

    fn doc_with(rows: &[(&str, FleetNetRow)]) -> FleetNetDoc {
        let mut d = FleetNetDoc::default();
        for (id, r) in rows {
            d.devices.insert((*id).to_string(), r.clone());
        }
        d
    }

    #[test]
    fn empty_doc_yields_self_only() {
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        let out = build_vine_relay_set(&FleetNetDoc::default(), SELF_ID, self_entry(), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].iroh_endpoint_id, SELF_EP);
    }

    #[test]
    fn self_snapshot_row_replaced_by_live_entry() {
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        // self's snapshot row carries STALE transport (ep 0x11); must be dropped
        // in favor of self_entry's live ep 0xEE.
        let doc = doc_with(&[
            (SELF_ID, row(0x11, "https://old-self.example", now)),
            ("bb", row(0x22, "https://b.example", now)),
        ]);
        let out = build_vine_relay_set(&doc, SELF_ID, self_entry(), now);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out.iter().filter(|e| e.iroh_endpoint_id == SELF_EP).count(),
            1
        );
        assert!(
            out.iter().all(|e| e.iroh_endpoint_id != [0x11; 32]),
            "stale self row must be replaced"
        );
        assert!(out.iter().any(|e| e.iroh_endpoint_id == [0x22; 32]));
    }

    #[test]
    fn caps_at_max_with_self_forced() {
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        // 6 fresh siblings, self NOT among them → self force-included, capped.
        let rows: Vec<(&str, FleetNetRow)> = vec![
            ("s0", row(0x30, "https://s.example", now)),
            ("s1", row(0x31, "https://s.example", now)),
            ("s2", row(0x32, "https://s.example", now)),
            ("s3", row(0x33, "https://s.example", now)),
            ("s4", row(0x34, "https://s.example", now)),
            ("s5", row(0x35, "https://s.example", now)),
        ];
        let out = build_vine_relay_set(&doc_with(&rows), SELF_ID, self_entry(), now);
        assert_eq!(out.len(), VINE_RELAY_SET_MAX);
        assert_eq!(
            out.iter().filter(|e| e.iroh_endpoint_id == SELF_EP).count(),
            1
        );
        assert_eq!(
            out[0].iroh_endpoint_id, SELF_EP,
            "force-inserted self leads"
        );
    }

    #[test]
    fn stale_sibling_excluded() {
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        let doc = doc_with(&[
            ("bb", row(0x22, "https://b.example", now)),
            (
                "cc",
                row(0x33, "https://c.example", now - BUTLER_SET_FRESHNESS_MS - 1),
            ),
        ]);
        let out = build_vine_relay_set(&doc, SELF_ID, self_entry(), now);
        assert_eq!(out.len(), 2, "self (forced) + fresh bb; stale cc excluded");
        assert!(out.iter().any(|e| e.iroh_endpoint_id == [0x22; 32]));
        assert!(out.iter().all(|e| e.iroh_endpoint_id != [0x33; 32]));
    }

    #[test]
    fn future_skewed_sibling_does_not_outrank_present() {
        // Inherits the ZEB-852 clamp from butler_set_order: an in-window but
        // future-dated sibling must not out-rank an honest present row. Both are
        // clamped to `now`, then the ascending device-id tiebreak decides.
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        let doc = doc_with(&[
            ("bb-honest", row(0x22, "https://b.example", now)),
            (
                "zz-skewed",
                row(0x33, "https://z.example", now + BUTLER_SET_FRESHNESS_MS / 2),
            ),
        ]);
        // self force-inserts at front; assert honest bb precedes skewed zz.
        let out = build_vine_relay_set(&doc, SELF_ID, self_entry(), now);
        let pos = |ep: [u8; 32]| out.iter().position(|e| e.iroh_endpoint_id == ep).unwrap();
        assert!(
            pos([0x22; 32]) < pos([0x33; 32]),
            "clamped honest row must precede future-skewed row"
        );
    }

    #[test]
    fn pinned_sibling_leads_when_self_force_inserted() {
        // 4 fresh siblings (cap-filling) with one pinned, self NOT in the doc →
        // self is force-included, but the pinned sibling must keep slot 0
        // (pinned-first, mirroring build_butler_set), not be displaced by self.
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        let mut doc = doc_with(&[
            ("bb", row(0x22, "https://b.example", now)),
            ("cc", row(0x33, "https://c.example", now)),
            ("dd", row(0x44, "https://d.example", now)),
            ("ee", row(0x55, "https://e.example", now)),
        ]);
        doc.pinned = Some("bb".to_string());
        let out = build_vine_relay_set(&doc, SELF_ID, self_entry(), now);
        assert_eq!(out.len(), VINE_RELAY_SET_MAX);
        assert_eq!(
            out[0].iroh_endpoint_id, [0x22; 32],
            "pinned sibling keeps slot 0"
        );
        assert_eq!(
            out[1].iroh_endpoint_id, SELF_EP,
            "self inserted right after the pin"
        );
    }

    #[test]
    fn vine_selection_view_catches_changes_beyond_the_butler_prefix() {
        // ZEB-820 (Greptile P1): the vine re-publish gate must fire on a change
        // outside the butler top-2 prefix. Here the 3rd device changes its relay
        // — the butler selection_view is unchanged, the vine one is not.
        let now = BUTLER_SET_FRESHNESS_MS * 10;
        let cutoff = now.saturating_sub(BUTLER_SET_FRESHNESS_MS);
        let base = doc_with(&[
            ("aa", row(0x11, "https://a.example", now)),
            ("bb", row(0x22, "https://b.example", now)),
            ("cc", row(0x33, "https://c.example", now)),
        ]);
        let changed = doc_with(&[
            ("aa", row(0x11, "https://a.example", now)),
            ("bb", row(0x22, "https://b.example", now)),
            ("cc", row(0x99, "https://c2.example", now)),
        ]);
        assert_eq!(
            selection_view(&base, cutoff),
            selection_view(&changed, cutoff),
            "butler top-2 view must be unchanged"
        );
        assert_ne!(
            vine_selection_view(&base, cutoff),
            vine_selection_view(&changed, cutoff),
            "vine top-4 view must catch the 3rd-device change"
        );
    }
}
