# ZEB-334 — Self-notes default (replace the "#general floating in the void")

- **Issue:** [ZEB-334](https://linear.app/zeblith/issue/ZEB-334) (parent: ZEB-327 alpha umbrella)
- **Status:** design approved 2026-06-02
- **Author:** J Eng
- **Surfaced during:** v0.1.0-alpha smoke test on Windows, 2026-05-28

## Problem

On first launch with no community joined, the user lands on a default `#general`
channel that is backed by nothing (`App.svelte` initializes `activeChannel`/
`activeChannelName` to the literal `'general'` with no real space behind it).
The user can type into it, but it's unclear where the text goes, whether it
persists, who can see it, or how to get back to it. It's a degenerate empty
state that *feels broken even though it isn't* — a poor first impression for
alpha testers.

## Goal

Replace the floating-`#general` empty state with a **self-notes** space: a
private, always-present scratchpad the user has with themselves. It gives the
zero-community state a real, useful function and a clear identity.

### Constraints (locked in the issue)

1. **Private** — visible only to the user.
2. **Always present** — a fallback whenever no community is joined, and still
   usable as a quick scratch space after communities exist.
3. **Clearly labeled** as a private space so the user understands what it is.
4. **Local-only for v1.** Multi-device sync depends on owner→device binding
   (ZEB-336 / ZEB-340), which is not yet wired into the client. Ship local-only
   now; add sync as a follow-up.

### Out of scope

- Multi-device sync of the self-notes (deferred to a follow-up once owner→device
  binding lands).
- Auto-migrating any text already typed into the old `#general` void — discard
  it; it's alpha noise.

## Approach

**Approach A — frontend-owned, local-only notepad store.** Chosen over a
self↔self DM on the network substrate (B) and a local pseudo-community (C)
because it is the only option that is *always available* (constraint #2),
including before onboarding and while offline, and it keeps the deferred sync as
a clean follow-up rather than coupling this empty-state fix to owner→device
binding. B/C also drag in network/CRDT machinery unnecessary for a single-user
scratchpad.

## Design

### Unit 1 — `NotesService` (`src/lib/notes-service.ts`) — new

A tiny, network-free, per-identity store. Mirrors the localStorage pattern
already used by `profile-service.ts`.

- **State:** `entries: NoteEntry[]` where `NoteEntry = { id: string; text: string; timestamp: number }`; `onChange?: () => void`.
- **API:**
  - `getEntries(ownerKey): NoteEntry[]`
  - `append(ownerKey, text): NoteEntry` — appends, persists, fires `onChange`.
  - `load(ownerKey): void` — hydrates `entries` from storage.
- **Persistence:** `localStorage['harmony-notes:' + ownerId]` (JSON array).
- **`ownerId`:** the owner identity hex. The `WelcomeModal` hard gate
  (ZEB-338) forces identity creation before the app is usable, so an owner id is
  always present by the time Notes is reachable — there is no pre-identity
  bucket to migrate. If `ownerId` is somehow absent (defensive), the service
  returns an empty list and `append` is a no-op rather than writing to an
  un-keyed bucket.
- **Isolation:** the interface is deliberately thin so the persistence layer can
  be swapped for the synced substrate later without touching callers.

### Unit 2 — `NotesView.svelte` (`src/lib/components/NotesView.svelte`) — new

Renders the notes stream + a compose box. Reuses the existing message-row
*styling* but **not** the network message-service plumbing — it reads/writes
through `NotesService` only. This keeps the local store decoupled from the
network feeds.

- Header/subtitle makes privacy explicit, e.g. *"Private — only you can see
  this. Will sync across your devices in a future update."*
- Author label uses the configured display name (consistent with ZEB-337).
- Compose box appends to `NotesService`; Enter-to-send, Shift+Enter newline
  (matching the channel/DM compose affordance).

### Unit 3 — Nav placement

A synthetic, always-present pinned nav node **"Notes"**:

- New `NavNodeType: 'notes'` (in `types.ts`); fixed id `self-notes`.
- Injected by the frontend (not from the `nav-updated` IPC), pinned at the top
  of the messages-mode nav, and **never cleared** on adapter connect (unlike
  mock-seeded nodes).
- Selecting it routes to `NotesView` (analogous to how DM/community nodes route
  to their feeds).

### Unit 4 — Default selection

Replace the misleading `activeChannel = 'general'` startup default
(`App.svelte:1768–1770`): when no community/DM is selected, default the
messages view to the self-notes space. The `'general'` void default is removed.

### Label

**"Notes"** — neutral, immediately legible as a scratchpad. (Considered
"Personal" and "@yourself"; "@yourself" is cuter but more ambiguous.)

## Testing (TDD)

- **`NotesService`:** append→persist→reload round-trip; per-`ownerId`
  isolation (two identities don't see each other's notes); absent-`ownerId` is a
  safe no-op (`getEntries` returns `[]`, `append` does nothing); empty-state
  returns `[]`.
- **`NotesView`:** renders existing entries; compose appends a new entry and it
  appears; author label reflects the configured display name.
- **Default selection:** with no community joined, the active default is the
  self-notes space (not `'general'`).

## Future follow-up (separate ticket)

Multi-device sync of self-notes once owner→device binding is wired
(ZEB-336/340). At that point `NotesService`'s persistence layer is swapped for
the synced substrate; the `NotesService`/`NotesView` interfaces stay put.

## Implementation notes (2026-06-02)

- **Nav placement implemented as a pinned `NavPanel` row, not a `NavNodeType:
  'notes'`.** During implementation a dedicated row proved cleaner and
  lower-risk: a synthetic `NavNode` would carry `unreadCount`/`unreadLevel`/
  `expanded` and flow through sort/color-ancestry machinery built for real
  spaces, plus ripple into `NavTree`/`NavNodeRow` and the `NavNodeType` union.
  The pinned row (`NavPanel.svelte`, gated on `appMode === 'messages'`) is a
  fixed affordance with `onSelectNotes` + `notesActive` props — zero changes to
  the nav-node type system. Same user-facing result, far less surface.
- **Selection** is a single `notesSelected` boolean in `App.svelte` (defaults
  `true`). The feed pane renders `{#if selectedCommunityNode} CommunityView
  {:else if notesSelected} NotesView {:else} TextFeed`. Any real-node click sets
  it false; `selectNotes()` sets it true and clears the community.
- **Verified:** `NotesService` (5 unit tests), `NotesView` (4 component tests),
  `tsc` clean, full suite 2355 green; live in the Tauri app — Notes is the
  zero-community default, the write→render→persist round-trip works, and notes
  persist under `harmony-notes:<ownerId>`.
