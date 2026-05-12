# ZEB-252 — Sub-D Phase 6: Direct-join IPC for open communities

**Status:** Design
**Date:** 2026-05-12
**Ticket:** [ZEB-252](https://linear.app/zeblith/issue/ZEB-252) (Phase 6 of [ZEB-218](https://linear.app/zeblith/issue/ZEB-218) Sub-D)
**Predecessors:** Phase 1 PR #108, Phase 2 PR #109 ([ZEB-279](https://linear.app/zeblith/issue/ZEB-279)), Phase 3 PR #110 ([ZEB-280](https://linear.app/zeblith/issue/ZEB-280)), Phase 4 PR #112 ([ZEB-281](https://linear.app/zeblith/issue/ZEB-281))

## 1. Goal

Add a typed `join_open_community(community_id)` IPC that the library-directory Join button uses instead of the current `redeem_invite(invite_url)` round-trip. The IPC re-resolves the directory entry server-side at click time, then delegates to the existing `redeem_invite_inner` codepath. Existing `redeem_invite(url)` remains available for hand-pasted URLs.

## 2. Context

Sub-D Phase 1 (PR #108) shipped a click-to-join path for open communities listed in library directories. The frontend reads `entry.invite_url` from the verified `DirectoryEntry`, passes the string up to `App.svelte`, and calls `redeem_invite(invite_url)`. This works correctly — Phase 1's `library_directory::verify_entry` binds each entry's `invite_url` to the entry's `(community_id, admin_addr)` at receive time, so the URL the frontend sees is the URL the server attested.

Two latent issues motivate Phase 6:

1. **Indirect IPC contract.** The directory UI semantically wants to "join the community I'm looking at" — community identity is `community_id`, not an opaque URL. Passing the URL string forces the frontend to remember it from the rendered DTO, increasing the chance of a stale/wrong URL being passed (e.g., if the renderer holds the entry across an aggregation refresh).

2. **No server-side authority over which URL gets redeemed.** A compromised or buggy renderer could call `redeem_invite(url)` with a URL the user never saw. Today's mitigation is "the URL was verified at receive" — which is sound but cedes the freshness check to whatever the renderer last cached.

Phase 6 closes both by routing the directory click path through an IPC that takes only `community_id` and re-resolves the matching `LibraryDirectoryEntry` from the current aggregation server-side. The actual join machinery (URL decode, HLC reservation, bootstrap-Join mint, engine spawn, owner-state commit) is unchanged — Phase 6 strictly wraps it.

### 2.1 What Phase 6 is NOT

- **Not a new wire format.** No new CBOR types, no new Zenoh topics, no new signatures.
- **Not a refactor of `redeem_invite_inner`.** The 10-step flow at `lib.rs:8784+` is unchanged. Phase 6 is a strict caller.
- **Not a bypass of the invite URL.** The URL still encodes the EpochKey + epoch number + admin attribution that `redeem_invite_inner` needs. Phase 6 just looks the URL up server-side instead of accepting it from the IPC caller.
- **Not a stable-MembershipKey directory entry.** ZEB-252's original (pre-[ZEB-249](https://linear.app/zeblith/issue/ZEB-249)) framing called for directory entries to carry a flat per-community key. Post-ZEB-249, EpochKeys rotate on every kick/leave, so the open-community invite URL with unsealed 32-byte EpochKey + EpochCatchup self-healing remains the correct mechanism. This rewrite supersedes the original ZEB-252 description.

## 3. IPC surface

### 3.1 New: `join_open_community`

```rust
#[tauri::command]
async fn join_open_community(
    app: tauri::AppHandle,
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
) -> Result<RedeemInviteResultDto, String>
```

**Parameter:**
- `community_id`: hex-encoded `SpaceId` (32 hex chars) — same shape as `DirectoryEntry.community_id` in the existing Phase 1 DTO.

**Return:** Same `RedeemInviteResultDto { community_id, community_name, is_invite_only }` shape as `redeem_invite`. `is_invite_only` will always be `false` for successful calls (per §4.2).

**Tauri parameter naming:** Rust `community_id` ↔ JS `communityId` per the project's snake_case/camelCase IPC convention.

### 3.2 Unchanged: `redeem_invite(url)`

Stays exactly as today. Used for hand-pasted invite URLs (paste-into-textbox flow), QR-code scan flows (future), and any other path where the URL is supplied directly by the user.

## 4. Backend behavior

### 4.1 Flow

The IPC handler mirrors the existing `redeem_invite` handler structure:

1. **Snapshot NodeState handles** under the std lock; drop the lock before any `.await` (same pattern as `redeem_invite` at `lib.rs:9332+`). Snapshot the same set of handles: `crdt_state`, `hlc_tracker`, `device_id`, `self_owner`, `community_registry`, `community_adapter_request_tx`, `unicast_send_tx`, `channel_log_registry`, `dm_outbox`, `generation`.
2. **Look up the entry.** Find the `LibraryDirectoryEntry` matching `community_id` in the current aggregation. If absent, return `Err` with the §4.3 message. The aggregation source-of-truth is the same data `browse_library(None)` reads.
3. **Defensive invite-only check.** Decode the entry's `invite_url` and reject if `payload.is_invite_only` (defense-in-depth per §4.4).
4. **Delegate to `redeem_invite_inner(entry.invite_url, ...)`.** Pass the same snapshotted handles + the same `fence_check` closure shape `redeem_invite` constructs. The 10-step flow runs unchanged.
5. **Emit `nav-updated`** (`action: "added"`, `kind: "community"`) with the result DTO's `community_id` + `community_name`. Same shape as `redeem_invite` emits at `lib.rs:9438`.
6. **Return** the `RedeemInviteResultDto` produced by `redeem_invite_inner`.

### 4.2 Entry lookup

The lookup helper walks the current `crdt_state.libraries` view's aggregated entries (same data exposed via the existing `browse_library(None)` IPC, which reads through `library_directory::aggregate_directory` or equivalent). The implementer chooses between:

- (a) Calling the existing aggregation function and filtering its `Vec<DirectoryEntryDTO>` for the matching `community_id`, OR
- (b) Adding a small `pub fn find_open_community_entry_for_join(...) -> Option<LibraryDirectoryEntry>` helper in `library_directory.rs` that returns the raw `LibraryDirectoryEntry` (avoiding the DTO conversion).

Either is acceptable; (b) is slightly more direct since the IPC needs the raw `invite_url`. The implementation plan picks.

**Lookup uniqueness.** The Phase 1 aggregation deduplicates by `community_id` across all attesting libraries — there is at most one canonical entry per community in the result. If two libraries attest the same community, Phase 1 has already picked the canonical entry (same admin sig + same `invite_url`; choice between attesters is informational only and doesn't affect the join). So `community_id` is a sufficient lookup key.

### 4.3 Hard-fail on missing entry

If no entry matches `community_id`, return `Err("This community is no longer listed by any of your libraries")`. The frontend surfaces this inline via the existing `joinError` state in `LibraryDirectoryBrowser.svelte`. The user resolves by refreshing the directory (close + reopen, or wait for the next `library-directory-updated` event).

Rationale: the race window is small (200ms aggregation debounce). Falling back to a frontend-cached `invite_url` would partially defeat the server-side authority property that motivates this IPC. Auto-refresh-then-retry inside the IPC is more complex (two responsibilities in one IPC) for a marginal UX win.

### 4.4 Defensive invite-only rejection

Phase 1's `verify_entry` (`library_directory.rs:359-364`) already rejects invite-only URLs at receive time, so an entry with `payload.is_invite_only == true` should be unreachable in the aggregation. Phase 6 still re-decodes the URL and re-checks: if `payload.is_invite_only`, return `Err("Invite-only community cannot be joined directly from the directory")`.

This is belt-and-suspenders against future Phase 1 regressions (e.g., a refactor that loosens `verify_entry`'s gating). Cost is one extra `decode_invite_url` call inside the IPC — irrelevant in practice.

### 4.5 Pass-through errors

All errors produced by `redeem_invite_inner` propagate verbatim — URL parse errors, signature failures, engine spawn errors, HLC reservation errors, node generation race errors, owner-state apply errors. Users see the same strings they'd see from `redeem_invite(invite_url)` today.

### 4.6 Idempotency

Same as `redeem_invite` (see `lib.rs:9323-9330`): **NOT idempotent**. A second call with the same `community_id` mints a fresh self-Join event with a new random `event_id`. Materialized state is unchanged (LWW on `MemberState`); the event log grows by one per retry. `registry.spawn_engine` IS idempotent.

## 5. Frontend changes

### 5.1 `LibraryDirectoryBrowser.svelte`

Two minimal edits:

1. **Update the `onJoin` prop type** from `(inviteUrl: string) => Promise<void>` to `(communityId: string) => Promise<void>`. Update the JSDoc to reflect "Wired to join_open_community" instead of "Wired to redeem_invite".
2. **Update the `handleJoin` call site** (currently `await onJoin(entry.invite_url)`) to `await onJoin(entry.community_id)`. No other changes — the `joinPending` / `joinError` state plumbing stays.

### 5.2 `community-service.ts` (or wherever `redeemInvite` lives)

Add a sibling method `joinOpenCommunity(communityId: string): Promise<RedeemInviteResult>`:

```typescript
async joinOpenCommunity(communityId: string): Promise<RedeemInviteResult> {
  return (await this.adapter.invoke('join_open_community', { communityId })) as RedeemInviteResult;
}
```

Use the same `RedeemInviteResult` type the existing `redeemInvite` returns — no DTO duplication.

### 5.3 `App.svelte`

The handler currently wired to call `redeemInvite(invite_url)` for the browser's `onJoin` callback switches to `joinOpenCommunity(community_id)`. The post-call flow (consuming the DTO, updating nav, etc.) is unchanged — same DTO shape, same `nav-updated` event flow.

### 5.4 Error extraction

Per the project's Tauri-error convention, all `catch (e)` blocks use `e instanceof Error ? e.message : String(e)`. No change to the existing pattern in `LibraryDirectoryBrowser.handleJoin`.

## 6. Testing

### 6.1 Rust unit tests

Co-located with `redeem_invite_inner_tests` in `lib.rs`:

1. **`join_open_community_returns_err_when_community_not_in_aggregation`** — call the IPC handler (or its inner helper) with a `community_id` that has no matching entry; assert `Err` with the §4.3 message.
2. **`join_open_community_returns_err_when_aggregated_entry_is_invite_only`** — stub the aggregation with a test entry whose `invite_url` decodes to `is_invite_only: true` (use the `test-fixtures` feature's deterministic helpers to mint such a URL); assert `Err` with the §4.4 message.
3. **`join_open_community_happy_path_succeeds_and_returns_dto`** — stub the aggregation with one open entry; assert success and DTO equality with what `redeem_invite_inner` would return for the same URL.

### 6.2 Rust integration test

In `src-tauri/tests/community_join_integration.rs` (or wherever Phase 1's `redeem_invite` e2e test lives — implementer locates):

4. **`join_open_community_e2e_joins_open_community_from_directory`** — two-engine setup mirroring the existing `redeem_invite` e2e:
   - Peer A creates an open community + publishes a `LibraryDirectoryEntry` for it
   - Peer B adds A as a library, waits for the entry to land in B's aggregation
   - Peer B calls `join_open_community(A_community_id)`
   - Assert B's owner-state contains a Community Space for A's community with the correct `admin_addr`, `epoch_key`, `epoch`, and a self-Join `MembershipEvent`.

### 6.3 Vitest

5. **`LibraryDirectoryBrowser` Join button click wires through community_id** — update existing test (if present) or add new: simulate click, assert `onJoin` was called with `entry.community_id`, not `entry.invite_url`.
6. **`community-service.joinOpenCommunity` invokes the right IPC** — assert `adapter.invoke('join_open_community', { communityId: 'abc...' })` is called and returns the DTO.

### 6.4 Wire-format pinning

**N/A.** No new wire types, no new CBOR-encoded payloads. The existing `wire_format_*_fixtures.rs` files are unaffected.

### 6.5 Test count

~6 new tests total (3 Rust unit + 1 Rust integration + 2 vitest). Plus the existing Phase 1 redeem_invite tests stay green unchanged.

## 7. Acceptance criteria

1. New IPC `join_open_community(community_id)` exists and is registered in `tauri::generate_handler!`.
2. Calling `join_open_community(valid_community_id_from_aggregation)` produces byte-identical end state in owner-state to calling `redeem_invite(equivalent_invite_url)` — same Space row, same self-Join event, same `current_epoch_key`.
3. Directory Browser Join button calls `join_open_community(community_id)`, not `redeem_invite(invite_url)`.
4. `redeem_invite(url)` remains callable and behaves unchanged (verified by existing tests).
5. Calling `join_open_community(unknown_community_id)` returns the §4.3 error.
6. All 6 CI gates green: `cargo fmt --all -- --check`, `cargo clippy --features test-fixtures -- -D warnings`, `cargo nextest run --features test-fixtures`, `cargo check (msrv) --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.

## 8. Out of scope (explicit non-goals)

- Stable-per-community membership keys carried in directory entries (rejected upfront by ZEB-249 epoch-rotation invariant).
- Invite-only direct-join from the directory (Phase 1 explicitly excludes invite-only entries from the directory; they require the `redeem_invite(url)` paste flow or future Reticulum unicast handshake).
- Pre-warming `redeem_invite` flow with the aggregated entry to skip URL decode entirely (refactor surface in `redeem_invite_inner` — YAGNI until a second non-URL caller exists).
- "Verify against this specific library only" join mode (not requested; aggregation already deduped).
- Auto-refresh-then-retry inside the IPC on missing-entry (single-responsibility — IPC redeems, doesn't refresh).
- UI affordance for "which library did I join via?" telemetry display (informational gold-plating).

## 9. References

- Phase 1 spec: `docs/specs/2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md` §12 (deferred row).
- Phase 1 entry verification: `src-tauri/src/library_directory.rs:303-425` (`verify_entry`).
- Existing `redeem_invite` IPC: `src-tauri/src/lib.rs:9300-9453`.
- Existing `redeem_invite_inner`: `src-tauri/src/lib.rs:8784+`.
- Directory Browser component: `src/lib/components/LibraryDirectoryBrowser.svelte`.
- Library directory service: `src/lib/library-directory-service.ts`.
- ZEB-249 epoch rotation: `docs/specs/2026-05-11-zeb-249-community-backward-secrecy-design.md` (governs why directory entries can't carry stable membership keys).
