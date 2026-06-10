# ZEB-418 SP2 Phase 2: Outbound hold + fresh butler-set advertisement — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Any online device in the sender's fleet can complete delivery of a pending DM (direct or via the recipient's butler), and the butler-set advertisement stays fresh with a sibling secondary entry + pin-a-butler setting.

**Architecture:** Two new small `FleetSyncEngine` datasets — `dm-outhold-v1` (content side-table: message_cid → storage blob, applied into each device's local CAS) and `fleet-net-v1` (per-device net-info rows + pinned LWW, feeding a synchronous snapshot for the pkarr blob builder). Drain candidacy extends to sent-but-never-acked pairs (ZEB-422, folded in). All retry/delivery state stays in `state.outbox`; drain already runs fleet-wide with no originator filter.

**Tech Stack:** Rust (tokio, ciborium canonical CBOR, FleetSyncEngine/SP1), Svelte frontend, pkarr/BEP44 via `harmony_pkarr::PkarrPublisher`.

**Spec:** `docs/specs/2026-06-10-zeb-418-sp2-p2-outbound-hold-design.md` (D11–D18). **Plan-time amendment to D14:** GC removes the outhold dataset **row only**; the sibling's CAS copy is retained — it is legitimate DM-history content the sibling's UI references via the replicated InboxEntry (it would re-fetch the same CID on demand anyway), and no CAS-delete op exists. StorageTier owns blob retention. The spec doc is amended in the same commit as this plan.

**Branch:** `zeb-418-sp2-p2-outbound-hold` (off `e39a3339`). NO worktrees. NO pushes by implementers.

**Per-task gates** (from `src-tauri/`, `set -o pipefail`, commit BEFORE running gates, 10-min wall-clock kill switch per command):
```bash
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(<task scope>)'
```
Integration-test tasks add `cargo nextest run --locked -p harmony-app --features test-fixtures --test <name>`. The final sweep (Task 10) is the only `--all-targets` run. Frontend gates run from the repo root: `npx tsc --noEmit && npx vitest run`.

---

### Task 1: `DmOutholdDoc` CRDT + persist module

**Files:**
- Create: `src-tauri/src/dm_outhold.rs` (mirror `src-tauri/src/dm_inbox_crdt.rs` exactly — header comment, CanonicalPayload registration via the two manual impls, tests at bottom)
- Create: `src-tauri/src/dm_outhold_persist.rs` (mirror `src-tauri/src/dm_inbox_persist.rs`: `DM_OUTHOLD_FILENAME`/`DM_OUTHOLD_REPLAY_FILENAME` consts, `load_doc_or_recover`, `load_replay_or_recover`, `DmOutholdPersist` sink struct implementing the same persist trait `DmInboxPersist` implements)
- Modify: `src-tauri/src/lib.rs` module list (after line 142 `pub mod dm_inbox_persist;`): add `pub mod dm_outhold;` + `pub mod dm_outhold_persist;`

- [ ] **Step 1: Write the doc + failing unit tests**

```rust
/// Key = "{space_id_hex}:{message_cid_hex}" — same composite as DmInboxDoc::key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmOutholdEntry {
    /// The CAS storage blob ([ver][nonce][ct][tag]) — already encrypted.
    #[serde(rename = "pl", with = "serde_bytes")]
    pub storage_blob: Vec<u8>,
    #[serde(rename = "sp")]
    pub space_id: [u8; 16],
    #[serde(rename = "ca")]
    pub created_at: Hlc,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmOutholdDoc {
    #[serde(rename = "en")]
    pub entries: BTreeMap<String, DmOutholdEntry>,
}
```

`key(space_id, message_cid)` identical to `DmInboxDoc::key`. `merge_from(remote) -> MergeOutcome`: insert-only union — same key carries identical content (content-addressed), so existing local entries are NEVER overwritten; `changed` true only for new keys. **Removal does NOT replicate** (state-CRDT): a row deleted locally can resurrect from a stale sibling's publish; that's harmless because the GC sweep (Task 5) re-deletes any row whose outbox entry is terminal — converges since outbox status replicates via OwnerState.

Tests (in-module, mirror `dm_inbox_crdt.rs` test names): `merge_inserts_new_entry_and_is_idempotent`, `merge_never_overwrites_existing_entry`, `merge_unchanged_when_remote_subset`.

- [ ] **Step 2: Run tests, verify fail** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(dm_outhold)'` → FAIL (module missing).
- [ ] **Step 3: Implement; run to green.**
- [ ] **Step 4: Wire-pin fixture test** (same module): canonical-CBOR hex pin of a 1-entry doc, exact style of `butler_deposit.rs`'s `EXPECTED_DEPOSIT_FRAME_HEX` (fixed bytes, no regeneration helper). NEVER regenerate pinned hex once committed.
- [ ] **Step 5: Persist module** — copy `dm_inbox_persist.rs`, rename types/consts/filenames (`dm-outhold-v1.cbor` style matching `DM_INBOX_FILENAME`'s actual pattern — read the const), keep the corrupt-file recovery test.
- [ ] **Step 6: Gates + commit** — `feat(zeb-418-p2): DmOuthold content side-table CRDT + persist`

### Task 2: `FleetNetDoc` CRDT + persist module + selection logic

**Files:**
- Create: `src-tauri/src/fleet_net.rs`
- Create: `src-tauri/src/fleet_net_persist.rs` (same mirror as Task 1 Step 5)
- Modify: `src-tauri/src/lib.rs` module list

- [ ] **Step 1: Write doc + failing tests**

```rust
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetNetDoc {
    /// Keyed by SP1 64-hex device id (same form as DmInboxEntry.deposited_by).
    #[serde(rename = "dv")]
    pub devices: BTreeMap<String, FleetNetRow>,
    /// Owner-level pinned butler device (64-hex), LWW by `pa`.
    #[serde(rename = "pn", default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
    /// LWW stamp for `pinned`. Hlc::default() (zero) when never set.
    #[serde(rename = "pa", default, skip_serializing_if = "is_default_hlc")]
    pub pinned_at: Hlc,
}
```

`merge_from`: per-row LWW by `seen_at` (`Hlc::is_strictly_newer_than`, see `dm_inbox_crdt.rs:merge_from` for the comparison idiom — remote replaces local only when strictly newer); `pinned`/`pinned_at` LWW as a pair by `pinned_at`. `changed` on any row replacement/insert or pin change.

Selection fn (pure, unit-testable — the blob builder calls this on a snapshot):

```rust
/// Ordered butler-set candidates: pinned first (if row fresh), then by
/// most-recent seen_at, self included wherever it falls. Rows with
/// seen_at.wall_ms older than `stale_before_ms` are excluded entirely.
pub fn butler_set_order(doc: &FleetNetDoc, stale_before_ms: u64) -> Vec<(String, FleetNetRow)>
```

Tests: `row_lww_keeps_strictly_newer`, `pin_lww_pair_merges`, `order_pinned_first_then_recency`, `stale_rows_excluded`, `pinned_but_stale_falls_back_to_recency`.

- [ ] **Step 2: Fail → implement → green.** Same wire-pin fixture style as Task 1 Step 4 (doc with 2 rows + pin).
- [ ] **Step 3: Persist module + gates + commit** — `feat(zeb-418-p2): FleetNet device net-info CRDT + persist`

### Task 3: `send_dm` writes the outhold row

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` — `DmOutbox` gains `outhold_doc: Option<Arc<tokio::sync::Mutex<crate::dm_outhold::DmOutholdDoc>>>` + `outhold_notify: Option<Arc<dyn Fn() + Send + Sync>>` (the engine's `notify_dirty`), both `None` in `new`/`new_synthetic`, installed via `pub fn set_outhold(…)` (mirror `set_butler_deposit_client` at `dm_outbox.rs:529`)
- Test: in-module `mod outhold_write_tests`

- [ ] **Step 1: Failing test** — `send_dm_writes_outhold_row_alongside_outbox_entry`: build outbox via the existing test harness (see `send_dm_creates_outbox_entry` at `dm_outbox.rs:2757` for the setup), install an outhold doc, `send_dm`, assert the doc contains `DmOutholdDoc::key(&space_id.0, &message_cid.to_bytes())` with the same `storage_blob` bytes that landed in CAS, and the notify closure fired. Also `send_dm_without_outhold_installed_unchanged` (None → no panic, exact P1 `set_butler_deposit_client` degradation pattern).
- [ ] **Step 2: Implement** — in `send_dm` step 5 (after `cas.put(message_cid, storage_blob).await?`, `dm_outbox.rs:618`): the blob was moved into `cas.put`; clone it ONCE before the put when `outhold_doc.is_some()`. Insert after the `apply_outbox` success arm (so a rejected entry never holds), then call the notify closure. Lock ordering: outhold lock taken AFTER `state` mutations complete, never held across `.await` on CAS.
- [ ] **Step 3: Gates + commit** — `feat(zeb-418-p2): send_dm writes dm-outhold row`

### Task 4: ZEB-422 — deposit candidacy for sent-but-never-acked pairs

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` Phase C (`drain_phase_c`, Ok-arm at `dm_outbox.rs:1016-1037`, candidacy block at `:1056-1090`)
- Modify: `src-tauri/src/butler_deposit.rs` — add `pub const DEPOSIT_NOACK_WINDOWS: u32 = 2;` next to the existing P1 consts (doc comment: ZEB-422, N windows sent-without-ack before the rung fires on the Ok path)

- [ ] **Step 1: Failing tests** (in `mod butler_rung_tests`, reuse `MockDepositClient` at `dm_outbox.rs:3314`):
  - `ok_send_without_ack_accumulates_failure_count`: two drain ticks past backoff with Ok-returning transport and no ack → backoff `failure_count` reaches 2 (expose via existing `#[cfg(test)]` accessors; add `pub(crate) fn failure_count_for(&self, …) -> Option<u32>` if needed).
  - `noack_after_n_windows_triggers_deposit_rung`: Ok-transport, no acks, drain past 2 windows → MockDepositClient received a deposit for the pair; rung ack marks delivered (reuse `deposit_ack_marks_owner_delivered_and_emits_dm_delivered` shape at `:3444`).
  - `first_ok_send_does_not_trigger_rung`: one Ok window → no deposit call.
  - `rung_outcome_never_touches_attempt_state_on_ok_path`: deposit Failed → `AttemptState` identical before/after rung (never-worse, P1 invariant).
- [ ] **Step 2: Implement.** Ok-arm (`:1027`): replace the unconditional `insert(…failure_count: 1)` with entry-API accumulate:

```rust
let st = self.backoff.entry((r.entry_id, r.recipient)).or_insert(AttemptState {
    last_attempt_wall_ms: 0,
    failure_count: 0,
});
let pre_failure_count = st.failure_count;
st.last_attempt_wall_ms = backoff_now_ms;
st.failure_count = st.failure_count.saturating_add(1);
// ZEB-422: sent-but-never-acked candidacy. The pair has completed
// pre_failure_count full backoff windows without an ack; from
// DEPOSIT_NOACK_WINDOWS onward each further window also tries the
// butler rung. Existing Transient-arm candidacy is unchanged; rung
// outcomes never touch the AttemptState written above (spec §4 P2).
if pre_failure_count >= crate::butler_deposit::DEPOSIT_NOACK_WINDOWS
    && self.butler_deposit_client.is_some()
{ /* push deposit candidate — same construction as the Err arm at :1068-1090 */ }
```

Extract the candidate-push into a small private helper so the Ok and Err arms share it (the Err arm keeps its `pre_failure_count >= 1 && Transient` condition). Note the intentional side effect in a comment: direct-send backoff now grows toward the 5-min cap for unresponsive recipients (was pinned at window 1).
- [ ] **Step 3: Check P1 rung tests still green** — `-E 'test(butler) + test(drain)'` (the throttle test `drain_throttles_post_ok_send_until_backoff_elapses` at `:3991` asserts window-1 behavior — update its expectations ONLY if it hard-codes `failure_count == 1` semantics; the throttle itself is unchanged for the first window).
- [ ] **Step 4: Gates + commit** — `feat(zeb-418-p2): ZEB-422 no-ack deposit candidacy + accumulating backoff`

### Task 5: Outhold apply sweeper (CAS insert + GC)

**Files:**
- Create: `src-tauri/src/dm_outhold_apply.rs` — mirror `src-tauri/src/dm_inbox_ingest.rs`'s trigger model: capacity-1 nudge channel, `outhold_nudge_on_applied(tx) -> OnApplied` helper, `run_dm_outhold_sweeper(doc, ctx, nudge_rx, notify_dirty, debounce)` with one startup sweep then debounced sweeps; `pub const OUTHOLD_SWEEP_DEBOUNCE_MS` matching `INGEST_SWEEP_DEBOUNCE_MS`'s value
- Test: in-module with a stub ctx (the `DmInboxIngestCtx` trait-object pattern)

```rust
#[async_trait::async_trait]
pub trait DmOutholdCtx: Send + Sync {
    /// Local CAS admit (RuntimeContentStore::put in prod). Idempotent by CID.
    async fn cas_put(&self, cid: ContentId, blob: Vec<u8>) -> Result<(), String>;
    /// Outbox status for the GC decision: None = entry unknown/deleted.
    async fn outbox_status(&self, space_id: &[u8; 16], message_cid: &ContentId)
        -> Option<DeliveryStatus>;
}
```

Sweep semantics per entry: (1) parse key → (space_id, cid); (2) `outbox_status` → `None | Complete | Expired` ⇒ **remove the row** (GC; resurrection from stale siblings re-deleted next sweep — see Task 1 merge note) and `notify_dirty` so the deletion publishes; (3) otherwise `cas_put` (unconditional — CAS put is idempotent; the originator's copy predates the row and is byte-identical). A `cas_put` failure leaves the row for the next nudge/startup sweep (dirty-latch retry, P1 pattern). Prod ctx (`ProdDmOutholdCtx`) holds `Arc<Mutex<OwnerState>>`-bearing `crdt_state` + `Arc<dyn ContentStore>` — exactly `ProdDmInboxIngestCtx`'s fields (see `lib.rs:3494-3503`); `outbox_status` does ONE short `crdt_state.lock()` (`state.outbox.get(...)` by scanning for the (space_id, message_cid) pair — index by iterating `state.outbox.values()`; the map is keyed by `OutboxEntryId`, not CID — keep the scan, outbox is small and the sweep is debounced).

- [ ] **Step 1: Failing tests** — `sweep_inserts_blob_for_pending_entry`, `sweep_gcs_row_when_outbox_complete`, `sweep_gcs_row_when_outbox_missing`, `sweep_retries_row_on_cas_failure`, `gc_publishes_via_notify_dirty`.
- [ ] **Step 2: Implement → green → gates + commit** — `feat(zeb-418-p2): outhold apply sweeper — CAS admit + status-driven GC`

### Task 6: Engine wiring (start_node + event_loop)

**Files:**
- Modify: `src-tauri/src/lib.rs` — immediately after the dm-inbox engine block (`lib.rs:3424-3530`), construct BOTH new engines with the IDENTICAL recipe (paths from the same `identity_dir`; tags `b"dm-outhold-v1"` / `b"fleet-net-v1"`; `publish_seen: true`; `debounce_ms: DEFAULT_DEBOUNCE_MS`; fresh 64-cap channels; merger closures; persist sinks from Tasks 1–2). dm-outhold gets `on_applied: Some(dm_outhold_apply::outhold_nudge_on_applied(tx))` + spawn `run_dm_outhold_sweeper` with `ProdDmOutholdCtx`; fleet-net gets `on_applied` = snapshot-refresh closure (Task 7 wires the snapshot itself; this task stores the engine + doc Arcs). Add the four NodeState fields (mirror `dm_inbox_*` fields at `lib.rs:877-886`) + the stop_inner take/clear + shutdown calls (mirror `lib.rs:1636-1639` and `:1990-1998`). Install `dm_outbox.set_outhold(doc, notify)` where `set_butler_deposit_client` is installed (grep its call site in `lib.rs`). **Fleet-net self-row upsert:** right after engine construction, lock the doc, upsert `devices[device_id] = FleetNetRow { iroh_endpoint_id, home_relay, seen_at: next_hlc(...) }` from the bound iroh endpoint (same snapshot vars the blob builder uses, `lib.rs:4946-4951`), `notify_dirty` + `flush_now`.
- Modify: `src-tauri/src/event_loop.rs` — add ONE new struct `P2SyncHandles { outhold: DatasetHandles, fleet_net: DatasetHandles }` where `DatasetHandles { addr_hex, outbound_rx, inbound_tx }` (the `DmInboxSyncHandles` shape at `event_loop.rs:72`); ONE new `Option<P2SyncHandles>` run() arg. Zenoh wiring: copy the `dm_inbox_sync_handles` consumption blocks (grep `dm_inbox_sync_handles` in `event_loop.rs`) for topics `harmony/owner/{addr}/ds/dm-outhold-v1` and `…/ds/fleet-net-v1`.
- Modify ALL `event_loop::run` call sites: production caller in `lib.rs` passes the handles; **grep `tests/` for every direct `event_loop::run(` caller and add the `None` arg with the standard comment** (`None, // ZEB-418 P2: p2_sync_handles not exercised in this test`). P1 round taught us: 9 call sites across 5 integration files — find them ALL before declaring done (`grep -rn "event_loop::run(" tests/ src/`).

- [ ] **Step 1: Engine-wiring proof test** — exactly the P1 Task 7 shape (see plan note in `docs/plans/2026-06-09-zeb-418-sp2-p1-inbound-deposit-plan.md` Task 7 / `notes_commands.rs:331-384`): construct each engine as start_node does, insert an entry, `notify_dirty` + `flush_now`, assert a publish frame on the outbound channel. One test per dataset.
- [ ] **Step 2: Implement wiring; compile ALL targets** — `cargo nextest list --locked -p harmony-app --features test-fixtures --all-targets` (list, not run — catches integration-test signature breaks in minutes, the P1 lesson).
- [ ] **Step 3: Gates + commit** — `feat(zeb-418-p2): wire dm-outhold + fleet-net engines through start_node/event_loop`

### Task 7: Snapshot + blob-builder secondary entry + refresh triggers

**Files:**
- Modify: `src-tauri/src/lib.rs` — `FleetNetSnapshot`: `Arc<std::sync::RwLock<crate::fleet_net::FleetNetDoc>>` (std RwLock — the blob builder is a sync closure). Built before the blob builder; fleet-net engine's `on_applied` + the self-row upsert path both write a fresh doc clone into it. Blob builder (`lib.rs:4952-5018`): replace the self-only `butler_set` vec with `crate::fleet_net::butler_set_order(&snapshot.read(), now - BUTLER_SET_FRESHNESS_MS)` mapped to `ButlerSetEntry` (take 2; `vk` per device from `owner_device_cache` — the cache Arc is available in this scope; devices without a cache entry are skipped with a `tracing::debug!`), falling back to the P1 self-only entry when the snapshot yields nothing (boot, single-device). `pinned: doc.pinned.as_deref() == Some(device_id)` per entry.
- Modify: `src-tauri/src/butler_deposit.rs` — `pub const BUTLER_SET_REFRESH_MS: u64 = BUTLER_SET_FRESHNESS_MS / 2;` + `pub const FLEET_CHANGE_REPUBLISH_DEBOUNCE_MS: u64 = 60_000;`
- Modify: `src-tauri/src/lib.rs` + `src-tauri/src/event_loop.rs` — refresh triggers: build `routing_republish: Arc<dyn Fn() + Send + Sync>` in start_node that `tokio::spawn`s re-`enable()`/re-register of the ACTIVE case publishers (identity publisher only when discoverability is enabled — reuse whatever flag gates the original `enable()` call; friend slots already reconcile via the existing reachability tick — verify and note). `harmony_pkarr::PkarrPublisher::register` (publisher.rs:93) sets `next_publish_at = now` + wakes the driver, so re-register IS an immediate re-publish — no harmony-repo change. Event loop: re-publish every `BUTLER_SET_REFRESH_MS` (extend the existing 250ms-tick counter pattern — see the "every 20 timer ticks" peer-refresh at `event_loop.rs:~2698`), plus on fleet-net snapshot change (selection-relevant: row set, pin, relay) re-publish after a 60s debounce (single pending flag + deadline, NOT a queue).

- [ ] **Step 1: Failing unit tests** — blob-builder-level: `blob_builder_emits_sibling_secondary_from_snapshot` (snapshot with 2 fresh rows → 2 entries, fleet-global order), `blob_builder_falls_back_to_self_when_snapshot_empty`, `pinned_device_leads_butler_set`. Test the pure pieces (`butler_set_order` already covered; here pin a tiny harness around the mapping closure if the full builder is untestable — extract the mapping into `pub(crate) fn build_butler_set(snapshot: &FleetNetDoc, self_entry: ButlerSetEntry, vk_lookup: impl Fn(&str) -> Option<[u8;32]>, now_ms: u64) -> Vec<ButlerSetEntry>` in `fleet_net.rs` and test THAT; lib.rs calls it).
- [ ] **Step 2: Implement; debounce test** — `fleet_change_republish_debounces` (two snapshot changes 1s apart → one republish scheduled) — test the debounce state machine as a pure struct if the event-loop hook resists unit testing.
- [ ] **Step 3: Gates + commit** — `feat(zeb-418-p2): fleet-net snapshot drives butler-set secondary + refresh triggers`

### Task 8: Pin-a-butler IPC + device-admin UI

**Files:**
- Modify: `src-tauri/src/lib.rs` — `#[tauri::command] async fn set_butler_pin(device_id: Option<String>, …)`: writes `pinned`/`pinned_at` through the fleet-net doc + `notify_dirty` + `flush_now`; rejects device ids not in the enrolled set (same enrolled snapshot recipe as `dm_inbox_enrolled`, `lib.rs:3484-3490`). Extend the existing device-listing IPC payload (find it: `grep -n "tauri::command" src/lib.rs` then locate the command `DevicesPanel.svelte` invokes) with `butler_pinned: bool` per device. Register the command in the `invoke_handler` list.
- Modify: `src/lib/components/DevicesPanel.svelte` + its service module — a "Always-on butler" toggle per device row: single-select (pinning one unpins the previous — server-side LWW already guarantees it; UI reflects via re-fetch), low-risk so NO confirmation dialog. IPC params camelCase (`deviceId`), error extraction via `e instanceof Error ? e.message : String(e)`.
- Test: component/service test next to the existing DevicesPanel/device-label tests (`src/lib/device-label-service.test.ts` shows the house service-test shape); Rust-side `set_butler_pin_rejects_unknown_device` + `set_butler_pin_roundtrip` unit tests against the doc.

- [ ] **Step 1: Rust failing tests → implement → green.**
- [ ] **Step 2: Frontend toggle + test;** run `npx tsc --noEmit && npx vitest run` from repo root.
- [ ] **Step 3: Gates + commit** — `feat(zeb-418-p2): pin-a-butler setting — IPC + device-admin toggle`

### Task 9: Two-engine integration test (fleet handoff → deposit → ingest)

**Files:**
- Create: `src-tauri/tests/butler_outhold_integration.rs` — base it on `src-tauri/tests/butler_deposit_integration.rs` (P1 Task 9; its harness builds two `FleetSyncEngine`s + the P1 acceptor end-to-end)

- [ ] **Step 1: The headline-path test** — `sibling_completes_delivery_via_butler_after_originator_stops`:
  1. Owner-A engines A1+A2 with outhold datasets bridged (the P1 two-engine bridge); A1 `send_dm` → outhold row + outbox entry replicate to A2 (assert A2's doc has the row, A2's stub CAS got the blob via the sweeper).
  2. Stop bridging A1 (originator "offline").
  3. Drive A2's drain with an Ok-no-ack transport past `DEPOSIT_NOACK_WINDOWS` (ZEB-422 path) with a deposit client pointed at owner-B's P1 acceptor (the in-process acceptor harness from `butler_deposit_integration.rs`).
  4. Assert: deposit accepted + persisted on B, ack marked the entry delivered on A2, delivered state present in A2's OwnerState.
  5. GC: after Complete, run A2's outhold sweep → row removed.
The spec §9 "sibling direct-delivery variant" is covered by step 1.1's CAS-presence assertion (the necessary condition for fetch-back serving) plus the existing serve-queryable machinery tests (ZEB-343/P1); the full direct-path round-trip is exercised end-to-end by the manual cross-WAN proof post-merge.

- [ ] **Step 2: Run** — `cargo nextest run --locked -p harmony-app --features test-fixtures --test butler_outhold_integration` → green.
- [ ] **Step 3: Commit** — `test(zeb-418-p2): two-engine fleet-handoff deposit integration test`

### Task 10: Final sweep + docs

- [ ] **Step 1:** `cargo fmt --all -- --check` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (expect ZEB-420 `rename_content_integration` port-4242 flakes possible locally — pre-existing, NEVER chase) + frontend gates from repo root.
- [ ] **Step 2:** Spec amendments (if any deviations surfaced) + this plan's "Implementation notes" section; verify the D14 row-only amendment landed in the spec.
- [ ] **Step 3:** Commit — `chore(zeb-418-p2): final sweep + docs`

---

**Out of scope for P2 (spec §8 P1 doc):** group DMs/community backfill (P3), relay + UCAN/PoW (P4), rotating rendezvous, DM-history dataset migration (D6), CAS retention changes.

**PR body rules:** NO closing keywords for ANY ticket; reference ZEB-418 P2 + ZEB-422 by plain text only (Linear cascade closes title/branch-named tickets regardless — reopen ZEB-418 post-merge, ZEB-422 SHOULD close). NEVER write `@greptile`.
