# ZEB-710: owner-state durability residue (post-ZEB-703/708/709)

Branch `zeb-710-durability-residue` off `main@024ad3a1`. One bundled PR closing ZEB-710.
Four items from the ticket; all hardening/cleanup — no known user-visible loss path.

## Verified current state

- **V1** `FleetSyncEngine::persist_now` (fleet_sync.rs:424) routes through `persist_now_tx`
  → the single `select!` task (arm at :665). `publish_root_now` awaits `publisher_tx.send()`
  INLINE in the loop (:753) — a wedged send starves every arm, including `persist_now_rx`,
  `flush_now_rx`, and `shutdown_rx`. The three production persist callers
  (`/v1/shutdown` pre-ack flush, `fence_owner_state_flush`, `stop_inner` backstop)
  accept bounded-timeout → WARN as today's degraded mode.
- **V2** The existing test `persist_now_persists_without_publishing_under_saturated_publisher`
  (:1470) saturates the publisher while the task is IDLE — the task still services
  `persist_now_rx`. The uncovered scenario is the task wedged INSIDE the send
  (dirty debounce fired against a full channel). That is the red test.
- **V3** The handle already holds `replay_tracker` (:336); `Ctx` holds
  `state: Arc<Mutex<S>>` (:526) and `persist: Arc<dyn FleetPersist<S>>` (:532) — both
  come from `Config` Arcs, so the handle can hold clones with no ownership redesign.
- **V4** `S: CanonicalPayload` (sealed, `: serde::Serialize`) with
  `canonical_cbor_encode` (owner_state_crypto.rs:693) gives deterministic bytes to hash
  for the tripwire.
- **V5** In-task mutation site: `process_inbound` → `Applied` → `persist_now(ctx)`
  (fleet_sync.rs:953). Remote merges are exempt from the notify discipline
  ([[reference_owner_state_crdt_notify_dirty]]) — the tripwire must mark these
  accounted or it false-positives on every inbound merge.
- **V6** Item-4 WARN sites: Phase-C fence exhaustion (dm_outbox.rs:3116,
  `try_acquire_owned` Err → WARN + skip) and stop_inner outbox `try_lock` contention
  (lib.rs:2688 → WARN + no-fence). Health-counter precedent: additive source structs
  of `AtomicU64` registered via `set_*_source` (network_health.rs:980
  `set_butler_deposit_source`; `DialTelemetry` :272).

## D1 — direct-persist seam (item 1)

- New shared `persist_sink: Arc<tokio::sync::Mutex<()>>` held by BOTH `Ctx` and the handle.
- Extract the free `persist_now(&ctx)` body into a shared fn parameterized over
  `(state, replay_tracker, persist, sink)`: **lock sink → snapshot state+tracker →
  spawn_blocking write → unlock**. Snapshotting UNDER the sink lock makes concurrent
  persists strictly ordered (a later acquirer snapshots later state), so no
  older-clobbers-newer torn write.
- Handle `persist_now()` becomes DIRECT (no channel round-trip). Remove
  `persist_now_tx`/`persist_now_rx` + the select arm — dead once the only sender goes
  direct. `owner_state_sync::SyncEngine::persist_now` delegates unchanged; all three
  production callers benefit with zero call-site changes. `flush_now`/`shutdown` stay
  task-routed (they need the publish leg).
- Red test: capacity-1 publisher, saturate, `notify_dirty` + elapse debounce so the task
  wedges in `publisher_tx.send()`, then `timeout(2s, engine.persist_now())` → must be
  `Ok(Ok(()))`. RED today (starves), GREEN with the seam.

## D2 — dirty-window tripwire (item 2)

- `#[cfg(any(test, feature = "test-fixtures"))]` fields shared by handle+Ctx:
  `tripwire_dirty_seen: Arc<AtomicBool>`, `tripwire_last_hash: Arc<StdMutex<Option<u64>>>`.
- `notify_dirty()` sets `tripwire_dirty_seen`. `process_inbound` marks it too before its
  persist (the documented remote-merge exemption, V5).
- In the shared persist fn (under the sink lock, post-snapshot):
  hash `canonical_cbor_encode(&state_snap)`; if hash != last AND `!dirty_seen.swap(false)`
  → `tracing::error!` + `debug_assert!` ("un-notified owner-state mutation window").
  Consume the flag ONLY on hash change (a stale flag must not mask a later un-notified
  mutation). First persist (`last == None`) never fires.
- Tests (via the D1 direct seam so the panic lands on the caller's stack):
  `#[should_panic]` mutate-without-notify → `persist_now()`; positive twin with
  `notify_dirty()` → no panic.
- Catches the ZEB-703 class generically for FUTURE mutation sites; the per-site
  dirty-count pins from #487 remain the current-surface guard.

## D3 — orphaned CidNotify handler deletion (item 3)

`DmOutbox::handle_unicast` (dm_outbox.rs:1833) + `handle_cidnotify_lifted` (:2040):
zero production callers post-Reticulum-teardown (ZEB-709 audit). Live path =
`dm_inbox_ingest::ingest_dm_packet`, which shares the extracted admission/verify helpers.
CAUTION: `community_invite::handle_unicast` is a different, LIVE function — untouched.

Deletion set / port set / keep set: mapped by cascade recon (agent report appended to
PR notes). Principle: delete driver-plumbing tests with the drivers; port any test
pinning real verify/decrypt/admission behavior not already covered on the live path
(`ingest_dm_packet` or shared-helper unit tests); never delete a property pin without
a named surviving equivalent. Integration files in scope:
`tests/dm/dm_cert_identity_integration.rs`, `tests/dm/dm_revocation_cutoff_integration.rs`.

## D4 — fence degraded-mode visibility (item 4)

- New `DmFenceStats { phase_c_saturated_skips: AtomicU64, stop_fence_skipped_contended: AtomicU64 }`
  created at boot; Arc clones to (a) the drain-tick caller → `drain_lifted` param for the
  :3116 arm, (b) NodeState for the lib.rs:2688 arm (recordable WITHOUT the outbox lock —
  that arm fires precisely when the lock is contended), (c) network_health registry via
  `set_dm_fence_source` → `NetworkHealthSnapshot.dm_fence: Option<DmFenceHealth>`
  (additive, camelCase DTO like `ButlerDepositHealth`). WARNs stay; counters add
  cross-restart-visible-while-running wedge signal.
- Tests: unit-drive both arms (saturate the semaphore → tick → counter increments;
  contended try_lock at stop → counter increments) — extend the existing zeb703 fence tests.

## Gates

Red-first for D1/D2/D4 counters; D3 is deletion+port (ports red-first where a property
lacks live coverage). fmt, clippy `--all-targets`, `scripts/test-select --context task`
per task, `--context round` + full sweep before PR. PR body: "Closes ZEB-710."
