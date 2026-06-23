# Public Custom Emoji — Design Spec

**Status:** Approved (brainstorm), ready for implementation plan
**Date:** 2026-06-22
**Builds on:** custom CAS-backed emoji reactions (merged `main` @ `40cc9446`)
**Enables:** the personal custom-emoji nickname/reference system (separate, later spec)

## Goal

Make custom reaction emoji **public by default** — unencrypted, content-addressed by
`hash(plaintext)`, so one image is one content-id (CID) network-wide: deduplicated,
freely cached and served by anyone who holds it, and never expiring. Preserve an
**opt-in encrypted mode** (the existing behavior) for the occasional emoji a community
wants access-controlled.

## Background & motivation

The merged custom-emoji-reactions work treats every custom emoji as an **encrypted**
CAS blob: ingest encrypts the image under the community's live epoch key, the CID is the
hash of the *ciphertext*, and both the mint IPC and `verify_channel_event` **reject** any
custom-emoji CID whose `encrypted` flag is not set.

That choice gives confidentiality but forfeits the defining superpower of a
content-addressed store — deduplication:

- A content address is `hash(bytes)`. Encrypt first and the CID is
  `hash(ciphertext under community-epoch-key)`, so the **same image is a different CID in
  every community** (and again after every key rotation). Hosting is fragmented; nothing
  is shared.
- Skip encryption and the CID is `hash(plaintext)` — **one image, one CID, globally,
  forever.** Every node that has ever seen the image holds and serves the *same* blob;
  popular emoji get cached across the whole network for free.

Custom emoji are the content type that most wants to be public: people want them
portable and reusable, not locked to a single community. Public custom emoji win on four
axes simultaneously:

1. **Dedup / shared hosting** — one canonical copy, network-wide caching.
2. **Truly global identity** — a future personal name like `:catjam:` maps to one
   canonical CID that works in *every* community, not just where it was first seen.
3. **Durability** — a plaintext blob never becomes undecryptable, eliminating the
   epoch-key-rotation failure mode that affects encrypted emoji.
4. **Simpler render / serve / auth** — the serve gate already allows unencrypted CIDs
   with no allowlist, and the authorize-then-decrypt path collapses to a plain fetch.

The cost of public is real but bounded: a public blob is **world-readable** to anyone who
learns its CID and is **un-publishable** (the store has no garbage collection). That is
why we keep an encrypted escape hatch for sensitive emoji rather than dropping
encryption entirely.

## Model

The CID's `encrypted` flag is the discriminator. It already exists, and the serve gate
already branches on it (`content_cid_servable = !cid.flags().encrypted ||
serve_allowlist.contains(cid)`, `event_loop.rs:~8079`). The merged encrypted path stays
exactly as it is. This spec:

1. Flips the **ingest default** to public.
2. Relaxes the two **"must be encrypted"** invariants (mint + verify) to "public *or*
   encrypted."
3. Adds a public, **no-decrypt** render branch.

Net effect is mostly additive: we unlock the `false` value of an axis that already exists
and was being rejected by two guard clauses.

## Architecture — backend seams

All five touch points are already located. Line numbers are approximate (post-merge) and
for orientation only.

1. **Ingest** — `ingest_channel_artifact_bytes_inner` (`src-tauri/src/lib.rs:~20751–20814`).
   Today it always encrypts: `encrypt_blob(&epoch_key, &plaintext)` (using the live epoch
   key from `spaces.get(community_id).current_epoch_key`), then chunks the ciphertext via
   `streaming_ingest_with_options(..., ContentFlags { encrypted: true, .. }, serveable: true)`.
   - **Add a visibility mode** (default public).
   - *Public:* skip `encrypt_blob`; feed the **plaintext** to `streaming_ingest_with_options`
     with `ContentFlags { encrypted: false, .. }`. The CID becomes `hash(plaintext)`.
   - *Encrypted:* unchanged.
   - `size` remains the **plaintext length** in both modes (it already is — the DTO `size`
     is plaintext length, independent of ciphertext length).

2. **Mint** — `set_message_reaction_impl` (`src-tauri/src/lib.rs:~20383–20502`).
   Today it rejects a custom-emoji CID whose `encrypted` flag is unset
   (`~20468–20472`, "custom emoji cid must reference an encrypted CAS blob").
   - **Remove that rejection**; accept either flag.
   - Keep the existing checks: CID hex length 64, decodes to `[u8; 32]`, size ≤
     `MAX_CUSTOM_EMOJI_BYTES`, custom-emoji-must-not-also-carry-unicode.

3. **Verify** — `verify_channel_event` React block (`src-tauri/src/community_channel_log.rs`).
   Today it returns `ChannelEventError::CustomEmojiNotEncrypted` for an unencrypted custom
   CID.
   - **Relax** to accept public *or* encrypted.
   - Keep `CustomEmojiWithUnicode` (no unicode alongside a custom emoji) and the
     image-mime / size invariants — these still bind remote peers.

4. **Render / fetch** — `authorize_and_fetch_artifact` (`src-tauri/src/lib.rs:~20954–21061`),
   reached from `preview_reaction_emoji_impl` (`~21207–21232`) with
   `AttachmentScope::ReactionEmoji`.
   - **Branch on the CID's `encrypted` flag.**
   - *Encrypted:* existing authorize + fetch + decrypt.
   - *Public:* skip decryption (return bytes as-is), but **keep the channel-scoping** — the
     CID must still appear in a signed React in this channel before we fetch — so this IPC
     does not become a general "fetch any public blob" primitive. The channel-React scan
     also supplies the descriptor `size`.
   - The frontend decode-bomb guard (header-dims parse before `createImageBitmap`) is
     unchanged and applies to both modes.

5. **Serve** — *no change.* Unencrypted CIDs are already serveable with no allowlist;
   encrypted emoji keep `serveable: true` at ingest (auto-allowlisted). A public emoji is
   therefore hosted by every node that holds it, automatically.

## Architecture — frontend changes

Small; the render path already handles both flags.

- **`handleCustomEmojiPick`** (`src/lib/components/ChannelMessageFeed.svelte:~482–504`):
  file-pick → `ingestEmojiBytes` → `{ cid, size }` → `reactToMessage`. Add a per-upload
  **"keep private to this community"** checkbox to the flow, default **off** (public), with
  the permanence warning as helper text (see below).
- **`ingestEmojiBytes`** (`src/lib/channel-message-service.ts:~571`): add a visibility /
  `encrypted` parameter threaded to the `ingest_channel_artifact_bytes` IPC, defaulting to
  public.
- **`ReactionEmojiImage` / `previewReactionEmoji`**: unchanged. The backend branches on the
  flag; the decode-bomb guard is identical for both. No new fetch or security code on the
  frontend.

## Compatibility, abuse surface, permanence

- **Backward-compatible, no migration.** Existing encrypted emoji keep rendering via the
  retained encrypted branch. New emoji default public. The same image existing as an old
  encrypted CID and a new public CID simply coexist; the public one becomes the dedup'd
  canonical going forward.
- **Abuse surface.** Public ingest is effectively "publish an arbitrary public blob to the
  network." It is bounded by the *existing* `MAX_CUSTOM_EMOJI_BYTES` cap and image-mime
  validation (same guards in both modes), so the surface is "small images only." Flag this
  explicitly for code-review bots.
- **Permanence is the sharp edge.** Public = world-readable to anyone who learns the CID
  and **un-publishable** (no GC). This is irreversible, so the upload UI shows clear inline
  helper text — *"Public emoji can be cached and re-shared by anyone and can't be deleted
  later"* — rather than a heavy typed-confirm (overkill for an emoji). The "keep private"
  toggle is the escape hatch for anything sensitive.

## Testing

**Rust (`cargo nextest`, `--all-targets`):**

- *Ingest:* public mode yields a CID with the `encrypted` flag **false** and
  `CID == hash(plaintext)`; encrypted mode unchanged (flag true, hash of ciphertext).
- *Mint* (`set_message_reaction_impl`): now **accepts** a public custom-emoji CID; still
  accepts encrypted; still rejects custom-with-unicode and oversize.
- *Verify* (`verify_channel_event`): accepts a public React **and** an encrypted React;
  still rejects unicode+custom, bad mime, oversize.
- *Render* (`authorize_and_fetch_artifact`): public CID → returns bytes with **no decrypt**,
  still gated on the CID appearing in a channel React; encrypted CID → existing
  authorize+decrypt.
- *Two-engine integration:* a public emoji reacted in community A is fetchable by a peer,
  **and** the same plaintext ingested twice produces the **identical** CID (dedup proof).

**Deliberate test inversion (not drift).** The merged tests
`set_message_reaction_rejects_unencrypted_custom_emoji_cid` and
`verify_react_rejects_unencrypted_custom_emoji_cid` asserted rejection of unencrypted
CIDs. They are rewritten to assert **acceptance** of public, keeping an encrypted-still-valid
case alongside. This is an intentional behavior change.

**Frontend (`npx vitest run` — full suite):** `handleCustomEmojiPick` defaults public and
routes the private checkbox to encrypted; `ingestEmojiBytes` param defaults public;
`ReactionEmojiImage` tests stay green unchanged.

**Gates:** `cargo fmt --all -- --check`; `cargo clippy --all-targets … -D warnings`;
`cargo nextest run --all-targets`; `tsc --noEmit`; full `vitest`.

## Out of scope / handoff

- **Use any public emoji anywhere** (react in a channel where the emoji has never
  appeared) — deliberately deferred. The model now permits it; it lands with the nickname
  work via a separate, intentional render path rather than by loosening this one.
- **The personal nickname/reference system** — the next spec, built on this foundation.
  Its design is already brainstormed and approved at the foundation level: a personal
  `name → CID` alias layer stored in owner-state (synced device→device, never published),
  global-to-user names (`[A-Za-z0-9_-]`, ≤32 chars, soft set-time uniqueness, CID-keyed),
  used for faster reaction *picking* via a single new picker popover with name-search +
  in-context "name this emoji" on a reaction chip, retain-on-name. Public emoji make it
  strictly better (global CIDs, automatic hosting, no durability caveat).
- **Encrypted-emoji epoch-key retention** (the durability caveat for the now-rare
  encrypted path) — a small separate follow-up if it ever bites.
- **Attachments public-reuse** generalization, and a **per-community default
  public/private policy** — both out.

## Risks / open items

- Relaxing the two encryption guards must not weaken the *other* React invariants
  (no-unicode, mime, size). The plan keeps those checks; tests assert they still bind.
- The public render branch must keep channel-scoping; a regression there would turn a
  scoped emoji-render IPC into an unscoped public-CAS fetch.
- Confirm `streaming_ingest_with_options` accepts `ContentFlags { encrypted: false }` and
  that downstream serve/fetch treat the resulting CID as public (expected from the serve
  gate, to be verified during implementation).
