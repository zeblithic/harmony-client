# ZEB-977 — Contacts: petnames + private notes for any identity + name-provenance anti-spoofing

Status: approved (Jake, 2026-08-23, in-session). Scope: ZEB-977 items 1–3.
Deferred: collision warnings (item 4, fast-follow), verified-flag / community
nicknames / identicon strengthening (item 5). VineFeed `creatorName` split out
as ZEB-978.

## Problem

Display names are self-published and non-unique: a profile card's signature
proves *card ownership*, not *name ownership*. Anyone can publish a card named
"Jake", and people cannot recognize each other by owner-id hex — so name
collision enables cheap impersonation, worst at first-contact surfaces (DM
invite toast, mail, join requests).

Petname-system framing — three name kinds per identity:

1. **key** (owner_id) — global, unique, unmemorable;
2. **card name** — memorable, peer-chosen, NOT unique, forgeable by collision;
3. **petname** — memorable, unique per viewer, locally assigned, never on wire.

The petname's security property is **unforgeability of the local binding**,
not secrecy: an attacker cannot make your client render *their* key with
*your* petname, no matter what they learn. Petname knowledge only helps them
if a card name can visually pass as a petname. Hence the load-bearing
invariant:

> **A card-sourced name must never be renderable in the petname style.**

## What exists (ground truth, verified 2026-08-22)

- `friend_nicknames.rs` (ZEB-419): `BTreeMap<owner_id_hex, {nickname,
  updated_ms}>` in `friend_nicknames.json` beside `connectivity_settings.json`,
  structurally outside published OwnerState, guard-tested. `set_friend_nickname`
  (lib.rs ~70672) hard-rejects non-Active friends; editor only in FriendsPanel.
- Display-name ladder `nickname → card → (roster/wire) → hex` via
  `resolveMentionLabel` / `resolveAuthorLabel` (mention-render.ts) and
  `resolveMemberName` (display-label.ts); resolvers passed as parameters, so
  call sites can and do omit rungs. Six bypass surfaces: MailReader/MailInbox,
  DmCreateDialog, DmInviteToast, ShareList, NotificationSettingsPanel,
  file-manager `granterDisplay`.
- Mentions are `@pubkey` on the wire (ZEB-588). Drill-downs deliberately show
  only the signed card name + full hex (ProfilePopover) — kept.
- Fleet Sync substrate (ZEB-417): `FleetSyncEngine<T>`; Notes is the template
  consumer (`notes_crdt.rs` / `notes_persist.rs` / `notes_commands.rs`, engine
  wired in `start_node`, bridged to zenoh topic
  `harmony/owner/{addr_hex}/ds/notes-v1`, files in the identity dir).
- Per-contact notes and local first-seen: nothing exists.

## Design

### 1. `ContactsDoc` CRDT (`contacts_crdt.rs`)

Mirrors `NotesDoc` exactly (LWW-element-set):

```rust
pub struct ContactEntry {
    pub owner_id_hex: String,        // key duplicated in-entry, like Note.id
    pub petname: Option<String>,
    pub notes: Option<String>,
    pub first_seen_ms: u64,          // LOCAL wall clock at entry creation
    pub created_at: Hlc,
    pub updated_at: Hlc,
    pub deleted_at: Option<Hlc>,     // tombstone
}
pub struct ContactsDoc { pub contacts: BTreeMap<String, ContactEntry> }
```

- Key: lowercase 32-char owner_id hex — the person-level signing identity
  (`decode_owner_id_16`), never a device hash / device vk.
- Entry-level LWW on `updated_at` (Notes precedent). Concurrent petname-edit
  vs notes-edit on different devices loses one write; accepted for v1
  (field-level LWW is the upgrade if it ever matters).
- `merge_from` copies the ZEB-847 forward-skew rejection verbatim: reject
  future-stamped entries, never clamp.
- Clearing both petname and notes tombstones the entry (a contact record with
  no annotations should not linger). `first_seen_ms` is set once at creation
  and survives edits; it exists for item-4 warnings later and for "you first
  annotated this identity on…" display. v1 does NOT track first-seen for
  unannotated peers.
- Manual `CanonicalPayload` impls, like `NotesDoc`.

### 2. Persistence (`contacts_persist.rs`)

Mirror of `notes_persist.rs` (the codebase idiom is per-dataset persist
modules): 1-byte schema version + CBOR, atomic write via
`owner_state_persist::save_atomically`, corrupt-file quarantine
(`.corrupt-<ms>`), ZEB-460 transient-vs-corrupt contract, `ContactsPersist`
implementing `FleetPersist<ContactsDoc>`. Files `contacts.cbor` +
`contacts_replay.cbor` in the **identity dir**, beside `notes.cbor`.
Plaintext CBOR at rest (decision: match Notes/owner-state precedent;
encryption-at-rest is a follow-up covering Notes + Contacts together).

### 3. Commands (`contacts_commands.rs`)

Same core/wrapper split and HLC discipline as `notes_commands.rs`
(peek-and-commit under one tracker lock; never mint on a no-op; superseded
writes return recoverable `Err`; `notify_dirty()` only on change):

- `contacts_list() -> Vec<ContactView>` (live entries).
- `set_contact_petname(owner_id_hex, petname: Option<String>)`
- `set_contact_notes(owner_id_hex, notes: Option<String>)`

Validation: `decode_owner_id_16`; petname ≤ 64 chars (existing cap); notes
≤ 4096 chars; trim, blank = clear. **No friend-status gate** — any valid
identity can be annotated. Both setters upsert (creating the entry with
`first_seen_ms = now` if absent) and tombstone when the write leaves both
fields empty. `on_applied` (remote merge) emits `contacts-changed`; local
setters also emit `contacts-changed` after `notify_dirty`, plus
`friend-list-changed` (FriendsPanel joins nicknames into `FriendDto`).

### 4. Engine wiring (`start_node`, `event_loop`)

Mirror the Notes block site-for-site: `FleetSyncEngine<ContactsDoc>` with the
shared kt / device_id / content_store / adopt_floor, lookup tag
`b"contacts-v1"`, zenoh topic `harmony/owner/{addr_hex}/ds/contacts-v1`,
`ContactsSyncHandles` mirroring `NotesSyncHandles`, NodeState handles
(`contacts_doc` / `contacts_tracker` / `contacts_sync` /
`contacts_device_id`), and shutdown handling mirroring `notes_sync`'s
(engine taken under the flush lock, shut down after the loop exits).

### 5. Migration from `friend_nicknames.json`

At engine construction, if `contacts.cbor` is absent and
`friend_nicknames.json` exists: import every entry as a live `ContactEntry`
(`petname = nickname`, `first_seen_ms = updated_ms`, minted HLCs), persist,
then rename the legacy file to `friend_nicknames.json.migrated` so re-runs
don't re-import. The `set_friend_nickname` command is **removed** (in-repo
frontend is its only caller; it was never on the HTTP surface) and
`FriendDto.nickname` is re-joined from `ContactsDoc` at `list_friends`
projection, so existing consumers keep working. `friend_nicknames.rs` is
retired with its file (module kept only if the migration reader needs its
serde shape — as `pub(crate)` legacy types).

### 6. HTTP RPC

Add `contacts_list` / `set_contact_petname` / `set_contact_notes` as `rpc!`
entries in `api/rpc.rs` (headless parity + e2e driveability). Args structs
follow the surface's `deny_unknown_fields` convention.

### 7. Frontend

- **`contacts-service.ts`**: fetch/caches `contacts_list`, `$state` version
  counter, refetch on `contacts-changed`; exposes
  `resolvePetname(id)` / `resolveNotes(id)` and setters wrapping the IPC.
- **App.svelte**: `resolveNickname()` re-sourced from ContactsService
  (dropping `nicknameMapFromFriends`) — this alone extends petnames to
  non-friends on every already-laddered surface.
- **`ResolvedName`**: the ladder functions (`resolveMentionLabel`,
  `resolveAuthorLabel`, `resolveMemberName`) change to return
  `{ label: string, source: 'petname' | 'card' | 'roster' | 'wire' | 'hex' | 'self' }`.
  All call sites are updated (mechanical). Text-only contexts (aria-labels,
  confirm dialogs, window titles) use `.label`; visual contexts render via:
- **`PeerName.svelte`**: the single component that renders a `ResolvedName`.
  Petname source ⇒ a small leading badge rendered as a styled DOM element
  (never a text character — a card name containing a lookalike glyph cannot
  imitate it) + a distinct style token. Card ⇒ plain. Wire ⇒ subtle
  unverified styling. Hex ⇒ muted mono. The invariant holds by construction:
  no surface can render a card name in petname style because the style is
  keyed off `source` inside one component.
- **Editor**: ProfilePopover (owner-card mode) gains petname input + private
  notes textarea ("Only you see these") for ANY identity; FriendsPanel's
  editor rewires to the same service. The popover's identity section is
  unchanged: signed card name + full hex only, petname shown separately.
- **Bypass closes**: route MailReader/MailInbox, DmCreateDialog,
  DmInviteToast, ShareList, NotificationSettingsPanel, and
  `granterDisplay` through the ladder + `PeerName`.

### Explicitly unchanged

Mention wire format (`@pubkey`); drill-down signed-card convention; published
OwnerState serialization (guard test ports over and extends to ContactsDoc).

## Security invariants

1. Contact data is never published/broadcast: structural (own files + own
   engine topic under the owner's fleet encryption) + guard test.
2. Provenance is unforgeable in the UI: petname styling derives only from
   `ResolvedName.source` inside `PeerName.svelte`.
3. `merge_from` rejects forward-skewed stamps (ZEB-847 pattern).
4. Owner-id validation on every setter; petname/notes length caps.
5. Fleet sync rides the owner-authenticated encrypted channel (ZEB-417) —
   contacts never leave the owner's device set.

## Testing

- `contacts_crdt.rs`: LWW/merge/tombstone/forward-skew unit tests (port the
  NotesDoc suite).
- `contacts_persist.rs`: round-trip, quarantine, transient-IO tests (port).
- `contacts_commands.rs`: core tests incl. superseded-write no-mint, blank
  handling, both-fields-cleared tombstone, no-friend-gate; two-engine
  cross-device convergence suite (port the notes forwarder harness).
- Migration: nicknames file → contacts import, idempotence (`.migrated`
  rename), absent-file no-op.
- Guard: published owner-state bytes contain no petname/notes (extend
  ZEB-419's test).
- Frontend (vitest): ladder returns correct `source` per rung; `PeerName`
  renders badge for petname-source only; ContactsService set/clear/refetch;
  editor visibility for non-friends.
- e2e (`e2e-harness`): RPC-set a petname on a non-friend community member →
  member list / message author renders it; clear → falls back to card name.

## Rollout

One PR on `zeb-977-contacts-petnames` (bundle rule). Full local gates
(fmt, clippy `--all-targets`, nextest, tsc, vitest) before push; CI green +
CodeRabbit convergence before ready.
