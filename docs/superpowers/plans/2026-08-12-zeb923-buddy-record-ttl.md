# ZEB-923 Buddy-Pact Record TTL Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Received `pledge_lists`/`backup_sets` decay after `STORAGE_RECORD_TTL_MS` (3 days) of non-renewal; renewal is a publisher-side hourly re-mint republish; decay flows through the existing 30 s reconciliation.

**Architecture:** Receiver: TTL sweep keyed on the (newly persisted) local `received_at_ms`, with a boot-grace floor at load. Publisher: the hosting-report task becomes a unified storage-record publisher that re-mints + re-signs + republishes non-empty pledge/backup records hourly via new component-taking `_with` builders sharing the NodeState mint clocks (converted to `Arc<AtomicU64>`).

**Tech Stack:** Rust (src-tauri), cargo nextest, existing `storage_records`/`storage_signing`/`buddy_pin_planner` modules.

Spec: `docs/superpowers/specs/2026-08-12-zeb923-buddy-record-ttl-design.md` (main `1128034c`).

## Global Constraints

- Cargo commands run from `src-tauri/`; always `--locked --features test-fixtures`; clippy adds `--all-targets --no-deps -- -D warnings`.
- Constants (spec §2): `STORAGE_RECORD_REFRESH_INTERVAL_MS = 3_600_000`, `STORAGE_RECORD_TTL_MS = 259_200_000` (3 days), `STORAGE_RECORD_BOOT_GRACE_MS = 43_200_000` (12 h).
- Sweep boundary mirrors `sweep_hosting`: `now_ms.saturating_sub(stamp) < TTL` retains (exactly-at-bound drops).
- `storage_records.json` file version stays 1; the new disk field is `#[serde(default)]` additive.
- Behavior pins that must stay green: `local_clock_rollback_keeps_established`, `hosting_sweep_drops_stale_reports`, all LWW/cap/eviction/v2 tests, planner suite, `pledges_and_backup_sets_survive_disk_reload_hosting_does_not` (extended, not weakened).
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D`.

---

### Task 1: Receiver core — TTL constants, sweep, boot grace, stamp persistence

**Files:**
- Modify: `src-tauri/src/storage_records.rs` (constants ~:61, on-disk structs :139-151, load :247-294, save :355-374, new methods after `sweep_hosting` ~:726, doc comments :97-100/:112-115, tests mod)

**Interfaces:**
- Produces: `pub const STORAGE_RECORD_REFRESH_INTERVAL_MS: u64`, `pub const STORAGE_RECORD_TTL_MS: u64`, `pub const STORAGE_RECORD_BOOT_GRACE_MS: u64`; `pub fn sweep_stale_pledges_and_backups(&mut self, now_ms: u64) -> bool` (save-on-change, like `purge_revoked`); `pub fn apply_boot_grace(&mut self, now_ms: u64)` (raise-only, RAM-only).

- [ ] **Step 1: Write the failing tests** (in `mod tests`, after `hosting_sweep_drops_stale_reports`; the helpers `test_identity`/`addr_of`/`signed_pledge_bytes`/`signed_backup_bytes`/`pledge`/`public_durable_cid_hex`/`rvk` already exist):

```rust
#[test]
fn record_ttl_sweep_boundary_is_strict_and_leaves_hosting_alone() {
    let mut store = StorageRecordStore::new(None);
    let id = test_identity();
    let owner = addr_of(&id);
    let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("someone", 5)], 10);
    assert!(store.on_pledge_list_sample(&topic, &bytes, &rvk(), 1_000).changed());
    let (topic, bytes) = signed_backup_bytes(&id, vec![], 10);
    assert!(store.on_backup_set_sample(&topic, &bytes, &rvk(), 1_000).changed());
    let (topic, bytes) = signed_hosting_bytes(&id, vec![], 10);
    assert!(store.on_hosting_report_sample(&topic, &bytes, &rvk(), 1_000).changed());

    assert!(
        !store.sweep_stale_pledges_and_backups(1_000 + STORAGE_RECORD_TTL_MS - 1),
        "fresh records: no change"
    );
    assert!(store.pledge_list(&owner).is_some(), "TTL-1 pledge kept");
    assert!(store.backup_set(&owner).is_some(), "TTL-1 backup set kept");

    assert!(store.sweep_stale_pledges_and_backups(1_000 + STORAGE_RECORD_TTL_MS));
    assert!(store.pledge_list(&owner).is_none(), "exactly-at-TTL pledge dropped");
    assert!(store.backup_set(&owner).is_none(), "exactly-at-TTL backup set dropped");
    assert!(
        store.hosting_report(&owner).is_some(),
        "the record TTL sweep must not touch hosting reports"
    );
}

#[test]
fn record_ttl_renewed_record_survives_the_sweep_that_kills_its_cohort() {
    let mut store = StorageRecordStore::new(None);
    let alice = test_identity();
    let bob = test_identity();
    let (topic, bytes) = signed_pledge_bytes(&alice, vec![pledge("x", 1)], 10);
    assert!(store.on_pledge_list_sample(&topic, &bytes, &rvk(), 1_000).changed());
    let (topic, bytes) = signed_pledge_bytes(&bob, vec![pledge("x", 1)], 10);
    assert!(store.on_pledge_list_sample(&topic, &bytes, &rvk(), 1_000).changed());
    // Alice renews: strictly-newer updated_at ⇒ UpdatedNewer ⇒ fresh stamp.
    let (topic, bytes) = signed_pledge_bytes(&alice, vec![pledge("x", 1)], 11);
    assert_eq!(
        store.on_pledge_list_sample(&topic, &bytes, &rvk(), 2_000),
        RecordOutcome::UpdatedNewer
    );
    assert!(store.sweep_stale_pledges_and_backups(1_000 + STORAGE_RECORD_TTL_MS));
    assert!(store.pledge_list(&addr_of(&alice)).is_some(), "renewed survives");
    assert!(store.pledge_list(&addr_of(&bob)).is_none(), "non-renewed cohort decays");
}

#[test]
fn record_ttl_sweep_persists_so_expired_records_stay_gone_after_reload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("storage_records.json");
    let id = test_identity();
    let owner = addr_of(&id);
    {
        let mut store = StorageRecordStore::new(Some(path.clone()));
        let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("someone", 5)], 10);
        assert!(store.on_pledge_list_sample(&topic, &bytes, &rvk(), 1_000).changed());
        assert!(store.sweep_stale_pledges_and_backups(1_000 + STORAGE_RECORD_TTL_MS));
    }
    let reloaded = StorageRecordStore::new(Some(path));
    assert!(
        reloaded.pledge_list(&owner).is_none(),
        "sweep must save() — an expired record must not resurrect at reload"
    );
}

#[test]
fn received_at_ms_round_trips_disk_and_legacy_files_default_to_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("storage_records.json");
    let id = test_identity();
    let owner = addr_of(&id);
    {
        let mut store = StorageRecordStore::new(Some(path.clone()));
        let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("someone", 7)], 10);
        assert!(store.on_pledge_list_sample(&topic, &bytes, &rvk(), 9_000).changed());
        let (topic, bytes) = signed_backup_bytes(&id, vec![], 10);
        assert!(store.on_backup_set_sample(&topic, &bytes, &rvk(), 9_500).changed());
    }
    let reloaded = StorageRecordStore::new(Some(path));
    assert_eq!(reloaded.pledge_lists.get(&owner).unwrap().received_at_ms, 9_000);
    assert_eq!(reloaded.backup_sets.get(&owner).unwrap().received_at_ms, 9_500);

    // Legacy file (no receivedAtMs field) loads with 0 (⇒ boot-grace floor).
    let legacy = dir.path().join("legacy.json");
    std::fs::write(
        &legacy,
        format!(
            r#"{{"version":1,"pledgeLists":[{{"owner":"{owner}","pledges":[],"updatedAt":1}}],"backupSets":[],"signerPins":[]}}"#
        ),
    )
    .unwrap();
    let store = StorageRecordStore::new(Some(legacy));
    assert_eq!(store.pledge_lists.get(&owner).unwrap().received_at_ms, 0);
}

#[test]
fn apply_boot_grace_floors_stale_stamps_and_leaves_fresh_ones() {
    let mut store = StorageRecordStore::new(None);
    let old = test_identity();
    let fresh = test_identity();
    let now = 10 * STORAGE_RECORD_TTL_MS;
    let (topic, bytes) = signed_pledge_bytes(&old, vec![pledge("x", 1)], 10);
    assert!(store.on_pledge_list_sample(&topic, &bytes, &rvk(), 0).changed());
    let (topic, bytes) = signed_pledge_bytes(&fresh, vec![pledge("x", 1)], 10);
    assert!(store
        .on_pledge_list_sample(&topic, &bytes, &rvk(), now - 1_000)
        .changed());

    store.apply_boot_grace(now);
    let floor = now - (STORAGE_RECORD_TTL_MS - STORAGE_RECORD_BOOT_GRACE_MS);
    assert_eq!(
        store.pledge_lists.get(&addr_of(&old)).unwrap().received_at_ms,
        floor,
        "ancient stamp raised to the grace floor"
    );
    assert_eq!(
        store.pledge_lists.get(&addr_of(&fresh)).unwrap().received_at_ms,
        now - 1_000,
        "fresh stamp untouched (raise-only)"
    );

    // Small test clocks saturate to a no-op floor of 0.
    let mut small = StorageRecordStore::new(None);
    let (topic, bytes) = signed_pledge_bytes(&old, vec![pledge("x", 1)], 11);
    assert!(small.on_pledge_list_sample(&topic, &bytes, &rvk(), 9_000).changed());
    small.apply_boot_grace(9_000);
    assert_eq!(small.pledge_lists.get(&addr_of(&old)).unwrap().received_at_ms, 9_000);
}

#[test]
fn record_ttl_expiry_unfreezes_the_owner_cap() {
    let mut store = StorageRecordStore::new(None);
    // Fill to cap with direct rows (established working set), all stale.
    for i in 0..MAX_TRACKED_OWNERS {
        let seq = store.next_insert_seq();
        store.pledge_lists.insert(
            format!("owner-{i:04}"),
            PledgeListRecord {
                pledges: vec![],
                updated_at: 1,
                received_at_ms: 1_000,
                seq,
            },
        );
    }
    // Frozen: a genuinely new honest owner self-evicts.
    let id = test_identity();
    let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("x", 1)], 10);
    assert_eq!(
        store.on_pledge_list_sample(&topic, &bytes, &rvk(), 2_000),
        RecordOutcome::IgnoredAtCap
    );
    // TTL expiry frees the slots…
    assert!(store.sweep_stale_pledges_and_backups(1_000 + STORAGE_RECORD_TTL_MS));
    // …and the same newcomer is now admitted.
    let (topic, bytes) = signed_pledge_bytes(&id, vec![pledge("x", 1)], 11);
    assert_eq!(
        store.on_pledge_list_sample(&topic, &bytes, &rvk(), 1_000 + STORAGE_RECORD_TTL_MS),
        RecordOutcome::Inserted
    );
}
```

- [ ] **Step 2: Run the new tests, verify they fail to compile** (missing constants/methods/disk field):
`cargo nextest run --locked --features test-fixtures -E 'test(record_ttl) + test(apply_boot_grace) + test(received_at_ms_round_trips)'` — expect compile errors.

- [ ] **Step 3: Implement.** (a) Constants after `HOSTING_REPORT_STALE_MS` (:61):

```rust
/// ZEB-923: cadence at which the local node re-mints, re-signs, and
/// republishes its (non-empty) pledge list and backup set — the renewal
/// signal for the receiver-side TTL below. Checked by the storage-record
/// publisher task's 30 s poll.
pub const STORAGE_RECORD_REFRESH_INTERVAL_MS: u64 = 3_600_000;
/// ZEB-923: receiver-side TTL for pledge lists and backup sets, keyed on
/// the LOCAL receipt clock (`received_at_ms`), never the peer-controlled
/// `updated_at`. Deliberately not the in-file 3× refresh idiom: 72
/// refresh intervals positions decay as a growth bound for
/// permanently-dark owners, not liveness detection.
pub const STORAGE_RECORD_TTL_MS: u64 = 3 * 24 * 60 * 60 * 1_000;
/// ZEB-923: minimum post-boot runway `apply_boot_grace` guarantees every
/// reloaded record before it can decay — ample for any alive buddy's
/// hourly renewal to land, so a long-offline receiver never mass-decays
/// alive buddies at boot.
pub const STORAGE_RECORD_BOOT_GRACE_MS: u64 = 12 * 60 * 60 * 1_000;
const _: () = assert!(STORAGE_RECORD_BOOT_GRACE_MS < STORAGE_RECORD_TTL_MS);
```

(b) On-disk rows (`PledgeListOnDisk`, `BackupSetOnDisk`) each gain:

```rust
    /// ZEB-923: local receipt clock, persisted so the record TTL survives
    /// restarts. `default` keeps legacy files loadable (missing ⇒ 0 ⇒
    /// raised to the boot-grace floor at load).
    #[serde(default)]
    received_at_ms: u64,
```

(c) Load: `received_at_ms: row.received_at_ms` for both families (replacing the `0` at :268/:290) and trim the now-stale halves of the reload comments (:251-261, :281-283): `seq` is still stamped in disk order — only the "received_at_ms is not persisted" claims change. (d) Save: add `received_at_ms: r.received_at_ms` to both row constructions. (e) Methods after `sweep_hosting`:

```rust
    /// ZEB-923: drop pledge lists and backup sets not re-affirmed within
    /// [`STORAGE_RECORD_TTL_MS`] (same strict boundary as `sweep_hosting`;
    /// `saturating_sub` ⇒ a wall-clock rollback decays nothing). These
    /// families are persisted, so removal must reach disk or expired
    /// records resurrect at reload — save-on-change, like `purge_revoked`.
    pub fn sweep_stale_pledges_and_backups(&mut self, now_ms: u64) -> bool {
        let before = self.pledge_lists.len() + self.backup_sets.len();
        let fresh =
            |stamp: u64| now_ms.saturating_sub(stamp) < STORAGE_RECORD_TTL_MS;
        self.pledge_lists.retain(|_, r| fresh(r.received_at_ms));
        self.backup_sets.retain(|_, r| fresh(r.received_at_ms));
        let changed = self.pledge_lists.len() + self.backup_sets.len() != before;
        if changed {
            self.save();
        }
        changed
    }

    /// ZEB-923: one-shot post-load floor, called once at the production
    /// construction site. Guarantees every reloaded record at least
    /// [`STORAGE_RECORD_BOOT_GRACE_MS`] before it can decay — a receiver
    /// offline longer than the TTL must not mass-decay alive buddies at
    /// boot before their next renewal lands. Raise-only, RAM-only (not
    /// saved: a reload re-floors, which is consistent); saturates to a
    /// no-op for small clocks, leaving test fixtures untouched.
    pub fn apply_boot_grace(&mut self, now_ms: u64) {
        let floor =
            now_ms.saturating_sub(STORAGE_RECORD_TTL_MS - STORAGE_RECORD_BOOT_GRACE_MS);
        for r in self.pledge_lists.values_mut() {
            r.received_at_ms = r.received_at_ms.max(floor);
        }
        for r in self.backup_sets.values_mut() {
            r.received_at_ms = r.received_at_ms.max(floor);
        }
    }
```

(f) Update the `received_at_ms` doc comments on `PledgeListRecord`/`BackupSetRecord` (:97-100, :112-115): the field now drives the ZEB-923 TTL (drop "no trust meaning"; keep the "never the peer `updated_at`; eviction is `seq`" halves).

- [ ] **Step 4: Run the new tests + the module suite, verify green:**
`cargo nextest run --locked --features test-fixtures -E 'binary_id(harmony-app) and test(storage_records)'` and the Step-2 filter. Also `cargo nextest run --locked --features test-fixtures -E 'test(pledges_and_backup_sets_survive_disk_reload)'` (the reload pin must stay green — it asserts nothing about stamps, and both families still persist).

- [ ] **Step 5: Commit** `feat(zeb-923): record TTL core — sweep, boot grace, persisted receipt stamps`.

---

### Task 2: Wire the sweep into the buddy tick + boot-grace at construction

**Files:**
- Modify: `src-tauri/src/event_loop.rs:6674-6682` (tick step 2)
- Modify: `src-tauri/src/lib.rs:4094-4096` (store construction)

**Interfaces:**
- Consumes: `sweep_stale_pledges_and_backups`, `apply_boot_grace` (Task 1).

- [ ] **Step 1: Extend tick step (2)** — the existing lock block becomes:

```rust
                {
                    let mut records = storage_records
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    records.sweep_hosting(crate::wall_clock_ms());
                    // ZEB-923: decay pledge/backup records not renewed
                    // within the TTL; the planner in step (3) observes the
                    // decay in this same tick and releases the pins.
                    if records.sweep_stale_pledges_and_backups(crate::wall_clock_ms()) {
                        tracing::info!("storage records: stale pledge/backup records decayed");
                        crate::node_event_sink::emit_ser(
                            app.as_ref(),
                            "storage-buddies-updated",
                            &serde_json::Value::Null,
                        );
                    }
                    if records.purge_revoked(&revoked_projection) {
                        tracing::info!("storage records purged for revoked signer(s)");
                    }
                }
```

(The `app` sink handle is in scope in this arm — the same select loop's ingest arm uses it at :4698; verify and, if the arm shadows it, hoist the emit to immediately after the block with a `bool` local.)

- [ ] **Step 2: Boot grace at the production construction site** (`lib.rs:4094`):

```rust
    let storage_records_arc = std::sync::Arc::new(std::sync::Mutex::new({
        let mut records = storage_records::StorageRecordStore::new(Some(
            app_data_dir.join("storage_records.json"),
        ));
        // ZEB-923: one-shot post-load grace floor — see apply_boot_grace.
        records.apply_boot_grace(wall_clock_ms());
        records
    }));
```

- [ ] **Step 3: Compile + targeted suites:** `cargo nextest run --locked --features test-fixtures -E 'test(note_storage_record_sample) + test(storage_records) + test(buddy)'` — green.

- [ ] **Step 4: Commit** `feat(zeb-923): decay sweep rides the buddy tick; boot grace at store construction`.

---

### Task 3: Shared mint clocks + monotonic-floor hardening

**Files:**
- Modify: `src-tauri/src/lib.rs` — fields :885-886, constructors :2102-2103 and :82213-82214, boot re-seed :13668-13672, `next_storage_updated_at` :20047-20057, floor writes :20179 and :20252

**Interfaces:**
- Produces: `pledge_clock`/`backup_set_clock` as `std::sync::Arc<std::sync::atomic::AtomicU64>` (cloneable into the publisher task in Task 5); race-free `next_storage_updated_at`.

- [ ] **Step 1: Convert the fields** to `std::sync::Arc<std::sync::atomic::AtomicU64>`; constructors wrap with `std::sync::Arc::new(...)`. Call sites passing `&guard.pledge_clock` to `next_storage_updated_at` keep compiling via deref coercion.

- [ ] **Step 2: Boot re-seed becomes raise-only on the SHARED cell** (replacing the field reassignment — a task clone must never be disconnected, and an in-memory clock ahead of a stale-saved floor must never regress):

```rust
                        guard
                            .pledge_clock
                            .fetch_max(storage_settings_loaded.pledge_floor, std::sync::atomic::Ordering::Relaxed);
                        guard
                            .backup_set_clock
                            .fetch_max(storage_settings_loaded.backup_set_floor, std::sync::atomic::Ordering::Relaxed);
```

- [ ] **Step 3: Make the mint race-free** (two minters now share each clock — the sync IPC path and the Task-5 publisher task):

```rust
fn next_storage_updated_at(clock: &std::sync::atomic::AtomicU64) -> u64 {
    use std::sync::atomic::Ordering;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let prev = clock
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
            Some(now_secs.max(prev + 1))
        })
        .unwrap_or_else(|prev| prev); // closure always returns Some
    now_secs.max(prev + 1)
}
```

- [ ] **Step 4: Floor writes become grow-only** (concurrent minters can persist out of order; a floor must never move backwards): at :20179 `settings.pledge_floor = settings.pledge_floor.max(updated_at);` and at :20252 `settings.backup_set_floor = settings.backup_set_floor.max(updated_at);`.

- [ ] **Step 5: Run the clock/builder tests:** `cargo nextest run --locked --features test-fixtures -E 'test(storage_clock) + test(pledge_build) + test(backup)'` — green (`storage_clock_is_strictly_monotonic_within_session` pins the mint contract).

- [ ] **Step 6: Commit** `refactor(zeb-923): share storage mint clocks (Arc), race-free mint, grow-only floors`.

---

### Task 4: Component-taking `_with` builders

**Files:**
- Modify: `src-tauri/src/lib.rs` — `build_signed_pledge_list` :20171, `build_signed_backup_set` :20222; new tests beside `pledge_build_advances_and_persists_the_floor` (~:21243)

**Interfaces:**
- Produces: `build_signed_pledge_list_with(identity, node_addr, settings, settings_path: Option<&Path>, clock, v2_material) -> Result<(String, Vec<u8>), String>` and `build_signed_backup_set_with(identity, node_addr, content_index, settings, settings_path, clock, v2_material)` — same return shape as the guard builders, callable from the publisher task (no `&NodeState`). Guard builders become thin wrappers, so every existing builder test exercises the `_with` core.

- [ ] **Step 1: Write the failing test** (lib.rs tests, near :21243):

```rust
    #[test]
    fn with_builders_mint_monotonic_stamps_and_persist_floors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, addr) = signed_state();
        let identity = state.owner_private_identity.clone().unwrap();
        let settings_path = storage_settings::settings_path(dir.path());
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        state.storage_settings.lock().unwrap().my_pledges.insert("buddy-a".into(), 500);

        let (_, bytes_a) = build_signed_pledge_list_with(
            &identity, &addr, &state.storage_settings, Some(&settings_path), &clock, None,
        )
        .expect("build a");
        let (_, bytes_b) = build_signed_pledge_list_with(
            &identity, &addr, &state.storage_settings, Some(&settings_path), &clock, None,
        )
        .expect("build b");
        let a: storage_signing::PledgeListPayload = serde_json::from_slice(&bytes_a).unwrap();
        let b: storage_signing::PledgeListPayload = serde_json::from_slice(&bytes_b).unwrap();
        assert!(b.updated_at > a.updated_at, "re-mint must be strictly newer (LWW renewal)");
        assert!(storage_signing::verify_pledge_list(&b).is_ok(), "renewal is freshly signed");
        let persisted = storage_settings::load_or_default(&settings_path);
        assert!(persisted.pledge_floor >= b.updated_at, "floor persisted");
    }
```

(Adjust the `verify_pledge_list` call to the actual verifier signature at `storage_signing.rs:232` when writing the test — it may take `(payload)` or `(topic, payload)`; make the assertion match.)

- [ ] **Step 2: Run it, verify it fails** (no `_with` fn): `cargo nextest run --locked --features test-fixtures -E 'test(with_builders_mint)'`.

- [ ] **Step 3: Extract the cores.** `build_signed_pledge_list_with` performs: signer-address check (as `build_signed_hosting_report_with` :20352-20357 does), mint via the passed `clock`, floor max-write + `storage_settings::save` when `settings_path` is `Some` (preserving floor-then-publish ordering), pledge collection from `settings.my_pledges`, v2-or-legacy signing with the passed `v2_material`, topic + serialize — i.e., the existing :20171-20215 body with `guard.X` replaced by parameters. `build_signed_backup_set_with` likewise wraps :20222-20313 (content-index read, dedup/sort/cap, shrink-resign loop) with `content_index: &Arc<Mutex<content_index::ContentIndex>>` as a parameter. Mark both `#[allow(clippy::too_many_arguments)]` (owned-handle posture comment, as on the hosting spawn). Guard wrappers become:

```rust
fn build_signed_pledge_list(guard: &NodeState) -> Result<(String, Vec<u8>), String> {
    let identity = storage_signer(guard, "pledge list")?;
    let v2 = storage_v2_material(guard);
    build_signed_pledge_list_with(
        identity,
        &guard.node_addr,
        &guard.storage_settings,
        guard.storage_settings_path.as_deref(),
        &guard.pledge_clock,
        v2.as_ref(),
    )
}
```

(and the backup analogue, passing `&guard.content_index`).

- [ ] **Step 4: Run the storage builder suite:** `cargo nextest run --locked --features test-fixtures -E 'test(pledge_build) + test(backup_set) + test(with_builders) + test(storage_clock)'` — all green.

- [ ] **Step 5: Commit** `refactor(zeb-923): component-taking pledge/backup builders (publisher-task callable)`.

---

### Task 5: Unified storage-record publisher (periodic renewal)

**Files:**
- Modify: `src-tauri/src/lib.rs` — `spawn_hosting_report_publisher` :20444-20540 (rename + extend), spawn site :15030-15041, boot comment :15015-15017; new gating helper + test

**Interfaces:**
- Consumes: `_with` builders (Task 4), `Arc` clocks (Task 3), `STORAGE_RECORD_REFRESH_INTERVAL_MS` (Task 1).
- Produces: `spawn_storage_record_publisher(...)` — hosting behavior unchanged; hourly pledge/backup renewal.

- [ ] **Step 1: Write the failing gating-helper test** (lib.rs tests):

```rust
    #[test]
    fn storage_record_refresh_gating_truth_table() {
        let i = storage_records::STORAGE_RECORD_REFRESH_INTERVAL_MS;
        assert!(storage_record_refresh_due(i, true), "due + non-empty publishes");
        assert!(storage_record_refresh_due(i + 1, true));
        assert!(!storage_record_refresh_due(i - 1, true), "not yet due");
        assert!(!storage_record_refresh_due(i, false), "empty family never renews periodically — its receiver rows should decay");
        assert!(!storage_record_refresh_due(0, false));
    }
```

- [ ] **Step 2: Run it, verify it fails** (`storage_record_refresh_due` undefined).

- [ ] **Step 3: Implement.** Module-level helper beside the spawn fn:

```rust
/// ZEB-923: a family periodically renews ONLY while non-empty. An empty
/// family stays silent, so its rows at receivers decay via the record
/// TTL — retraction convergence is the boot/on-change publish's job.
fn storage_record_refresh_due(elapsed_ms: u64, non_empty: bool) -> bool {
    non_empty && elapsed_ms >= storage_records::STORAGE_RECORD_REFRESH_INTERVAL_MS
}
```

Rename `spawn_hosting_report_publisher` → `spawn_storage_record_publisher`; add parameters `content_index: std::sync::Arc<Mutex<content_index::ContentIndex>>`, `pledge_clock: std::sync::Arc<std::sync::atomic::AtomicU64>`, `backup_set_clock: std::sync::Arc<std::sync::atomic::AtomicU64>`; update its doc comment (now the unified per-generation storage-record publisher: hosting on change/5 min, pledge+backup renewal hourly, ZEB-923). In the loop, after the hosting publish section, add:

```rust
            // ZEB-923: hourly renewal republish of the non-empty own
            // records. Re-minting (strictly-increasing updated_at) + fresh
            // signature is what makes every receiver's LWW take the
            // UpdatedNewer path and restamp its TTL clock — a byte-identical
            // republish would be IgnoredOlder and renew nothing.
            let elapsed = last_record_refresh.elapsed().as_millis() as u64;
            let has_pledges = !settings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .my_pledges
                .is_empty();
            let has_backup = {
                let idx = content_index
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                idx.entries().any(|e| {
                    e.backup
                        && !e.archived
                        && harmony_content::cid::ContentId::from_bytes(e.cid).content_class()
                            == harmony_content::cid::ContentClass::PublicDurable
                })
            };
            let mut renewed = false;
            if storage_record_refresh_due(elapsed, has_pledges) {
                match build_signed_pledge_list_with(
                    &identity, &node_addr, &settings, Some(settings_path.as_path()),
                    &pledge_clock, v2_material.as_ref(),
                ) {
                    Ok((topic, bytes)) => {
                        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
                        match publish_tx.try_send(event_loop::PublishRequest {
                            key_expr: topic, payload: bytes, reply: reply_tx,
                        }) {
                            Ok(()) => renewed = true,
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                            Err(e) => tracing::warn!(error = %e, "pledge renewal deferred (channel full)"),
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "pledge renewal skipped"),
                }
            }
            if storage_record_refresh_due(elapsed, has_backup) {
                match build_signed_backup_set_with(
                    &identity, &node_addr, &content_index, &settings,
                    Some(settings_path.as_path()), &backup_set_clock, v2_material.as_ref(),
                ) {
                    Ok((topic, bytes)) => { /* same try_send shape as above; renewed = true on Ok */ }
                    Err(e) => tracing::warn!(error = %e, "backup-set renewal skipped"),
                }
            }
            if renewed {
                last_record_refresh = std::time::Instant::now();
            }
```

with `let mut last_record_refresh = std::time::Instant::now();` declared beside `last_publish` (boot publish covers t=0, so the first renewal at ~1 h is correct). A failed family retries next tick until a success resets the shared timer — acceptable self-heal. Spawn site (:15030) passes `guard.content_index.clone(), guard.pledge_clock.clone(), guard.backup_set_clock.clone()` and the boot comment (:15015-15017) is updated ("hosting + periodic pledge/backup renewal are the publisher task's job").

- [ ] **Step 4: Run:** `cargo nextest run --locked --features test-fixtures -E 'test(storage_record_refresh) + test(with_builders) + test(pledge) + test(backup) + test(hosting)'` — green.

- [ ] **Step 5: Commit** `feat(zeb-923): unified storage-record publisher — hourly pledge/backup renewal`.

---

### Task 6: Planner decay pin, docs, full gates

**Files:**
- Modify: `src-tauri/src/buddy_pin_planner.rs` (tests mod, after `inactive_pact_and_dropped_entries_release` :407)
- Modify: `docs/specs/2026-07-11-zeb-669-storage-buddies-design.md` (:46, :63 — annotate the ZEB-923 renewal/TTL)

**Interfaces:**
- Consumes: Task 1 sweep; `seeded_records` fixture (`buddy_pin_planner.rs:209`, ingests at `now_ms = 9_000`, `updated_at = 1`).

- [ ] **Step 1: Write the decay-to-release test** (this is the acceptance pin — decayed records release pins through the EXISTING planner):

```rust
    #[test]
    fn ttl_decayed_records_release_pins_via_the_existing_plan() {
        use crate::storage_records::STORAGE_RECORD_TTL_MS;
        let (mut records, owners) =
            seeded_records(&[("alice", 50, vec![entry(b"kept", 10)])]);
        let my_pledges: BTreeMap<String, u64> = [(owners[0].clone(), 100)].into();
        let mut ledger = StorageLedger::new(None);
        ledger.record_fetched(&owners[0], &cid_hex(b"kept"), 10);

        // Decay everything alice published (permanently-dark buddy).
        assert!(records.sweep_stale_pledges_and_backups(9_000 + STORAGE_RECORD_TTL_MS));
        let plan = plan(ME, &my_pledges, &records, &ledger, 1_000, &HashMap::new(), NO_BACKOFF);
        assert_eq!(plan.release_buddies, vec![owners[0].clone()],
            "decayed pledge list ⇒ pact inactive ⇒ release everything");
        assert!(plan.fetch.is_empty(), "no fetching from a decayed pact");
    }
```

(Adjust the `ledger.record_fetched` seeding call to the actual `StorageLedger` API used by `inactive_pact_and_dropped_entries_release` :407-431 — mirror exactly how that test seeds alice's ledger rows.)

- [ ] **Step 2: Run it, verify it fails only if wiring is wrong** — it should PASS immediately (the planner needs no changes; this pins that fact). If it fails, the decay→release flow is broken: stop and diagnose before proceeding.

- [ ] **Step 3: Update the ZEB-669 design doc** — at the record-family table (:46) and the backup-set republish line (:63), add one-line ZEB-923 annotations: pledge/backup now also republish hourly while non-empty (renewal), and receivers decay them after `STORAGE_RECORD_TTL_MS` (boot-grace floored).

- [ ] **Step 4: Full local gates** (working tree, then confirm `git status` clean after commit):

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Full sweep is the pre-PR gate (not test-select). Check `${pipestatus[1]}` on any piped command.

- [ ] **Step 5: Commit** `test(zeb-923): decay-to-release planner pin + zeb-669 doc annotations`, push branch, open PR.

---

## Self-review notes

- Spec coverage: §2a → Tasks 3-5; §2b(1) sweep → Task 1+2; §2b(2) persistence → Task 1; §2b(3) boot grace → Tasks 1-2; §2c no-planner-change → Task 6 pin; constants → Task 1; test plan T1-T6 → Task 1, T7 → Task 6, T8 → Task 4, T9 → Task 5.
- Type consistency: `sweep_stale_pledges_and_backups(&mut self, u64) -> bool` used identically in Tasks 1/2/6; `Arc<AtomicU64>` clocks introduced in Task 3, consumed in Tasks 4-5; `_with` signatures in Task 4 match Task 5's call sites.
- Known look-up-at-write-time points (flagged inline): `verify_pledge_list` exact signature (Task 4 Step 1), `StorageLedger` seeding API (Task 6 Step 1), `app` sink scope in the tick arm (Task 2 Step 1).
