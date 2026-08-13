# ZEB-924: Bounded Relay-Hold Retention Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Suppress resurrection-by-merge of TTL-expired never-acked relay holds with a bounded, restart-durable, local-only tombstone set, so a hold's lifetime on a replica is ≤ first-observation + TTL + one sweep interval.

**Architecture:** A `#[serde(skip)]` `expired_at_ms: BTreeMap<String, u64>` on `RelayHoldDoc` (mirroring the ZEB-862 `first_observed_ms` posture): `gc()` tombstones TTL-removed keys and bounds the set (2×TTL age-out, 4096 cap, oldest-first eviction); `merge_from()` suppresses re-insert of tombstoned keys; a new `relay_hold_expired.cbor` sidecar makes it restart-durable, with tombstones winning over a stale doc file at boot. No wire/canonical-bytes change; deposits stay ungated.

**Tech Stack:** Rust, serde/ciborium CBOR sidecars, cargo nextest.

**Spec:** `docs/superpowers/specs/2026-08-12-zeb924-relay-hold-tombstone-retention-design.md`

## Global Constraints

- Cargo commands run from `src-tauri/`; always `--locked --features test-fixtures`; clippy `--all-targets --no-deps -- -D warnings`; `cargo fmt --all` before each commit.
- Canonical wire bytes of `RelayHoldDoc` MUST NOT change (`#[serde(skip)]` only).
- `PartialEq for RelayHoldDoc` stays entries-only (already manual).
- Existing `gc_*` / `restore_*` / merge-convergence test families stay green.
- Constants: `RELAY_HOLD_TOMBSTONE_RETENTION_MS = 2 * RELAY_HOLD_TTL_MS`, `RELAY_HOLD_TOMBSTONE_CAP = 4 * RELAY_HOLD_GLOBAL_CAP`, plus `const _: () = assert!(RELAY_HOLD_TOMBSTONE_CAP >= RELAY_HOLD_GLOBAL_CAP);`.

---

### Task 1: CRDT core — constants, gc tombstoning, merge suppression, restore

**Files:**
- Modify: `src-tauri/src/community_relay.rs` (constants, after `RELAY_HOLD_GC_INTERVAL_MS` ~:138)
- Modify: `src-tauri/src/community_relay_hold_crdt.rs` (field, `gc`, `merge_from`, accessors, tests)

**Interfaces:**
- Produces: `RelayHoldDoc::expired_at_ms(&self) -> &BTreeMap<String, u64>`; `RelayHoldDoc::restore_expired(&mut self, map: BTreeMap<String, u64>, now_ms: u64)`; constants `RELAY_HOLD_TOMBSTONE_RETENTION_MS: u64`, `RELAY_HOLD_TOMBSTONE_CAP: usize` (pub in `community_relay`).

- [ ] **Step 1: Write the failing tests** (append to `community_relay_hold_crdt.rs` tests module, in a `// ZEB-924` section):

```rust
    // ----------------------------------------------------------------
    // ZEB-924: local expiry tombstones vs resurrection-by-merge
    // ----------------------------------------------------------------

    #[test]
    fn gc_ttl_expiry_tombstones_the_key_but_coverage_removal_does_not() {
        let mut doc = RelayHoldDoc::default();
        let ttl_key = key_rr(1, 1);
        let cov_key = key_rr(2, 2);
        doc.entries.insert(
            ttl_key.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.entries.insert(
            cov_key.clone(),
            entry([2; 16], [9; 16], space(3), hlc(1, "a"), "relay", &["dev-1"]),
        );
        assert!(doc.gc(1_000), "covered-at-start entry removed immediately");
        assert!(
            doc.expired_at_ms().is_empty(),
            "coverage removal must NOT tombstone (fleet-deterministic already)"
        );
        // TTL-expire the survivor: stamped at 1_000, sweep past TTL.
        let later = 1_000 + RELAY_HOLD_TTL_MS + 1;
        assert!(doc.gc(later), "TTL removal");
        assert_eq!(
            doc.expired_at_ms().get(&ttl_key),
            Some(&later),
            "TTL removal records a tombstone at the sweep time"
        );
    }

    #[test]
    fn merge_suppresses_resurrection_of_tombstoned_key() {
        let mut doc = RelayHoldDoc::default();
        let k = key_rr(1, 1);
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.gc(1_000); // stamp
        doc.gc(1_000 + RELAY_HOLD_TTL_MS + 1); // expire + tombstone
        assert!(doc.entries.is_empty());

        // A still-holding peer's doc re-offers the entry.
        let mut remote = RelayHoldDoc::default();
        remote.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let out = doc.merge_from(remote);
        assert!(!out.changed, "suppressed resurrection is silent (no flush churn)");
        assert!(doc.entries.is_empty(), "tombstoned key never re-enters entries");
    }

    #[test]
    fn resurrection_lifetime_bound_across_merge_traffic() {
        // Acceptance pin: interleave merges from a still-holding peer with
        // sweeps — after the first TTL expiry the key never re-enters.
        let k = key_rr(1, 1);
        let mut peer = RelayHoldDoc::default();
        peer.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let mut doc = RelayHoldDoc::default();
        doc.merge_from(peer.clone());
        doc.gc(0); // first observation stamp at 0
        let expiry_sweep = RELAY_HOLD_TTL_MS + 1;
        assert!(doc.gc(expiry_sweep), "expires at TTL");
        for i in 1..=5u64 {
            let now = expiry_sweep + i * 600_000; // merge every sweep interval
            let out = doc.merge_from(peer.clone());
            assert!(!out.changed, "merge round {i} suppressed");
            assert!(!doc.gc(now), "nothing to remove in round {i}");
            assert!(doc.entries.is_empty(), "never re-enters (round {i})");
        }
    }

    #[test]
    fn covered_resurrection_still_converges_without_tombstone() {
        // Existing semantics pinned: a resurrected COVERED entry carries its
        // pulled_by and is deterministically re-removed on the next sweep.
        let k = key_rr(1, 1);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &["dev-1"]),
        );
        assert!(doc.gc(1_000), "covered-at-start → removed, no tombstone");
        let mut peer = RelayHoldDoc::default();
        peer.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &["dev-1"]),
        );
        let out = doc.merge_from(peer);
        assert!(out.changed, "covered entry MAY resurrect (no tombstone)");
        assert!(doc.gc(2_000), "and is deterministically re-removed next sweep");
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn tombstone_ages_out_after_retention_and_reopens() {
        use crate::community_relay::RELAY_HOLD_TOMBSTONE_RETENTION_MS;
        let k = key_rr(1, 1);
        let mut doc = RelayHoldDoc::default();
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.gc(0);
        let expiry = RELAY_HOLD_TTL_MS + 1;
        assert!(doc.gc(expiry));
        assert!(doc.expired_at_ms().contains_key(&k));
        // Past retention the tombstone is dropped…
        assert!(!doc.gc(expiry + RELAY_HOLD_TOMBSTONE_RETENTION_MS + 1));
        assert!(doc.expired_at_ms().is_empty(), "tombstone aged out");
        // …and a pathological late holder re-arms ONE more TTL window
        // (documented bounded harm, spec §6).
        let mut peer = RelayHoldDoc::default();
        peer.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        assert!(doc.merge_from(peer).changed, "post-retention resurrection re-inserts");
    }

    #[test]
    fn tombstone_cap_evicts_oldest_first() {
        use crate::community_relay::RELAY_HOLD_TOMBSTONE_CAP;
        let mut doc = RelayHoldDoc::default();
        // Overfill via restore (unit seam), then run gc to enforce the cap.
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        for i in 0..(RELAY_HOLD_TOMBSTONE_CAP + 2) {
            // Distinct keys; stamps strictly increasing so "oldest" is i=0,1.
            let content: [u8; 32] = {
                let mut c = [0u8; 32];
                c[0] = (i / 256) as u8;
                c[1] = (i % 256) as u8;
                c
            };
            m.insert(RelayHoldDoc::key(&[7; 16], &content), 1_000 + i as u64);
        }
        let newest_stamp = 1_000 + (RELAY_HOLD_TOMBSTONE_CAP + 1) as u64;
        doc.restore_expired(m, u64::MAX - 1);
        assert!(!doc.gc(newest_stamp), "cap enforcement removes no entries");
        assert_eq!(doc.expired_at_ms().len(), RELAY_HOLD_TOMBSTONE_CAP);
        assert!(
            !doc.expired_at_ms().values().any(|t| *t < 1_002),
            "the two OLDEST tombstones were evicted"
        );
    }

    #[test]
    fn restore_expired_removes_tombstoned_entries_and_clamps_future_stamps() {
        let k = key_rr(1, 1);
        let live = key_rr(2, 2);
        let mut doc = RelayHoldDoc::default();
        // Stale doc file resurrected an entry this replica already expired…
        doc.entries.insert(
            k.clone(),
            entry([1; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        doc.entries.insert(
            live.clone(),
            entry([2; 16], [9; 16], space(3), hlc(1, "a"), "relay", &[]),
        );
        let now = 1_000_000u64;
        // …and the sidecar carries a FUTURE stamp (backward clock across restart).
        doc.restore_expired([(k.clone(), now + 5_000_000)].into_iter().collect(), now);
        assert!(
            !doc.entries.contains_key(&k),
            "tombstone wins over the stale doc file"
        );
        assert!(doc.entries.contains_key(&live), "non-tombstoned entry untouched");
        assert_eq!(doc.expired_at_ms()[&k], now, "future stamp rebased to now");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(tombstone) or test(resurrection) or test(restore_expired)'`
Expected: FAIL to compile — `expired_at_ms`/`restore_expired`/constants not found.

- [ ] **Step 3: Implement**

`community_relay.rs` (after `RELAY_HOLD_GC_INTERVAL_MS`):

```rust
/// ZEB-924: how long a LOCAL expiry tombstone suppresses resurrection-by-merge
/// of a TTL-expired hold. A still-holding peer expires the same entry within
/// ITS OWN TTL of ITS first observation, so 2× the TTL covers realistic
/// cross-device observation skew; past it, a pathological late holder re-arms
/// at most one more TTL window before being re-tombstoned (bounded harm).
pub const RELAY_HOLD_TOMBSTONE_RETENTION_MS: u64 = 2 * RELAY_HOLD_TTL_MS;

/// ZEB-924: hard cap on the tombstone set so expiry memory cannot itself
/// become unbounded state. Oldest-first eviction (the entry peers have most
/// likely already expired themselves); ~100 B per tombstone keeps the worst
/// case near 400 KB.
pub const RELAY_HOLD_TOMBSTONE_CAP: usize = 4 * RELAY_HOLD_GLOBAL_CAP;

const _: () = assert!(RELAY_HOLD_TOMBSTONE_CAP >= RELAY_HOLD_GLOBAL_CAP);
```

`community_relay_hold_crdt.rs`:

1. Import: extend line 8 to `use crate::community_relay::{RELAY_HOLD_TOMBSTONE_CAP, RELAY_HOLD_TOMBSTONE_RETENTION_MS, RELAY_HOLD_TTL_MS};`
2. Field after `first_observed_ms` (with the doc comment from spec §2a):

```rust
    /// ZEB-924: LOCAL expiry memory — keys this replica TTL-expired, mapped
    /// to the local wall-ms of expiry. Suppresses resurrection-by-merge (a
    /// still-holding peer's anti-entropy re-insert) so a never-acked hold's
    /// lifetime here is bounded by first-observation + TTL + one sweep.
    /// Never serialized (canonical wire bytes unchanged), excluded from
    /// `PartialEq`, restart-durable via a local sidecar
    /// (`relay_hold_persist::save_expired`). Bounded by
    /// `RELAY_HOLD_TOMBSTONE_RETENTION_MS` age-out and
    /// `RELAY_HOLD_TOMBSTONE_CAP` oldest-first eviction in [`Self::gc`].
    #[serde(skip)]
    expired_at_ms: BTreeMap<String, u64>,
```

3. `merge_from` `None =>` arm becomes:

```rust
                None => {
                    // ZEB-924: a key this replica already TTL-expired must not
                    // be resurrected by a still-holding peer's merge — suppress
                    // the insert entirely (no `changed`, no flush churn, and
                    // `held_for` never sees it). Deposits are unaffected: a
                    // fresh send mints a fresh content-id key and
                    // `persist_hold` inserts directly, not through here.
                    if self.expired_at_ms.contains_key(&k) {
                        continue;
                    }
                    changed = true;
                    self.entries.insert(k, r);
                }
```

4. In `gc()`, replace from `let before = self.entries.len();` through the side-map prune with:

```rust
        let before = self.entries.len();
        let first_observed = &self.first_observed_ms;
        // ZEB-924: record WHICH keys the TTL rule removes — they become local
        // tombstones so a peer merge cannot resurrect them. Coverage-only
        // removals are fleet-deterministic (every replica re-removes a covered
        // resurrection) and need no tombstone.
        let mut ttl_removed: Vec<String> = Vec::new();
        self.entries.retain(|key, _e| {
            let observed = first_observed.get(key).copied().unwrap_or(now_ms);
            let ttl_expired = observed.saturating_add(RELAY_HOLD_TTL_MS) < now_ms;
            if ttl_expired {
                ttl_removed.push(key.clone());
            }
            !(ttl_expired || covered_at_start.contains(key))
        });
        for key in ttl_removed {
            self.expired_at_ms.insert(key, now_ms);
        }
        // Age-out + cap so expiry memory cannot itself become unbounded state.
        self.expired_at_ms
            .retain(|_, t| now_ms.saturating_sub(*t) < RELAY_HOLD_TOMBSTONE_RETENTION_MS);
        while self.expired_at_ms.len() > RELAY_HOLD_TOMBSTONE_CAP {
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
        // Prune the side-map for removed keys (bounded with `entries`).
        let live: BTreeSet<String> = self.entries.keys().cloned().collect();
        self.first_observed_ms.retain(|k, _| live.contains(k));
        self.entries.len() != before
```

Also update the `gc()` doc comment's final sentence (the `:176-181` block): replace "so it may persist beyond a single TTL window in a continuously-merging fleet — bounded by the store's caps." with "ZEB-924: expired keys leave a bounded LOCAL tombstone (`expired_at_ms`) that `merge_from` consults, so a resurrection-by-merge is suppressed and lifetime here is bounded by first-observation + TTL + one sweep."

5. Accessors after `restore_first_observed` (doc comments from spec §2d):

```rust
    /// ZEB-924: read the LOCAL expiry tombstones for durable sidecar
    /// persistence. Never leaves this replica and never enters the wire.
    pub fn expired_at_ms(&self) -> &BTreeMap<String, u64> {
        &self.expired_at_ms
    }

    /// ZEB-924: restore the LOCAL expiry tombstones on boot from the sidecar.
    ///
    /// - A stamp GREATER than `now_ms` (a backward local clock step across
    ///   restart) is rebased to `now_ms` (mirrors
    ///   [`Self::restore_first_observed`] Q-1).
    /// - Any restored tombstone key still present in `entries` (a stale doc
    ///   file from a crash between atomic writes resurrected an entry this
    ///   replica already expired) is REMOVED from `entries` — expiry is
    ///   monotone; the tombstone wins.
    ///
    /// Callers MUST load `entries` first and MUST call this BEFORE
    /// [`Self::restore_first_observed`], whose orphan-pruning then drops the
    /// removed entries' stamps (the boot path does).
    pub fn restore_expired(&mut self, mut map: BTreeMap<String, u64>, now_ms: u64) {
        for v in map.values_mut() {
            *v = (*v).min(now_ms);
        }
        self.entries.retain(|k, _| !map.contains_key(k));
        self.expired_at_ms = map;
    }
```

- [ ] **Step 4: Run the new tests + the whole module**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_relay_hold_crdt)'`
Expected: PASS (all new + all pre-existing).

- [ ] **Step 5: Commit** — `feat(relay): ZEB-924 T1 — local expiry tombstones in RelayHoldDoc (gc + merge suppression)`

---

### Task 2: Sidecar persistence for the tombstone map

**Files:**
- Modify: `src-tauri/src/relay_hold_persist.rs` (new section + `RelayHoldPersist` field + tests)
- Modify: `src-tauri/src/lib.rs` (`RelayHoldPersist` construction ~:6539 gains `expired_path` — compile requirement for this task)

**Interfaces:**
- Consumes: `RelayHoldDoc::expired_at_ms()` (Task 1).
- Produces: `RELAY_HOLD_EXPIRED_FILENAME: &str`, `save_expired`, `load_expired`, `load_expired_or_recover` (same signatures as the `first_observed` trio), `RelayHoldPersist { …, expired_path: PathBuf }`.

- [ ] **Step 1: Write the failing tests** (append to `relay_hold_persist.rs` tests):

```rust
    #[test]
    fn expired_round_trips_and_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RELAY_HOLD_EXPIRED_FILENAME);
        assert!(load_expired(&path).unwrap().is_empty(), "missing file → empty");
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        m.insert("k1".into(), 42);
        save_expired(&path, &m).unwrap();
        assert_eq!(load_expired(&path).unwrap(), m);
    }

    #[test]
    fn load_expired_rejects_trailing_bytes_and_recover_quarantines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RELAY_HOLD_EXPIRED_FILENAME);
        let mut m: BTreeMap<String, u64> = BTreeMap::new();
        m.insert("k1".into(), 42);
        save_expired(&path, &m).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0x00);
        std::fs::write(&path, &bytes).unwrap();
        assert!(matches!(
            load_expired(&path).unwrap_err(),
            SyncError::CborDecode(_)
        ));
        assert!(load_expired_or_recover(&path).unwrap().is_empty());
        assert!(!path.exists(), "corrupt file quarantined away");
    }

    #[test]
    fn persist_writes_expired_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let p = RelayHoldPersist {
            doc_path: dir.path().join("relay_hold.cbor"),
            replay_path: dir.path().join("relay_hold_replay.cbor"),
            first_observed_path: dir.path().join("relay_hold_first_observed.cbor"),
            expired_path: dir.path().join(RELAY_HOLD_EXPIRED_FILENAME),
        };
        let mut doc = RelayHoldDoc::default();
        let m: BTreeMap<String, u64> = [("gone-key".to_string(), 7u64)].into_iter().collect();
        doc.restore_expired(m.clone(), u64::MAX);
        use crate::fleet_sync::FleetPersist;
        p.persist(&doc, &BTreeMap::new()).unwrap();
        assert_eq!(load_expired(&p.expired_path).unwrap(), m);
    }
```

(Adjust the existing `RelayHoldPersist` construction in `roundtrips_all_three_files`-style tests to add `expired_path` so the crate compiles.)

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(relay_hold_persist)'`
Expected: FAIL to compile — `RELAY_HOLD_EXPIRED_FILENAME`/`save_expired` not found.

- [ ] **Step 3: Implement** — in `relay_hold_persist.rs`, after the first-observed section, add the ZEB-924 section mirroring it byte-for-byte (filename `relay_hold_expired.cbor`, `RELAY_HOLD_EXPIRED_SCHEMA_V1: u8 = 1`, tuple-struct `RelayHoldExpiredFileV1(BTreeMap<String, u64>)`, `load_expired` with the same NotFound→empty / empty-file error / version check / trailing-bytes rejection, `load_expired_or_recover` with the same quarantine contract, `save_expired` atomic write). Add `pub expired_path: std::path::PathBuf` to `RelayHoldPersist` (doc comment: `/// ZEB-924: local-only expiry-tombstone sidecar (see RELAY_HOLD_EXPIRED_FILENAME).`) and `save_expired(&self.expired_path, state.expired_at_ms())?;` after the first-observed save in `persist`. In `lib.rs` ~:6539, add `expired_path: relay_hold_expired_path.clone()` — declare `let relay_hold_expired_path = identity_dir.join(crate::relay_hold_persist::RELAY_HOLD_EXPIRED_FILENAME);` beside the other path declarations (~:6489); the boot-restore call itself is Task 3.

- [ ] **Step 4: Run** — same filter as Step 2. Expected: PASS. Also `cargo check --locked --features test-fixtures` (lib compiles with the new field).

- [ ] **Step 5: Commit** — `feat(relay): ZEB-924 T2 — relay_hold_expired.cbor sidecar (save/load/quarantine + persist wiring)`

---

### Task 3: Boot restore + GC sweep persistence detection

**Files:**
- Modify: `src-tauri/src/lib.rs` (boot block ~:6494-6510; GC task ~:12824-12854)

**Interfaces:**
- Consumes: `restore_expired`, `load_expired_or_recover`, `expired_at_ms()` (Tasks 1-2).

- [ ] **Step 1: Boot restore.** In the `relay_hold_doc` construction block, insert BEFORE the existing `doc.restore_first_observed(...)` call:

```rust
                            // ZEB-924: restore expiry tombstones BEFORE the
                            // first-observed clock — restore_expired removes any
                            // stale-doc-resurrected entries, and
                            // restore_first_observed's orphan-pruning then drops
                            // their stamps.
                            doc.restore_expired(
                                crate::relay_hold_persist::load_expired_or_recover(
                                    &relay_hold_expired_path,
                                )
                                .map_err(|e| format!("load relay-hold expired: {e}"))?,
                                now_ms,
                            );
```

- [ ] **Step 2: Sweep detection.** Replace the `(changed, fo_grew)` computation with:

```rust
                                    let (changed, sidecars_changed) = {
                                        let mut d = gc_doc.lock().await;
                                        let fo_before = d.first_observed_ms().len();
                                        let tomb_before = d.expired_at_ms().len();
                                        let changed = d.gc(now_ms);
                                        (
                                            changed,
                                            d.first_observed_ms().len() != fo_before
                                                || d.expired_at_ms().len() != tomb_before,
                                        )
                                    };
```

and rename `fo_grew` → `sidecars_changed` in the `else if`, extending the ZEB-862 comment: "ZEB-924: same for tombstone age-out/eviction shrinkage — tombstone ADDS always ride a `changed = true` sweep (they coincide with entry removal), so the length delta only needs to catch stamp growth and tombstone shrinkage."

- [ ] **Step 3: Gates.** `cd src-tauri && cargo check --locked --features test-fixtures` then `scripts/test-select --context task` (from repo root). Expected: PASS.

- [ ] **Step 4: Commit** — `feat(relay): ZEB-924 T3 — boot tombstone restore + sweep sidecar-change persistence`

---

### Task 4: Spec amendment + full gates

**Files:**
- Modify: `docs/specs/2026-06-13-zeb-458-community-sealed-relay-design.md` (~:161, after the TTL bullet)

- [ ] **Step 1: Amend the ZEB-458 design doc** — after the `**TTL** = 30 days` bullet, add:

```markdown
- **ZEB-924 (2026-08-12):** TTL expiry now leaves a bounded LOCAL tombstone
  (`expired_at_ms`: 2×TTL retention, cap 4×`RELAY_HOLD_GLOBAL_CAP`,
  `relay_hold_expired.cbor` sidecar) that suppresses resurrection-by-merge
  from a still-holding peer, so a never-acked hold's lifetime on a replica is
  bounded by first-observation + TTL + one sweep. Coverage GC is unchanged
  (fleet-deterministic, needs no tombstones) and deposits are ungated (a fresh
  send mints a fresh content-id key). See
  `docs/superpowers/specs/2026-08-12-zeb924-relay-hold-tombstone-retention-design.md`.
```

- [ ] **Step 2: Full pre-PR gates** (working tree committed + clean first):

```bash
cd src-tauri && cargo fmt --all -- --check \
  && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings \
  && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean, clippy clean, full sweep green.

- [ ] **Step 3: Commit** — `docs(specs): ZEB-924 — amend ZEB-458 relay design with tombstone retention` — then push branch, open PR.

## Self-Review Notes

- Spec §2a-§2g each map to a task: 2a/2b/2c/2g → Task 1; 2d → Tasks 2-3; 2e → Task 3; 2f is a no-change decision pinned by the Task 1 merge test comment.
- Type consistency: `expired_at_ms()` / `restore_expired(map, now_ms)` used identically in Tasks 1-3; `expired_path` field name consistent across Task 2 code and tests.
- T5 cap test uses `restore_expired` as the seam to overfill (no `Date::now` in tests; all clocks explicit).
