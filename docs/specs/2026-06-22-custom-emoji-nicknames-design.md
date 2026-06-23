# Custom-Emoji Nicknames — Design Spec

**Status:** Approved (brainstorm), ready for implementation plan
**Date:** 2026-06-22
**Builds on:** public custom emoji (ZEB-542, merged `main`) and CAS-backed emoji reactions (ZEB-541)
**Enables:** faster, name-driven reuse of custom reaction emoji across channels

## Goal

Let a user assign a short human-readable **name** to a custom reaction emoji so they can
find and reuse it by name instead of by its 32-byte content-id (CID). Names are
**personal and local to the user** (an alias layer over emoji the user has encountered),
**global to the user** (one name works in every community), and surfaced through **one new
popover** that doubles as quick-pick and manager, plus a lightweight in-context "name this
emoji" affordance. No protocol or wire change: a reaction on the wire is still just a CID.

## Background & motivation

Custom emoji are content-addressed: a reaction references a `cid` (hex `ContentId`) and the
image renders by fetching that CID. Today the only handle a user has on a custom emoji is
the raw 64-hex CID — there is no way to say "react with the one I call `catjam`."

The just-merged **public custom emoji** foundation (ZEB-542) is the prerequisite that makes
names meaningful and reusable:

- A public emoji is `hash(plaintext)` — **one global CID per image**, fetchable by anyone
  who learns the CID, with no channel or epoch scope. A name can therefore map to a single
  canonical CID that renders **in every channel**, including ones where the emoji has never
  appeared.
- An **encrypted** emoji is `hash(ciphertext)` under a community epoch key — a different CID
  per community/epoch, renderable only inside its origin community. It cannot be a globally
  reusable named emoji. Naming is therefore **public-only** by construction.

## Decisions (settled in brainstorm)

1. **Storage home — local-only now, sync later.** Names live in a new local-only
   `emoji_names.json`, mirroring the existing friend-nickname store
   (`src-tauri/src/friend_nicknames.rs`). They are **not** placed in `OwnerState` (which has
   shared/published parts — a private alias there would risk leaking). Device-sync is
   deferred to the same future ZEB-417 fleet-sync migration that friend nicknames already
   await; the store's LWW-by-`updated_ms` shape is chosen for forward-compatibility with it.
2. **Picker contents — named emoji only.** The popover lists/searches only emoji the user
   has named. The name map *is* the collection; no separate "encountered CIDs" index is
   built. The list starts empty and fills as the user names emoji.
3. **Name eligibility — public emoji only.** Only public custom emoji can be named; the
   naming affordance is hidden/disabled on encrypted reaction chips with a short
   explanation. (Public is the default since ZEB-542, so this covers nearly all emoji.)
4. **Charset / length** — `[A-Za-z0-9_-]`, 1–32 chars.
5. **One name per CID, CID-keyed, soft set-time uniqueness** — the map is keyed by CID;
   typing a name another CID already uses warns but is allowed (front-end soft-check).
6. **Retain-on-name** — naming an emoji best-effort pins its bytes into local CAS so it
   keeps rendering later; a transient fetch failure does not block naming (preview degrades
   gracefully).
7. **One surface** — a single popover for quick-pick + manage (search, react, rename,
   remove); plus a lightweight inline "name this emoji" affordance on chips and at upload.

## Model

A personal map, keyed by the public emoji's CID:

```rust
// src-tauri/src/emoji_names.rs  (new; mirrors friend_nicknames.rs structure)
pub struct EmojiNames {
    pub entries: BTreeMap<String /* cid hex */, EmojiNameEntry>,
}
pub struct EmojiNameEntry {
    pub name: String,    // [A-Za-z0-9_-], 1–32
    pub mime: String,    // e.g. "image/png" — stored so reuse needs no re-fetch
    pub size: u64,       // advisory plaintext byte length for the React descriptor
    pub updated_ms: u64, // LWW key (wall-clock ms); shaped for the ZEB-417 sync migration
}
```

- **Key = CID hex**, value carries the name plus the `mime`/`size` needed to re-react
  without a round-trip. One name per CID.
- **Name-search** scans values (personal-scale N; trivial cost).
- **Soft uniqueness** is a front-end concern: before confirming a name already used by
  another CID, warn and allow. The backend does **not** enforce name uniqueness.

## Architecture — backend seams

Mirrors the friend-nickname end-to-end path (the closest existing analog). Line numbers are
approximate and for orientation only.

1. **Store** — new `src-tauri/src/emoji_names.rs`, mirroring `friend_nicknames.rs`
   (`FriendNicknames`/`NicknameEntry`, `load_or_default` + `save_atomically`). File
   `emoji_names.json` lives in the identity dir alongside `friend_nicknames.json`,
   **outside** `OwnerState`. A process-global `EMOJI_NAMES_WRITE_LOCK` (tokio `Mutex`)
   serializes load-modify-save, like `NICKNAME_WRITE_LOCK`.

2. **`set_emoji_name(cid: String, name: Option<String>)`** IPC (model on `set_friend_nickname`,
   `src-tauri/src/lib.rs:~45272`):
   - Validate `cid` is 64-hex and decodes to `[u8;32]`; reject if `ContentId::from_bytes(cid).flags().encrypted`
     ("encrypted emoji can't be named — they can't be reused outside their community").
   - On set: validate `name` charset/length; **retain-on-name** — best-effort fetch the
     public bytes (local CAS → network) and ensure they are stored locally; capture `mime`
     (default `image/png` if unknown) and `size`. If the fetch fails, still save the name
     (the entry's preview degrades to a fetch-on-demand / fallback icon; naming never blocks).
   - On `None`: clear the entry (bytes are left in place — CAS has no GC).
   - Write under `EMOJI_NAMES_WRITE_LOCK`; emit **`emoji-names-changed`** (no payload) so the
     frontend re-fetches.

3. **`list_emoji_names()`** IPC → `Vec<EmojiNameDto { cid, name, mime, size }>` for the popover.

4. **`preview_named_emoji(cid: String)`** IPC → `Vec<u8>` — the one genuinely new primitive.
   **Public-only, non-channel-scoped** fetch by CID (local CAS → network fallback), capped at
   `MAX_CUSTOM_EMOJI_BYTES`. Reject encrypted CIDs. This differs from the existing
   `preview_reaction_emoji` (`src-tauri/src/lib.rs:~21195`), which authorizes the CID against a
   signed React **in a specific channel** — a constraint that cannot be satisfied when
   previewing a named emoji in a channel where it has not appeared. Public CAS makes a
   scope-free fetch-by-CID legitimate; the frontend decode-bomb header guard still applies.

5. **Reaction send/verify — no change.** Post-ZEB-542, `set_message_reaction` and
   `verify_channel_event` already accept a public CID with no prior channel presence, so
   reacting with a named emoji in a fresh channel "just works"; peers render via public CAS.

## Architecture — frontend changes

1. **`src/lib/emoji-name-service.ts`** (new): `setEmojiName(cid, name | null)`,
   `listEmojiNames()`, `previewNamedEmoji(cid)`. Subscribes to `emoji-names-changed` and
   re-fetches, mirroring how the friend UI reacts to `friend-list-changed`.

2. **Named-emoji popover** (new component, the one new surface): a live name-search box over
   a thumbnail grid of named emoji; clicking a tile reacts on the target message with the
   stored `{ cid, mime, size }`; hovering a tile exposes rename / remove. Thumbnails render
   via `previewNamedEmoji` reusing the decode-bomb header guard from `ReactionEmojiImage`.
   Opened from a small button in the reaction-picker row in `ChannelMessageFeed.svelte`
   (alongside the existing 😊 picker toggle at `~line 719`).

   ```
   ┌─ React with a named emoji ─────────────┐
   │ 🔎 [ catjam___________ ]                │
   │ ───────────────────────────────────── │
   │  [catjam]  [shipit]  [thisis]          │
   │  [partyR]  [fixed!]  [+23  ]           │
   │  hover a tile → ✎ rename   🗑 remove    │
   └────────────────────────────────────────┘
   ```

3. **In-context naming** (lightweight, not a second surface): on a **public** custom-emoji
   reaction chip (`ChannelMessageFeed.svelte:~686`, chips keyed `cid:${r.emojiCid}`), a hover
   affordance opens a small inline name input anchored to the chip. Encrypted chips do not
   show it (disabled-state tooltip explains why). Additionally, the existing custom-emoji
   upload flow (`handleCustomEmojiPick`, `~line 457`) offers an **optional** name field at
   upload — the most natural naming moment. Naming is always optional; unnamed emoji behave
   exactly as today.

4. **Reuse a `ReactionEmojiImage` variant** (or parametrize it) so the popover/preview path
   uses `previewNamedEmoji` (no channel context) while in-channel chips keep using
   `preview_reaction_emoji`.

## Reuse / cross-channel flow

Pick a tile in the popover → `reactToMessage(communityId, channelId, messageId, '', true,
{ cid, mime, size })` (`channel-message-service.ts:~539`) using the stored descriptor. The
post-ZEB-542 mint/verify accept the public CID with no prior channel presence; other members
render it through public CAS network-wide. Encrypted emoji are excluded because they
physically cannot render outside their origin community.

## Compatibility, privacy, permanence

- **No wire/protocol change, no migration.** Reactions on the wire are unchanged (a CID).
  Names are purely local metadata.
- **Private by construction.** Names live only in `emoji_names.json` and the DTO projection,
  never in `OwnerState` or any broadcast path — the same structural privacy guarantee
  friend nicknames rely on. A regression test asserts names never appear in published
  owner-state (mirroring the friend-nickname privacy test).
- **Removing a name** drops the map entry only; the pinned bytes persist (CAS has no GC).
- **Public-only gate** keeps sensitive/encrypted emoji out of a global personal reuse list.

## Testing

**Rust (`cargo nextest`, `--all-targets`):**

- Name CRUD round-trips through the store; `load_or_default` tolerates a missing/corrupt file.
- Charset/length validation rejects out-of-charset and >32-char names.
- **Public-only:** `set_emoji_name` and `preview_named_emoji` reject an encrypted CID;
  accept a public CID.
- Soft uniqueness is **not** backend-enforced (two CIDs may share a name).
- **Retain-on-name:** a successful name pins bytes locally; a simulated fetch failure still
  saves the name entry (does not block).
- `preview_named_emoji` is **non-channel-scoped** (returns a public CID's bytes with no
  channel React present) and size-capped.
- LWW-by-`updated_ms` merge picks the newer entry (forward-compat for ZEB-417).

**Frontend (`npx vitest run` — full suite):**

- `emoji-name-service` set/list/preview, and refresh on `emoji-names-changed`.
- Popover: name-search filters; clicking a tile calls `reactToMessage` with the stored
  `{ cid, mime, size }`; rename/remove call `setEmojiName`.
- In-context affordance is hidden on encrypted chips, shown on public chips.
- Upload-time optional naming routes the entered name to `setEmojiName` after ingest.

**Gates:** `cargo fmt --all -- --check`; `cargo clippy --all-targets … -D warnings`;
`cargo nextest run --all-targets --features test-fixtures`; `tsc --noEmit`; full `vitest`.

## Out of scope / handoff

- **Device-sync of names** — deferred to the ZEB-417 fleet-sync migration (rides along with
  friend nicknames; the `updated_ms` LWW field is already shaped for it).
- **`:name:` shortcode autocomplete** in the message composer — a different surface and more
  scope; not built.
- **Encrypted-emoji naming**, an **encountered-CID index**, and **multiple aliases per CID** —
  all out.

## Risks / open items

- `preview_named_emoji` must remain strictly public-only; a regression that let it fetch an
  encrypted CID, or that reintroduced channel-scoping incorrectly, would either break named
  preview or (worse) turn it into an unintended general fetch primitive. Tests pin both ends.
- Retain-on-name is best-effort; confirm during implementation that the public content-fetch
  path can fetch-and-store a CID by content-id alone (expected from the ZEB-542 serve/fetch
  generalization), and that "already pinned" is a cheap no-op.
- The named-preview render path must reuse the existing decode-bomb header guard; do not add
  a second, weaker image-decode path.
