//! ZEB-669 slice 2: the auto-pin engine's PURE reconciliation planner.
//!
//! Every policy decision — pact activation, list-order priority, pledge
//! slices, the serialized shared-budget gate, physical-CID dedup,
//! releases — lives here as a pure function over snapshots, so the
//! whole decision surface is unit-testable without zenoh, tokio, or a
//! cache. The event-loop tick supplies snapshots + in-flight state and
//! mechanically applies the returned plan; `get_contribution_summary`
//! runs the same function in dry-run (empty in-flight) for the
//! "Catching up" health verdict — one rule, two consumers.
//!
//! Budget semantics (spec §3, PR #448 review):
//! - A pact is active when BOTH sides pledge (0 bytes is a valid accept).
//! - Per pact, the buddy's BackupSet is walked IN LIST ORDER; the walk
//!   stops when the next entry would exceed `min(my pledge to them,
//!   remaining shared budget)` — a truncated pact reads "Catching up".
//! - Claimed sizes are admission HINTS: candidates reserve claimed bytes
//!   (`inflight`), and the caller reconciles to actuals on completion.
//! - A CID already pinned via another pact is attributed without a fetch
//!   and without global-budget cost (bytes are stored once).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::storage_ledger::StorageLedger;
use crate::storage_records::StorageRecordStore;

/// A reservation the tick holds while a fetch task runs: which pact the
/// fetch is for and the claimed bytes reserved against the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightFetch {
    pub buddy: String,
    pub claimed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchCandidate {
    pub buddy: String,
    /// 64-hex ContentId (ingest-validated public durable).
    pub cid: String,
    /// The record's claimed size — the reservation amount.
    pub claimed: u64,
}

/// What the tick should do this round. Applied mechanically, in order:
/// releases/evictions first (they only free), then attributions, then
/// fetch spawns (bounded by the caller's in-flight cap).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PinPlan {
    pub fetch: Vec<FetchCandidate>,
    /// (buddy, cid, actual_size): already pinned via another pact —
    /// record the attribution, no fetch, no global-budget cost.
    pub attribute_only: Vec<(String, String, u64)>,
    /// (buddy, cid): the buddy's current BackupSet no longer lists it.
    pub release: Vec<(String, String)>,
    /// Pact no longer active — release every attribution.
    pub release_buddies: Vec<String>,
    /// Budget shrunk below the distinct-bytes numerator — evict
    /// newest-first down to this target.
    pub evict_to: Option<u64>,
    /// True when desired work is deferred (fetches pending/blocked, a
    /// pact truncated by pledge or budget) — the "Catching up" signal.
    pub catching_up: bool,
}

impl PinPlan {
    /// Anything left to do? (The health rule: work pending ⇒ not yet
    /// "Healthy".)
    pub fn has_work(&self) -> bool {
        !self.fetch.is_empty()
            || !self.attribute_only.is_empty()
            || !self.release.is_empty()
            || !self.release_buddies.is_empty()
            || self.evict_to.is_some()
    }
}

/// Compute the reconciliation plan. `me` is the local owner address;
/// `backoff_blocked` reports cids whose retry is not yet due.
pub fn plan(
    me: &str,
    my_pledges: &BTreeMap<String, u64>,
    records: &StorageRecordStore,
    ledger: &StorageLedger,
    shared_budget: u64,
    inflight: &HashMap<String, InflightFetch>,
    backoff_blocked: &dyn Fn(&str) -> bool,
) -> PinPlan {
    let mut out = PinPlan::default();

    let pledgers: HashSet<String> = records
        .owners_pledging_to(me)
        .into_iter()
        .map(|(owner, _)| owner)
        .collect();
    let active: Vec<&String> = my_pledges
        .keys()
        .filter(|owner| pledgers.contains(*owner))
        .collect(); // BTreeMap keys ⇒ sorted ⇒ deterministic

    let distinct = ledger.distinct_pinned_bytes();
    let mut reserved: u64 = inflight.values().map(|f| f.claimed).sum();
    if distinct > shared_budget {
        out.evict_to = Some(shared_budget);
    }
    // Once the shared budget can't fit the next fetch, every later fetch
    // candidate (any pact) is suppressed too — attribution-only entries
    // still pass (they cost nothing).
    let mut global_full = false;

    for buddy in &active {
        let slice = my_pledges[*buddy];
        let mut attributed = ledger.bytes_for_buddy(buddy)
            + inflight
                .values()
                .filter(|f| f.buddy == **buddy)
                .map(|f| f.claimed)
                .sum::<u64>();
        let Some(set) = records.backup_set(buddy) else {
            continue;
        };
        for entry in &set.entries {
            if ledger.holds(buddy, &entry.cid) {
                continue;
            }
            if inflight.contains_key(&entry.cid) {
                // A fetch for another pact is running; attribution for
                // this pact happens next tick via held_anywhere.
                out.catching_up = true;
                continue;
            }
            if let Some(actual) = ledger.held_anywhere(&entry.cid) {
                if attributed.saturating_add(actual) > slice {
                    out.catching_up = true;
                    break; // list order: nothing later fits either
                }
                out.attribute_only
                    .push(((*buddy).clone(), entry.cid.clone(), actual));
                attributed += actual;
                continue;
            }
            if attributed.saturating_add(entry.size) > slice {
                out.catching_up = true;
                break;
            }
            if global_full
                || distinct.saturating_add(reserved).saturating_add(entry.size) > shared_budget
            {
                global_full = true;
                out.catching_up = true;
                break;
            }
            if backoff_blocked(&entry.cid) {
                out.catching_up = true;
                continue;
            }
            reserved += entry.size;
            attributed += entry.size;
            out.fetch.push(FetchCandidate {
                buddy: (*buddy).clone(),
                cid: entry.cid.clone(),
                claimed: entry.size,
            });
        }
    }

    // Releases: attributions whose pact or backup-set entry is gone.
    let active_set: HashSet<&str> = active.iter().map(|s| s.as_str()).collect();
    for buddy in ledger.buddies() {
        if !active_set.contains(buddy.as_str()) {
            out.release_buddies.push(buddy);
            continue;
        }
        let desired: HashSet<&str> = records
            .backup_set(&buddy)
            .map(|set| set.entries.iter().map(|e| e.cid.as_str()).collect())
            .unwrap_or_default();
        for cid in ledger.cids_for_buddy(&buddy) {
            if !desired.contains(cid.as_str()) {
                out.release.push((buddy.clone(), cid));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_signing::{
        self, BackupEntry, BackupSetPayload, PledgeEntry, PledgeListPayload,
    };
    use harmony_content::cid::{ContentFlags, ContentId};

    const ME: &str = "my-address";
    const NO_BACKOFF: &dyn Fn(&str) -> bool = &|_| false;

    fn cid_hex(data: &[u8]) -> String {
        hex::encode(
            ContentId::for_book(data, ContentFlags::default())
                .unwrap()
                .to_bytes(),
        )
    }

    /// A record store seeded with signed records from `buddies`, each
    /// pledging `pledge_to_me` to ME and publishing the given backup
    /// set entries.
    fn seeded_records(
        buddies: &[(&str, u64, Vec<BackupEntry>)],
    ) -> (StorageRecordStore, Vec<String>) {
        let mut store = StorageRecordStore::new(None, None);
        let mut owners = Vec::new();
        for (name, pledge_to_me, entries) in buddies {
            let id = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);
            let owner = hex::encode(id.public_identity().address_hash);
            let mut pl = PledgeListPayload {
                owner_address: owner.clone(),
                pledges: vec![PledgeEntry {
                    to: ME.into(),
                    bytes: *pledge_to_me,
                }],
                updated_at: 1,
                identity_pub: None,
                sig: None,
                v2: Default::default(),
            };
            storage_signing::sign_pledge_list(&id, &mut pl);
            let topic = format!("harmony/storage/{owner}/pledges");
            assert!(store
                .on_pledge_list_sample(
                    &topic,
                    &serde_json::to_vec(&pl).unwrap(),
                    &crate::revoked_device_projection::RevokedDeviceProjection::new(),
                    9_000,
                )
                .changed());
            let mut bs = BackupSetPayload {
                owner_address: owner.clone(),
                entries: entries.clone(),
                updated_at: 1,
                identity_pub: None,
                sig: None,
                v2: Default::default(),
            };
            storage_signing::sign_backup_set(&id, &mut bs);
            let topic = format!("harmony/storage/{owner}/backup-set");
            assert!(store
                .on_backup_set_sample(
                    &topic,
                    &serde_json::to_vec(&bs).unwrap(),
                    &crate::revoked_device_projection::RevokedDeviceProjection::new(),
                    9_000,
                )
                .changed());
            let _ = name; // labels are for the test author; owners map by index
            owners.push(owner);
        }
        (store, owners)
    }

    fn entry(data: &[u8], size: u64) -> BackupEntry {
        BackupEntry {
            cid: cid_hex(data),
            size,
        }
    }

    #[test]
    fn fetches_in_list_order_within_pledge_slice() {
        let (records, owners) = seeded_records(&[(
            "alice",
            0,
            vec![entry(b"a", 40), entry(b"b", 40), entry(b"c", 40)],
        )]);
        let my_pledges: BTreeMap<String, u64> = [(owners[0].clone(), 100)].into();
        let ledger = StorageLedger::new(None, None);

        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert_eq!(
            plan.fetch
                .iter()
                .map(|f| f.cid.as_str())
                .collect::<Vec<_>>(),
            vec![cid_hex(b"a"), cid_hex(b"b")],
            "third entry exceeds the 100-byte slice"
        );
        assert!(plan.catching_up, "truncated pact reads catching-up");
    }

    #[test]
    fn zero_pledge_pact_is_active_but_fetches_nothing() {
        let (records, owners) = seeded_records(&[("alice", 50, vec![entry(b"a", 10)])]);
        let my_pledges: BTreeMap<String, u64> = [(owners[0].clone(), 0)].into();
        let ledger = StorageLedger::new(None, None);

        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert!(plan.fetch.is_empty(), "0-byte slice admits nothing");
        assert!(plan.release_buddies.is_empty(), "pact IS active");
    }

    #[test]
    fn one_sided_pledge_is_not_a_pact() {
        // They pledge to me but I never pledged back ⇒ no fetching.
        let (records, owners) = seeded_records(&[("alice", 50, vec![entry(b"a", 10)])]);
        let my_pledges: BTreeMap<String, u64> = BTreeMap::new();
        let ledger = StorageLedger::new(None, None);

        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert_eq!(plan, PinPlan::default(), "no pact, no work");
        let _ = owners;
    }

    #[test]
    fn global_budget_stop_spans_buddies_and_counts_reservations() {
        let (records, owners) = seeded_records(&[
            ("alice", 0, vec![entry(b"a", 60)]),
            ("bob", 0, vec![entry(b"b", 60)]),
        ]);
        let my_pledges: BTreeMap<String, u64> =
            [(owners[0].clone(), 1_000), (owners[1].clone(), 1_000)].into();
        let ledger = StorageLedger::new(None, None);
        // 50 bytes already reserved by an in-flight fetch ⇒ budget 100
        // fits neither 60-byte entry.
        let inflight: HashMap<String, InflightFetch> = [(
            cid_hex(b"other"),
            InflightFetch {
                buddy: owners[0].clone(),
                claimed: 50,
            },
        )]
        .into();

        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            100,
            &inflight,
            NO_BACKOFF,
        );
        assert!(
            plan.fetch.is_empty(),
            "reservations count against the budget"
        );
        assert!(plan.catching_up);
    }

    #[test]
    fn shared_cid_across_pacts_attributes_without_refetch_or_double_budget() {
        let shared = entry(b"shared", 70);
        let (records, owners) = seeded_records(&[
            ("alice", 0, vec![shared.clone()]),
            ("bob", 0, vec![shared.clone()]),
        ]);
        let my_pledges: BTreeMap<String, u64> =
            [(owners[0].clone(), 100), (owners[1].clone(), 100)].into();
        let mut ledger = StorageLedger::new(None, None);
        // Alice's copy already fetched+pinned (actual 70).
        ledger.record_pin(&owners[0], &shared.cid, 70, 1);

        // Budget exactly at the distinct bytes: no headroom for a
        // refetch — which is fine, none is needed.
        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            70,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert!(plan.fetch.is_empty(), "stored once, never refetched");
        assert_eq!(
            plan.attribute_only,
            vec![(owners[1].clone(), shared.cid.clone(), 70)],
            "bob's pact gets attribution at zero global cost"
        );
    }

    #[test]
    fn inactive_pact_and_dropped_entries_release() {
        let kept = entry(b"kept", 10);
        let (records, owners) =
            seeded_records(&[("alice", 0, vec![kept.clone()]), ("bob", 0, vec![])]);
        // I pledge only to alice ⇒ bob's pact (with ledger residue) is
        // inactive; alice dropped "gone" from her set.
        let my_pledges: BTreeMap<String, u64> = [(owners[0].clone(), 100)].into();
        let mut ledger = StorageLedger::new(None, None);
        ledger.record_pin(&owners[0], &kept.cid, 10, 1);
        ledger.record_pin(&owners[0], &cid_hex(b"gone"), 5, 2);
        ledger.record_pin(&owners[1], &cid_hex(b"bobs"), 7, 3);

        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert_eq!(plan.release, vec![(owners[0].clone(), cid_hex(b"gone"))]);
        assert_eq!(plan.release_buddies, vec![owners[1].clone()]);
        assert!(plan.fetch.is_empty());
    }

    #[test]
    fn ttl_decayed_records_release_pins_via_the_existing_plan() {
        use crate::storage_records::STORAGE_RECORD_TTL_MS;
        // ZEB-923: a permanently-dark buddy's records decay (fixture
        // ingests at now_ms = 9_000) and the UNCHANGED planner releases
        // everything — no new pin machinery.
        let kept = entry(b"kept", 10);
        let (mut records, owners) = seeded_records(&[("alice", 50, vec![kept.clone()])]);
        let my_pledges: BTreeMap<String, u64> = [(owners[0].clone(), 100)].into();
        let mut ledger = StorageLedger::new(None, None);
        ledger.record_pin(&owners[0], &kept.cid, 10, 1);

        assert!(records.sweep_stale_pledges_and_backups(9_000 + STORAGE_RECORD_TTL_MS));
        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert_eq!(
            plan.release_buddies,
            vec![owners[0].clone()],
            "decayed pledge list ⇒ pact inactive ⇒ release everything"
        );
        assert!(plan.fetch.is_empty(), "no fetching from a decayed pact");
    }

    #[test]
    fn ttl_backup_only_decay_releases_every_pinned_cid() {
        use crate::storage_records::{StorageRecordStore, STORAGE_RECORD_TTL_MS};
        // ZEB-923: pledge renews but the backup set decays ⇒ the pact
        // stays active and `desired` collapses to empty ⇒ every pinned
        // CID for the buddy lands in `release` (the unwrap_or_default
        // path), not `release_buddies`.
        let mut records = StorageRecordStore::new(None, None);
        let id = harmony_identity::PrivateIdentity::generate(&mut rand::rngs::OsRng);
        let owner = hex::encode(id.public_identity().address_hash);
        let rvk = crate::revoked_device_projection::RevokedDeviceProjection::new();
        let kept = entry(b"kept", 10);

        let mut pl = PledgeListPayload {
            owner_address: owner.clone(),
            pledges: vec![PledgeEntry {
                to: ME.into(),
                bytes: 50,
            }],
            updated_at: 1,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        storage_signing::sign_pledge_list(&id, &mut pl);
        let topic = format!("harmony/storage/{owner}/pledges");
        assert!(records
            .on_pledge_list_sample(&topic, &serde_json::to_vec(&pl).unwrap(), &rvk, 9_000)
            .changed());
        let mut bs = BackupSetPayload {
            owner_address: owner.clone(),
            entries: vec![kept.clone()],
            updated_at: 1,
            identity_pub: None,
            sig: None,
            v2: Default::default(),
        };
        storage_signing::sign_backup_set(&id, &mut bs);
        let topic = format!("harmony/storage/{owner}/backup-set");
        assert!(records
            .on_backup_set_sample(&topic, &serde_json::to_vec(&bs).unwrap(), &rvk, 9_000)
            .changed());

        // Only the pledge renews (strictly-newer re-mint, fresh receipt).
        pl.updated_at = 2;
        pl.sig = None;
        pl.identity_pub = None;
        storage_signing::sign_pledge_list(&id, &mut pl);
        let topic = format!("harmony/storage/{owner}/pledges");
        assert!(records
            .on_pledge_list_sample(
                &topic,
                &serde_json::to_vec(&pl).unwrap(),
                &rvk,
                9_000 + STORAGE_RECORD_TTL_MS - 1_000,
            )
            .changed());

        assert!(records.sweep_stale_pledges_and_backups(9_000 + STORAGE_RECORD_TTL_MS));
        assert!(records.pledge_list(&owner).is_some(), "renewed pledge kept");
        assert!(records.backup_set(&owner).is_none(), "backup set decayed");

        let my_pledges: BTreeMap<String, u64> = [(owner.clone(), 100)].into();
        let mut ledger = StorageLedger::new(None, None);
        ledger.record_pin(&owner, &kept.cid, 10, 1);
        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert_eq!(plan.release, vec![(owner.clone(), kept.cid.clone())]);
        assert!(plan.release_buddies.is_empty(), "pact itself stays active");
    }

    #[test]
    fn budget_shrink_sets_evict_target() {
        let (records, _) = seeded_records(&[]);
        let my_pledges = BTreeMap::new();
        let mut ledger = StorageLedger::new(None, None);
        ledger.record_pin("anyone", "cid", 500, 1);

        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            200,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert_eq!(plan.evict_to, Some(200));
    }

    #[test]
    fn backoff_blocked_cid_is_skipped_but_later_entries_proceed() {
        let blocked = entry(b"blocked", 10);
        let ok = entry(b"ok", 10);
        let (records, owners) = seeded_records(&[("alice", 0, vec![blocked.clone(), ok.clone()])]);
        let my_pledges: BTreeMap<String, u64> = [(owners[0].clone(), 100)].into();
        let ledger = StorageLedger::new(None, None);
        let blocked_cid = blocked.cid.clone();

        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            &|cid| cid == blocked_cid,
        );
        assert_eq!(
            plan.fetch
                .iter()
                .map(|f| f.cid.as_str())
                .collect::<Vec<_>>(),
            vec![ok.cid.as_str()],
            "backoff skips the cid without stalling the walk"
        );
        assert!(plan.catching_up, "a blocked fetch is pending work");
    }

    #[test]
    fn inflight_cid_is_not_replanned() {
        let e = entry(b"a", 10);
        let (records, owners) = seeded_records(&[("alice", 0, vec![e.clone()])]);
        let my_pledges: BTreeMap<String, u64> = [(owners[0].clone(), 100)].into();
        let ledger = StorageLedger::new(None, None);
        let inflight: HashMap<String, InflightFetch> = [(
            e.cid.clone(),
            InflightFetch {
                buddy: owners[0].clone(),
                claimed: 10,
            },
        )]
        .into();

        let plan = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &inflight,
            NO_BACKOFF,
        );
        assert!(plan.fetch.is_empty(), "already in flight");
        assert!(plan.catching_up);
    }

    #[test]
    fn deterministic_across_calls() {
        let (records, owners) = seeded_records(&[
            ("alice", 0, vec![entry(b"a", 10)]),
            ("bob", 0, vec![entry(b"b", 10)]),
        ]);
        let my_pledges: BTreeMap<String, u64> =
            [(owners[0].clone(), 100), (owners[1].clone(), 100)].into();
        let ledger = StorageLedger::new(None, None);

        let p1 = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            NO_BACKOFF,
        );
        let p2 = plan(
            ME,
            &my_pledges,
            &records,
            &ledger,
            1_000,
            &HashMap::new(),
            NO_BACKOFF,
        );
        assert_eq!(p1, p2);
        assert_eq!(p1.fetch.len(), 2);
    }
}
