# ZEB-919 Epoch-Pin Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move presence, address-book, and open-join-acceptor key derivation off the engine's spawn-pinned `membership_key()` onto live epoch-key reads (publisher-degrades) with previous-epoch open candidates, per the ZEB-919 spec.

**Architecture:** Reuse `live_epoch_key` / `epoch_key_candidates` (ZEB-249/918). Add one typed publisher helper. Extract candidate-open loops as pure functions so rotation tests need no zenoh/iroh harness.

**Tech Stack:** Rust (src-tauri), cargo-nextest, existing OwnerState/Space test fixtures (`community_state_sync.rs:12398+`).

## Global Constraints

- Gates from `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; tests via `scripts/test-select --context task` (repo root) per task; full `--workspace --all-targets --features test-fixtures` sweep before PR.
- No wire-format / CRDT-schema / IPC changes.
- Spec: `docs/superpowers/specs/2026-08-12-zeb919-epoch-pin-audit-design.md`.

---

### Task 1: `community_publish_epoch_key_typed`

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (next to `community_publish_epoch_key`, ~:3353; tests next to `candidates_*` fixtures ~:12398)

**Interfaces:**
- Produces: `pub(crate) async fn community_publish_epoch_key_typed(community_id: SpaceId, crdt_state: Option<&Arc<Mutex<crate::owner_state_crdt::OwnerState>>>, fallback: &EpochKey) -> EpochKey`
- `community_publish_epoch_key` becomes a delegating wrapper (bytes = `*typed(...).as_bytes()`; its signature keeps `&Arc<...>` — wrap with `Some(..)`).

- [ ] **Step 1: Failing tests** (mirror the `candidates_*` fixture shape): `typed_key_none_crdt_falls_back_to_spawn`, `typed_key_reads_live_current`, `typed_key_incomplete_space_degrades_to_spawn`.
- [ ] **Step 2: Run** `scripts/test-select --dry-run` sanity, then targeted `cargo nextest run --locked --features test-fixtures -E 'test(typed_key_)'` → FAIL (unresolved fn).
- [ ] **Step 3: Implement** — body is `match live_epoch_key(community_id, crdt_state, fallback).await { Ok((k, _)) => k, Err(_) => fallback.clone() }` with `None` handled inside `live_epoch_key` already; delegate the bytes fn.
- [ ] **Step 4: Targeted run** → PASS; clippy + fmt.
- [ ] **Step 5: Commit** `ZEB-919: add community_publish_epoch_key_typed (live publisher key, typed)`

### Task 2: Presence live keys

**Files:**
- Modify: `src-tauri/src/community_presence.rs` (`:442-493` publisher, `:527-620` subscriber, module doc `:424-431`, tests `mod tests` `:640+`)
- Modify: `src-tauri/src/event_loop.rs:4232,4249` (thread `Some(crdt_state)`; the enclosing scope's owner-state Arc is available as `EventLoopCtx.crdt_state`)

**Interfaces:**
- Both spawn fns gain `crdt_state: Option<Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>>` (last param before `closing`-group to keep diffs tight; `None` = documented spawn-key degraded mode).
- Produces: `pub(crate) fn open_presence_with_any(mks: &[EpochKey], community: &SpaceId, bytes: &[u8]) -> Option<SignedPresenceBeacon>` — derives `derive_presence_key` per candidate, tries `open_presence_beacon`, first success wins.

- [ ] **Step 1: Failing tests**: `open_presence_with_any_previous_candidate_opens_old_sealed` (seal under OLD, open with `[NEW, OLD]`), `open_presence_with_any_rejects_unrelated_key` (`[NEW]` only → None), `publisher_seals_under_live_key_when_rotated` (call the per-tick key selection: `community_publish_epoch_key_typed` with a rotated OwnerState fixture, assert derived key opens with NEW not OLD).
- [ ] **Step 2: Targeted run** → FAIL.
- [ ] **Step 3: Implement** — publisher tick: replace `let mk = engine.membership_key();` with `let mk = crate::community_state_sync::community_publish_epoch_key_typed(community, crdt_state.as_ref(), &engine.membership_key()).await;`. Subscriber packet: `let mks = crate::community_state_sync::epoch_key_candidates(community, crdt_state.as_ref(), &engine.membership_key()).await;` then `open_presence_with_any(&mks, ...)`. Rewrite the false "follows epoch rotation automatically" comments (module doc + both loop comments) to describe live-read + candidates + degraded mode. Thread `Some(Arc::clone(&...crdt_state))` at both event_loop call sites; fix any other callers the compiler surfaces (tests pass `None`).
- [ ] **Step 4: Targeted run + clippy + fmt** → PASS.
- [ ] **Step 5: Commit** `ZEB-919: presence seals under the live epoch key, opens [current, previous]`

### Task 3: Address book live keys

**Files:**
- Modify: `src-tauri/src/address_book_sync.rs` (`ingest_sealed_packet` `:335`, `spawn_addrbook_subscriber` `:627`, `spawn_addrbook_snapshot_queryable` `:747`, `request_snapshot_once` `:794`, `spawn_addrbook_snapshot_requester` `:922`, tests `:1030+`)
- Modify: `src-tauri/src/event_loop.rs:4448,4455,4466` (thread `Some(crdt_state)`)
- Modify: `src-tauri/src/lib.rs:9798` (announce arm) and `:12596` (relay arm): replace `derive_addrbook_key(&engine.membership_key(), &c)` with derive over `community_publish_epoch_key_typed(c, Some(&crdt_state_handle_in_scope), &engine.membership_key()).await` — the announce arm's block already locks `crdt_state`; the relay arm holds `slot_crdt_state` (ZEB-918).

**Interfaces:**
- Produces: `pub(crate) fn open_records_with_any(mks: &[EpochKey], community: &SpaceId, packet: &[u8]) -> Option<Vec<AddressBookRow>>`.
- `ingest_sealed_packet`, the three spawn fns, and `request_snapshot_once` gain a `crdt_state: Option<Arc<tokio::sync::Mutex<OwnerState>>>` param (reference flavor `Option<&Arc<...>>` on the two non-spawn fns).

- [ ] **Step 1: Failing tests**: `open_records_with_any_previous_candidate_opens_old_sealed`, `open_records_with_any_rejects_unrelated_key`, `snapshot_seal_key_follows_rotated_live_state` (typed helper + `derive_addrbook_key` against rotated fixture opens NEW-sealed packet).
- [ ] **Step 2: Targeted run** → FAIL.
- [ ] **Step 3: Implement** — ingest: candidates + `open_records_with_any`; queryable serve: typed helper before `derive_addrbook_key`; thread params through the spawn fns → event_loop passes `Some`; lib.rs arms as above. Update the `:618` and `:763` doc comments (same false-claim class as presence).
- [ ] **Step 4: Targeted run + clippy + fmt** → PASS.
- [ ] **Step 5: Commit** `ZEB-919: address-book seals under the live epoch key, ingests [current, previous]`

### Task 4: Open-join acceptor verifies against the live key

**Files:**
- Modify: `src-tauri/src/iroh_invite_acceptor.rs:707-717` (+ a doc block stating the posture decision)

**Interfaces:**
- Consumes: `community_publish_epoch_key_typed` (Task 1) with `Some(&self.crdt_state)`.
- No new public surface; `verify_and_admit_open_join` untouched (key already a parameter).

- [ ] **Step 1: Failing test** — in the acceptor's test module (or `community_state_sync` tests if the acceptor has none): `admission_key_is_live_current_not_spawn_pin`: rotated OwnerState fixture + spawn-key fallback → helper returns NEW; assert `verify_epoch_auth` minted under OLD fails against it and minted under NEW passes (imports from `open_join_admit`).
- [ ] **Step 2: Targeted run** → FAIL (test asserts against current pinned behavior via new wiring; write it against the helper so it fails only until wiring exists — if it passes trivially, assert on the acceptor's actual key-choice expression by extracting `fn admission_epoch_key` locally and calling it).
- [ ] **Step 3: Implement** — `let epoch_key = crate::community_state_sync::community_publish_epoch_key_typed(community_id, Some(&self.crdt_state), &engine.membership_key()).await;` + doc block: current-only hard cut (ZEB-911/918 posture), degrade-to-spawn rationale, why no previous-key rung (spec §4).
- [ ] **Step 4: Targeted run + clippy + fmt** → PASS.
- [ ] **Step 5: Commit** `ZEB-919: open-join acceptor verifies capability against the live epoch key (hard cut)`

### Task 5: Hygiene sites + stale TODO

**Files:**
- Modify: `src-tauri/src/lib.rs:36677` (create hook), `:42474` (redeem hook): replace `let mk = engine.membership_key(); ... *mk.as_bytes()` with `community_publish_epoch_key(...)` (bytes flavor; both scopes hold a crdt/state handle — create's `state` lock exposes it, redeem likewise; pass the engine key as fallback).
- Modify: `src-tauri/src/lib.rs:53754-53760`: delete the stale `TODO(zeb-249-followup)` block, leaving the accurate comment below it.

- [ ] **Step 1:** No new tests (sites are spawn==live by construction; behavior pinned by Task 1's helper tests). Make the edits.
- [ ] **Step 2:** `grep -n "membership_key()" src/` — assert the only remaining direct consumers are: fallback args to live helpers, channel-log family (deferred ticket), and `community_state_sync.rs` internals. Paste the grep into the commit message body.
- [ ] **Step 3: Targeted run (`--context task`) + clippy + fmt** → PASS.
- [ ] **Step 4: Commit** `ZEB-919: normalize case-C registration hooks to the live key; drop stale rotation TODO`

### Task 6: Follow-up ticket, full sweep, PR

- [ ] **Step 1:** File the Linear follow-up for the channel-log key family (spec §5): live-key provider through `ChannelLogRegistry::spawn` + re-key on rotation + decrypt candidates; cite `lib.rs:9037/32449/32507`, wire-only impact, plaintext-at-rest finding. Use the REAL ticket ID thereafter (never invent one).
- [ ] **Step 2:** Full sweep from `src-tauri/`: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; clippy `--all-targets`; `cargo fmt --all -- --check`; `git status` clean.
- [ ] **Step 3:** Push branch; open PR (`Closes ZEB-919`, spec/plan links, verdict table summary, follow-up ticket reference, standard footer); fire `@coderabbitai review` once.
