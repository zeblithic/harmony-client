# ZEB-583: channel-name-uniqueness guard in create_channel

**Goal:** Stop `create_channel` from appending a duplicate-named channel. `create_community` auto-seeds `#general`; an explicit `create_channel("general")` afterward currently produces a second `#general` because `ChannelCreate` dedups only by random `channel_id`, never by name.

**Design (settled — Approach A, hard-error):** Add an IPC-level name-uniqueness guard for `create_channel`: `Err` if a **live** (non-tombstoned) channel already has the same **normalized** name (`trim().to_lowercase()`). Fast-fail exactly like the existing empty-name/length validations.

**Atomicity (PR #368 review — CodeRabbit):** the check must be **atomic with the append**, or two concurrent local `create_channel` IPC calls can both observe "no duplicate" and both succeed (TOCTOU). So the check does NOT run as a separate `engine_arc.state()` lock in `create_channel_impl`; instead it runs **inside the same `state` lock guard that commits the event** — a new `CommunitySyncEngine::insert_local_channel_create` threads an `Option<LocalInsertPrecheck>` into the shared `insert_event_with_resolved_pubs` body and runs the name check immediately before `insert_event`, under one lock acquisition. The two pre-existing insert callers pass `None` (zero behavior change). A duplicate surfaces as `LocalInsertError::DuplicateChannelName { display }`, whose `Display` is the user-facing message.

**Why IPC-only, not a CRDT/verify gate:** `verify_event` for `ChannelCreate` *deliberately* does not reject duplicates (`community_membership.rs:3262`) — a receive-order-dependent verify-time rejection would make replica A accept what replica B rejects → log divergence. So name-uniqueness must NOT be a verify gate. This guard is a local UX fast-fail (consistent with the other IPC validations, which `verify_event` also re-checks for defense-in-depth). It deterministically fixes the reported sequential operator-flow bug. A rare concurrent *cross-device* same-name create still materializes as a cosmetic dup (same as today) — documented as a follow-up; the convergent materialize-time fix is out of scope.

**Tech Stack:** Rust, tokio. All cargo from `src-tauri/`.

## Global Constraints

- Normalization: `name.trim().to_lowercase()` (so `"General"`, `" general "`, `"general"` collide; matches the auto-seed's lowercase `"general"`).
- Live-only: compare against channels with `deleted_at.is_none()` (a deleted channel's name is reusable).
- Placement: the check runs **under the same `state` lock as the append** (inside `insert_event_with_resolved_pubs`, immediately before `insert_event`), reached via `engine_arc.insert_local_channel_create(event, normalized, display)`. `name` is moved into `mint_channel_create_event`, so capture the normalized + display name **before** the mint. On the (rare) duplicate path the HLC reservation + mint are wasted — documented-as-fine HLC-burn.
- Error message: `"a channel named '<trimmed>' already exists in this community"` (String, matching the IPC's `Result<String, String>`).
- Local gates green before PR: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, scoped nextest then the `--all-targets` sweep.

---

## Task 1: Failing test — duplicate name rejected (TDD red)

**Files:**
- Test: `src-tauri/src/lib.rs` (`#[cfg(test)] mod tests`, near `create_channel_eagerly_spawns_channel_log_engine_for_immediate_post`)

- [ ] **Step 1:** Mirror the eager-spawn test scaffold: `build_create_community_test_fixture()` → `create_community_inner("dup-name-community", false, …)` (auto-seeds `#general`) → owner-loaded `NodeState` with both registries + `dm_outbox`.
- [ ] **Step 2:** Assert `create_channel_impl(&node_state, community_id_hex, "general", 0, None)` returns `Err` whose message contains `"already exists"`. Assert `create_channel_impl(.., " GENERAL ", ..)` also `Err` (normalization). Assert `create_channel_impl(.., "team-chat", ..)` returns `Ok` (distinct name still works).
- [ ] **Step 3:** Run `cargo nextest run -p harmony-app --lib --features test-fixtures -E 'test(create_channel_rejects_duplicate)'` → FAILS (current code creates a second `#general`).

## Task 2: Implement the guard (TDD green)

**Files:**
- Modify: `src-tauri/src/lib.rs` (`create_channel_impl`)

- [ ] **Step 1:** Before the mint (`~20100`), capture `let normalized_name = name.trim().to_lowercase();`.
- [ ] **Step 2:** After `engine_arc` resolution (`20147`), before `insert_local_event`: lock `engine_arc.state()`, `materialized(engine_arc.admin_addr())`, and if any `ch.deleted_at.is_none() && ch.name.trim().to_lowercase() == normalized_name`, return the `Err`.
- [ ] **Step 3:** Run the Task 1 test → PASS.

## Task 3: Fix test drift + gates

**Files:**
- Modify: `src-tauri/tests/api_server.rs` (Phase 6g)

- [ ] **Step 1:** That test creates `"general"` in a community that auto-seeds `#general` and asserts 200 — it relied on the bug. Rename its channel to a non-colliding name (e.g. `"team-chat"`) so the create→post RPC plumbing test still passes. (Only test-drift; the grep found no other `create_channel("general")`.)
- [ ] **Step 2:** `cargo nextest run -p harmony-app --lib --features test-fixtures` (full lib) + `--test api_server` → green. `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`. Commit.

## Task 4: PR

- [ ] Push `channel-name-uniqueness-guard`; open PR (`Closes ZEB-583`; body: bug, the verify-vs-materialize rationale, scope note on the deferred concurrent-cross-device case); trigger CodeRabbit; converge bots; Jake merges.
