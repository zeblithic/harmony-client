# ZEB-404: Inviter-side roster + nickname live-refresh — Implementation Plan

> **For agentic workers:** TDD, frequent commits. Steps use `- [ ]` checkboxes.

**Goal:** When a peer joins a community that's already on-screen, the inviter's member roster (and, transitively, the joiner's nickname) updates without re-opening the community.

**Architecture:** The member isn't missing from the backend — Koya accepted the joiner's channel post, which (ZEB-399) requires the joiner's membership to be materialized in the *same* `CommunityState` the roster reads. The bug is that the on-screen roster only refreshes on community open/switch or on a live `community-members-changed` event, and that live event was not delivered during the session. Fix adds two frontend refresh triggers that don't depend on the live delta, plus a backend regression test pinning the "membership change ⇒ delta emitted" invariant. Card subscription is gated on the roster (`CommunityView.svelte:288`), so the nickname resolves automatically once the roster converges — no separate WS-B work.

**Tech Stack:** Svelte 5 (`$state`), TS/vitest frontend; Rust/nextest backend.

---

## File Structure

- `src/lib/community-service.ts` — add pure helper `rosterHasJoinedAuthor(members, authorHex)`.
- `src/lib/__tests__/community-service.test.ts` — unit-test the helper.
- `src/App.svelte` — wire `channelMessageService.onMessage` (unknown-author → debounced roster refetch) + reconnect refetch in the `zenoh-status` handler.
- `src-tauri/src/community_state_sync.rs` (tests module) OR `src-tauri/tests/` — regression test: admin learning a joiner via the receive path emits a `CommunityMembershipDelta`.

---

### Task 1: Unknown-author → roster refetch (frontend, load-bearing)

**Files:**
- Modify: `src/lib/community-service.ts`
- Test: `src/lib/__tests__/community-service.test.ts`
- Modify: `src/App.svelte`

- [ ] **Step 1: Failing test for the helper**

In `community-service.test.ts`:
```ts
import { rosterHasJoinedAuthor } from '../community-service';

const m = (address: string, status: 'joined' | 'invited' | 'left' = 'joined') =>
  ({ address, status }) as any;

describe('rosterHasJoinedAuthor', () => {
  it('true when a joined member matches (case-insensitive)', () => {
    expect(rosterHasJoinedAuthor([m('AB12')], 'ab12')).toBe(true);
  });
  it('false when the author is absent', () => {
    expect(rosterHasJoinedAuthor([m('ab12')], 'cd34')).toBe(false);
  });
  it('false when the only match is not joined', () => {
    expect(rosterHasJoinedAuthor([m('ab12', 'invited')], 'ab12')).toBe(false);
  });
  it('false on empty roster', () => {
    expect(rosterHasJoinedAuthor([], 'ab12')).toBe(false);
  });
});
```

- [ ] **Step 2: Run it — expect FAIL** (`npx vitest run src/lib/__tests__/community-service.test.ts`) — `rosterHasJoinedAuthor` is not exported.

- [ ] **Step 3: Implement the helper** in `community-service.ts` (top-level export, near the `CommunityMember` type):
```ts
/**
 * True iff `authorHex` is a currently-JOINED member of `members`.
 * Used to detect a message from someone not (yet) in our roster — a
 * signal that the roster is stale and should be re-fetched (ZEB-404).
 */
export function rosterHasJoinedAuthor(
  members: Pick<CommunityMember, 'address' | 'status'>[],
  authorHex: string,
): boolean {
  const a = authorHex.toLowerCase();
  return members.some((x) => x.status === 'joined' && x.address.toLowerCase() === a);
}
```

- [ ] **Step 4: Run it — expect PASS.**

- [ ] **Step 5: Wire `onMessage` in `App.svelte`** beside the other service-callback wirings (near `communityService.onMembersChanged`, ~line 1226). Add a coalescing timer + handler:
```ts
// ZEB-404: a message from an author not in our roster means the roster is
// stale (we missed a live community-members-changed). Re-fetch, coalescing
// bursts so a backfill of many unknown-author messages triggers one fetch.
let rosterRefetchTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleRosterRefetch(id: string) {
  if (rosterRefetchTimer !== null) return;
  rosterRefetchTimer = setTimeout(() => {
    rosterRefetchTimer = null;
    if (selectedCommunityId === id) void refreshCommunityMembers(id);
  }, 400);
}
channelMessageService.onMessage = (communityId, _channelId, message) => {
  if (communityId !== selectedCommunityId) return;
  if (rosterHasJoinedAuthor(communityMembers, message.author)) return;
  scheduleRosterRefetch(communityId);
};
```
Add `rosterHasJoinedAuthor` to the existing `community-service` import.

- [ ] **Step 6: tsc + vitest** (`npx tsc --noEmit && npx vitest run`) — expect green.

- [ ] **Step 7: Commit** — `feat(zeb-404): refetch roster on message from unknown author + helper test`

---

### Task 2: Reconnect roster refetch (frontend)

**Files:** Modify `src/App.svelte` (the `zenoh-status` `'connected'` handler, ~line 1488, right after `await reloadBackendState();`).

- [ ] **Step 1: Implement** — add:
```ts
// ZEB-404: a reconnect may have missed live community-members-changed deltas
// while the session was down; converge the active community's roster.
if (selectedCommunityId !== null) {
  void refreshCommunityMembers(selectedCommunityId);
}
```

- [ ] **Step 2: tsc** (`npx tsc --noEmit`) — expect green. (No unit test: this is a one-line wiring inside the boot IIFE's listener; covered by the live two-node re-test.)

- [ ] **Step 3: Commit** — `feat(zeb-404): refetch active community roster on zenoh reconnect`

---

### Task 3: Backend regression test — membership change ⇒ delta emitted

**Files:** add a test (prefer an existing `community_state_sync.rs` `#[cfg(test)]` two-engine helper; otherwise a focused `src-tauri/tests/` integration test). Scope cargo to the single target to avoid the ~97-binary relink.

- [ ] **Step 1: Locate the closest existing pattern** — a test that builds a `CommunitySyncEngine` with a `delta_tx: Some(mpsc::channel)` and feeds an inbound publish/insert. Model on it.

- [ ] **Step 2: Write the test** — admin engine learns of a new joiner (via the receive path that produced the live success), assert a `CommunityMembershipDelta` for that joiner is received on the `delta_tx` channel within a bounded `recv` timeout. This pins the "real membership transition ⇒ delta" invariant.

- [ ] **Step 3: Run scoped** — `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(<test_name>)'`. If it FAILS, an emit-gap is real on that path → add the missing `delta_tx.try_send` next to the insert, re-run to green. If it PASSES, the invariant already holds (the live miss was pure delivery timing) — keep the test as a guard.

- [ ] **Step 4: Commit** — `test(zeb-404): pin membership-change ⇒ delta-emitted invariant` (+ any emit fix folded in).

---

### Task 4: Final gate sweep + PR

- [ ] **Step 1: Frontend gates** — `npx tsc --noEmit && npx vitest run` (repo root).
- [ ] **Step 2: Rust gates** — `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(<test_name>)'`. (Reserve full `--all-targets` nextest for CI.)
- [ ] **Step 2b:** commit-before-gate; 10-min wall-clock kill switch on long cargo steps.
- [ ] **Step 3: Push + open PR** referencing ZEB-404, this plan, and the live test evidence. Then autonomous bot-review loop (CodeRabbit/Cursor/CodeAnt/Qodo — never Greptile). Pushover at ready-to-merge. Do NOT self-merge.

---

## Self-Review
- **Coverage:** AC1 (roster updates without reopen) ← Task 1 (unknown-author) + Task 2 (reconnect). AC2 (nickname resolves) ← automatic via roster-gated card subscription. AC3 (backend invariant) ← Task 3. AC4 (FE tests) ← Task 1 helper test.
- **No placeholders** except the backend test name (resolved in Task 3 Step 1 against the actual existing harness).
- **Type consistency:** `rosterHasJoinedAuthor` signature matches `CommunityMember` (`address: string`, `status: 'joined' | ...`); `onMessage` signature matches `ChannelMessageService` (`(communityId, channelId, ChannelMessageDto)`), `ChannelMessageDto.author: string`.
