//! Butler dm-inbox CRDT (ZEB-418 P1): deposited-but-not-yet-ingested DM
//! deliveries, replicated across the owner's fleet via FleetSyncEngine.
//! NOT a migration of DM history (spec D6).

use crate::fleet_sync::MergeOutcome;
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::Hlc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Key = "{space_id_hex}:{message_cid_hex}" — mirrors InboxKey, string-keyed
/// for canonical CBOR map encoding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInboxEntry {
    #[serde(rename = "so")]
    pub sender_owner: [u8; 16],
    /// Signed CidNotify packet bytes (discriminant+body+sig). ZEB-505: `None`
    /// for a standalone durable DM *invite* deposit (no message) — then
    /// `invite_packet` is the sole payload and `storage_blob` is empty.
    /// Symmetric to `invite_packet`/`iv`; backward-compatible since legacy
    /// deposits always carry `cn` (decoding to `Some`).
    #[serde(
        rename = "cn",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub cidnotify_packet: Option<Vec<u8>>,
    /// The CAS storage blob ([ver][nonce][ct][tag]). Empty for an invite-only
    /// deposit (`cidnotify_packet` is `None`).
    #[serde(rename = "pl", with = "serde_bytes")]
    pub storage_blob: Vec<u8>,
    /// ZEB-483: optional signed DmInvite packet bytes, carried through from the
    /// sealed `DepositPayload` by the butler acceptor. Applied on recover to
    /// bootstrap the DM Space before CidNotify admission. `None` for non-DM /
    /// legacy deposits.
    #[serde(
        rename = "iv",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub invite_packet: Option<Vec<u8>>,
    /// ZEB-691: signed `RevocationPush` frame bytes (a `DmPacket::RevocationPush`),
    /// carried through from the sealed `DepositPayload` by the butler acceptor.
    /// Applied on recover via `handle_revocation_push`. `None` for message /
    /// invite / legacy deposits.
    #[serde(
        rename = "rp",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub revocation_push: Option<Vec<u8>>,
    /// ZEB-674 (C4): opaque `grant_push` wire value (canonical CBOR of
    /// `Vec<serde_bytes Vec<u8>>` — the per-device sealed
    /// [`crate::file_sharing::FileGrantInner`] blobs), carried through from the
    /// sealed `DepositPayload` by the butler acceptor. Applied on recover via
    /// `file_sharing::ingest_grant_push`. `None` for message / invite /
    /// revocation / legacy deposits. Symmetric to the other optional
    /// sub-payloads (`cn`/`iv`/`rp`); backward-compatible since absent → `None`.
    #[serde(
        rename = "gp",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub grant_push: Option<Vec<u8>>,
    /// ZEB-730: opaque `grant_revoke` wire value (canonical CBOR of the revoked
    /// root ContentId — `butler_deposit::encode_grant_revoke`), carried through
    /// from the sealed `DepositPayload` by the butler acceptor. Applied on recover
    /// via `file_sharing::ingest_grant_revoke`. `None` for message / invite /
    /// revocation / grant / legacy deposits. Symmetric to the other optional
    /// sub-payloads (`cn`/`iv`/`rp`/`gp`); backward-compatible since absent →
    /// `None`.
    #[serde(
        rename = "gr",
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_bytes"
    )]
    pub grant_revoke: Option<Vec<u8>>,
    #[serde(rename = "da")]
    pub deposited_at: Hlc,
    /// SP1 device_id (64-hex).
    #[serde(rename = "db")]
    pub deposited_by: String,
    #[serde(rename = "ig", default, skip_serializing_if = "BTreeSet::is_empty")]
    pub ingested_by: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DmInboxDoc {
    #[serde(rename = "en")]
    pub entries: BTreeMap<String, DmInboxEntry>,
    /// ZEB-851: LOCAL per-replica first-observation clock (ms) keyed by
    /// entry key, driving TTL GC instead of the untrusted butler
    /// `deposited_at`. Never serialized (canonical wire bytes unchanged) and
    /// excluded from `PartialEq` below.
    #[serde(skip)]
    first_observed_ms: BTreeMap<String, u64>,
    /// ZEB-925: LOCAL expiry tombstones (ms) keyed by entry key — memory of
    /// keys this replica removed by TTL expiry, so a still-holding sibling's
    /// merge cannot resurrect them and re-arm a fresh TTL window. Never
    /// serialized (canonical wire bytes unchanged) and excluded from
    /// `PartialEq` below, mirroring `first_observed_ms`. Bounded by
    /// `INBOX_TOMBSTONE_RETENTION_MS` age-out + `INBOX_TOMBSTONE_CAP`
    /// oldest-first eviction (`prune_tombstones`).
    #[serde(skip)]
    expired_at_ms: BTreeMap<String, u64>,
}

impl PartialEq for DmInboxDoc {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}
impl Eq for DmInboxDoc {}

// Manual CanonicalPayload registration: the `impl_canonical!` macro in
// owner_state_types.rs is module-private, so we register these types with the
// two impls the macro expands to (mirroring `notes_crdt`).
impl CanonicalPayloadSealed for DmInboxEntry {}
impl CanonicalPayload for DmInboxEntry {}
impl CanonicalPayloadSealed for DmInboxDoc {}
impl CanonicalPayload for DmInboxDoc {}

impl DmInboxDoc {
    /// ZEB-851 GC: drop entries that are either covered (`covered`) or past
    /// this replica's per-replica local TTL, returning whether `entries`
    /// changed. Expiry keys off the LOCAL `first_observed_ms` (lazily stamped
    /// here on the first sweep that sees each entry), never the butler-minted
    /// `deposited_at` — a backdated deposit must not drop a DM as pre-expired.
    ///
    /// ZEB-862: `first_observed_ms` is LOCAL-only but restart-DURABLE (persisted
    /// to and restored from a local sidecar, `dm_inbox_persist::save_first_observed`),
    /// so a restart no longer resets the TTL. A stamp read back GREATER than the
    /// boot `now_ms` (a backward local clock step across restart) is rebased to
    /// `now_ms` in [`Self::restore_first_observed`], so a future stamp cannot
    /// delay expiry.
    ///
    /// Borrows `entries` and `first_observed_ms` as disjoint fields across the
    /// `retain` (no per-sweep clone of the side-map), mirroring `RelayHoldDoc::gc`
    /// in `community_relay_hold_crdt`.
    pub(crate) fn gc_expired(&mut self, now_ms: u64, covered: &BTreeSet<String>) -> bool {
        // Lazy-stamp first observation for any entry not yet seen.
        for key in self.entries.keys().cloned().collect::<Vec<_>>() {
            self.first_observed_ms.entry(key).or_insert(now_ms);
        }
        let before = self.entries.len();
        let first_observed = &self.first_observed_ms;
        // ZEB-925: split the removal reason. A TTL expiry is tombstoned so a
        // sibling's merge cannot resurrect it (see merge_from); a coverage
        // removal is NOT — coverage is a fleet-deterministic function of the
        // grow-only `ingested_by` union, so a resurrected covered entry
        // converges out again without suppression. Covered wins when both
        // apply.
        let mut ttl_removed: Vec<String> = Vec::new();
        self.entries.retain(|key, _e| {
            if covered.contains(key) {
                return false;
            }
            let observed = first_observed.get(key).copied().unwrap_or(now_ms);
            let ttl_expired = observed.saturating_add(crate::butler_deposit::INBOX_TTL_MS) < now_ms;
            if ttl_expired {
                ttl_removed.push(key.clone());
                return false;
            }
            true
        });
        for key in ttl_removed {
            self.expired_at_ms.insert(key, now_ms);
        }
        self.prune_tombstones(now_ms);
        // Prune the side-map for removed keys (bounded with `entries`).
        let live: BTreeSet<String> = self.entries.keys().cloned().collect();
        self.first_observed_ms.retain(|k, _| live.contains(k));
        self.entries.len() != before
    }

    /// ZEB-862: read the LOCAL first-observation clock for durable sidecar
    /// persistence. Never leaves this replica and never enters the wire.
    pub fn first_observed_ms(&self) -> &BTreeMap<String, u64> {
        &self.first_observed_ms
    }

    /// ZEB-862: restore the LOCAL first-observation clock on boot from the
    /// sidecar file, so TTL GC survives restart instead of re-stamping `now`.
    /// `now_ms` is the boot wall clock.
    ///
    /// - Q-2: orphan stamps for keys not in `entries` (e.g. a crash between the
    ///   doc and sidecar writes) are dropped, so the restored clock is
    ///   self-consistent and `persist` never re-writes dead keys.
    /// - Q-1: a stamp GREATER than `now_ms` (a backward local clock step left a
    ///   future stamp in the sidecar) is rebased down to `now_ms`, so it cannot
    ///   delay TTL expiry beyond `now_ms + TTL`. Self-heals on each boot.
    /// - Q-3 (ZEB-998): an entry present in `entries` but MISSING from the
    ///   sidecar — the mirror image of Q-2's torn multi-file write (the doc
    ///   rename landed, the first-observed rename did not; also a quarantined
    ///   or pre-ZEB-862 sidecar) — inherits its own `deposited_at.wall_ms` as
    ///   the observation floor instead of being lazily re-stamped `now` by the
    ///   next sweep. A crash between the sidecar renames must not extend an
    ///   entry's TTL past its original observation floor; the deposit stamp
    ///   travels in the same file as the entry, so it survives any torn write.
    ///   It is butler-minted (untrusted), so it is clamped to `now_ms` like
    ///   Q-1 — a backdated deposit can only shorten a crash-window entry's
    ///   retention (the safe side: sibling outholds redeliver), never extend
    ///   it. Runtime arrivals (fleet merge while running) still lazy-stamp
    ///   `now` in `gc_expired` — that is a genuine local first observation,
    ///   not a lost one.
    ///
    /// Callers MUST load `entries` before calling this (the boot path does).
    pub fn restore_first_observed(&mut self, mut map: BTreeMap<String, u64>, now_ms: u64) {
        map.retain(|k, _| self.entries.contains_key(k));
        for v in map.values_mut() {
            *v = (*v).min(now_ms);
        }
        for (k, e) in &self.entries {
            map.entry(k.clone())
                .or_insert_with(|| e.deposited_at.wall_ms.min(now_ms));
        }
        self.first_observed_ms = map;
    }

    /// ZEB-925: read the LOCAL expiry-tombstone map for durable sidecar
    /// persistence. Never leaves this replica and never enters the wire.
    pub fn expired_at_ms(&self) -> &BTreeMap<String, u64> {
        &self.expired_at_ms
    }

    /// ZEB-925: restore the LOCAL expiry tombstones on boot from the sidecar,
    /// BEFORE [`Self::restore_first_observed`] (whose orphan-prune then drops
    /// the stamps of any entry removed here). Future stamps are clamped to
    /// `now_ms` (mirroring Q-1); aged-out tombstones are pruned BEFORE the
    /// entries sweep (an expired tombstone must neither suppress nor delete);
    /// a surviving tombstone wins over a stale doc file: its entry is removed.
    pub fn restore_expired(&mut self, mut map: BTreeMap<String, u64>, now_ms: u64) {
        for v in map.values_mut() {
            *v = (*v).min(now_ms);
        }
        self.expired_at_ms = map;
        self.prune_tombstones(now_ms);
        let tombstones = &self.expired_at_ms;
        self.entries.retain(|k, _| !tombstones.contains_key(k));
    }

    /// ZEB-925 (spec §2f): forget the expiry tombstone for `key`. Called by
    /// the deposit acceptor when it ACCEPTS a deposit for the key —
    /// acceptance is a fresh local decision to hold, and a live entry must
    /// never coexist with its own tombstone ([`Self::restore_expired`] would
    /// delete the acked entry at the next boot).
    pub fn clear_tombstone(&mut self, key: &str) {
        self.expired_at_ms.remove(key);
    }

    /// Bound the tombstone map: age out stamps older than
    /// `INBOX_TOMBSTONE_RETENTION_MS`, then evict oldest-first down to
    /// `INBOX_TOMBSTONE_CAP`.
    ///
    /// `pub(crate)` for the production merger closure (lib.rs), which prunes
    /// by wall clock BEFORE every inbound merge (PR #668 R1): a suppressed
    /// re-merge flags no change and schedules no sweep, so on a quiet inbox
    /// nothing else would ever age out the tombstone and suppression could
    /// outlive `INBOX_TOMBSTONE_RETENTION_MS`. Pruning at the merge boundary
    /// keeps the CRDT itself time-free while making the merge that would be
    /// wrongly suppressed the one that re-admits.
    pub fn prune_tombstones(&mut self, now_ms: u64) {
        self.expired_at_ms.retain(|_, t| {
            now_ms.saturating_sub(*t) < crate::butler_deposit::INBOX_TOMBSTONE_RETENTION_MS
        });
        while self.expired_at_ms.len() > crate::butler_deposit::INBOX_TOMBSTONE_CAP {
            let oldest = self
                .expired_at_ms
                .iter()
                .min_by_key(|(_, t)| **t)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.expired_at_ms.remove(&k);
                }
                None => break,
            }
        }
    }
}

impl DmInboxDoc {
    pub fn key(space_id: &[u8; 16], message_cid: &[u8]) -> String {
        format!("{}:{}", hex::encode(space_id), hex::encode(message_cid))
    }

    /// ZEB-505: deposit key for a standalone invite-only entry (no message).
    /// The `:invite` suffix can't collide with a message key, whose second
    /// half is always 64 hex chars — so one standalone invite per space.
    pub fn invite_key(space_id: &[u8; 16]) -> String {
        format!("{}:invite", hex::encode(space_id))
    }

    /// ZEB-691: deposit key for a standalone device-revocation entry (no message,
    /// no space). Keyed by the revoking friend's owner + the revoked device id, so
    /// re-depositing the same revocation is idempotent (one entry per revoked
    /// device). The literal `revoke` first segment can never be 32 hex chars, so it
    /// cannot collide with a message key (`{space_hex}:{cid_hex}`) or an invite key
    /// (`{space_hex}:invite`).
    pub fn revoke_key(revoked_owner: &[u8; 16], revoked_target: &[u8; 16]) -> String {
        format!(
            "revoke:{}:{}",
            hex::encode(revoked_owner),
            hex::encode(revoked_target)
        )
    }

    /// ZEB-674 (C4): deposit key for a standalone file-share grant entry (no
    /// message, no invite, no revocation). Keyed by the depositing granter's
    /// owner + a blake3 hash of the opaque `grant_push` bytes, so re-depositing
    /// the SAME grant is idempotent (one entry per distinct grant payload) while
    /// two grants from the same granter (different files, or a re-seal to a
    /// changed device set) get distinct keys. The literal `grant` first segment
    /// can never be 32 hex chars, so it cannot collide with a message key
    /// (`{space_hex}:{cid_hex}`), an invite key (`{space_hex}:invite`), or a
    /// revoke key (`revoke:...`).
    pub fn grant_key(granter_owner: &[u8; 16], grant_push: &[u8]) -> String {
        format!(
            "grant:{}:{}",
            hex::encode(granter_owner),
            hex::encode(blake3::hash(grant_push).as_bytes())
        )
    }

    /// ZEB-730: deposit key for a standalone file-grant REVOKE entry (no
    /// message, no invite, no revocation, no grant). Keyed by the revoking
    /// granter's owner + a blake3 hash of the opaque `grant_revoke` bytes, so
    /// re-depositing the SAME revoke is idempotent (one entry per distinct
    /// revoke payload). The literal `grant-revoke` first segment is a DISTINCT
    /// domain from `grant_key`'s `grant` segment, so a revoke key can never
    /// collide with a grant key for the same `(granter_owner, bytes)` — and
    /// neither can it collide with a message key (`{space_hex}:{cid_hex}`), an
    /// invite key (`{space_hex}:invite`), or a revoke-device key (`revoke:...`),
    /// since none of those begins with `grant-revoke:`.
    pub fn grant_revoke_key(granter_owner: &[u8; 16], grant_revoke: &[u8]) -> String {
        format!(
            "grant-revoke:{}:{}",
            hex::encode(granter_owner),
            hex::encode(blake3::hash(grant_revoke).as_bytes())
        )
    }

    /// Insert-once + ig-union merge. Same key redeposited carries identical
    /// payload (same CidNotify + blob), so first-writer-wins by `da` is safe;
    /// `ingested_by` always merges by union (grow-only set — concurrent
    /// ingestion by siblings can never race). `changed` flags only new
    /// entries or ig growth (it drives `on_applied` → ingestion wakeups;
    /// deposit-metadata churn must not wake anything).
    pub fn merge_from(&mut self, remote: DmInboxDoc) -> MergeOutcome {
        let mut changed = false;
        for (k, r) in remote.entries {
            match self.entries.get_mut(&k) {
                None => {
                    // ZEB-925: a key this replica expired by TTL is suppressed
                    // — no insert, no `changed` (no ingest wakeup, no flush
                    // churn) — until the tombstone ages out
                    // (`prune_tombstones`).
                    if self.expired_at_ms.contains_key(&k) {
                        continue;
                    }
                    changed = true;
                    self.entries.insert(k, r);
                }
                Some(l) => {
                    let before = l.ingested_by.len();
                    l.ingested_by.extend(r.ingested_by);
                    // ZEB-483 (CodeRabbit): reconcile the optional bootstrap
                    // invite. Same-key replicas can legitimately differ
                    // (`None` vs `Some`) — a pre-ZEB-483 entry merged against a
                    // sibling that re-deposited carrying the invite, or
                    // retry-timing skew. Promote `None → Some` so bootstrap
                    // bytes are never lost, and flag `changed` so the promotion
                    // nudges ingestion (an entry that previously rejected with
                    // `SpaceNotFound` can now bootstrap its Space). A `Some ≠
                    // Some` divergence is not expected in the common case (the
                    // invite is a deterministic rebuild of a stable Space
                    // record), but it CAN arise — e.g. a re-deposit after a
                    // content-key rotation or member change, or a multi-device
                    // sender signing the rebuilt invite with different keys. Both
                    // copies bootstrap the same Space, so keep the local one
                    // (first-writer-wins, consistent with the deposit-metadata
                    // rule below) and warn rather than churn.
                    let mut invite_promoted = false;
                    match (&l.invite_packet, &r.invite_packet) {
                        (None, Some(inv)) => {
                            l.invite_packet = Some(inv.clone());
                            invite_promoted = true;
                        }
                        (Some(a), Some(b)) if a != b => {
                            tracing::warn!(
                                key = %k,
                                "dm_inbox merge: conflicting invite_packet for same entry key; keeping local"
                            );
                        }
                        _ => {}
                    }
                    // Keep earliest deposit metadata (first-writer-wins):
                    // only when the local entry is strictly newer does the
                    // remote's earlier deposit replace it.
                    if l.deposited_at.is_strictly_newer_than(&r.deposited_at) {
                        l.deposited_at = r.deposited_at;
                        l.deposited_by = r.deposited_by;
                    }
                    changed |= l.ingested_by.len() != before || invite_promoted;
                }
            }
        }
        MergeOutcome { changed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;
    use std::collections::BTreeSet;

    fn hlc(w: u64, d: &str) -> Hlc {
        Hlc {
            wall_ms: w,
            logical: 0,
            device_id: d.into(),
        }
    }

    fn entry(at: Hlc, by: &str, ig: &[&str]) -> DmInboxEntry {
        DmInboxEntry {
            sender_owner: [7u8; 16],
            cidnotify_packet: Some(vec![1, 2, 3]),
            storage_blob: vec![4, 5, 6],
            invite_packet: None,
            revocation_push: None,
            grant_push: None,
            grant_revoke: None,
            deposited_at: at,
            deposited_by: by.into(),
            ingested_by: ig.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn key() -> String {
        DmInboxDoc::key(&[1u8; 16], &[2u8; 32])
    }

    // ----------------------------------------------------------------
    // ZEB-862: restart-durable first-observation clock
    // ----------------------------------------------------------------

    #[test]
    fn restored_old_first_observed_expires_across_restart() {
        let mut doc = DmInboxDoc::default();
        let k = key();
        doc.entries
            .insert(k.clone(), entry(hlc(1, "a"), "butler", &[]));
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        doc.restore_first_observed([(k.clone(), 1u64)].into_iter().collect(), now);
        assert!(
            doc.gc_expired(now, &BTreeSet::new()),
            "old restored stamp → entry ages out"
        );
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn runtime_arrival_without_restore_lazy_stamps_at_now() {
        // An entry that appears WHILE RUNNING (fleet merge) with no restore in
        // between is genuinely first-observed at the next sweep: lazy-stamp
        // `now`, survives. The boot path is different — see the ZEB-998 Q-3
        // tests below: restore inherits the deposit floor for missing stamps.
        let mut doc = DmInboxDoc::default();
        let k = key();
        doc.entries
            .insert(k.clone(), entry(hlc(1, "a"), "butler", &[]));
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        assert!(
            !doc.gc_expired(now, &BTreeSet::new()),
            "runtime lazy stamp at now → survives"
        );
        assert_eq!(doc.entries.len(), 1);
    }

    #[test]
    fn restore_and_read_first_observed_round_trips() {
        let mut doc = DmInboxDoc::default();
        let k = key();
        doc.entries
            .insert(k.clone(), entry(hlc(1, "a"), "butler", &[]));
        let m: BTreeMap<String, u64> = [(k.clone(), 12_345u64)].into_iter().collect();
        doc.restore_first_observed(m.clone(), u64::MAX);
        assert_eq!(doc.first_observed_ms(), &m);
    }

    #[test]
    fn gc_grows_first_observed_once_per_entry() {
        // Stamp-only detection (ZEB-862 finding A) keys off the side-map length
        // delta: gc_expired grows it once per newly-seen entry, not thereafter.
        let mut doc = DmInboxDoc::default();
        doc.entries.insert(key(), entry(hlc(1, "a"), "butler", &[]));
        assert_eq!(doc.first_observed_ms().len(), 0);
        assert!(!doc.gc_expired(1_000, &BTreeSet::new()));
        assert_eq!(
            doc.first_observed_ms().len(),
            1,
            "gc_expired stamped the entry"
        );
        assert!(!doc.gc_expired(2_000, &BTreeSet::new()));
        assert_eq!(doc.first_observed_ms().len(), 1, "no further growth");
    }

    #[test]
    fn restore_clamps_future_stamp_to_now() {
        // ZEB-862 Q-1: a restored FUTURE stamp is rebased to `now`.
        let mut doc = DmInboxDoc::default();
        let k = key();
        doc.entries
            .insert(k.clone(), entry(hlc(1, "a"), "butler", &[]));
        let now = 1_000_000u64;
        doc.restore_first_observed([(k.clone(), now + 5_000_000)].into_iter().collect(), now);
        assert_eq!(
            doc.first_observed_ms()[&k],
            now,
            "future stamp rebased to now"
        );
        assert!(doc.gc_expired(
            now + crate::butler_deposit::INBOX_TTL_MS + 1,
            &BTreeSet::new()
        ));
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn restore_prunes_orphan_stamps() {
        // ZEB-862 Q-2: a sidecar stamp with no matching entry is dropped.
        let mut doc = DmInboxDoc::default();
        let k = key();
        doc.entries
            .insert(k.clone(), entry(hlc(1, "a"), "butler", &[]));
        let orphan = DmInboxDoc::key(&[9u8; 16], &[9u8; 32]);
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        m.insert(k.clone(), 5);
        m.insert(orphan.clone(), 5);
        doc.restore_first_observed(m, u64::MAX);
        assert!(doc.first_observed_ms().contains_key(&k));
        assert!(
            !doc.first_observed_ms().contains_key(&orphan),
            "orphan stamp pruned"
        );
    }

    // ----------------------------------------------------------------
    // ZEB-998 Q-3: restore reconciles doc entries missing sidecar stamps
    // ----------------------------------------------------------------

    #[test]
    fn restore_missing_stamp_inherits_deposit_floor() {
        // Torn multi-file write: the doc rename landed, the first-observed
        // sidecar rename did not. The entry must inherit its own deposit stamp
        // as the observation floor — NOT be lazily re-stamped `now` by the
        // next sweep, which would restart its TTL from the boot time.
        let mut doc = DmInboxDoc::default();
        let k = key();
        doc.entries
            .insert(k.clone(), entry(hlc(1_000, "a"), "butler", &[]));
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        doc.restore_first_observed(BTreeMap::new(), now);
        assert_eq!(
            doc.first_observed_ms()[&k],
            1_000,
            "missing stamp inherits the entry's deposit floor"
        );
        assert!(
            doc.gc_expired(now, &BTreeSet::new()),
            "inherited old floor → entry ages out instead of restarting TTL"
        );
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn restore_missing_stamp_clamps_future_deposit_to_now() {
        // The deposit stamp is butler-minted (untrusted): a future-dated
        // deposit must not delay expiry past now + TTL (mirrors Q-1).
        let mut doc = DmInboxDoc::default();
        let k = key();
        let now = 1_000_000u64;
        doc.entries
            .insert(k.clone(), entry(hlc(now + 5_000_000, "a"), "butler", &[]));
        doc.restore_first_observed(BTreeMap::new(), now);
        assert_eq!(
            doc.first_observed_ms()[&k],
            now,
            "future deposit floor clamped to now"
        );
        assert!(doc.gc_expired(
            now + crate::butler_deposit::INBOX_TTL_MS + 1,
            &BTreeSet::new()
        ));
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn restore_torn_write_only_missing_entries_inherit() {
        // Mixed-generation shape: the sidecar covers the older entry (its
        // stamp survives verbatim); only the entry the crash orphaned inherits
        // its deposit floor.
        let mut doc = DmInboxDoc::default();
        let k1 = DmInboxDoc::key(&[1u8; 16], &[2u8; 32]);
        let k2 = DmInboxDoc::key(&[3u8; 16], &[4u8; 32]);
        doc.entries
            .insert(k1.clone(), entry(hlc(1, "a"), "butler", &[]));
        doc.entries
            .insert(k2.clone(), entry(hlc(7_777, "a"), "butler", &[]));
        let m: BTreeMap<String, u64> = [(k1.clone(), 5u64)].into_iter().collect();
        doc.restore_first_observed(m, 1_000_000);
        assert_eq!(doc.first_observed_ms()[&k1], 5, "sidecar stamp kept");
        assert_eq!(
            doc.first_observed_ms()[&k2],
            7_777,
            "orphaned entry inherits its deposit floor"
        );
    }

    #[test]
    fn merge_inserts_new_entry_and_is_idempotent() {
        let mut a = DmInboxDoc::default();
        let mut b = DmInboxDoc::default();
        b.entries.insert(key(), entry(hlc(1, "B"), "dev-b", &[]));

        let out = a.merge_from(b.clone());
        assert!(out.changed, "new entry must flag changed");
        assert_eq!(a.entries.len(), 1);
        assert_eq!(a.entries[&key()], b.entries[&key()]);

        let out = a.merge_from(b.clone());
        assert!(!out.changed, "re-merge of identical doc is a no-op");
        assert_eq!(a, b);
    }

    #[test]
    fn ingested_by_merges_by_union_no_lww_race() {
        let mut base = DmInboxDoc::default();
        base.entries.insert(key(), entry(hlc(1, "X"), "dev-x", &[]));

        // Concurrent ingestion acks on two replicas of the same entry.
        let mut a = base.clone();
        a.entries
            .get_mut(&key())
            .unwrap()
            .ingested_by
            .insert("dev-1".into());
        let mut b = base.clone();
        b.entries
            .get_mut(&key())
            .unwrap()
            .ingested_by
            .insert("dev-2".into());

        let out = a.merge_from(b.clone());
        assert!(out.changed, "ig growth must flag changed");
        let both: BTreeSet<String> = ["dev-1".to_string(), "dev-2".to_string()].into();
        assert_eq!(a.entries[&key()].ingested_by, both);

        let mut b2 = b.clone();
        b2.merge_from(a.clone());
        assert_eq!(b2, a, "union merge converges from both sides");
    }

    #[test]
    fn concurrent_insert_same_key_converges() {
        // Same key deposited on two butlers: payload identical by invariant,
        // deposit metadata differs. First-writer-wins on deposited_at.
        let mut a = DmInboxDoc::default();
        a.entries.insert(key(), entry(hlc(5, "A"), "dev-a", &[]));
        let mut b = DmInboxDoc::default();
        b.entries.insert(key(), entry(hlc(3, "B"), "dev-b", &[]));

        let mut a2 = a.clone();
        a2.merge_from(b.clone());
        let mut b2 = b.clone();
        b2.merge_from(a.clone());

        assert_eq!(a2, b2, "both merge orders converge to one entry");
        assert_eq!(
            a2.entries[&key()].deposited_at,
            hlc(3, "B"),
            "earliest deposit wins"
        );
        assert_eq!(a2.entries[&key()].deposited_by, "dev-b");
    }

    #[test]
    fn visible_change_flag_only_on_new_entries_or_ig_growth() {
        let mut a = DmInboxDoc::default();
        a.entries
            .insert(key(), entry(hlc(5, "A"), "dev-a", &["dev-1"]));

        // Identical remote: no change.
        let out = a.merge_from(a.clone());
        assert!(!out.changed, "identical merge is not a change");

        // Earlier deposit metadata, same ig: metadata swaps but is NOT
        // flagged (changed drives ingestion wakeups, not metadata churn).
        let mut earlier = DmInboxDoc::default();
        earlier
            .entries
            .insert(key(), entry(hlc(3, "B"), "dev-b", &["dev-1"]));
        let out = a.merge_from(earlier);
        assert!(!out.changed, "metadata-only swap must not flag changed");
        assert_eq!(a.entries[&key()].deposited_by, "dev-b");

        // ig growth: flagged.
        let mut grown = DmInboxDoc::default();
        grown
            .entries
            .insert(key(), entry(hlc(3, "B"), "dev-b", &["dev-1", "dev-2"]));
        let out = a.merge_from(grown);
        assert!(out.changed, "ig growth must flag changed");

        // ig subset of local (no growth): not flagged.
        let mut subset = DmInboxDoc::default();
        subset
            .entries
            .insert(key(), entry(hlc(3, "B"), "dev-b", &["dev-2"]));
        let out = a.merge_from(subset);
        assert!(!out.changed, "ig subset adds nothing — not a change");

        // New entry under a different key: flagged.
        let mut fresh = DmInboxDoc::default();
        fresh.entries.insert(
            DmInboxDoc::key(&[9u8; 16], &[9u8; 32]),
            entry(hlc(7, "C"), "dev-c", &[]),
        );
        let out = a.merge_from(fresh);
        assert!(out.changed, "new entry must flag changed");
    }

    /// ZEB-483 (CodeRabbit): a same-key merge must promote a missing bootstrap
    /// invite (`None → Some`) and flag `changed` so ingestion is re-nudged — an
    /// entry that previously rejected with `SpaceNotFound` can now bootstrap its
    /// Space once a sibling supplies the invite.
    #[test]
    fn merge_promotes_invite_none_to_some_and_flags_changed() {
        let mut local = DmInboxDoc::default();
        local
            .entries
            .insert(key(), entry(hlc(1, "A"), "dev-a", &[]));
        assert!(local.entries[&key()].invite_packet.is_none());

        // Remote replica of the SAME entry that carries the invite.
        let mut remote = DmInboxDoc::default();
        let mut with_invite = entry(hlc(1, "A"), "dev-a", &[]);
        with_invite.invite_packet = Some(vec![0xAA, 0xBB, 0xCC]);
        remote.entries.insert(key(), with_invite);

        let out = local.merge_from(remote);
        assert!(
            out.changed,
            "invite promotion must flag changed (nudges ingest)"
        );
        assert_eq!(
            local.entries[&key()].invite_packet.as_deref(),
            Some(&[0xAA, 0xBB, 0xCC][..]),
            "missing invite promoted from the sibling"
        );

        // Idempotent: a re-merge of the now-equal docs is a no-op, and an
        // already-present invite is never overwritten / re-flagged.
        let out = local.merge_from(local.clone());
        assert!(
            !out.changed,
            "re-merge with the invite already present is a no-op"
        );
    }

    #[test]
    fn cbor_round_trips_canonically() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let mut d = DmInboxDoc::default();
        d.entries
            .insert(key(), entry(hlc(1, "A"), "dev-a", &["dev-1", "dev-2"]));
        // Also pin the empty-ig skip path round-trips.
        d.entries.insert(
            DmInboxDoc::key(&[9u8; 16], &[9u8; 32]),
            entry(hlc(2, "B"), "dev-b", &[]),
        );
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: DmInboxDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d);
    }

    #[test]
    fn dm_inbox_entry_invite_only_cidnotify_none_round_trips() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        // ZEB-505: a fleet-replicated invite-only deposit carries the bootstrap
        // invite ALONE — `cidnotify_packet: None`, no storage blob — keyed by
        // `invite_key`. None must survive the canonical-CBOR round-trip
        // (skip_serializing_if omits `cn`; absent → None via default).
        let mut e = entry(hlc(1, "A"), "dev-a", &["dev-1"]);
        e.cidnotify_packet = None;
        e.storage_blob = Vec::new();
        e.invite_packet = Some(vec![0xAA, 0xBB, 0xCC]);
        let mut d = DmInboxDoc::default();
        let k = DmInboxDoc::invite_key(&[1u8; 16]);
        d.entries.insert(k.clone(), e);
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: DmInboxDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d);
        assert_eq!(back.entries[&k].cidnotify_packet, None);
    }

    #[test]
    fn revoke_key_de_collides_with_message_and_invite_keys() {
        let space = [0xAB; 16];
        let cid = [0xCD; 32];
        let owner = [0x11; 16];
        let device = [0x22; 16];
        let msg = DmInboxDoc::key(&space, &cid);
        let inv = DmInboxDoc::invite_key(&space);
        let rev = DmInboxDoc::revoke_key(&owner, &device);
        assert!(rev.starts_with("revoke:"));
        assert_ne!(rev, msg);
        assert_ne!(rev, inv);
        // A revoke key's first segment is the literal "revoke", never 32 hex chars,
        // so it can never alias a space-scoped key.
        assert!(!msg.starts_with("revoke:"));
        assert!(!inv.starts_with("revoke:"));
    }

    #[test]
    fn dm_inbox_entry_round_trips_revocation_push() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let e = DmInboxEntry {
            sender_owner: [1u8; 16],
            cidnotify_packet: None,
            storage_blob: Vec::new(),
            invite_packet: None,
            revocation_push: Some(vec![0x05, 0xAA, 0xBB]),
            grant_push: None,
            grant_revoke: None,
            deposited_at: Hlc {
                wall_ms: 1,
                logical: 0,
                device_id: "d".into(),
            },
            deposited_by: "d".into(),
            ingested_by: Default::default(),
        };
        let bytes = canonical_cbor_encode(&e).unwrap();
        let back: DmInboxEntry = canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(back.revocation_push, Some(vec![0x05, 0xAA, 0xBB]));
    }

    /// ZEB-674 (C4): a standalone file-share grant entry carries `grant_push`
    /// ALONE (no message / invite / revocation). The field must survive the
    /// canonical-CBOR round-trip (skip_serializing_if omits `gp` when `None`;
    /// absent → `None` via default) and merge into a sibling doc insert-once,
    /// exactly like its optional-sub-payload siblings.
    #[test]
    fn dm_inbox_entry_grant_push_merge_persist() {
        use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};
        let mut e = entry(hlc(1, "A"), "dev-a", &["dev-1"]);
        e.cidnotify_packet = None;
        e.storage_blob = Vec::new();
        e.grant_push = Some(vec![0xDE, 0xAD, 0xBE, 0xEF]);

        // Persist round-trip: `grant_push` survives canonical CBOR.
        let mut d = DmInboxDoc::default();
        let k = DmInboxDoc::grant_key(&[7u8; 16], &[0xDE, 0xAD, 0xBE, 0xEF]);
        d.entries.insert(k.clone(), e.clone());
        let bytes = canonical_cbor_encode(&d).expect("encode");
        let back: DmInboxDoc = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(back, d, "grant entry round-trips through canonical CBOR");
        assert_eq!(
            back.entries[&k].grant_push,
            Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            "grant_push preserved verbatim"
        );

        // Merge into an empty sibling: insert-once carries `grant_push` through
        // (first-writer-wins, like `revocation_push`), and re-merge is a no-op.
        let mut sibling = DmInboxDoc::default();
        let out = sibling.merge_from(d.clone());
        assert!(out.changed, "new grant entry flags changed");
        assert_eq!(
            sibling.entries[&k].grant_push,
            Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            "merged sibling holds the grant payload"
        );
        let out = sibling.merge_from(d.clone());
        assert!(
            !out.changed,
            "re-merge of the identical grant entry is a no-op"
        );
    }

    /// A grant key can't alias a message / invite / revoke key.
    #[test]
    fn grant_key_de_collides_with_other_keys() {
        let owner = [0x33; 16];
        let gp = [0xAB, 0xCD, 0xEF];
        let grant = DmInboxDoc::grant_key(&owner, &gp);
        assert!(grant.starts_with("grant:"));
        assert_ne!(grant, DmInboxDoc::key(&[0x33; 16], &[0xCD; 32]));
        assert_ne!(grant, DmInboxDoc::invite_key(&[0x33; 16]));
        assert_ne!(grant, DmInboxDoc::revoke_key(&owner, &[0x22; 16]));
        // Distinct grant payloads from the same granter get distinct keys.
        assert_ne!(grant, DmInboxDoc::grant_key(&owner, &[0x01, 0x02]));
    }

    /// ZEB-730: a grant-REVOKE key can't alias a grant / message / invite /
    /// revoke key, and — critically — never collides with a grant key for the
    /// SAME `(granter_owner, bytes)` (distinct `grant-revoke:` domain segment).
    #[test]
    fn grant_revoke_key_de_collides_with_other_keys() {
        let owner = [0x33; 16];
        let bytes = [0xAB, 0xCD, 0xEF];
        let revoke = DmInboxDoc::grant_revoke_key(&owner, &bytes);
        assert!(revoke.starts_with("grant-revoke:"));
        // Same owner + same bytes as a grant key → still distinct (domain split).
        assert_ne!(revoke, DmInboxDoc::grant_key(&owner, &bytes));
        assert_ne!(revoke, DmInboxDoc::key(&[0x33; 16], &[0xCD; 32]));
        assert_ne!(revoke, DmInboxDoc::invite_key(&[0x33; 16]));
        assert_ne!(revoke, DmInboxDoc::revoke_key(&owner, &[0x22; 16]));
        // Distinct revoke payloads from the same granter get distinct keys.
        assert_ne!(revoke, DmInboxDoc::grant_revoke_key(&owner, &[0x01, 0x02]));
    }

    // ----------------------------------------------------------------
    // ZEB-925: local expiry tombstones stop resurrection-by-merge
    // ----------------------------------------------------------------

    fn key_n(space_byte: u8, cid_byte: u8) -> String {
        DmInboxDoc::key(&[space_byte; 16], &[cid_byte; 32])
    }

    #[test]
    fn gc_ttl_expiry_tombstones_the_key_but_coverage_removal_does_not() {
        let mut doc = DmInboxDoc::default();
        let k_ttl = key_n(1, 1);
        let k_cov = key_n(2, 2);
        doc.entries
            .insert(k_ttl.clone(), entry(hlc(1, "a"), "butler", &[]));
        doc.entries
            .insert(k_cov.clone(), entry(hlc(1, "a"), "butler", &[]));
        // Both stamps ancient → both entries are past TTL at `now`.
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        doc.restore_first_observed(
            [(k_ttl.clone(), 1u64), (k_cov.clone(), 1u64)]
                .into_iter()
                .collect(),
            now,
        );
        let covered: BTreeSet<String> = [k_cov.clone()].into();
        assert!(doc.gc_expired(now, &covered));
        assert!(doc.entries.is_empty(), "both removed");
        assert!(
            doc.expired_at_ms().contains_key(&k_ttl),
            "TTL-only removal is tombstoned"
        );
        assert!(
            !doc.expired_at_ms().contains_key(&k_cov),
            "covered removal is NOT tombstoned even when also past TTL \
             (coverage is fleet-deterministic; suppression is dead state)"
        );
    }

    #[test]
    fn merge_suppresses_resurrection_of_tombstoned_key() {
        let mut doc = DmInboxDoc::default();
        let k = key_n(3, 3);
        doc.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        doc.restore_first_observed([(k.clone(), 1u64)].into_iter().collect(), now);
        assert!(doc.gc_expired(now, &BTreeSet::new()));
        assert!(doc.entries.is_empty());

        // A still-holding sibling re-merges the expired entry.
        let mut remote = DmInboxDoc::default();
        remote
            .entries
            .insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        let out = doc.merge_from(remote);
        assert!(!out.changed, "suppressed re-insert must not flag changed");
        assert!(
            !doc.entries.contains_key(&k),
            "tombstoned key never re-enters entries"
        );
    }

    #[test]
    fn resurrection_lifetime_bound_across_merge_traffic() {
        // A never-covered entry's lifetime on this replica is bounded by
        // first-observation + TTL + one sweep, regardless of merge traffic.
        let mut doc = DmInboxDoc::default();
        let k = key_n(4, 4);
        doc.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        assert!(!doc.gc_expired(1_000, &BTreeSet::new()), "stamped at 1s");
        let mut remote = DmInboxDoc::default();
        remote
            .entries
            .insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        // Merge traffic every "day" for 90 days: the entry must be gone from
        // every sweep after expiry (1_000 + TTL).
        let day = 24 * 60 * 60 * 1_000u64;
        let expiry = 1_000 + crate::butler_deposit::INBOX_TTL_MS;
        for d in 1..=90u64 {
            let now = 1_000 + d * day;
            doc.merge_from(remote.clone());
            doc.gc_expired(now, &BTreeSet::new());
            if now > expiry {
                assert!(
                    !doc.entries.contains_key(&k),
                    "day {d}: entry resurrected past its TTL"
                );
            }
        }
    }

    #[test]
    fn covered_resurrection_still_converges_without_tombstone() {
        let mut doc = DmInboxDoc::default();
        let k = key_n(5, 5);
        doc.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        let covered: BTreeSet<String> = [k.clone()].into();
        assert!(doc.gc_expired(1_000, &covered), "covered removal");
        assert!(doc.expired_at_ms().is_empty(), "no tombstone for coverage");

        // Resurrected by a slower sibling: insert-once ADMITS it (changed),
        // and the next sweep's deterministic coverage removes it again.
        let mut remote = DmInboxDoc::default();
        remote
            .entries
            .insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        let out = doc.merge_from(remote);
        assert!(out.changed, "covered key is not suppressed");
        assert!(doc.gc_expired(2_000, &covered));
        assert!(doc.entries.is_empty(), "coverage converges by determinism");
    }

    #[test]
    fn tombstone_ages_out_after_retention_and_reopens() {
        let mut doc = DmInboxDoc::default();
        let k = key_n(6, 6);
        doc.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        doc.restore_first_observed([(k.clone(), 1u64)].into_iter().collect(), now);
        assert!(doc.gc_expired(now, &BTreeSet::new()));
        assert!(doc.expired_at_ms().contains_key(&k));

        let mut remote = DmInboxDoc::default();
        remote
            .entries
            .insert(k.clone(), entry(hlc(1, "a"), "b", &[]));

        // One ms before retention elapses: still suppressed (the aging gc
        // sweep runs first, then the merge).
        let just_before = now + crate::butler_deposit::INBOX_TOMBSTONE_RETENTION_MS - 1;
        doc.gc_expired(just_before, &BTreeSet::new());
        assert!(!doc.merge_from(remote.clone()).changed);

        // At/after retention: the tombstone ages out; the key may re-enter
        // (and gets a fresh first-observation window — the accepted
        // once-per-retention residual).
        let after = now + crate::butler_deposit::INBOX_TOMBSTONE_RETENTION_MS;
        doc.gc_expired(after, &BTreeSet::new());
        assert!(!doc.expired_at_ms().contains_key(&k), "tombstone aged out");
        assert!(doc.merge_from(remote).changed, "key readmitted");
    }

    #[test]
    fn tombstone_cap_evicts_oldest_first() {
        // Cap enforcement at restore: CAP+2 tombstones with distinct stamps →
        // the two oldest are evicted, the newest survive.
        let cap = crate::butler_deposit::INBOX_TOMBSTONE_CAP;
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        for i in 0..(cap + 2) {
            // Distinct keys; stamp = i+1 so ordering is by insertion index.
            m.insert(format!("cap-key-{i:05}"), (i + 1) as u64);
        }
        let newest = (cap + 2) as u64;
        let mut doc = DmInboxDoc::default();
        doc.restore_expired(m, newest);
        assert_eq!(doc.expired_at_ms().len(), cap, "pruned down to cap");
        assert!(
            !doc.expired_at_ms().contains_key("cap-key-00000")
                && !doc.expired_at_ms().contains_key("cap-key-00001"),
            "oldest two evicted"
        );
        assert!(
            doc.expired_at_ms()
                .contains_key(&format!("cap-key-{:05}", cap + 1)),
            "newest kept"
        );
    }

    #[test]
    fn restore_prunes_aged_out_tombstones_and_lets_their_entries_live() {
        let mut doc = DmInboxDoc::default();
        let k = key_n(7, 7);
        doc.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        // Sidecar stamp older than retention at boot: the tombstone must be
        // pruned BEFORE the entries sweep, so the (re-deposited) entry lives.
        let boot = crate::butler_deposit::INBOX_TOMBSTONE_RETENTION_MS + 50_000;
        doc.restore_expired([(k.clone(), 1u64)].into_iter().collect(), boot);
        assert!(doc.expired_at_ms().is_empty(), "aged-out tombstone dropped");
        assert!(
            doc.entries.contains_key(&k),
            "entry survives an expired tombstone"
        );
    }

    /// PR #668 R1 (CodeRabbit): a suppressed re-merge flags no change, so it
    /// schedules no sweep — on a quiet inbox nothing would ever prune an
    /// aged-out tombstone and suppression could outlive retention. The
    /// production merger closure (lib.rs) therefore prunes by wall clock
    /// BEFORE every inbound merge; this pins that composition: the same
    /// merge that would be wrongly suppressed is the one that re-admits.
    #[test]
    fn merge_path_prune_reopens_after_retention_without_any_sweep() {
        let mut doc = DmInboxDoc::default();
        let k = key_n(10, 10);
        doc.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        let now = crate::butler_deposit::INBOX_TTL_MS + 10_000;
        doc.restore_first_observed([(k.clone(), 1u64)].into_iter().collect(), now);
        assert!(doc.gc_expired(now, &BTreeSet::new()));
        assert!(doc.expired_at_ms().contains_key(&k));

        let mut remote = DmInboxDoc::default();
        remote
            .entries
            .insert(k.clone(), entry(hlc(1, "a"), "b", &[]));

        // NO gc sweeps from here on — only the merger-closure composition
        // (prune_tombstones then merge_from).

        // Before retention: still suppressed.
        let before = now + crate::butler_deposit::INBOX_TOMBSTONE_RETENTION_MS - 1;
        doc.prune_tombstones(before);
        assert!(!doc.merge_from(remote.clone()).changed, "still suppressed");
        assert!(!doc.entries.contains_key(&k));

        // After retention: the pre-merge prune drops the tombstone and the
        // SAME merge re-admits the key.
        let after = now + crate::butler_deposit::INBOX_TOMBSTONE_RETENTION_MS;
        doc.prune_tombstones(after);
        assert!(
            doc.merge_from(remote).changed,
            "the merge after retention re-admits without any sweep"
        );
        assert!(doc.entries.contains_key(&k));
    }

    #[test]
    fn restore_expired_removes_tombstoned_entries_and_clamps_future_stamps() {
        let mut doc = DmInboxDoc::default();
        let k_dead = key_n(8, 8);
        let k_live = key_n(9, 9);
        doc.entries
            .insert(k_dead.clone(), entry(hlc(1, "a"), "b", &[]));
        doc.entries
            .insert(k_live.clone(), entry(hlc(1, "a"), "b", &[]));
        let boot = 1_000_000u64;
        // k_dead: fresh tombstone (wins over the stale doc). Also a FUTURE
        // stamp — must be clamped to boot so it cannot outlive retention.
        doc.restore_expired(
            [(k_dead.clone(), boot + 5_000_000)].into_iter().collect(),
            boot,
        );
        assert!(
            !doc.entries.contains_key(&k_dead),
            "tombstone wins over a stale doc"
        );
        assert!(doc.entries.contains_key(&k_live), "untombstoned key lives");
        assert_eq!(
            doc.expired_at_ms()[&k_dead],
            boot,
            "future stamp clamped to boot"
        );
    }
}
