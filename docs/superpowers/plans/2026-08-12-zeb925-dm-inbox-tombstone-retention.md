# ZEB-925: DM-Inbox Tombstone Retention Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound never-acked DM-inbox entry retention by tombstoning TTL expiries locally, so resurrection-by-merge cannot re-arm a fresh 30-day window (port of the ZEB-924 relay-hold mechanism).

**Architecture:** A `#[serde(skip)]` local tombstone map on `DmInboxDoc` (wire bytes unchanged), stamped by `gc_expired` for TTL-only removals, suppressing `merge_from` re-inserts, persisted in a new sidecar written FIRST in `DmInboxPersist::persist`, restored at boot before `restore_first_observed`, with a retry latch in the ingest sweeper and tombstone-clear on deposit acceptance. Design: `docs/superpowers/specs/2026-08-12-zeb925-dm-inbox-tombstone-retention-design.md`.

**Tech Stack:** Rust (src-tauri), cargo-nextest, ciborium CBOR.

## Global Constraints

- All cargo commands run from `src-tauri/`, always `--locked --features test-fixtures`.
- Canonical wire bytes MUST NOT change (`#[serde(skip)]`; entries-only `PartialEq`).
- Coverage-GC semantics unchanged: covered removals are NEVER tombstoned.
- Constants: `INBOX_TOMBSTONE_RETENTION_MS = 2 * INBOX_TTL_MS`, `INBOX_TOMBSTONE_CAP = 4 * INBOX_GLOBAL_CAP`, `const _: () = assert!(cap >= global_cap)`.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.

---

### Task 1: Constants + CRDT tombstone core

**Files:**
- Modify: `src-tauri/src/butler_deposit.rs` (after `INBOX_TTL_MS`, line ~165)
- Modify: `src-tauri/src/dm_inbox_crdt.rs` (doc field, `gc_expired`, `merge_from`, new methods, tests)

**Interfaces:**
- Produces: `DmInboxDoc::expired_at_ms() -> &BTreeMap<String, u64>`, `restore_expired(map, now_ms)`, `clear_tombstone(&str)`, `butler_deposit::{INBOX_TOMBSTONE_RETENTION_MS, INBOX_TOMBSTONE_CAP}` — consumed by Tasks 2–4.

- [ ] **Step 1: Write the failing tests** (append to `dm_inbox_crdt.rs` `mod tests`; helpers `hlc`/`entry`/`key` already exist; add a distinct-key helper)

```rust
    fn key_n(space_byte: u8, cid_byte: u8) -> String {
        DmInboxDoc::key(&[space_byte; 16], &[cid_byte; 32])
    }

    // ----------------------------------------------------------------
    // ZEB-925: local expiry tombstones stop resurrection-by-merge
    // ----------------------------------------------------------------

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
        remote.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
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
        remote.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
        // Merge traffic every "day" for 90 days: entry must be gone from
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
        remote.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));
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
        remote.entries.insert(k.clone(), entry(hlc(1, "a"), "b", &[]));

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
        // the two oldest are evicted, newest survive.
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
        assert!(!doc.entries.contains_key(&k_dead), "tombstone wins over doc");
        assert!(doc.entries.contains_key(&k_live), "untombstoned key lives");
        assert_eq!(
            doc.expired_at_ms()[&k_dead],
            boot,
            "future stamp clamped to boot"
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(tombstone) or test(resurrection) or test(restore_expired)'`
Expected: compile FAIL — `expired_at_ms`/`restore_expired`/`INBOX_TOMBSTONE_*` not found.

- [ ] **Step 3: Implement**

`butler_deposit.rs`, directly after `INBOX_TTL_MS`:

```rust
/// ZEB-925: local expiry-tombstone retention — how long a TTL-expired key's
/// tombstone suppresses resurrection-by-merge (2×TTL bounds tombstone age).
pub const INBOX_TOMBSTONE_RETENTION_MS: u64 = 2 * INBOX_TTL_MS;

/// ZEB-925: hard bound on the tombstone map (4× the live-entry cap gives
/// headroom for churn; oldest-first eviction beyond it).
pub const INBOX_TOMBSTONE_CAP: usize = 4 * INBOX_GLOBAL_CAP;

// Eviction must never make the tombstone set smaller than what a full inbox
// can expire in one sweep.
const _: () = assert!(INBOX_TOMBSTONE_CAP >= INBOX_GLOBAL_CAP);
```

`dm_inbox_crdt.rs` — field after `first_observed_ms`:

```rust
    /// ZEB-925: LOCAL expiry tombstones (ms) keyed by entry key — memory of
    /// keys this replica removed by TTL expiry, so a still-holding sibling's
    /// merge cannot resurrect them and re-arm a fresh TTL window. Never
    /// serialized (canonical wire bytes unchanged) and excluded from
    /// `PartialEq` below, mirroring `first_observed_ms`. Bounded by
    /// `INBOX_TOMBSTONE_RETENTION_MS` age-out + `INBOX_TOMBSTONE_CAP`
    /// oldest-first eviction (`prune_tombstones`).
    #[serde(skip)]
    expired_at_ms: BTreeMap<String, u64>,
```

`gc_expired` retain block replaced (split the removal reason; tombstone then prune, before the existing fo live-key prune):

```rust
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
```

`merge_from` `None` arm:

```rust
                None => {
                    // ZEB-925: a key this replica expired by TTL is suppressed
                    // — no insert, no `changed` (no ingest wakeup, no flush
                    // churn) — until the tombstone ages out.
                    if self.expired_at_ms.contains_key(&k) {
                        continue;
                    }
                    changed = true;
                    self.entries.insert(k, r);
                }
```

New methods in the ZEB-851/862 `impl DmInboxDoc` block:

```rust
    /// ZEB-925: read the LOCAL expiry-tombstone map for durable sidecar
    /// persistence. Never leaves this replica and never enters the wire.
    pub fn expired_at_ms(&self) -> &BTreeMap<String, u64> {
        &self.expired_at_ms
    }

    /// ZEB-925: restore the LOCAL expiry tombstones on boot from the sidecar,
    /// BEFORE `restore_first_observed` (whose orphan-prune then drops the
    /// stamps of any entry removed here). Future stamps are clamped to
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
    /// never coexist with its own tombstone (`restore_expired` would delete
    /// the acked entry at the next boot).
    pub fn clear_tombstone(&mut self, key: &str) {
        self.expired_at_ms.remove(key);
    }

    /// Bound the tombstone map: age out stamps older than
    /// `INBOX_TOMBSTONE_RETENTION_MS`, then evict oldest-first down to
    /// `INBOX_TOMBSTONE_CAP`.
    fn prune_tombstones(&mut self, now_ms: u64) {
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
```

- [ ] **Step 4: Run to verify pass** (same command as Step 2, plus the pre-existing crdt families)

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'binary_id(harmony-app) and test(dm_inbox)'`
Expected: PASS (new 8 + all existing).

- [ ] **Step 5: Commit** — `feat(dm-inbox): ZEB-925 Task 1 — local expiry tombstones in DmInboxDoc`

---

### Task 2: Expired sidecar persistence (write-first) + construction sites

**Files:**
- Modify: `src-tauri/src/dm_inbox_persist.rs` (sidecar trio, `DmInboxPersist.expired_path`, persist order, tests)
- Modify: `src-tauri/src/lib.rs:~6190` (path decl + construction field only)
- Modify: `src-tauri/src/dm_inbox_ingest.rs:~2269` (test construction)
- Modify: `src-tauri/tests/library/butler_outhold_integration.rs:~320`, `src-tauri/tests/library/butler_deposit_integration.rs:~597/610/904/917` (test constructions)

**Interfaces:**
- Consumes: `DmInboxDoc::{expired_at_ms, restore_expired}` (Task 1).
- Produces: `DM_INBOX_EXPIRED_FILENAME`, `load_expired`, `load_expired_or_recover`, `save_expired`, `DmInboxPersist.expired_path` — consumed by Task 3 boot wiring.

- [ ] **Step 1: Write the failing tests** (append to `dm_inbox_persist.rs` `mod tests`)

```rust
    // ── expired-tombstone sidecar (ZEB-925) ──────────────────────────────────

    #[test]
    fn expired_round_trips_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_expired.cbor");
        assert!(load_expired(&path).unwrap().is_empty());
        let mut m = std::collections::BTreeMap::new();
        m.insert("k1".to_string(), 111u64);
        m.insert("k2".to_string(), 222u64);
        save_expired(&path, &m).unwrap();
        assert_eq!(load_expired(&path).unwrap(), m);
    }

    #[test]
    fn load_expired_rejects_trailing_bytes_and_recover_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dm_inbox_expired.cbor");
        let mut m = std::collections::BTreeMap::new();
        m.insert("k".to_string(), 5u64);
        save_expired(&path, &m).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0xFF);
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            load_expired(&path).unwrap_err(),
            SyncError::CborDecode(_)
        ));
        assert!(load_expired_or_recover(&path).unwrap().is_empty());
        assert!(!path.exists(), "corrupt sidecar was quarantined");
    }

    #[test]
    fn persist_writes_expired_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let p = DmInboxPersist {
            doc_path: dir.path().join("dm_inbox.cbor"),
            replay_path: dir.path().join("dm_inbox_replay.cbor"),
            first_observed_path: dir.path().join("dm_inbox_first_observed.cbor"),
            expired_path: dir.path().join("dm_inbox_expired.cbor"),
        };
        use crate::fleet_sync::FleetPersist;
        let mut doc = sample_doc();
        let m: std::collections::BTreeMap<String, u64> =
            [("gone-key".to_string(), 7u64)].into_iter().collect();
        // Boot time near the stamp so the restore-time retention prune keeps it.
        doc.restore_expired(m.clone(), 7);
        p.persist(&doc, &std::collections::BTreeMap::new()).unwrap();
        assert_eq!(load_expired(&p.expired_path).unwrap(), m);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(expired)'`
Expected: compile FAIL — `load_expired`/`save_expired`/`expired_path` not found.

- [ ] **Step 3: Implement** — in `dm_inbox_persist.rs` after the first-observed section, mirroring it symbol-for-symbol:

```rust
// ── expired-tombstone sidecar (ZEB-925) ───────────────────────────────────────

/// File name for the persisted LOCAL expiry-tombstone map. Lives alongside
/// `dm_inbox.cbor`. Local-only: never replicated, never on the wire — it makes
/// the `#[serde(skip)]` `DmInboxDoc::expired_at_ms` suppression survive
/// restart (a tombstone that forgot across reboot would let a sibling's merge
/// resurrect the expired entry with a fresh TTL window).
pub const DM_INBOX_EXPIRED_FILENAME: &str = "dm_inbox_expired.cbor";

const DM_INBOX_EXPIRED_SCHEMA_V1: u8 = 1;

#[derive(Serialize, Deserialize)]
struct DmInboxExpiredFileV1(BTreeMap<String, u64>);

/// Load the LOCAL expiry-tombstone map from `path`. Returns
/// `Ok(BTreeMap::new())` if the file does not exist yet (→ no suppression;
/// worst case one extra TTL window — exactly pre-ZEB-925 behavior).
pub fn load_expired(path: &Path) -> Result<BTreeMap<String, u64>, SyncError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(SyncError::Persist(format!("read {}: {e}", path.display()))),
    };
    if bytes.is_empty() {
        return Err(SyncError::CborDecode(format!(
            "dm-inbox expired file is empty: {}",
            path.display()
        )));
    }
    let version = bytes[0];
    let payload = &bytes[1..];
    match version {
        DM_INBOX_EXPIRED_SCHEMA_V1 => {
            let mut cursor = Cursor::new(payload);
            let file: DmInboxExpiredFileV1 = from_reader(&mut cursor)
                .map_err(|e| SyncError::CborDecode(format!("load_expired {}: {e}", path.display())))?;
            // Reject trailing bytes after the CBOR value (mirrors `load`).
            let pos = cursor.position() as usize;
            if pos != payload.len() {
                return Err(SyncError::CborDecode(format!(
                    "trailing bytes after dm-inbox expired value: consumed {} of {}",
                    pos,
                    payload.len()
                )));
            }
            Ok(file.0)
        }
        v => Err(SyncError::CborDecode(format!(
            "unknown dm-inbox expired schema version {v:#x} in {}",
            path.display()
        ))),
    }
}

/// Same recovery contract as [`load_doc_or_recover`]: `CborDecode` corruption
/// is quarantined (`.corrupt-<ms>`, bytes preserved) and an empty map
/// returned; a transient `Persist` error is left untouched and propagated
/// (ZEB-460). A missing/empty map is safe — suppression is lost, retention
/// falls back to one TTL window per resurrection until re-tombstoned.
pub fn load_expired_or_recover(path: &Path) -> Result<BTreeMap<String, u64>, SyncError> {
    match load_expired(path) {
        Ok(m) => Ok(m),
        Err(e @ SyncError::CborDecode(_)) => {
            quarantine(path, &e);
            Ok(BTreeMap::new())
        }
        Err(e) => Err(e),
    }
}

/// Save the LOCAL expiry-tombstone map to `path` atomically.
pub fn save_expired(path: &Path, map: &BTreeMap<String, u64>) -> Result<(), SyncError> {
    let mut bytes = vec![DM_INBOX_EXPIRED_SCHEMA_V1];
    into_writer(&DmInboxExpiredFileV1(map.clone()), &mut bytes)
        .map_err(|e| SyncError::CborEncode(format!("encode expired {}: {e}", path.display())))?;
    atomic_write(path, &bytes)
}
```

`DmInboxPersist` gains the field (docstring: "ZEB-925: local-only expiry-tombstone sidecar"), and `persist` is reordered — expired FIRST:

```rust
    fn persist(
        &self,
        state: &DmInboxDoc,
        tracker: &BTreeMap<String, Hlc>,
    ) -> Result<(), SyncError> {
        // ZEB-925: tombstones FIRST. A crash between writes then leaves
        // tombstone-present + stale-doc — healed by restore_expired at boot —
        // instead of fresh-doc + missing-tombstone, which resurrects the
        // expired entry with a fresh TTL window (un-healable).
        save_expired(&self.expired_path, state.expired_at_ms())?;
        save(&self.doc_path, state)?;
        save_replay(&self.replay_path, tracker)?;
        save_first_observed(&self.first_observed_path, state.first_observed_ms())?;
        Ok(())
    }
```

Add `expired_path: <dir>.join("dm_inbox_expired.cbor")`-style fields to EVERY construction site: `dm_inbox_persist.rs` test (`dm_inbox_persist_writes_all_files`), `lib.rs:~6251` (declare `let dm_inbox_expired_path = identity_dir.join(crate::dm_inbox_persist::DM_INBOX_EXPIRED_FILENAME);` beside the sibling path decls and pass `expired_path: dm_inbox_expired_path.clone()`), `dm_inbox_ingest.rs:~2269`, `butler_outhold_integration.rs:~320`, `butler_deposit_integration.rs:~597/610/904/917`.

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(dm_inbox) or test(expired)'`
Expected: PASS.

- [ ] **Step 5: Commit** — `feat(dm-inbox): ZEB-925 Task 2 — expired-tombstone sidecar, written first`

---

### Task 3: Boot restore + sweeper sidecar latch

**Files:**
- Modify: `src-tauri/src/lib.rs:~6195-6211` (boot restore ordering)
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`sweep_once` + `run_dm_inbox_ingest_sweeper`, tests)

**Interfaces:**
- Consumes: `restore_expired` (Task 1), `load_expired_or_recover` (Task 2).
- Produces: `sweep_once(..., sidecar_persist_pending: &mut bool)` (private; both callers are in `run_dm_inbox_ingest_sweeper`).

- [ ] **Step 1: Write the failing tests** (append to `dm_inbox_ingest.rs` `mod tests`; `ProbeCtx`/`make_entry` helpers exist)

```rust
    /// ZEB-925: a sweep whose only effect is tombstone-map change (age-out on
    /// an otherwise no-op sweep) persists the sidecars via persist_now.
    #[tokio::test]
    async fn tombstone_delta_only_sweep_persists_via_persist_now() {
        let now_ms: u64 = INBOX_TTL_MS + 1_000_000;
        let ctx = ProbeCtx {
            now_ms,
            ..ProbeCtx::new()
        };
        let doc = Arc::new(Mutex::new(DmInboxDoc::default()));
        {
            // Tombstone from a "previous run", already aged past retention at
            // this sweep's `now` — the sweep's prune drops it: a tombstone-map
            // shrink with NO entry change and NO fo growth.
            let mut guard = doc.lock().await;
            guard.restore_expired(
                [("old-key".to_string(), 1u64)].into_iter().collect(),
                // Restore-time "boot" near the stamp keeps it installed…
                2u64,
            );
            assert_eq!(guard.expired_at_ms().len(), 1);
        }
        let persist_calls = Arc::new(AtomicUsize::new(0));
        let persist_now: PersistNowFn = {
            let calls = Arc::clone(&persist_calls);
            Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })
        };
        let mut pending = false;
        // …and the sweep at `now_ms` (>> retention) prunes it.
        sweep_once(&doc, &ctx, &|| {}, &persist_now, &mut pending).await;
        assert_eq!(doc.lock().await.expired_at_ms().len(), 0, "aged out");
        assert_eq!(
            persist_calls.load(Ordering::SeqCst),
            1,
            "tombstone-only delta persisted via persist_now"
        );
        assert!(!pending, "successful persist leaves the latch clear");
    }

    /// ZEB-925 (R1 lesson): a FAILED sidecar persist_now latches and retries
    /// on the next sweep even when that sweep changes nothing.
    #[tokio::test]
    async fn failed_sidecar_persist_latches_and_retries_next_sweep() {
        let now_ms: u64 = INBOX_TTL_MS + 1_000_000;
        let ctx = ProbeCtx {
            now_ms,
            ..ProbeCtx::new()
        };
        let doc = Arc::new(Mutex::new(DmInboxDoc::default()));
        {
            // A never-covered entry with an empty clock: the first sweep
            // lazy-stamps it (fo growth → sidecar delta, no entry change).
            let (key, entry) = make_entry([3; 16], [3; 32], now_ms - 1_000, &[]);
            doc.lock().await.entries.insert(key, entry);
        }
        let persist_calls = Arc::new(AtomicUsize::new(0));
        let fail_first = Arc::new(AtomicUsize::new(1));
        let persist_now: PersistNowFn = {
            let calls = Arc::clone(&persist_calls);
            let fail = Arc::clone(&fail_first);
            Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                let should_fail = fail.swap(0, Ordering::SeqCst) == 1;
                Box::pin(async move {
                    if should_fail {
                        Err(crate::fleet_sync::SyncError::Persist("disk full".into()))
                    } else {
                        Ok(())
                    }
                })
            })
        };
        let mut pending = false;
        sweep_once(&doc, &ctx, &|| {}, &persist_now, &mut pending).await;
        assert_eq!(persist_calls.load(Ordering::SeqCst), 1);
        assert!(pending, "failed persist arms the latch");
        // Second sweep: nothing new (entry already stamped, ingest is a
        // no-op) — the latch alone must drive the retry.
        sweep_once(&doc, &ctx, &|| {}, &persist_now, &mut pending).await;
        assert_eq!(persist_calls.load(Ordering::SeqCst), 2, "latch retried");
        assert!(!pending, "successful retry clears the latch");
    }
```

Note: the second test's first sweep must not ingest (a `ProbeCtx::new()` with no CAS/verify data rejects the entry and leaves `changed == false` — same shape `gc_removes_when_ig_covers_enrolled_set_or_ttl` relies on). If `ingest_pending` flags `changed` for the probe entry, give the entry an `ingested_by` already containing `SELF_ID` (self-ack short-circuits ingestion) while keeping it un-covered (enrolled set = self + sibling).

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(sidecar) or test(tombstone_delta)'`
Expected: compile FAIL — `sweep_once` arity.

- [ ] **Step 3: Implement**

`sweep_once` (replaces the `(changed, fo_grew)` shape):

```rust
async fn sweep_once(
    doc: &Arc<Mutex<DmInboxDoc>>,
    ctx: &dyn DmInboxIngestCtx,
    notify_dirty: &(dyn Fn() + Send + Sync),
    persist_now: &PersistNowFn,
    sidecar_persist_pending: &mut bool,
) {
    let (changed, sidecars_changed) = {
        let mut guard = doc.lock().await;
        let fo_before = guard.first_observed_ms().len();
        let tomb_before = guard.expired_at_ms().len();
        let changed = ingest_pending(&mut guard, ctx).await;
        // When `changed` is false the fo side-map cannot shrink (no entry
        // was removed, so its live-key prune removes nothing) — a length
        // change means `gc_expired` lazily stamped a newly-seen entry. The
        // tombstone map CAN shrink on an otherwise no-op sweep (ZEB-925
        // retention age-out), so both deltas mark the sidecars dirty.
        let sidecars_changed = guard.first_observed_ms().len() != fo_before
            || guard.expired_at_ms().len() != tomb_before;
        (changed, sidecars_changed)
    };
    if changed {
        // `notify_dirty` schedules a debounced publish + persist, which also
        // captures sidecar mutations from this sweep (DmInboxPersist writes
        // every file). Its failure/retry semantics are the engine's dirty
        // latch — not cleared or set here.
        notify_dirty();
    } else if sidecars_changed || *sidecar_persist_pending {
        // ZEB-862 stamp-only / ZEB-925 tombstone-delta sweep: persist the
        // LOCAL sidecars without a fleet republish. On failure, latch so the
        // next sweep retries even if nothing else changes (the deltas are
        // already in memory and will never re-fire on their own).
        match persist_now().await {
            Ok(()) => *sidecar_persist_pending = false,
            Err(e) => {
                *sidecar_persist_pending = true;
                tracing::warn!(error = %e,
                    "dm-inbox sidecar persist_now failed; will retry next sweep");
            }
        }
    }
}
```

`run_dm_inbox_ingest_sweeper`: add `let mut sidecar_persist_pending = false;` before the startup sweep; pass `&mut sidecar_persist_pending` at both `sweep_once` call sites.

`lib.rs` boot block (inside the `dm_inbox_doc` construction, BEFORE `restore_first_observed`):

```rust
                            // ZEB-925: restore expiry tombstones FIRST — the
                            // tombstone wins over a stale doc file, and
                            // restore_first_observed's orphan-prune then drops
                            // the removed entries' stamps.
                            doc.restore_expired(
                                crate::dm_inbox_persist::load_expired_or_recover(
                                    &dm_inbox_expired_path,
                                )
                                .map_err(|e| format!("load dm-inbox expired: {e}"))?,
                                now_ms,
                            );
```

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(dm_inbox) or test(sidecar) or test(sweep)'`
Expected: PASS (incl. the existing `run_dm_inbox_ingest_sweeper` test).

- [ ] **Step 5: Commit** — `feat(dm-inbox): ZEB-925 Task 3 — boot restore + sweeper sidecar retry latch`

---

### Task 4: Acceptance clears the tombstone

**Files:**
- Modify: `src-tauri/src/iroh_butler_acceptor.rs` (`ProdButlerDepositCtx::persist_entry` + unit test)
- Modify: `src-tauri/tests/library/butler_deposit_integration.rs` (`TestButlerCtx::persist_entry` — keep the "verbatim" copy verbatim)

**Interfaces:**
- Consumes: `DmInboxDoc::clear_tombstone` (Task 1).

- [ ] **Step 1: Write the failing test** (in `iroh_butler_acceptor.rs` `mod tests` — against the PRODUCTION impl, engine construction cribbed from `dm_inbox_ingest.rs`'s `local_deposit_write_publishes` shape)

```rust
    /// ZEB-925 (spec §2f): accepting a deposit clears the key's expiry
    /// tombstone — on Inserted (fresh acceptance) and Duplicate (defensive
    /// heal) — while CapExceeded leaves suppression intact. Without the
    /// clear, the persisted (live entry + stale tombstone) pair is deleted
    /// by restore_expired at the next boot AFTER the butler acked it.
    #[tokio::test]
    async fn deposit_acceptance_clears_expiry_tombstone() {
        use crate::dm_inbox_persist::DmInboxPersist;
        use crate::fleet_sync::{FleetSyncConfig, FleetSyncEngine, Merger, DEFAULT_DEBOUNCE_MS};
        use crate::owner_state_crypto::KeyTree;
        use tokio::sync::{mpsc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let kt = Arc::new(KeyTree::derive(&[0x55u8; 32]).expect("derive kt"));
        let doc = Arc::new(Mutex::new(DmInboxDoc::default()));
        let tracker = Arc::new(Mutex::new(harmony_crdt_sync::ReplayTracker::new(
            "dev-A".to_string(),
        )));
        let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>(64);
        let (_in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
        let merger: Merger<DmInboxDoc> = Arc::new(|local, remote| local.merge_from(remote));
        let (nudge_tx, _nudge_rx) = mpsc::channel::<()>(1);
        let engine = Arc::new(FleetSyncEngine::<DmInboxDoc>::new(FleetSyncConfig {
            keys: Some(crate::owner_state_crypto::FleetKeySet::new(kt)),
            device_id: "dev-A".to_string(),
            state: Arc::clone(&doc),
            merger,
            replay_tracker: Arc::clone(&tracker),
            content_store: Arc::new(crate::content_store::InMemoryStub::default()),
            publisher_tx: out_tx,
            subscriber_rx: in_rx,
            persist: Arc::new(DmInboxPersist {
                doc_path: dir.path().join("dm_inbox.cbor"),
                replay_path: dir.path().join("dm_inbox_replay.cbor"),
                first_observed_path: dir.path().join("dm_inbox_first_observed.cbor"),
                expired_path: dir.path().join("dm_inbox_expired.cbor"),
            }),
            lookup_key_tag: b"dm-inbox-v1",
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            publish_seen: true,
            on_applied: None,
            sibling_acks: Arc::new(Mutex::new(harmony_crdt_sync::MonotoneMap::new())),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
        }));
        let ctx = ProdButlerDepositCtx {
            self_owner: [0x01; 16],
            device_id: "dev-A".to_string(),
            crdt_state: Arc::new(Mutex::new(crate::owner_state_crdt::OwnerState::default())),
            device_x25519_priv: zeroize::Zeroizing::new([0u8; 32]),
            dm_inbox_doc: Arc::clone(&doc),
            dm_inbox_tracker: Arc::clone(&tracker),
            adopt_floor: crate::hlc_adopt_floor::HlcAdoptFloor::new(),
            dm_inbox_engine: Arc::clone(&engine),
            ingest_nudge: nudge_tx.downgrade(),
        };

        let key = DmInboxDoc::key(&[0xAB; 16], &[0xCD; 32]);
        // (1) Inserted arm: tombstoned key redeposited → accepted, tombstone gone.
        doc.lock().await.restore_expired(
            [(key.clone(), 5u64)].into_iter().collect(),
            10,
        );
        let e = filler_entry([0x22; 16]);
        assert_eq!(
            ctx.persist_entry(key.clone(), e.clone()).await.unwrap(),
            DepositPersistVerdict::Inserted
        );
        {
            let guard = doc.lock().await;
            assert!(guard.entries.contains_key(&key), "entry accepted");
            assert!(
                !guard.expired_at_ms().contains_key(&key),
                "Inserted clears the tombstone"
            );
        }
        // (2) Duplicate arm: re-seed an inconsistent (live + tombstoned) pair
        //     — pre-fix disk state — a redelivery must heal it.
        doc.lock()
            .await
            .restore_expired([(key.clone(), 5u64)].into_iter().collect(), 10);
        // restore_expired removed the entry (tombstone wins); re-insert to
        // model the inconsistent pair, then clear via the Duplicate arm.
        doc.lock().await.entries.insert(key.clone(), e.clone());
        {
            // Seed the tombstone WITHOUT removing the entry this time.
            let mut guard = doc.lock().await;
            let with_tomb: std::collections::BTreeMap<String, u64> =
                [(key.clone(), 5u64)].into_iter().collect();
            // restore_expired would delete the entry; emulate the stale pair
            // through the public surface: tombstone map restored first, entry
            // re-inserted after.
            guard.restore_expired(with_tomb, 10);
            guard.entries.insert(key.clone(), e.clone());
            assert!(guard.expired_at_ms().contains_key(&key));
        }
        assert_eq!(
            ctx.persist_entry(key.clone(), e.clone()).await.unwrap(),
            DepositPersistVerdict::Duplicate
        );
        assert!(
            !doc.lock().await.expired_at_ms().contains_key(&key),
            "Duplicate heals the stale tombstone"
        );
        // (3) CapExceeded arm: a DIFFERENT tombstoned key rejected at a full
        //     inbox keeps its tombstone.
        let key_full = DmInboxDoc::key(&[0xEE; 16], &[0xEF; 32]);
        {
            let mut guard = doc.lock().await;
            guard.restore_expired([(key_full.clone(), 5u64)].into_iter().collect(), 10);
            // Fill the global cap with distinct senders (per-sender cap is
            // per-owner; vary the owner byte).
            for i in 0..INBOX_GLOBAL_CAP {
                let mut filler = filler_entry([(i % 251) as u8 + 1; 16]);
                filler.sender_owner[15] = (i / 251) as u8;
                guard
                    .entries
                    .insert(format!("fill:{i:05}"), filler);
            }
        }
        assert_eq!(
            ctx.persist_entry(key_full.clone(), filler_entry([0x33; 16]))
                .await
                .unwrap(),
            DepositPersistVerdict::CapExceeded
        );
        assert!(
            doc.lock().await.expired_at_ms().contains_key(&key_full),
            "CapExceeded must NOT weaken suppression"
        );
        let _ = Arc::try_unwrap(engine).map(|e| async move { e.shutdown().await });
    }
```

(Adjust the cap-fill to the actual `filler_entry` signature in that module; the invariant under test is the three-arm clear/keep behavior, and per-sender cap must not fire before the global cap.)

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(deposit_acceptance_clears)'`
Expected: FAIL — tombstone still present after Inserted (or compile fail if helpers drift; fix the harness, keep the assertions).

- [ ] **Step 3: Implement** — `ProdButlerDepositCtx::persist_entry`:

Duplicate arm (after the ZEB-483 invite-heal block, before the verdict):

```rust
                // ZEB-925 (spec §2f): defensively clear any expiry tombstone —
                // a live entry must never coexist with its own tombstone
                // (restore_expired would delete the acked entry at the next
                // boot). Heals pre-ZEB-925 inconsistent disk state.
                doc.clear_tombstone(&key);
                DepositPersistVerdict::Duplicate
```

Inserted arm:

```rust
                // ZEB-925 (spec §2f): acceptance is a fresh local decision to
                // hold — forget the key's expiry memory so restore_expired
                // cannot delete the acked entry at the next boot. The
                // CapExceeded return above stays un-cleared: a rejected
                // deposit must not weaken suppression.
                doc.clear_tombstone(&key);
                doc.entries.insert(key, entry);
                DepositPersistVerdict::Inserted
```

`tests/library/butler_deposit_integration.rs` `TestButlerCtx::persist_entry` ("ProdButlerDepositCtx::persist_entry verbatim"): add the same two `doc.clear_tombstone(&key);` lines in the same arms so the copy stays verbatim.

- [ ] **Step 4: Run to verify pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(deposit) or test(dm_inbox)'`
Expected: PASS.

- [ ] **Step 5: Commit** — `feat(dm-inbox): ZEB-925 Task 4 — deposit acceptance clears the expiry tombstone`

---

### Final gates (before PR)

- [ ] `cd src-tauri && cargo fmt --all` then `cargo fmt --all -- --check`
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] Full sweep: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] `git status` clean (local gates run the working tree, not the commit)
- [ ] Push branch, open PR (`Closes ZEB-925`), fire `@coderabbitai review` ONCE.
