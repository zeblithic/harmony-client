# ZEB-209: Mock-clear policy across services — design

> **Scope:** Bring `MessageService`, `VineService`, and `NavService` into compliance with the `FileManagerService` clear-on-connect contract established by ZEB-146.
>
> **Decision:** Uniform clear-on-connect. Each `connectAdapter()` synchronously discards all mock-seeded state at the top of the method, before any listener registration.

---

## 1. Context

`FileManagerService.connectAdapter` (file-manager-service.ts:158-167) overwrites its mock-seeded state with backend-derived data the moment a real Tauri adapter wires in. Three sister services do not:

| Service              | Constructor seeds                          | `connectAdapter` clears?         |
|----------------------|--------------------------------------------|----------------------------------|
| `FileManagerService` | `mockPrivateContent` etc.                  | **Yes** (ZEB-146 contract)        |
| `MessageService`     | `mockMessages` → `messages` + `seenIds`    | **No** — real events append      |
| `VineService`        | `mockVines` → `discoverVines` + `seenIds`  | **No** — real events append      |
| `NavService`         | `mockNavNodes` + `mockProfileStore`        | **No** — never cleared           |

The original rationale for keeping mocks (per ZEB-32) was "offline-first fallback so the UI is never empty." That rationale is no longer load-bearing now that the real-data backends have shipped (ZEB-215 nav, ZEB-228 DM, ZEB-286/147/103 vines). The current behavior produces a confusing hybrid in production: real network state appended on top of fictional Alice/Bob threads, an IPFS Crew folder, and five hardcoded "discover" vines pointing at non-existent CIDs.

## 2. Decision

**Uniform clear-on-connect, synchronous at the top of `connectAdapter`, before any `adapter.listen` call.**

```ts
async connectAdapter(adapter: TauriAdapter): Promise<void> {
  if (this.adapter) return;
  this.adapter = adapter;

  // Clear mock-seeded state before subscribing to real events.
  // ZEB-209: mock data is a browser/dev-mode demo aid only — in
  // production the adapter always wires in, so the mocks must go.
  this.<state-fields> = <empty>;

  const unlisten = await adapter.listen(...);
  // ...
}
```

### Alternatives considered

* **Clear-on-first-real-event** — wipe mocks only once a real event arrives. Rejected: produces a transient hybrid UI; "first real event" is ambiguous when a service handles multiple event topics (e.g. `NavService` listens to both `profile-update` and `nav-updated`); harder to test cleanly.
* **Keep-as-overlay (status quo)** — rejected per the ticket framing; produces the documented confusion.

### Why synchronous-before-listen is safe

JavaScript is single-threaded. Between the synchronous clear and the first `await adapter.listen(...)`, no listener callback can fire — the runtime only invokes callbacks after the surrounding `async` function returns control to the event loop. Listener callbacks scheduled by `adapter.listen` will not be invoked until *after* the entire `connectAdapter` body completes (including the synchronous clear). No race exists.

### Why dev/browser mode is unaffected

In `npm run dev` (browser preview, no Tauri shell) and in unit tests without an injected adapter, `connectAdapter()` is **never called** — the constructor's mock seeding is the only data the UI ever sees. The mocks remain a useful demo affordance for those contexts. This change only takes effect when a real adapter wires in.

## 3. Per-service changes

### 3.1 `MessageService` (`src/lib/message-service.ts`)

Fields cleared:

| Field          | Why                                                                  |
|----------------|----------------------------------------------------------------------|
| `messages`     | Drop `mockMessages` so the channel feed is empty until real events.  |
| `seenIds`      | Drop mock IDs so real events with collision-prone IDs aren't deduped.|

Fields **NOT** cleared (initialized empty in declaration, never mock-seeded):

- `dmThreadCursors`
- `loadedDmSpaces`

### 3.2 `VineService` (`src/lib/vine-service.ts`)

Fields cleared:

| Field            | Why                                                                |
|------------------|--------------------------------------------------------------------|
| `discoverVines`  | Drop `mockVines` — fictional CIDs lead to dead-end clicks.         |
| `followedVines`  | Empty in declaration but defensively reset for symmetry.           |
| `seenIds`        | Drop mock vine IDs from dedup set.                                 |
| `viewedIds`      | Drop any viewed-flags pulled from `mockVines[i].viewed === true`.  |
| `reactionMap`    | Empty in declaration but defensively reset for symmetry.           |
| `likePending`    | Defensive reset; should be empty pre-connect but safe to clear.    |

Fields **NOT** cleared:

- `followedAddresses` — empty in declaration, never mock-seeded.
- `ownAddress`, `ownDisplayName` — identity state, set by App.svelte after pairing/owner-state load. Independent of mock-clear.

### 3.3 `NavService` (`src/lib/nav-service.ts`)

Fields cleared:

| Field      | Why                                                                |
|------------|--------------------------------------------------------------------|
| `nodes`    | Drop `mockNavNodes` — mock channels/DMs are uninhabitable.         |
| `profiles` | Drop `mockProfileStore` — fictional Alice/Bob profiles.            |

Fields **NOT** cleared:

- `avatarResolver` — set separately via `setAvatarResolver()`, independent.
- `ownAddress` — see VineService rationale.

## 4. Ordering inside `connectAdapter`

For all three services, the body becomes:

```ts
async connectAdapter(adapter: TauriAdapter): Promise<void> {
  if (this.adapter) return;        // idempotency guard (unchanged)
  this.adapter = adapter;

  // ── ZEB-209: clear mock-seeded state ──────────────────────────
  this.<field> = <empty-init>;
  // ... (one line per cleared field per §3)
  this.onChange?.();               // notify UI that mocks are gone

  // ── (existing listener setup follows) ─────────────────────────
  const unlisten = await adapter.listen(...);
  // ...
}
```

**`onChange?.()` placement:** fires once after the clear so any UI subscribed via `onChange` re-renders against the empty state instead of waiting for the first real event to arrive. This avoids a "mocks still visible after Zenoh handshake" flash for users.

## 5. Behavior with idempotent re-connect

The existing `if (this.adapter) return;` guard means a second `connectAdapter()` call on an already-connected service is a no-op. The clear runs **only on first connect**. By that point real events may have populated state — re-clearing on the second call would be a bug. The guard sits at the top of the method, before the clear, so this is automatically correct.

## 6. Testing strategy

Each service gains one new test in its existing `.test.ts` file:

### 6.1 `src/lib/message-service.test.ts`

```ts
it('clears mock-seeded state on connectAdapter (ZEB-209)', async () => {
  const svc = new MessageService();
  expect(svc.messages.length).toBeGreaterThan(0); // sanity: mocks seeded
  const { adapter } = createMockAdapter();
  await svc.connectAdapter(adapter);
  expect(svc.messages).toEqual([]);
  // Internal seenIds is private; assert behavior indirectly: an event
  // whose id collides with a former mock message id is now accepted.
  // (Test impl uses a mock-id taken from mockMessages.)
});
```

### 6.2 `src/lib/vine-service.test.ts`

```ts
it('clears mock-seeded state on connectAdapter (ZEB-209)', async () => {
  const svc = new VineService();
  expect(svc.discoverVines.length).toBeGreaterThan(0); // sanity
  const { adapter } = createMockAdapter();
  await svc.connectAdapter(adapter);
  expect(svc.discoverVines).toEqual([]);
  expect(svc.followedVines).toEqual([]);
  expect(svc.viewedIds.size).toBe(0);
});
```

### 6.3 `src/lib/nav-service.test.ts`

```ts
it('clears mock-seeded state on connectAdapter (ZEB-209)', async () => {
  const svc = new NavService();
  expect(svc.nodes.length).toBeGreaterThan(0); // sanity
  expect(svc.profiles.size).toBeGreaterThan(0);
  const { adapter } = createMockAdapter();
  await svc.connectAdapter(adapter);
  expect(svc.nodes).toEqual([]);
  expect(svc.profiles.size).toBe(0);
});
```

### 6.4 `onChange` notification test (one per service)

Each service also gets a small assertion that `onChange` fires at least once during `connectAdapter` so the UI re-renders post-clear:

```ts
it('fires onChange after clearing mocks (ZEB-209)', async () => {
  const svc = new VineService();
  let calls = 0;
  svc.onChange = () => { calls++; };
  const { adapter } = createMockAdapter();
  await svc.connectAdapter(adapter);
  expect(calls).toBeGreaterThanOrEqual(1);
});
```

## 7. Out of scope

* No new IPC events or backend changes — pure frontend.
* No changes to `FileManagerService` (already compliant).
* No changes to dev/browser mode behavior — mocks stay when no adapter connects.
* No changes to identity state (`ownAddress`, `ownDisplayName`) — orthogonal to mock-clear.
* No changes to `mockMessages`, `mockVines`, `mockNavNodes`, `mockProfileStore` themselves — they remain valid for dev mode and for unit-test fixtures.
* `getContentDetail`'s hardcoded `mockPeers[0]/[1]` in `FileManagerService.getContentDetail` (ZEB-207) — tracked separately.
* `mockCleanupRecommendations.reason` staleness (ZEB-208) — tracked separately.

## 8. Acceptance criteria

1. `MessageService.connectAdapter` clears `messages` and `seenIds` before listener setup.
2. `VineService.connectAdapter` clears `discoverVines`, `followedVines`, `seenIds`, `viewedIds`, `reactionMap`, `likePending` before listener setup.
3. `NavService.connectAdapter` clears `nodes` and `profiles` before listener setup.
4. All three services fire `onChange?.()` once after clearing.
5. `connectAdapter` idempotency guard (`if (this.adapter) return;`) sits before the clear — second-connect does not wipe accumulated real state.
6. New tests added for each service asserting cleared-after-connect behavior.
7. New tests added for each service asserting `onChange` fires post-clear.
8. In-source comments updated: the constructor's "Seed with mock data — real messages append on top" comment becomes "Seed with mock data for dev/browser mode — `connectAdapter()` clears these before subscribing to real events (ZEB-209)."
9. All five CI gates pass: `cargo fmt --all -- --check` (no Rust changes but gate runs), `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.
