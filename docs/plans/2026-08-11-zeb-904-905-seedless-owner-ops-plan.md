# ZEB-904/905 Implementation Plan — seedless owner-ops decoupling + honest degradation

> **For agentic workers:** execute task-by-task; each task ends with a scoped
> test cycle. Spec: `docs/specs/2026-08-11-zeb-904-905-seedless-owner-ops-design.md`

**Goal:** a device with owner state on disk but no master seed and no
fleet-KeyTree material boots into local-only mode (communities/channels/local
ops fully working, fleet replication + friend features honestly disabled) with
a boot `warn!`, a `fleetCryptoMissing` response flag, and a non-blocking banner.

**Branch:** `zeblith/zeb-904-905-seedless-owner-ops`

## Global constraints

- Cargo from `src-tauri/`, always `--locked --features test-fixtures`; clippy
  `--all-targets --no-deps -D warnings`; `cargo fmt --all -- --check`; frontend
  `npx tsc --noEmit` + `npx vitest run` from repo root.
- Seeded-boot behavior must be byte-identical: no reordering of construction,
  no changed inputs — only scope re-layering.
- Never eye-count braces in the lib.rs move: anchor every edit at a use-site,
  validate with `cargo check` after each band move (scope-by-compiler).
- Commit per task; `scripts/test-select --context task` for iterative gates;
  full sweep only at Task 7.

---

### Task 1: keyless engine mode in `fleet_sync.rs` (+ plumbing)

**Files:** `src/fleet_sync.rs`, `src/owner_state_sync.rs`, callers constructing
`FleetSyncConfig` (`fleet_net.rs`, `owner_quorum_sync.rs`, `notes_commands.rs`,
`dm_outhold_apply.rs`, lib.rs engine constructions — wrap existing `keys` in
`Some(..)`).

1. `FleetSyncConfig.keys: Option<FleetKeySet>`; `Ctx.keys: Option<FleetKeySet>`.
2. `encode_root_wire`: `None` → typed skip (new internal outcome, not an error
   that callers escalate). Callers (debounce publish arm, `flush_now`, shutdown
   publish, root-serve arm) log debug + continue; **persist must still run**
   after the skipped publish (assert in test).
3. `handle_incoming_publish`: `None` → drop at debug (count in sync_stats if a
   dropped-counter exists; do not add new stats plumbing).
4. `owner_state_sync::SyncEngine::new` takes `Option<FleetKeySet>`; the lib.rs
   call site passes the boot-computed Option (Task 2 wires it; interim:
   `Some(keys)` to keep compiling).
5. Tests (fleet_sync in-file or owner_state_sync tests, following existing
   engine-test patterns): keys=None → (a) notify_dirty → both sidecar files
   written; (b) flush_now/shutdown return Ok, publish channel receives nothing;
   (c) inbound frame → Dropped, no panic; (d) root-serve request → no reply, no
   panic. keys=Some regression: existing tests untouched.
6. Gate: `scripts/test-select --context task` + `-E 'test(fleet_sync)'`
   targeted run. Commit.

### Task 2: lib.rs boot restructure — owner band hoist + fleet band gate

**Files:** `src/lib.rs` (`start_node_inner`, ~5613–12473)

1. Hoist the owner band (gate entry through SyncEngine + `sync_handles_opt`,
   currently ~5616–5849) to the outer `if let Some(ref loaded) = owner_loaded`
   scope. SyncEngine receives `fleet_crypto.as_ref().map(|(_, keys)| keys.clone())`.
2. Wrap the fleet band (BOOT-PROBE 02→08, mint through fleet-keys carrier +
   install task, ~5851–7377) in `if let Some((kt, keys)) = fleet_crypto.clone()`;
   `else` → `info!("skipping fleet engines: no fleet crypto")`.
3. Leave the community/iroh band inside the (now outer-only) owner scope;
   resolve every compile error from a band-2 output consumed downstream with an
   explicit `if let` skip on the pre-declared `Option` (no unwraps, no
   reordering). Known edges to expect: fleet-net snapshot refresh task,
   quorum-sweep carrier slot, ProdDmInboxIngestCtx (stays in band 2).
4. Add the ZEB-904 `warn!` in `fleet_crypto` construction (owner present, seed
   None, material None) with restore guidance.
5. Gate: `cargo check` after each band move; then
   `scripts/test-select --context task`. Commit.

### Task 3: six kt-sites — Option-threading + honest friend-op errors

**Files:** `src/lib.rs` (Case-D ×2, routing_republish closure, friend acceptor
wiring, friend broker, `keytree_for_state`), `src/iroh_friend_acceptor.rs` /
friend-token redeem guard sites.

1. Friend acceptor + broker `.with_owner_keytree`: wire only when
   `fleet_crypto` present (acceptor unwired = inbound friend handshakes
   refused at connection level — acceptable, logged once at info).
2. Case-D reconcile sites + routing_republish epoch-window steps: skip at debug
   when no keytree; the rest of routing republish (address-book publishes)
   keeps running.
3. `NodeState.owner_keytree` consumer audit (`grep owner_keytree`): every
   IPC-reachable `None` arm returns the honest friend-copy ("friend features
   need this device's sync keys — restore your recovery phrase to re-enable"),
   NOT owner-not-loaded copy. New const beside `OWNER_NO_IDENTITY_MSG`.
4. Tests: keyless `redeem_friend_token` (or the cheapest owner_keytree-gated
   IPC) returns the new message. Gate + commit.

### Task 4: `fleetCryptoMissing` response flag + pins

**Files:** `src/lib.rs` (`StartNodeResponse`, populate site, serialization pin
tests)

1. `pub fleet_crypto_missing: bool` on `StartNodeResponse` (camelCase serde
   already at struct level); populate from `owner_loaded.is_some() &&
   fleet_crypto.is_none()` (snapshot next to `has_owner_identity`).
2. Update the two pin tests (exact-key-count + camelCase name).
3. Gate + commit.

### Task 5: frontend — types + banner

**Files:** `src/lib/types/onboarding.ts`, new
`src/lib/components/FleetSyncDisabledBanner.svelte`, `src/App.svelte`, vitest
specs beside existing banner tests.

1. Add `fleetCryptoMissing?: boolean` to the StartNodeResponse DTO type.
2. Banner (pattern: BackupReminderBanner): shown when flag true; copy per spec
   §3.4 (device sync + friends paused; communities work; restore to re-enable);
   dismissible for the session; links/points to the existing restore-from-phrase
   flow.
3. `App.svelte`: hold the flag from the awaited start_node response (beside the
   ZEB-836 classification site), render banner. `classifyOwnerIdentity`
   untouched.
4. Tests: vitest — banner renders iff flag; dismiss works; tsc clean. Commit.

### Task 6: seedless boot integration test

**Files:** integration test beside existing start_node/profile boot tests (reuse
the cheapest existing harness that boots a real node with a prepared identity
dir).

1. Fixture: identity dir with valid `owner_state.cbor` (self-enrolled device),
   NO `master_seed.enc`, NO `fleet_keytree.enc` (mirror
   `install_joiner_state_inner`'s no-material output; reuse pairing persist
   helpers if exported for tests).
2. Assert: start succeeds; `fleet_crypto_missing == true`; `create_community`
   succeeds; stop; restart; community still present (persistence proof);
   BOOT warn line emitted (if log capture is cheap — otherwise skip log
   assertion).
3. Gate + commit.

### Task 7: full gates + PR + convergence

1. Full sweep: nextest workspace `--all-targets`, clippy `--all-targets`, fmt,
   tsc + vitest, MSRV check; `git status` clean (working tree == commit).
2. Push branch; open PR (body: closes ZEB-904, closes ZEB-905; capability
   table from spec §3.1); fire single `@coderabbitai review`; converge per
   protocol (all three comment buckets, bundle fixes, one push per round);
   Pushover at ready.
