# ZEB-435 (UI half) + ZEB-268 — left-communities management surface + leave_community detach fence

Bundle PR (one-PR-per-repo). Branch `zeb-435-left-communities-surface` off `main@5afdff0a`.

- **ZEB-435 scope #4** — the orphaned UI half of the merged PR #244 backend: a "left communities"
  management surface with a deliberately-distinct, typed-confirm (Tier-3) "Delete forever" gesture
  wired to the existing `remove_space` IPC.
- **ZEB-268** — mirror the `community_registry.is_none()` detach fence into `leave_community_impl`,
  matching the 11 sibling sites.

## Verified state (2026-07-18, main@5afdff0a)

| # | Fact | Where |
|---|------|-------|
| V1 | `leave_community_impl`'s post-mint re-lock checks ONLY `generation` — no registry fence | `lib.rs:38968-38978` |
| V2 | Fence prior art: generation check + `community_registry.is_none()` → `"community_registry detached during <op> (node stopped?)"` at 11 sites | e.g. `lib.rs:26812-26827` (create_channel) |
| V3 | `remove_space_impl`/IPC exist + registered (Tauri); leave-first guard + durable-flush-gated dir cleanup; **NOT in the headless RPC registry** | `lib.rs:39726,39876,61037`; absent from `api/rpc.rs` |
| V4 | No left-communities exposure anywhere: `communities_for_nav` filters `left_at.is_none()`; `CommunityNavDto` has no `leftAt`; runtime `nav-updated` emits filter too | `lib.rs:25021-25033,25006,7741,7918` |
| V5 | Stale doc comments call `remove_space` "unbuilt" (predate #244) | `lib.rs:~38724, ~38874` |
| V6 | `communityService.removeSpace()` wrapper exists, **zero UI callers**; `leaveCommunity` has one caller (`App.svelte:3690` onLeave) | `src/lib/community-service.ts:309-318` |
| V7 | ZEB-445 seam: `_impl` fns registered in `api/rpc.rs` with an expected-methods pin test | `api/rpc.rs:473,534,1904` |
| V8 | Tier-3 typed-confirm prior art: `TypedConfirmationModal.svelte` (`requiredText`, trim-trailing, disabled-until-match) + vitest template; severity tiers documented | `TypedConfirmationModal.svelte`; `__tests__/TypedConfirmationModal.test.ts`; `docs/specs/2026-05-08-zeb-263-community-frontend-design.md:292-317` |
| V9 | App-level `SettingsPanel.svelte` = tabbed container (`SettingsTab` union + `tabs` array, panels stay mounted, toggled via `hidden`); services are plain classes threaded as props from App.svelte | `SettingsPanel.svelte:62,75-79` |
| V10 | `leave_community_impl` has zero test callers; interleave seam exists: impl parks at `dm_outbox.lock().await` during mint, AFTER the snapshot, BEFORE the re-lock fence | `lib.rs:38955-38961` |

## Settled semantics (ZEB-435 comment 2026-06-27 — not relitigated here)

Three states: leave (reversible `left_at`) / clear-cache (NOT built, out of scope) / `remove_space`
(tombstone + dir delete, permanently NOT rejoinable — same-community re-invites are rejected).
NO cascade from leave. The surface must be separate from per-community settings (unreachable once
nav-hidden) and the gesture typed-confirm (irreversible tier).

## Design

### D1 — ZEB-268 fence (backend)

Inline mirror into the `lib.rs:38968` re-lock block, after the generation check:
`if g.community_registry.is_none() { return Err("community_registry detached during leave_community (node stopped?)") }`.
Inline (not a helper) — consistency with all 11 sibling sites beats DRY here.

**Regression test (red-first, first interleave coverage for this fence class):** fixture NodeState
with `hlc_tracker` + `dm_device_id` + `dm_self_owner` + real `CommunitySyncRegistry::new` (stub
`ContentStore`/`IdentityResolver`, tempdir) + `DmOutbox::new_synthetic` + crdt_state. Test holds the
`dm_outbox` tokio lock → spawns `leave_community_impl` (parks at mint) → sets
`node.community_registry = None` (same-crate field access; generation untouched — exactly the
stop_inner shape) → releases lock → asserts `Err` contains `"community_registry detached"`.
**Deterministically red pre-fix:** without the fence the impl proceeds to `engine_arc()` on the
snapshot clone and fails with the WRONG error (`"no engine for community"`). *(Outcome: the
interleave test shipped exactly as specified and went red pre-fix — no fallback was needed. Any
future rework must keep the integration-level interleave through `leave_community_impl`; a
helper-only test could pass with the production fence missing or unreachable.)*

### D2 — `list_left_communities` (backend, new)

- `LeftCommunityNavDto { space_id, name, left_at_ms: u64 }` (serde camelCase; `left_at_ms` =
  `left_at.wall_ms` for display).
- Pure `left_communities_for_nav(&OwnerState)`: `kind == Community && left_at.is_some()`, sorted
  `left_at_ms` DESC (most recently left first), tiebreak `space_id` asc. Tombstoned spaces are
  already absent from `spaces` — nothing extra to filter.
- `#[tauri::command] list_left_communities` + `list_left_communities_impl` (ZEB-445 seam) +
  Tauri handler registration.
- RPC: register `list_left_communities` AND the missing `remove_space` (`SpaceIdArgs`) in
  `api/rpc.rs` + expected-methods pin list — serves fleet test-community cleanup.
- Fix the two V5 stale "unbuilt" comments.

### D3 — Frontend surface

- `community-service.ts`: `interface LeftCommunityNavDto { spaceId; name; leftAtMs }` +
  `listLeftCommunities()` wrapper.
- New `src/lib/components/LeftCommunitiesPanel.svelte`: rows (name, "Left <date>",
  `Delete forever` danger button), empty state ("No left communities."), loads on activation,
  re-fetches after each successful delete. Error path: inline error text from the standard
  `e instanceof Error ? e.message : String(e)` extraction (surfaces the backend leave-first /
  still-active guard messages verbatim).
- Wire as a new `SettingsPanel` tab `'communities'` ("Communities") per V9: extend the
  `SettingsTab` union + `tabs` array, thread `communityService` from App.svelte.
- Confirm: reuse `TypedConfirmationModal` — `requiredText={community.name}`,
  `confirmLabel="Delete forever"`, description states irreversibility + blocks-re-invite.
  **Guard:** empty-name community would auto-match (`'' === ''.trimEnd()`); use
  `requiredText={community.name || 'delete'}`.
- On success: row removed + re-fetch. No nav-store interaction needed (left spaces were never in nav).

### Out of scope

Clear-cache/GC-only state (2); folder removal (documented backend Err); dm/group-dm delete-forever
UI (backend supports it; surface is communities-only per ticket scope #4); nav push events for the
left-list (pull-on-open is sufficient).

## Test plan (red-first)

- R1 (rust): interleave fence test → red (wrong error) → D1 → green.
- R2 (rust): `left_communities_for_nav` unit tests (live filtered out, left included w/ correct
  `left_at_ms`, sort desc + tiebreak, non-community kinds excluded) → red (fn absent) → D2 → green.
- R3 (rust): RPC pin test updated (expected-methods list) — red until registration.
- F1 (vitest): `listLeftCommunities` wrapper invokes `'list_left_communities'` and returns the
  DTO rows verbatim (no normalization layer).
- F2 (vitest): `LeftCommunitiesPanel` — renders rows + empty state; Delete forever opens typed
  modal with the community name; confirm calls `removeSpace(spaceId)` and re-fetches; error shown.
- Gates per task (iterative): `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`,
  `npx tsc --noEmit`, `npx vitest run`, and `scripts/test-select --context task` — paste the
  emitted `round=… bucket=…` summary line into the task report so the selection is auditable.
- Final (pre-PR, CI-parity):
  `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (full sweep —
  test-select is for iterative gates only), plus the same fmt/clippy/tsc/vitest set.

## Task order

1. Commit plan. 2. ZEB-268 red test → fence → green (+ V5 comment fixes). 3. D2 backend red →
green (incl. RPC). 4. D3 frontend (wrapper → panel → wiring, tests alongside). 5. Full gates → PR
(`Closes ZEB-435. Closes ZEB-268.`) → converge.
