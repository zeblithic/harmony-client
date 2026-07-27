//! ZEB-815: unified community address book — replaces the two standalone
//! `ReachabilityResolver` / `CommunityRelayResolver` stores with one keyed by
//! entry kind, so later tasks can seal + sync a single dataset instead of two.
//! This file (Task 1) is the in-memory store only: LWW rows, TTL read-filter,
//! per-community and per-member bounds, eviction. Sealed wire format and
//! event-loop wiring land in later tasks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ciborium::{from_reader, into_writer};
use serde::{Deserialize, Serialize};

use crate::community_relay_announce::{
    CommunityRelayAnnouncePayload, COMMUNITY_RELAY_AD_FRESHNESS_MS,
};
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use crate::reachability_record::ReachabilityAnnouncePayload;

/// Max reachability rows retained per (community, actor). An actor's
/// `ADDRBOOK_MAX_NODES_PER_MEMBER`-th-plus distinct device evicts that
/// actor's own oldest-stamped reachability row on insert.
pub const ADDRBOOK_MAX_NODES_PER_MEMBER: usize = 8;
/// Max relay rows retained per (community, actor) — the relay-class twin of
/// `ADDRBOOK_MAX_NODES_PER_MEMBER`. `relay_device_id` is a self-asserted
/// field inside the member's own signed payload, so without this bound a
/// single Joined member could mint unbounded distinct relay keys and exhaust
/// `ADDRBOOK_MAX_ROWS` for the whole community.
pub const ADDRBOOK_MAX_RELAYS_PER_MEMBER: usize = 8;
/// Hard cap on distinct keys per community. A brand-new key past this cap
/// is ignored (`UpsertOutcome::IgnoredCapped`); existing keys still update.
pub const ADDRBOOK_MAX_ROWS: usize = 4096;
/// Forward-clock tolerance applied to `stamped_at_ms` on `upsert` (mirrors
/// `reachability_resolver::FUTURE_SKEW_TOLERANCE_MS`, the sibling store's
/// forward-clock clamp at admission) — a maliciously future-stamped row
/// can't stay ahead of every future honest update.
pub const ADDRBOOK_SKEW_TOLERANCE_MS: u64 = 300_000;
/// Reachability row TTL applied on read (`rows_for_community`) — 24h. Relay
/// rows reuse the existing `COMMUNITY_RELAY_AD_FRESHNESS_MS` (15 min).
pub const ADDRBOOK_REACHABILITY_TTL_MS: u64 = 86_400_000;

/// Identifies one address-book row within a community: reachability rows key
/// on (actor, iroh node id); relay rows key on (actor, relay device id).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AddressBookKey {
    Reachability(OwnerAddr, [u8; 32]),
    Relay(OwnerAddr, [u8; 16]),
}

/// The two entry kinds an address-book row can carry — the payloads
/// previously routed through `ReachabilityResolver` and
/// `CommunityRelayResolver` respectively.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AddressBookEntry {
    Reachability(ReachabilityAnnouncePayload),
    Relay(CommunityRelayAnnouncePayload),
}

/// One address-book row: the entry payload plus the membership-envelope
/// attribution (`actor`/`device`/`at`) and the store's own admission stamp
/// (`stamped_at_ms`, clamped by `upsert` — see its doc comment).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressBookRow {
    pub entry: AddressBookEntry,
    pub actor: OwnerAddr,
    pub device: [u8; 32],
    pub at: Hlc,
    pub stamped_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Replaced,
    IgnoredOlder,
    IgnoredCapped,
}

/// The key a row occupies, derived from its entry kind's identity field.
pub fn key_for_row(row: &AddressBookRow) -> AddressBookKey {
    match &row.entry {
        AddressBookEntry::Reachability(p) => {
            AddressBookKey::Reachability(row.actor, p.iroh_node_id)
        }
        AddressBookEntry::Relay(p) => AddressBookKey::Relay(row.actor, p.relay.relay_device_id),
    }
}

/// TTL applied on read: reachability rows use the address book's own 24h
/// window; relay rows reuse the relay-ad freshness window so relay entries
/// don't drift from the freshness policy `CommunityRelayResolver` used to
/// apply on its own.
pub fn row_ttl_ms(entry: &AddressBookEntry) -> u64 {
    match entry {
        AddressBookEntry::Reachability(_) => ADDRBOOK_REACHABILITY_TTL_MS,
        AddressBookEntry::Relay(_) => COMMUNITY_RELAY_AD_FRESHNESS_MS,
    }
}

/// The smallest possible `AddressBookKey` for a community: `Reachability`
/// sorts before `Relay` (derived `Ord` follows enum declaration order), and
/// an all-zero `OwnerAddr`/node-id is each field's minimum. Seeds a
/// `BTreeMap::range` scan bounded to one community's key prefix instead of
/// walking the whole (possibly multi-community) map — this key is never
/// stored.
fn community_floor(community: SpaceId) -> (SpaceId, AddressBookKey) {
    (
        community,
        AddressBookKey::Reachability(OwnerAddr([0u8; 16]), [0u8; 32]),
    )
}

/// If `actor`'s same-class row count (per `same_actor_class`) in `community`
/// has reached `cap`, remove that set's oldest-stamped row so the caller's
/// insert holds the count at `cap` instead of growing it. Shared by the
/// reachability and relay per-member bounds in [`CommunityAddressBook::upsert`].
fn evict_oldest_if_at_cap(
    g: &mut BTreeMap<(SpaceId, AddressBookKey), AddressBookRow>,
    community: SpaceId,
    floor: &(SpaceId, AddressBookKey),
    cap: usize,
    same_actor_class: impl Fn(&AddressBookKey) -> bool,
) {
    let rows: Vec<(u64, AddressBookKey)> = g
        .range(floor.clone()..)
        .take_while(|((c, _), _)| *c == community)
        .filter(|((_, k), _)| same_actor_class(k))
        .map(|(k, row)| (row.stamped_at_ms, k.1.clone()))
        .collect();
    if rows.len() >= cap {
        let (_, oldest_key) = rows
            .into_iter()
            .min_by_key(|(ts, _)| *ts)
            .expect("non-empty: len >= cap >= 1");
        g.remove(&(community, oldest_key));
    }
}

#[derive(Default)]
pub struct CommunityAddressBook {
    inner: Mutex<BTreeMap<(SpaceId, AddressBookKey), AddressBookRow>>,
}

impl CommunityAddressBook {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// LWW upsert. First sweeps this community's TTL-expired rows (the spec
    /// §1 opportunistic write-side sweep — without it, expired rows occupy
    /// `ADDRBOOK_MAX_ROWS` budget until the next boot). `row.stamped_at_ms`
    /// is then clamped to at most `now_ms + ADDRBOOK_SKEW_TOLERANCE_MS`
    /// (the clamped value is stored back into the row) so a maliciously
    /// future-stamped row can't win every future honest update. An existing
    /// row at the same key wins ties (`stamped_at_ms >= effective` →
    /// `IgnoredOlder`); otherwise the new row replaces it (`Replaced`).
    ///
    /// A brand-new key is subject to the community-wide hard cap
    /// (`ADDRBOOK_MAX_ROWS` → `IgnoredCapped`). A new key additionally
    /// evicts the actor's own oldest-stamped row of the same class once the
    /// actor already holds that class's per-member cap in this community
    /// (`ADDRBOOK_MAX_NODES_PER_MEMBER` reachability rows /
    /// `ADDRBOOK_MAX_RELAYS_PER_MEMBER` relay rows), then inserts
    /// (`Inserted`) — so a hostile member can only ever churn their own
    /// bounded row set, never grow the community's.
    pub fn upsert(
        &self,
        community: SpaceId,
        mut row: AddressBookRow,
        now_ms: u64,
    ) -> UpsertOutcome {
        let effective = row
            .stamped_at_ms
            .min(now_ms.saturating_add(ADDRBOOK_SKEW_TOLERANCE_MS));
        row.stamped_at_ms = effective;
        let key = key_for_row(&row);
        let map_key = (community, key.clone());

        let mut g = self.inner.lock().unwrap();

        // All scans below (sweep, cap count, per-member eviction) are bounded
        // to this community's key prefix via `range(floor..)` + `take_while`
        // — the (SpaceId, AddressBookKey) ordering keeps one community's rows
        // contiguous, so each is O(this community's rows), not O(rows across
        // every community).
        let floor = community_floor(community);
        let expired: Vec<AddressBookKey> = g
            .range(floor.clone()..)
            .take_while(|((c, _), _)| *c == community)
            .filter(|(_, row)| now_ms.saturating_sub(row.stamped_at_ms) > row_ttl_ms(&row.entry))
            .map(|((_, k), _)| k.clone())
            .collect();
        for k in expired {
            g.remove(&(community, k));
        }

        if let Some(existing) = g.get(&map_key) {
            if existing.stamped_at_ms >= effective {
                return UpsertOutcome::IgnoredOlder;
            }
            g.insert(map_key, row);
            return UpsertOutcome::Replaced;
        }

        let community_row_count = g
            .range(floor.clone()..)
            .take_while(|((c, _), _)| *c == community)
            .count();
        if community_row_count >= ADDRBOOK_MAX_ROWS {
            return UpsertOutcome::IgnoredCapped;
        }

        match &key {
            AddressBookKey::Reachability(actor, _) => evict_oldest_if_at_cap(
                &mut g,
                community,
                &floor,
                ADDRBOOK_MAX_NODES_PER_MEMBER,
                |k| matches!(k, AddressBookKey::Reachability(a, _) if a == actor),
            ),
            AddressBookKey::Relay(actor, _) => evict_oldest_if_at_cap(
                &mut g,
                community,
                &floor,
                ADDRBOOK_MAX_RELAYS_PER_MEMBER,
                |k| matches!(k, AddressBookKey::Relay(a, _) if a == actor),
            ),
        }

        g.insert(map_key, row);
        UpsertOutcome::Inserted
    }

    /// Fresh rows for a community (TTL-filtered per [`row_ttl_ms`]). Range-
    /// scoped to the community's key prefix like `upsert`'s scans — this runs
    /// on the snapshot-serve and sidecar-persist paths, so it must not walk
    /// other communities' rows under the lock.
    pub fn rows_for_community(&self, community: &SpaceId, now_ms: u64) -> Vec<AddressBookRow> {
        let g = self.inner.lock().unwrap();
        g.range(community_floor(*community)..)
            .take_while(|((c, _), _)| c == community)
            .filter(|(_, row)| now_ms.saturating_sub(row.stamped_at_ms) <= row_ttl_ms(&row.entry))
            .map(|(_, row)| row.clone())
            .collect()
    }

    /// Drop all rows (both entry kinds) for an owner in a community (opt-out
    /// / leave / device retire). Returns the count removed.
    pub fn remove_owner(&self, community: &SpaceId, owner: &OwnerAddr) -> usize {
        let mut g = self.inner.lock().unwrap();
        let before = g.len();
        g.retain(|(c, k), _| {
            if c != community {
                return true;
            }
            match k {
                AddressBookKey::Reachability(a, _) | AddressBookKey::Relay(a, _) => a != owner,
            }
        });
        before - g.len()
    }

    /// Drop all rows for a community (community-fork / teardown). Returns the
    /// count removed.
    pub fn remove_community(&self, community: &SpaceId) -> usize {
        let mut g = self.inner.lock().unwrap();
        let before = g.len();
        g.retain(|(c, _), _| c != community);
        before - g.len()
    }

    /// Drop expired rows across every community. The routine write-side
    /// sweep lives inside [`Self::upsert`] (community-scoped); this whole-map
    /// variant serves explicit maintenance and tests. Returns the count
    /// removed.
    pub fn sweep_expired(&self, now_ms: u64) -> usize {
        let mut g = self.inner.lock().unwrap();
        let before = g.len();
        g.retain(|_, row| now_ms.saturating_sub(row.stamped_at_ms) <= row_ttl_ms(&row.entry));
        before - g.len()
    }
}

/// Sidecar file path for a community's address book:
/// `identity_dir/communities/{community_id_hex}/addrbook.cbor` — the same
/// `communities/{hex}/` directory `CommunitySyncRegistry::paths_for`
/// (`community_state_sync.rs`) derives for `crdt.cbor`/`replay.cbor`, so
/// the address book sidecar lives alongside them.
pub fn addrbook_path(identity_dir: &Path, community: &SpaceId) -> PathBuf {
    identity_dir
        .join("communities")
        .join(hex::encode(community.0))
        .join("addrbook.cbor")
}

/// Persist `rows` to `path` via temp-file + rename — same durability
/// posture as `community_state_persist::save_crdt` (no fsync: the address
/// book is fully peer-recoverable via republish, so the extra durability
/// cost doesn't pencil out at this granularity).
///
/// Like `save_crdt`, the fixed `.tmp` name is only race-free with a single
/// writer per `path`; a future caller invoking this concurrently for the
/// same community would need `tempfile::NamedTempFile::new_in` instead.
pub fn save_addrbook(path: &Path, rows: &[AddressBookRow]) -> Result<(), String> {
    let mut bytes = Vec::new();
    into_writer(rows, &mut bytes).map_err(|e| format!("encode addrbook: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create_dir_all: {e}"))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))?;
    Ok(())
}

/// Load rows from `path`, TTL-filtering per [`row_ttl_ms`]. A missing or
/// corrupt file returns an empty vec — loss-safe per spec §2: the address
/// book fully reconstructs from peer republish, so a hard error here would
/// needlessly block engine spawn.
pub fn load_addrbook(path: &Path, now_ms: u64) -> Vec<AddressBookRow> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    let rows: Vec<AddressBookRow> = match from_reader(bytes.as_slice()) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.into_iter()
        .filter(|row| now_ms.saturating_sub(row.stamped_at_ms) <= row_ttl_ms(&row.entry))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_relay_announce::CommunityRelayEntry;

    fn hlc(ms: u64) -> Hlc {
        Hlc {
            wall_ms: ms,
            logical: 0,
            device_id: "d".into(),
        }
    }

    fn reach_payload(seed: u8, ts: u64) -> ReachabilityAnnouncePayload {
        ReachabilityAnnouncePayload {
            iroh_node_id: [seed; 32],
            home_relay_url: "https://derp.example/".into(),
            direct_addresses: vec![],
            announced_at_ms: ts,
            identity_signature: [0; 64],
            butler_set: Vec::new(),
            bs_at: 0,
        }
    }

    fn relay_payload(seed: u8, ad_at: u64) -> CommunityRelayAnnouncePayload {
        CommunityRelayAnnouncePayload {
            relay: CommunityRelayEntry {
                relay_device_id: [seed; 16],
                iroh_endpoint_id: [seed; 32],
                relay_device_ed25519_verify: [seed; 32],
                home_relay: "https://r/".into(),
            },
            ad_at,
            identity_signature: [0; 64],
        }
    }

    fn reach_row(node_seed: u8, actor: OwnerAddr, ts: u64) -> AddressBookRow {
        AddressBookRow {
            entry: AddressBookEntry::Reachability(reach_payload(node_seed, ts)),
            actor,
            device: [node_seed; 32],
            at: hlc(ts),
            stamped_at_ms: ts,
        }
    }

    fn relay_row(device_seed: u8, actor: OwnerAddr, ts: u64) -> AddressBookRow {
        AddressBookRow {
            entry: AddressBookEntry::Relay(relay_payload(device_seed, ts)),
            actor,
            device: [device_seed; 32],
            at: hlc(ts),
            stamped_at_ms: ts,
        }
    }

    #[test]
    fn upsert_then_read_returns_row() {
        let b = CommunityAddressBook::new();
        let c = SpaceId([0xCC; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;

        assert_eq!(
            b.upsert(c, reach_row(0x01, actor, now), now),
            UpsertOutcome::Inserted
        );
        let rows = b.rows_for_community(&c, now);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].actor, actor);
        assert_eq!(rows[0].stamped_at_ms, now);
    }

    #[test]
    fn newer_stamp_replaces_older_ignored() {
        let b = CommunityAddressBook::new();
        let c = SpaceId([0xCC; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;

        assert_eq!(
            b.upsert(c, reach_row(0x01, actor, now), now),
            UpsertOutcome::Inserted
        );
        assert_eq!(
            b.upsert(c, reach_row(0x01, actor, now + 1), now),
            UpsertOutcome::Replaced,
            "newer stamp at the same key replaces"
        );
        assert_eq!(
            b.upsert(c, reach_row(0x01, actor, now - 1), now),
            UpsertOutcome::IgnoredOlder,
            "older stamp at the same key is ignored"
        );

        let rows = b.rows_for_community(&c, now + 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stamped_at_ms, now + 1);
    }

    #[test]
    fn future_stamp_clamped_to_skew_tolerance() {
        let b = CommunityAddressBook::new();
        let c = SpaceId([0xCC; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;
        let ten_min = 10 * 60_000;
        let six_min = 6 * 60_000;
        let one_min = 60_000;

        assert_eq!(
            b.upsert(c, reach_row(0x01, actor, now + ten_min), now),
            UpsertOutcome::Inserted
        );
        let rows = b.rows_for_community(&c, now);
        assert_eq!(
            rows[0].stamped_at_ms,
            now + ADDRBOOK_SKEW_TOLERANCE_MS,
            "a now+10min stamp is clamped down to now+5min (the skew tolerance)"
        );

        // A later call (now advances by 1 min) with a now+6min stamp is no
        // longer clamped — the tolerance window has caught up to it — so it
        // is strictly newer than the stored now+5min and replaces.
        let now2 = now + one_min;
        assert_eq!(
            b.upsert(c, reach_row(0x01, actor, now + six_min), now2),
            UpsertOutcome::Replaced
        );
        let rows2 = b.rows_for_community(&c, now2);
        assert_eq!(rows2[0].stamped_at_ms, now + six_min);
    }

    #[test]
    fn ttl_filters_on_read_reachability_24h_relay_15m() {
        let b = CommunityAddressBook::new();
        let c = SpaceId([0xCC; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;

        b.upsert(c, reach_row(0x01, actor, now), now);
        b.upsert(c, relay_row(0x02, actor, now), now);
        assert_eq!(b.rows_for_community(&c, now).len(), 2);

        // Past the relay freshness window: relay row drops, reachability stays.
        let after_relay_ttl = now + COMMUNITY_RELAY_AD_FRESHNESS_MS + 1;
        let rows = b.rows_for_community(&c, after_relay_ttl);
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0].entry, AddressBookEntry::Reachability(_)));

        // Past the reachability TTL: nothing left.
        let after_reach_ttl = now + ADDRBOOK_REACHABILITY_TTL_MS + 1;
        assert!(b.rows_for_community(&c, after_reach_ttl).is_empty());
    }

    #[test]
    fn per_member_node_cap_evicts_oldest() {
        let b = CommunityAddressBook::new();
        let c = SpaceId([0xCC; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;

        for i in 0..9u8 {
            let ts = now + i as u64;
            assert_eq!(
                b.upsert(c, reach_row(i, actor, ts), ts),
                UpsertOutcome::Inserted,
                "row {i} is a distinct key (distinct node id), always Inserted"
            );
        }

        let rows = b.rows_for_community(&c, now + 8);
        assert_eq!(rows.len(), 8, "cap holds at 8 despite 9 inserts");
        let has_oldest = rows.iter().any(|r| {
            matches!(&r.entry, AddressBookEntry::Reachability(p) if p.iroh_node_id == [0u8; 32])
        });
        assert!(
            !has_oldest,
            "the actor's oldest-stamped reachability row (seed 0) must be evicted"
        );
    }

    #[test]
    fn per_member_relay_cap_evicts_oldest() {
        let b = CommunityAddressBook::new();
        let c = SpaceId([0xCC; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let other = OwnerAddr([0x02; 16]);
        let now = 1_700_000_000_000;

        // Another member's single relay row must survive the actor's churn.
        assert_eq!(
            b.upsert(c, relay_row(0xAA, other, now), now),
            UpsertOutcome::Inserted
        );

        // Relay device ids are self-asserted, so a hostile member can mint
        // arbitrarily many distinct keys — the per-member cap must hold the
        // actor at ADDRBOOK_MAX_RELAYS_PER_MEMBER by evicting their own
        // oldest-stamped relay row (final-review I1).
        for i in 1..=9u8 {
            let ts = now + i as u64;
            assert_eq!(
                b.upsert(c, relay_row(i, actor, ts), ts),
                UpsertOutcome::Inserted,
                "row {i} is a distinct key (distinct relay device id), always Inserted"
            );
        }

        let rows = b.rows_for_community(&c, now + 9);
        let actor_relay: Vec<[u8; 16]> = rows
            .iter()
            .filter(|r| r.actor == actor)
            .filter_map(|r| match &r.entry {
                AddressBookEntry::Relay(p) => Some(p.relay.relay_device_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            actor_relay.len(),
            ADDRBOOK_MAX_RELAYS_PER_MEMBER,
            "cap holds at {ADDRBOOK_MAX_RELAYS_PER_MEMBER} despite 9 inserts"
        );
        assert!(
            !actor_relay.contains(&[1u8; 16]),
            "the actor's oldest-stamped relay row (seed 1) must be evicted"
        );
        assert!(
            rows.iter().any(|r| r.actor == other),
            "another member's relay row must be untouched by the actor's eviction"
        );
    }

    #[test]
    fn upsert_sweeps_expired_rows_scoped_to_community() {
        let b = CommunityAddressBook::new();
        let ca = SpaceId([0xAA; 16]);
        let cb = SpaceId([0xBB; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;
        // Relay TTL is COMMUNITY_RELAY_AD_FRESHNESS_MS (15 min); stamp 20 min
        // back so the rows are expired at `now` but valid when inserted.
        let old = now - 1_200_000;

        assert_eq!(
            b.upsert(ca, relay_row(0x0A, actor, old), old),
            UpsertOutcome::Inserted
        );
        assert_eq!(
            b.upsert(cb, relay_row(0x0B, actor, old), old),
            UpsertOutcome::Inserted
        );

        // The write-side sweep (spec §1) must reclaim community A's expired
        // row during this upsert — and must NOT reach into community B.
        assert_eq!(
            b.upsert(ca, reach_row(0x0C, actor, now), now),
            UpsertOutcome::Inserted
        );

        let a_rows = b.rows_for_community(&ca, now);
        assert_eq!(a_rows.len(), 1, "A holds only the fresh row");
        assert_eq!(
            b.sweep_expired(now),
            1,
            "only community B's expired row remains for the whole-map sweep — \
             A's was already reclaimed on write"
        );
    }

    #[test]
    fn hard_cap_ignores_new_keys() {
        let b = CommunityAddressBook::new();
        let c = SpaceId([0xCC; 16]);
        let now = 1_700_000_000_000;

        // Distinct-owner relay rows: each is its own key (actor differs),
        // and one row per actor never reaches the per-member relay cap, so
        // this fills the community-wide hard cap without tripping eviction.
        let owner_for = |i: u64| {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&i.to_be_bytes());
            OwnerAddr(bytes)
        };

        for i in 0..ADDRBOOK_MAX_ROWS as u64 {
            let actor = owner_for(i);
            assert_eq!(
                b.upsert(c, relay_row((i % 256) as u8, actor, now), now),
                UpsertOutcome::Inserted,
                "row {i} should insert under the hard cap"
            );
        }

        let extra_actor = owner_for(ADDRBOOK_MAX_ROWS as u64);
        assert_eq!(
            b.upsert(c, relay_row(0xEE, extra_actor, now), now),
            UpsertOutcome::IgnoredCapped,
            "the 4097th distinct key must be rejected by the hard cap"
        );
    }

    #[test]
    fn remove_owner_drops_both_kinds_scoped_to_community() {
        let b = CommunityAddressBook::new();
        let c1 = SpaceId([0xCC; 16]);
        let c2 = SpaceId([0xDD; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let other = OwnerAddr([0x02; 16]);
        let now = 1_700_000_000_000;

        b.upsert(c1, reach_row(0x01, actor, now), now);
        b.upsert(c1, relay_row(0x02, actor, now), now);
        b.upsert(c1, reach_row(0x03, other, now), now);
        b.upsert(c2, reach_row(0x04, actor, now), now);

        let removed = b.remove_owner(&c1, &actor);
        assert_eq!(
            removed, 2,
            "both the reachability and relay row for the actor in c1 are removed"
        );

        let rows_c1 = b.rows_for_community(&c1, now);
        assert_eq!(rows_c1.len(), 1);
        assert_eq!(rows_c1[0].actor, other);

        let rows_c2 = b.rows_for_community(&c2, now);
        assert_eq!(
            rows_c2.len(),
            1,
            "c2's row for the same actor is untouched — scoped to c1"
        );
    }

    #[test]
    fn remove_community_drops_all_and_sweep_expired_counts() {
        let b = CommunityAddressBook::new();
        let c1 = SpaceId([0xCC; 16]);
        let c2 = SpaceId([0xDD; 16]);
        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;

        b.upsert(c1, reach_row(0x01, actor, now), now);
        b.upsert(c1, relay_row(0x02, actor, now), now);
        b.upsert(c2, reach_row(0x03, actor, now), now);

        assert_eq!(b.remove_community(&c1), 2);
        assert!(b.rows_for_community(&c1, now).is_empty());
        assert_eq!(
            b.rows_for_community(&c2, now).len(),
            1,
            "c2 untouched by remove_community(c1)"
        );

        // Add a relay row in c2 that will go stale, leaving the reachability
        // row (24h TTL) fresh — sweep_expired must drop only the former.
        b.upsert(c2, relay_row(0x04, actor, now), now);
        let far_future = now + COMMUNITY_RELAY_AD_FRESHNESS_MS + 1;
        assert_eq!(
            b.sweep_expired(far_future),
            1,
            "only the stale relay row is swept"
        );
        assert_eq!(b.rows_for_community(&c2, far_future).len(), 1);
    }

    #[test]
    fn sidecar_round_trip_preserves_rows() {
        let dir = tempfile::tempdir().unwrap();
        let community = SpaceId([0xAB; 16]);
        let path = addrbook_path(dir.path(), &community);
        assert_eq!(
            path,
            dir.path()
                .join("communities")
                .join(hex::encode(community.0))
                .join("addrbook.cbor"),
            "mirrors community_state_sync::paths_for's communities/{{hex}}/ layout"
        );

        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;
        let rows = vec![reach_row(0x01, actor, now), relay_row(0x02, actor, now)];

        save_addrbook(&path, &rows).unwrap();
        assert!(path.exists());

        let loaded = load_addrbook(&path, now);
        assert_eq!(loaded.len(), 2);
        assert!(loaded
            .iter()
            .all(|r| r.actor == actor && r.stamped_at_ms == now));
        assert!(loaded.iter().any(
            |r| matches!(&r.entry, AddressBookEntry::Reachability(p) if p.iroh_node_id == [0x01; 32])
        ));
        assert!(loaded.iter().any(
            |r| matches!(&r.entry, AddressBookEntry::Relay(p) if p.relay.relay_device_id == [0x02; 16])
        ));
    }

    #[test]
    fn load_filters_expired_rows() {
        let dir = tempfile::tempdir().unwrap();
        let community = SpaceId([0xCD; 16]);
        let path = addrbook_path(dir.path(), &community);
        let actor = OwnerAddr([0x01; 16]);
        let now = 1_700_000_000_000;

        let rows = vec![reach_row(0x01, actor, now), relay_row(0x02, actor, now)];
        save_addrbook(&path, &rows).unwrap();

        assert_eq!(
            load_addrbook(&path, now).len(),
            2,
            "both rows are fresh immediately after write"
        );

        // Past the relay TTL (15 min) but before the reachability TTL
        // (24h): relay row filtered, reachability row survives.
        let after_relay_ttl = now + COMMUNITY_RELAY_AD_FRESHNESS_MS + 1;
        let loaded = load_addrbook(&path, after_relay_ttl);
        assert_eq!(loaded.len(), 1);
        assert!(matches!(loaded[0].entry, AddressBookEntry::Reachability(_)));

        // Past the reachability TTL too: nothing survives.
        let after_reach_ttl = now + ADDRBOOK_REACHABILITY_TTL_MS + 1;
        assert!(load_addrbook(&path, after_reach_ttl).is_empty());
    }

    #[test]
    fn load_missing_or_corrupt_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let community = SpaceId([0xEF; 16]);
        let path = addrbook_path(dir.path(), &community);

        assert!(
            load_addrbook(&path, 1_700_000_000_000).is_empty(),
            "nonexistent path returns empty, not an error"
        );

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not cbor garbage").unwrap();
        assert!(
            load_addrbook(&path, 1_700_000_000_000).is_empty(),
            "corrupt bytes return empty, not an error"
        );
    }
}
