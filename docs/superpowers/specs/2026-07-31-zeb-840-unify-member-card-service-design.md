# ZEB-840 — Unify the two `MemberCardService` instances behind a multi-source subscription model

Status: approved (Jake, 2026-07-31). Follow-up to ZEB-839 (PR #574).

## 1. Problem

Peer-card resolution (`owner_id` → `{displayName, statusText, avatarUrl, profilePageRoot}`) is driven by `MemberCardService.subscribeVisible(ids)`, which **reconciles the backend subscription set to *exactly* the passed array** (`member-card-service.ts:199-255`: subscribe anything new, unsubscribe anything absent). Because the argument is the *complete* desired set, every independent subscription driver fights over one reconcile target. Three consequences, all the same root cause:

1. **App-level clobber (pre-existing bug).** The App funnel `subscribeVisibleCards()` (`App.svelte:710-713`) does a raw *overwrite* `visibleCardOwners = ownerIdHexes`, and **three** drivers call it — the community roster (`CommunityView.svelte:304-309`), the 1:1 voice roster (`App.svelte:482-486`), and the group-call roster (`App.svelte:530-534`). Whichever fires last replaces the whole set, so a call-roster update while a community is open silently unsubscribes the community roster's cards (and vice versa). Only the DM peer survives, via a special-cased union (`dmCardOwner`, the ZEB-839 pin at `App.svelte:703-708`).
2. **A second instance to dodge the clobber.** The Friends panel runs a *separate* `MemberCardService` (`friendCardService`, `App.svelte:1763`) precisely because sharing one instance would make friends and the community roster unsubscribe each other (comment at `App.svelte:1758-1762`). Two instances = two independent 3s `get_cached_member_card` polls over disjoint owner sets, so **cards drift**: a card can land in the roster before the Friends panel, and the push listener (`applyCard` on both, `App.svelte:2702-2722`) only narrows — doesn't close — the window.
3. **`ProfilePanel` blank-name drift.** "View full profile" re-resolves via the **main** instance only (`App.svelte:4471` `resolveCard(...)`). Open a friend's profile for a friend who is *not* in the currently-open community, and the main instance has no card for them → the panel falls back to `{ displayName: '' }`.

## 2. Goal

Collapse to **one** `MemberCardService` instance driven by **named subscription buckets** whose **union** forms the backend subscription set. This removes the clobber, the drift, and the ProfilePanel bug in one structural change, and dissolves ZEB-839's DM merge funnel into an ordinary bucket.

Non-goals: the identity-switch cache-clear gap (neither instance clears `cards`/`subs` on `stop_node`/identity switch today) is pre-existing and out of scope — the refactor must not worsen it. No backend changes; this is frontend-only (the IPC surface `subscribe_member_card` / `unsubscribe_member_card` / `get_cached_member_card` is unchanged).

## 3. Design

### 3.1 `member-card-service.ts` — named buckets, union reconcile

Replace the single reconcile-set with a bucket map:

```ts
private buckets = new Map<string, Set<string>>();   // bucketName -> owner hexes
```

- **`setBucket(name: string, ownerIdHexes: string[]): Promise<void>`** — store this bucket's set, recompute the **union of all buckets**, and reconcile `this.subs` to the union using the *existing* subscribe-new / unsubscribe-gone / poll-start-stop logic (unchanged except it now diffs against the union rather than a single caller's array). Runs through the existing `runExclusive`/`opChain` (`member-card-service.ts:57-74`) so bucket mutations stay serialized — preserving the unmount/remount orphaned-subscription guard.
- Setting a bucket to `[]` clears that bucket's contribution to the union (drains only its owners, unless another bucket still wants them). No separate `clearBucket` needed, but a thin `clearBucket(name) = setBucket(name, [])` alias is fine for call-site readability.
- **`unsubscribeAll()`** stays for full teardown: clears every bucket, unsubscribes all subs, stops the poll.
- **`subscribeVisible` is removed.** All callers migrate to explicit buckets. (Rejected alternative: keep it as a `setBucket('community', …)` shim — smaller diff but leaves the confusing dual API the ticket wants gone.)
- Self-exclusion (`selfKey`, filtered in the reconcile) and avatar-CID tracking are unchanged and now apply uniformly across every bucket.

The union reconcile is idempotent: rapid successive `setBucket` calls each recompute from current bucket state, so the last call sees the full picture; each reconcile only diffs (subscribe new / unsubscribe departed).

### 3.2 `App.svelte` — one instance, per-driver buckets

Delete the funnel (`visibleCardOwners`, `dmCardOwner`, `reconcileCardSubscriptions`, `subscribeVisibleCards`, `unsubscribeCards`, `setDmCardOwner`; `App.svelte:701-727`). Each driver sets its own bucket on the single `memberCardService`:

| Driver | Old call | New call |
|---|---|---|
| Community roster | `subscribeVisibleCards(joinedOwnerIds)` (`CommunityView` prop) | `setBucket('community', joinedOwnerIds)` |
| Community teardown | `unsubscribeCards()` (`CommunityView.onDestroy`) | `setBucket('community', [])` |
| 1:1 voice roster | `subscribeVisibleCards(ownerHexes)` (`App.svelte:482`) | `setBucket('voice', ownerHexes)` |
| Group-call roster | `subscribeVisibleCards(ownerHexes)` (`App.svelte:530`) | `setBucket('groupCall', ownerHexes)` |
| DM peer | `setDmCardOwner(activeDmPeerOwner)` (`App.svelte:3824`) | `setBucket('dm', peer ? [peer] : [])` |

`voice` and `groupCall` are independent buckets so they never clobber each other; each must be cleared to `[]` when its call ends (verify the call/voice/group-call session teardown drives `onRosterOwners([])` or add an explicit clear on session end). Keep one app-lifetime `unsubscribeAll()` teardown for the single instance.

### 3.3 Collapse `friendCardService`

Remove the second instance. `FriendsPanel` stops owning a service; it receives:
- **read side:** `resolveCard` / `resolveNickname` closures (the same ones threaded into `MemberRow`/`TextFeed`; reactive via App's `cardVersion` `$state`, tracked transitively when the closure is called in the panel's template/`$derived`). Replaces `friendCardService.resolve()` in `cardName()` / `cardAvatarUrl()` / `openIdentity()`.
- **write side:** a `setFriendsBucket(ids)` callback → `memberCardService.setBucket('friends', ids)`, driven by the same `$effect` on `friends`/`pendingRequests` (`FriendsPanel.svelte:829-839`). Its `onDestroy` (`:386-390`) clears **only** the friends bucket (`setFriendsBucket([])`), **not** `unsubscribeAll()` (which would nuke every other bucket).

`FriendsPanel`'s local `cardVersion` (`:68`) and `onUpdate` wiring (`:332-335`) are removed. `SettingsPanel.svelte` prop threading (`:57,75`) changes from `cardService` to the new closures + callback. This implicitly fixes the ProfilePanel drift (§1.3), since the friends bucket now lives in the same instance the profile panel reads.

## 4. Hazards preserved

- **`opChain` serialization** now covers all bucket-driven mutations (not just raw `subscribeVisible`), so the unmount/remount race that could orphan a backend subscription stays guarded.
- **Self-seeding/exclusion** applies uniformly — verify self never leaks into the `friends` bucket once unified (it never appears in the friends/pending lists today; the `selfKey` filter is the belt-and-suspenders).
- **Shared avatar resolver** — both instances already share one resolver object; collapsing is a no-op here.
- **DM pin survives roster reconciles** — now an independent `dm` bucket rather than a special-cased union, preserving ZEB-839's behavior by construction.
- **Teardown ordering** — community/friends/call teardowns each clear only their own bucket; the app-lifetime `unsubscribeAll` remains for full shutdown.

## 5. Testing

Backend/service (`vitest`, `member-card-service.test.ts`):
- Migrate existing `subscribeVisible` cases to `setBucket`.
- Union across buckets: two buckets → subs is their union; each bucket independently settable.
- No clobber: `setBucket('friends', A)` then `setBucket('community', B)` yields subs = A ∪ B (regression for the core bug).
- Draining one bucket (`setBucket(name, [])`) unsubscribes only that bucket's owners, keeping owners another bucket still wants.
- Self excluded across every bucket.
- `opChain` still serializes concurrent bucket mutations (the existing race test, adapted).

Frontend (`vitest`): update `FriendsPanel.test.ts` for the closure/callback props (drop the `cardService` instance). Spot-check that a friend-only owner resolves in the profile panel path.

Gate: `npx tsc --noEmit` + `npx vitest run` green before opening the PR; CI (`ci.yml`) frontend job must pass.

## 6. Blast radius

- **Service:** `member-card-service.ts` — bucket map + `setBucket`, remove `subscribeVisible`.
- **App.svelte:** delete the funnel (701-727); rewire five call sites (community sub/teardown, voice, groupCall, dm); remove `friendCardService` construction/wiring/teardown; thread new props to `FriendsPanel` via `SettingsPanel`.
- **Components:** `FriendsPanel.svelte` (drop service instance → closures + callback; remove local `cardVersion`/`onUpdate`), `SettingsPanel.svelte` (prop rename), `CommunityView.svelte` (prop names community sub/teardown → bucket calls, mechanical).
- **Tests:** `member-card-service.test.ts`, `FriendsPanel.test.ts`.
