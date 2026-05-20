# Mint Phase 2 — CAS-backed multi-device ledger sync — Design

**Date:** 2026-05-19
**Status:** Design approved, plan pending
**Author:** Jake Englund (via brainstorming session with Claude)
**Depends on:** [Mint MVP (2026-05-19)](2026-05-19-mint-mvp-design.md); ZEB-215 Phase 3a owner-state sync infrastructure (`owner_state_sync.rs`, `content_store.rs`, `state_root_replay.cbor`); Phase 1 AEAD primitives in `owner_state_crypto.rs`.

## Goal

Sync the local Mint ledger (`<app_data_dir>/mint/ledger.db`) across a single user's multiple devices, transparently and without conflict resolution UI for the common case. Two devices that have both been online recently should converge within ~2 seconds of any mutation on either side. Two devices that diverge while offline should converge automatically on reconnection via last-write-wins on `updated_at`, with tombstone propagation for deletes.

This unlocks the original "harmony-mint" vision of a personal-finance tracker that *just works* across phone + laptop + desktop without manual export/import dances.

## Non-goals (Phase 2)

These are explicitly out of scope:

- **Multi-user / shared ledgers** — single owner identity per ledger. Future "split this expense with my partner" is a separate design.
- **Conflict-resolution UI** — no "your other device has changes, choose" dialogs. LWW resolves silently.
- **Cross-NAT / cloud relay sync** — Phase 2 rides the same LAN/mDNS Zenoh transport that owner-state uses. Cross-internet sync depends on the harmony transport layer maturing.
- **Per-row CAS blobs** — the whole ledger is one encrypted blob per snapshot. Per-row blob sharding (Phase 3b shape for owner-state) is unnecessary at expected mint scale.
- **Vector clocks / true CRDTs.** Wall-clock `updated_at` plus per-device HLC envelopes is sufficient for single-user offline-rare divergence patterns.
- **Pin-set integration for snapshot durability.** Mint snapshots aren't pinned via ZEB-156. The ContentStore's own retention policy is sufficient for the steady-state case; if GC pressure ever drops a recent snapshot, integrate then.
- **Hard-version migration handshake.** `schema_version` field exists for future use; Phase 2 ships at v1 only.

## Context

The harmony-client codebase already has a complete state-sync template — owner-state Phase 3a (`docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md`). The pattern: serialize typed state → encrypt with a per-domain lookup key → `put` to ContentStore → publish `{root_cid, hlc}` over a Zenoh topic → subscriber decrypts envelope → fetches blob → decodes → merges via CRDT-style apply methods → persists per-device replay tracker. Phase 2 mint sync mirrors this pattern with mint-specific encode/merge.

Mint v1 (PR #144) already laid the groundwork for sync:

- **UUIDv4 PKs** on accounts and transactions — no integer-PK collisions across devices.
- **`updated_at`** maintained on every mutation including the delete-account reassign branch — load-bearing for per-row LWW (decision D5 in MVP design).
- **TEXT decimal amounts** — portable across SQLite engine versions and language runtimes.

## Architecture

### Module layout

Three new files in `src-tauri/src/`:

| File | Lines (est.) | Role |
|---|---|---|
| `mint_sync.rs` | ~500 | `MintSyncEngine` struct: debounced publisher, Zenoh subscriber task, replay-tracker integration, public API (`new`, `notify_dirty`, `flush_now`, `shutdown`). |
| `mint_sync_persist.rs` | ~200 | Load/save `mint_sync_state.cbor` (the replay tracker + the account-deletion floor); atomic-rename + fsync. |
| `mint_sync_types.rs` | ~150 | `MintSnapshot`, `MintRootPublishPayload { root_cid, at: Hlc }`, snapshot row types, schema versioning. |

Three existing files modified:

- **`mint.rs`** — every mutation handler picks up an `engine.notify_dirty()` call before returning IPC response. Hard-delete on transactions becomes soft-delete (`UPDATE … SET deleted_at = ?, updated_at = ?`). All read paths add `WHERE deleted_at IS NULL`. Account-delete records an entry in the deletion floor.
- **`lib.rs`** — `NodeState` gains `Option<Arc<MintSyncEngine>>`; engine init gated on identity bootstrap, same gating point owner-state uses. The 12 existing mint IPC commands gain an optional `notify_dirty()` postcondition.
- **`MintLedger.svelte`** — adds a `mint-changed` Tauri event listener that re-invokes `load()`. No other UI surface changes — the table automatically reflects merged-in remote rows.

### Transport surface

| | Value |
|---|---|
| **Zenoh topic** | `harmony/owner/{addr_hex}/mint-root-v1` |
| **AEAD lookup key** | `space_lookup_key(&kt, b"mint-ledger-v1")` |
| **Wire envelope** | `MintRootPublishPayload { root_cid: ContentId, at: Hlc }`, encrypted via existing `encrypt_root_publish` |
| **ContentStore** | Reuses the same `Arc<dyn ContentStore>` already injected for owner-state sync |
| **Replay tracker** | Reuses Phase 1's `RootReplayTracker` type as-is |
| **HLC source** | Reuses Phase 3a's HLC generator (`Hlc { wall_ms, logical, device_id }`). Mint maintains its own `last_published_hlc` independent of owner-state's. |

### Disk surface

New file: `<app_data_dir>/mint/mint_sync_state.cbor`. CBOR-encoded struct holding:

```rust
struct MintSyncState {
    schema_version: u32,                              // currently 1
    replay_tracker: RootReplayTracker,                // per-publisher-device last-accepted HLC
    account_deletion_floor: HashMap<String, String>,  // AccountId → ISO8601 deletion timestamp
}
```

Atomic save: write to `mint_sync_state.cbor.tmp`, fsync, rename. Same pattern as `state_root_replay.cbor`.

The ledger itself remains at `<app_data_dir>/mint/ledger.db`. No relocation.

### Internal task loop

Identical shape to `owner_state_sync::internal_task`:

```rust
loop {
    select! {
        _ = dirty_flag.notified()  => schedule_wakeup_at(now + 250.ms)
        _ = scheduled_wakeup       => publish_root_now().await
        _ = flush_now_signal       => publish_root_now().await
        _ = shutdown_signal        => break
    }
}
```

`DEFAULT_DEBOUNCE_MS = 250`, configurable at `MintSyncEngine::new` for test override.

## Schema changes

### `transactions` table (v2)

```sql
ALTER TABLE transactions ADD COLUMN deleted_at TEXT NULL;
CREATE INDEX IF NOT EXISTS idx_tx_deleted_at ON transactions(deleted_at);
```

- `deleted_at IS NULL` ⇒ live row.
- `deleted_at IS NOT NULL` ⇒ tombstone, ISO 8601 timestamp recording when the delete happened.
- `mint_delete_transaction` IPC stops using `DELETE FROM`; instead:
  ```sql
  UPDATE transactions SET deleted_at = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL;
  ```
- All read paths (`mint_list_transactions`, `mint_get_transaction`, `mint_export_csv`, account `transaction_count` JOIN) add `WHERE t.deleted_at IS NULL`.

### `accounts` table (v2)

```sql
ALTER TABLE accounts ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
-- One-shot migration step (run inside apply_migrations after the ALTER):
UPDATE accounts SET updated_at = created_at WHERE updated_at = '';
```

`updated_at` is bumped on `mint_rename_account` and on `mint_create_account` (initialized equal to `created_at`). Account hard-delete is preserved (with reassign-to-target semantics) but `delete_account` also records the deletion in the deletion floor — see merge semantics.

### `settings` table (v2)

```sql
ALTER TABLE settings ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
UPDATE settings SET updated_at = ? WHERE updated_at = '';   -- backfill to migration time
```

`updated_at` is bumped on every `mint_set_default_currency`. New settings rows initialized with `updated_at = now()`.

### Migration idempotency

`apply_migrations` already follows the `CREATE TABLE IF NOT EXISTS` + best-effort `ALTER TABLE ADD COLUMN` pattern from MVP. Adding the three columns above is the same pattern; the `ADD COLUMN` is wrapped in `let _ = ...` (Rust idiom for tolerating "column already exists" error on a second run). The backfill `UPDATE` is conditional on `updated_at = ''` so it runs at most once even after re-invocations.

## Snapshot CBOR shape

```rust
// In mint_sync_types.rs:
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MintSnapshot {
    pub schema_version: u32,            // currently 1
    pub accounts: Vec<AccountRow>,
    pub transactions: Vec<TransactionRow>,
    pub settings: Vec<SettingRow>,
    pub captured_at: String,            // ISO 8601, debugging only — not used in merge
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub id: String,                     // UUIDv4
    pub name: String,
    pub created_at: String,
    pub updated_at: String,             // NEW in v2
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct TransactionRow {
    pub id: String,
    pub transaction_date: String,
    pub amount: String,
    pub currency: String,
    pub account_id: String,
    pub description: String,
    pub metadata: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,     // NEW in v2; presence = tombstone
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SettingRow {
    pub key: String,
    pub value: String,
    pub updated_at: String,             // NEW in v2
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct MintRootPublishPayload {
    #[serde(rename = "rc")]
    pub root_cid: ContentId,
    #[serde(rename = "at")]
    pub at: Hlc,
}
```

Encoded via the same canonical-CBOR helper Phase 1 established (sorted-key, same-length-keys precondition for the wire envelope's 2-char keys).

**Size envelope:** ~250 bytes per row before encryption. 10k transactions ≈ 2.5 MB raw → ~700 KB after CBOR compaction → ~700 KB encrypted blob. Fits comfortably in one CAS blob at decade-of-power-user scale.

**Forward compatibility:** New fields added in future schema_version=1 spec evolutions take `#[serde(default)]`. The receiver decodes old payloads without complaint; the sender adds new fields opportunistically. `schema_version` bumps only on hard breaking changes (e.g. amount-type changes from decimal-text to something else) — those require a hard-version handshake protocol that Phase 2 does not ship.

## Snapshot lifecycle

### Publisher path

```
Tauri IPC: e.g. mint_create_transaction(payload)
  │
  ▼ INSERT INTO transactions ...                          ← inside spawn_blocking
  │
  ▼ engine.notify_dirty()                                 ← non-blocking
  │
  └── (response returned to JS)

Internal SyncEngine task (debounced 250ms after notify_dirty):
  let snapshot = self.snapshot_current_db().await?;       ← reads accounts + transactions + settings
  let now = self.next_hlc();                              ← strictly newer than last_published
  let cbor = canonical_cbor_encode(&snapshot)?;
  let ciphertext = encrypt_entry(&self.kt, &mint_ledger_lookup_key, &cbor)?;
  let root_cid = ContentId(blake3::hash(&ciphertext).into());
  self.content_store.put(root_cid, ciphertext).await?;
  let payload = canonical_cbor_encode(&MintRootPublishPayload { root_cid, at: now })?;
  let wire = encrypt_root_publish(&self.kt, &payload)?;   ← random nonce, reuses Phase 1
  self.zenoh_publisher.put(wire).await?;
  self.last_published_hlc = now;
  self.persist_sync_state_debounced();                    ← schedules disk save
```

`snapshot_current_db()` runs three SELECTs inside one `BEGIN; … COMMIT;` so the snapshot is a consistent read across tables. It includes tombstoned transaction rows so peers see the tombstone and converge on delete.

Empty-snapshot short-circuit: if `accounts.is_empty() && transactions.is_empty() && settings.is_empty()`, the publish is skipped. This prevents a brand-new device's boot-hook flush from broadcasting an empty snapshot that would LWW-clobber an existing peer's data.

### Subscriber path

```
Zenoh delivery on harmony/owner/{addr_hex}/mint-root-v1:
  let payload = decrypt_root_publish(&self.kt, &wire)?;
  let MintRootPublishPayload { root_cid, at } = canonical_cbor_decode(&payload)?;
  if at.device_id == self.local_device_id { return; }     ← own-publish echo suppression
  if !self.tracker.accept(&at)? { return; }               ← replay protection
  let blob_ct = self.content_store.get(&root_cid).await?
      .ok_or(MintSyncError::MissingBlob)?;
  let blob_pt = decrypt_entry(&self.kt, &mint_ledger_lookup_key, &blob_ct)?;
  let remote: MintSnapshot = canonical_cbor_decode(&blob_pt)?;
  if remote.schema_version > LOCAL_MAX_SCHEMA_VERSION {
      emit_tauri_event("mint-sync-error", "incompatible schema version");
      return;
  }
  let conn = self.mint_db.clone();
  tokio::task::spawn_blocking(move || {
      let mut conn = conn.lock();
      let tx = conn.transaction()?;
      apply_remote_snapshot(&tx, &remote)?;               ← see merge semantics
      tx.commit()
  }).await??;
  self.persist_sync_state_debounced();                    ← saves the now-bumped replay tracker
  emit_tauri_event("mint-changed", ());                   ← MintLedger re-fetches
```

### Lifecycle

- **Boot:** After identity loads and `mint_db_handle` produces a connection, `MintSyncEngine::new(zenoh, content_store, kt, mint_db, sync_state, device_id)` spawns the internal task and opens the Zenoh subscriber.
- **Identity-not-ready:** If identity isn't established at app start (first run, pairing pending), engine init is deferred. The Tauri pairing hook re-triggers init once identity is set up — same gating point owner-state uses.
- **Boot-hook flush:** ~500 ms after engine init, schedule a one-shot `flush_now()`. This publishes our current state if non-empty, so a peer that came online after us still receives our latest ledger without waiting for a user mutation.
- **Shutdown:** `MintSyncEngine::shutdown().await` from the Tauri shutdown hook signals one last `publish_root_now()` if dirty, then synchronously writes `mint_sync_state.cbor`. Hard 5 s timeout on shutdown to bound exit latency. `Drop` is best-effort; `shutdown()` is the documented safe path.

## Merge semantics

The merge runs in a single SQLite transaction; either everything from the remote snapshot lands or nothing does.

### Apply order

Apply order matters because of the FK `transactions.account_id → accounts.id`:

```rust
fn apply_remote_snapshot(tx: &Transaction, remote: &MintSnapshot) -> Result<()> {
    for r_acct in &remote.accounts { upsert_account_lww(tx, r_acct)?; }
    for r_tx   in &remote.transactions { upsert_transaction_lww(tx, r_tx)?; }
    for r_set  in &remote.settings { upsert_setting_lww(tx, r_set)?; }
    Ok(())
}
```

### LWW rule (transactions)

```rust
fn upsert_transaction_lww(tx: &Transaction, r: &TransactionRow) -> Result<()> {
    let local: Option<TransactionRow> = tx.query_row(
        "SELECT * FROM transactions WHERE id = ?", [&r.id], row_to_tx
    ).optional()?;
    match local {
        None => insert_full(tx, r)?,                          // peer has a row we've never seen
        Some(l) if r.updated_at > l.updated_at => replace_full(tx, r)?,
        Some(_) => {}                                          // local newer-or-equal: keep
    }
    Ok(())
}
```

`INSERT … ON CONFLICT(id) DO UPDATE SET …` is used for the upsert primitive rather than `INSERT OR REPLACE`, which triggers FK cascade on the replaced row's references. Ties on `updated_at` are extremely rare (millisecond-resolution timestamps + two writers); we resolve by keeping local.

Tombstones propagate for free under this rule: a tombstoned remote row (`deleted_at = Some(t)`) with later `updated_at` overwrites a live local row. The reverse (live remote overwriting a local tombstone) is also correct if the live edit genuinely happened after the tombstone.

### LWW rule (settings)

Same as transactions but keyed on `key` instead of `id`. No tombstones; settings are never deleted.

### LWW rule + deletion floor (accounts)

Accounts use hard-delete with reassign in v1. To prevent the "delete on A; B republishes its old snapshot still containing the account; A re-inserts" zombie, each device persists a `account_deletion_floor: HashMap<AccountId, ISO8601Timestamp>` recording when it hard-deleted each account. The merge step is:

```rust
fn upsert_account_lww(tx: &Transaction, r: &AccountRow, floor: &HashMap<String, String>) -> Result<()> {
    if let Some(floor_ts) = floor.get(&r.id) {
        if &r.updated_at <= floor_ts {
            return Ok(());            // peer's row is stale w.r.t. our delete — drop it
        }
    }
    // normal LWW upsert
    let local: Option<AccountRow> = tx.query_row(...).optional()?;
    match local {
        None => insert_full(tx, r)?,
        Some(l) if r.updated_at > l.updated_at => update_account(tx, r)?,
        Some(_) => {}
    }
    Ok(())
}
```

The floor grows monotonically. **As of PR #147**: the floor is synced via `MintSnapshot.account_deletion_floor` — every outgoing snapshot carries this device's floor so peers can converge on deletions. On receive, `apply_remote_snapshot` iterates the remote floor: accounts present locally with `updated_at ≤ remote_floor_ts` are hard-deleted (along with their orphan transactions). The remote floor entries are returned to the caller and merged into local `MintSyncState.account_deletion_floor` (keeping max timestamps). Floor entries grow unboundedly and are dropped only on future cleanup (out of scope for Phase 2).

### What the merge does NOT do

- **No three-way field-level merge.** If peer changed `amount` and local changed `description` on the same row, the later-`updated_at` row wins entirely; the other edit is discarded. Acceptable single-user single-device-at-a-time assumption.
- **No vector clocks.** Wall-clock `updated_at` + per-device HLC envelopes are sufficient at this scale. Two devices with badly-skewed clocks can produce surprising LWW outcomes, but harmony-client already assumes well-synced clocks (NTP) for owner-state.
- **No metadata-field-level merge.** `metadata` is opaque JSON; LWW replaces the whole field.

## First-run + bootstrap

### Cold start (no `ledger.db`, no `mint_sync_state.cbor`)

1. App boots, identity loads, `mint_db_handle` runs v2 migrations → empty `ledger.db`.
2. `MintSyncEngine::new` constructs an empty `RootReplayTracker` and writes the initial `mint_sync_state.cbor`.
3. Zenoh subscriber opens. No publish yet — dirty flag is false.
4. Boot-hook flush fires ~500 ms later but no-ops because the snapshot is empty.
5. User opens the Mint panel: empty. Inserts use fresh UUIDs that won't collide with peers.
6. When a peer next publishes (their next mutation, or their boot-hook flush), the subscriber fetches + merges. Rows appear in the UI via the `mint-changed` event.

### Warm start

1. Load `mint_sync_state.cbor` → restores last-accepted HLC per known device and account deletion floor.
2. Engine starts. Subscriber will reject any republish whose HLC ≤ what we already accepted from that device.
3. User can begin writing immediately; debounced publishes go out as usual.
4. Boot-hook flush publishes our current state to wake up peers that came online after us.

### First-time pairing

When the user pairs a second device to their existing identity:

1. The pairing flow (existing infrastructure) transfers `master_seed` → both devices share `kt` derivation.
2. Both devices' `MintSyncEngine` are on the same Zenoh topic with mutually-decryptable envelopes.
3. New device boots empty; its boot-hook flush is a no-op (empty snapshot).
4. Existing device's boot-hook flush OR next mutation triggers a publish that reaches the new device.
5. Snapshot merges in. Done — typically < 5 seconds end-to-end on LAN.

No special "join sync" flow needed.

## Error handling

### Publisher-side

| Failure | Behavior |
|---|---|
| `snapshot_current_db()` SQL error | Log + skip publish; dirty flag stays set so next debounce retries. After 5 consecutive failures, emit `mint-sync-error` Tauri event. |
| `canonical_cbor_encode` error | Should be impossible (types are infallibly `Serialize`); log + skip + mark `Degraded` if it ever fires. |
| `encrypt_entry` error | Same — indicates key-derivation bug. Log + skip + `Degraded`. |
| `content_store.put` error | Log + skip; dirty flag stays set. Retry next debounce window. |
| Zenoh `put` error (offline, no peers) | Log at debug level; normal when offline. CAS already has the CID; next-publish-when-online catches up. |
| Shutdown flush > 5 s | Hard timeout; force-exit. |

### Subscriber-side

| Failure | Behavior |
|---|---|
| `decrypt_root_publish` fails | Drop silently. Expected if another harmony user is on the same LAN. |
| Replay-tracker rejects HLC | Drop silently. Expected — same publish hitting us twice. |
| `content_store.get` returns `None` | Retry once after 250 ms (race between publisher's `put` and our `get`). On second failure, drop. Replay tracker NOT bumped, so the next publish from same peer at later HLC triggers another fetch. |
| `decrypt_entry` fails on blob | Log loud + drop. Indicates corruption or cross-identity key mismatch. |
| `canonical_cbor_decode` fails | Log loud + drop. |
| Merge transaction fails (FK, constraint) | `ROLLBACK`; log loud; emit `mint-sync-error`. Replay tracker NOT bumped. Next publish from same peer triggers retry. |
| `tracker_dirty.set + persist` write fails | Log warn; in-memory state correct, on-disk replay tracker falls behind. Subsequent successful save catches up. |

### Schema-version drift

| Scenario | Behavior |
|---|---|
| v1 receiver gets v2 snapshot | `serde(default)` makes decode succeed; v1 ignores unknown fields. **Continues to work.** |
| v2 receiver gets v1 snapshot | All v1 fields decode; v2-only fields take `Default` (typically `None` or `""`). **Continues to work.** |
| Hard breaking change (e.g. amount-type changes) | `schema_version` bump; subscriber checks `remote.schema_version <= LOCAL_MAX_SCHEMA_VERSION`; if not, drop + emit `mint-sync-error` suggesting an app update. **Surfaces; no silent corruption.** |

Phase 2 ships at `schema_version = 1`. No hard-version handshake — just the version field and the receiver-side check.

### Observability

Structured tracing spans at `info` level for `publish_root_now` and `apply_remote_snapshot`, plus counter-style metrics (`mint_sync.publishes_total`, `mint_sync.merges_total`, `mint_sync.merge_errors_total`) — currently log-only; wiring to a Prometheus exporter is a follow-up.

## Testing plan

### Rust unit tests in `mint_sync.rs`

- `snapshot_current_db_round_trip` — fixtures → CBOR encode → decode → struct equality.
- `next_hlc_strictly_monotonic` — 1000× loop; each is strictly-newer-than the last.
- `apply_remote_snapshot_inserts_new_rows` — empty local; apply snapshot; verify all rows present.
- `apply_remote_snapshot_lww_keeps_newer_local` — local newer; incoming older; verify local preserved.
- `apply_remote_snapshot_lww_replaces_older_local` — opposite direction; verify incoming wins.
- `apply_remote_snapshot_propagates_tombstone` — incoming tombstone newer than local live; verify local now tombstoned.
- `apply_remote_snapshot_resurrects_after_tombstone` — incoming live newer than local tombstone; verify local now live (legitimate undelete).
- `apply_remote_snapshot_fk_ordering` — pathological out-of-order snapshot (handled by apply order, but assert behavior if FK does fire).
- `account_deletion_floor_blocks_stale_resurrect` — floor entry at `t2`; apply snapshot with account `updated_at = t1 < t2`; verify account stays deleted.
- `schema_version_too_new_drops_publish` — `schema_version = 999`; verify subscriber drops + emits event.
- `empty_snapshot_publish_is_skipped` — engine with empty DB; verify `publish_root_now` exits before Zenoh `put`.

### Rust integration tests in `tests/mint_sync_integration.rs`

Reuses Phase 3a's two-engine harness (`Arc<InMemoryStub>` ContentStore + in-memory Zenoh session).

- `two_engines_converge_on_inserts` — A inserts 5 transactions; B's subscriber fires; B has all 5.
- `two_engines_converge_on_updates` — A updates row R; verify B's R matches A's edit.
- `two_engines_converge_on_concurrent_writes` — both insert independent rows; both end up with both.
- `two_engines_converge_on_concurrent_edits_same_row` — both edit R; later-`updated_at` wins on both sides.
- `two_engines_converge_on_delete_then_propagate` — A soft-deletes R; both sides show R tombstoned.
- `account_deletion_floor_survives_restart` — A deletes account X; restart A; B republishes containing X; X stays deleted on A.
- `bootstrap_new_device_pulls_existing_state` — A has 100 tx; B starts empty; boot-hook flush propagates; B has all 100.
- `missing_blob_retries_then_drops` — first `get` returns None; verify single retry then drop; subsequent publish succeeds.
- `clock_skew_lww_chooses_later_wall_time` — A's clock 5 seconds fast; A and B both edit R; A's edit wins via LWW (documents the known limitation).

### Frontend tests

Minimal new scope. One new vitest case:

- `MintLedger reacts to mint-changed event` — synthesize the Tauri event; verify `load()` is called; verify table re-renders.

### Manual smoke test

After implementation lands, on Ildwyn (Windows) + Koya (Mac):

1. Build + install on both; pair via existing pairing flow.
2. Ildwyn: create account "Chase", add 3 transactions, set default currency to JPY.
3. Wait ≤ 2 seconds.
4. Koya: open Mint; verify all 3 transactions + Chase + JPY default appear without manual refresh.
5. Koya: edit transaction #2's description.
6. Ildwyn: verify edit appears within ~2 seconds.
7. Koya: delete transaction #1 (soft).
8. Ildwyn: verify it's gone from the live list.
9. Disconnect Koya. Ildwyn edits tx #3. Koya edits tx #3 (different field). Reconnect Koya. Within ~2 seconds both devices show the later-`updated_at` edit.

### Out-of-scope test areas

- **Clock-skew adversarial cases** — assume well-synced clocks. Skewed-clock testing would need wall-clock injection in HLC generation; v3+ ticket.
- **CAS GC interactions** — Phase 2 doesn't pin snapshots. If GC ever drops a recent snapshot we'd see `MissingBlob` and rely on next-publish recovery. Integrate ZEB-156 root-pin-set if GC pressure becomes a real concern.
- **Cross-version migration tests** — schema_version=1 is the only version Phase 2 ships. Forward-compat property of `#[serde(default)]` is verified by a unit test only.

## Decisions log

Continuing the MVP design's decision numbering.

- **D6: Dedicated MintSyncEngine vs. piggyback on OwnerState.** Chose dedicated engine. Owner-state's CRDT has tightly-designed invariants on its entry types; cramming a "mint snapshot CID" pointer into it would be an abstraction violation that future contributors may not respect. Every mint mutation would also force-republish the whole owner-state blob, increasing pressure on owner-state's Zenoh topic. Costs ~600 LOC of structural copy from `owner_state_sync.rs`, but the proven pattern reduces design risk to near zero.

- **D7: Typed CBOR row projection vs. sealed SQLite blob.** Chose typed CBOR. Portable across SQLite engine versions and language runtimes. Enables per-row LWW merge for offline-parallel-edits convergence. Size envelope is generous: ~700 KB encrypted at decade-of-power-user scale.

- **D8: Per-row LWW on `updated_at`.** Chose per-row over per-DB. Single-user assumption means conflicts are rare but real (edit on phone, edit on laptop later); per-row merge converges without silently losing one device's changes. `updated_at` was already maintained on every mutation per MVP design D5.

- **D9: Soft-delete tombstone column on transactions.** Chose soft-delete now rather than deferring. The mint trajectory memory explicitly warned that tombstones are "RISKIER post-sync due to tombstone propagation"; adding the column in the same PR as sync avoids that risk. Schema cost: one nullable column + filter-on-read in five query sites. UX cost: deletes are recoverable via a future "trash" view; for now, they're invisible after the soft-delete. Trade-off: full hard-delete-and-forget is gone; if a user actually wants to scrub a row, a future GC pass over old tombstones can hard-delete them.

- **D10: Hard-delete + deletion floor for accounts.** Chose hard-delete rather than soft-delete-everything. Accounts are deleted rarely; the deletion floor in `mint_sync_state.cbor` solves the zombie-resurrect problem at ~80 bytes per ever-deleted account. **As of PR #147 (Cursor HIGH fix)**: the floor is now synced via `MintSnapshot.account_deletion_floor`. The publish path attaches the local floor to every outgoing snapshot; the apply path iterates the remote floor and hard-deletes local accounts whose `updated_at ≤ remote_floor_ts`. Orphan transactions for those accounts are also hard-deleted in the same SQLite transaction (no `ON DELETE CASCADE` in schema; must be explicit). Each device still maintains its own floor copy; receiving a remote floor entry merges it in (keeping the later timestamp) so this device also blocks future zombie-resurrects from other stale peers. The "per-device" wording still applies to ownership — the snapshot just carries the originator's view for convergence. The original design assumed "peers learn about a delete by the account being absent from the next snapshot" — but `apply_remote_snapshot` only upserted rows present in the remote; it never acted on absence. The synced floor closes this gap.

- **D11: 250ms debounce + silent apply + boot-hook flush.** Mirrors owner-state's proven cadence. The boot-hook flush is a minor addition that materially improves the "new device just paired, where's my data?" UX without standing up a separate pull-request protocol.

- **D12: Replay tracker persisted in `mint_sync_state.cbor` (separate file).** Could share `state_root_replay.cbor` with owner-state but co-mingling two engines' trackers in one file invites concurrency footguns. Separate file is the lower-risk path; disk cost is negligible.

- **D13: Empty-snapshot publish is skipped.** Without this, a brand-new device's boot-hook flush would broadcast an empty `MintSnapshot` that — under LWW with later HLC — could wipe an online peer's existing data. The skip is a single `is_empty()` short-circuit; the trade-off is "a freshly-cleared ledger can't propagate its emptiness", which is correct behavior (the only way to "clear" a peer is to delete each row individually, which produces tombstones).

## Out-of-scope follow-ups

Track these as future tickets if Phase 2 proves out:

- **Cross-NAT / cloud-relay sync.** Depends on harmony transport maturing beyond LAN/mDNS Zenoh.
- **Schema migration handshake.** A real "v1 device, please upgrade" UX flow rather than the receiver-side drop.
- **Tombstone GC.** A pass that hard-deletes tombstoned transaction rows older than N days, after confirming all paired devices have synced past that point. Requires a "last-confirmed-sync HLC per device" gossip protocol.
- **Account-delete deletion-floor GC.** Same shape as tombstone GC. Floor entries grow unbounded otherwise.
- **Pinning recent snapshots.** Integrate with ZEB-156 root-pin-set so a `MissingBlob` from CAS GC becomes a non-issue. Add only if real-world `MissingBlob` events surface in observability.
- **Multi-user / shared ledgers.** "Split this with my partner" requires a different data model entirely (per-user views, role-based merge rules). Separate design.
- **Soft-delete UI surfaces.** Trash view, restore-from-trash, "deleted last N days" filter.
- **Vector clocks** if real LWW-via-skewed-clock issues ever materialize.
