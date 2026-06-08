# ZEB-344 — Receive-side avatar byte cap + decode-dimension guard

**Status:** Approved 2026-06-08
**Issue:** [ZEB-344](https://linear.app/zeblith/issue/ZEB-344) (Medium, harmony-client)
**Builds on:** ZEB-343 (CAS-served avatars, PR #172)

## Problem

ZEB-343 shipped CAS-served profile avatars. Size/decode limits are enforced
**only on our own ingest**, never on the **receive/fetch** side:

- **Decode bomb (sharp).** `AvatarResolver.fetchCid` (`src/lib/avatar-resolver.ts`)
  fetches raw bytes and hands a blob URL straight to `<img>` with **no
  decoded-dimension guard**. `assertDecodedDimsOk` / `AVATAR_MAX_DECODED_DIM=8192`
  (`src/lib/avatar-normalize.ts`) runs only on local ingest. A signed card pointing
  at a small-on-wire / huge-on-decode image can OOM a viewer's renderer.
- **Unbounded fetch byte size.** `fetch_content` (`src-tauri/src/lib.rs:11941`) is
  the generic CAS fetch with no upper byte bound. The ingest cap
  `MAX_AVATAR_BYTES = 512 * 1024` (a local `const` in `ingest_avatar_bytes_inner`,
  `lib.rs:8929`) bounds **ingest only**. A card advertising an oversized
  `avatar_cid` makes peers download the whole object.

**Threat model:** the `avatar_cid` rides an **owner-device-signed**
`ProfileCardBroadcast`, so this is **griefing-within-a-trusted-community** (a
malicious/compromised member), not open-internet DoS. The existing verify-on-fetch
`hash == CID` (ZEB-343 T4) closes byte-*substitution*; this ticket closes
byte-*size* and decoded-dimension, which hashing does not bound.

## Goal

Give the avatar **receive** path the same hard bounds the **ingest** path already
has — so a member can't OOM a viewer (decode bomb) or force an unbounded download
via an oversized `avatar_cid`.

## Architecture

Two complementary bounds, each mirroring an existing ingest-side guard:

- **Rust byte cap** — a new avatar-semantic `fetch_avatar` IPC that fetches with a
  byte ceiling threaded through `fetch_recursive`, so the download **aborts at/under
  the cap** rather than buffering the whole object first. Generic `fetch_content`
  stays untouched and unbounded for its other callers (profile docs, file content).
- **TS decode guard** — `AvatarResolver` decodes via `createImageBitmap` and runs
  the existing `assertDecodedDimsOk` before building the blob URL, falling back to
  identicon on reject. Parity with `normalizeAvatar`'s ingest guard.

## Component 1 — Rust: `fetch_avatar` + bounded `fetch_recursive`

1. **Shared cap constant.** Promote the ingest cap to a module-level
   `pub(crate) const AVATAR_MAX_BYTES: usize = 512 * 1024`, replacing the local
   `const MAX_AVATAR_BYTES` in `ingest_avatar_bytes_inner` (which now references the
   shared const). Ingest and receive then share one value and cannot drift. 512KB is
   the chosen ceiling: a 256×256 PNG is realistically ≤256KB, so 512KB already
   carries ~2× headroom; we control the ingest cap, so there is no version-skew
   argument for more (YAGNI).

2. **Bounded `fetch_recursive`.** Add `max_bytes: Option<usize>` to `FetchRequest`
   (`event_loop.rs:190`) and thread it into
   `fetch_recursive(fetch_one, root, max_bytes)` (`event_loop.rs:5248`). After each
   `out.extend_from_slice(&bytes)`, if `max_bytes` is `Some(cap)` and
   `out.len() > cap`, return `Err`. This bounds the assembled download to
   ≤ `cap + one chunk` (a single chunk is already bounded by `ChunkerConfig::DEFAULT`)
   and aborts before fetching further chunks. **Non-breaking:** the single production
   caller (the `fetch_rx` arm, `event_loop.rs:2529`) passes `req.max_bytes`; the four
   test callers and `fetch_content` pass `None` (unbounded — unchanged behavior).

3. **`fetch_avatar(cid)` IPC.** Identical hex-validation + `FetchRequest` oneshot
   path as `fetch_content`, but constructs the request with
   `max_bytes: Some(AVATAR_MAX_BYTES)`. Registered in the `invoke_handler!` registry.
   `fetch_content` is updated only to set `max_bytes: None`.

## Component 2 — TS: `AvatarResolver` decode guard

In `fetchCid` (`src/lib/avatar-resolver.ts`):

- Switch `invoke('fetch_content', { cid })` → `invoke('fetch_avatar', { cid })`.
- Before building the blob URL, decode and dimension-check:
  ```ts
  const bmp = await createImageBitmap(blob);
  try {
    assertDecodedDimsOk(bmp.width, bmp.height);
  } finally {
    bmp.close();
  }
  ```
  Only on success build the object URL and cache it. On throw (decode failure or
  over-dimension), treat it exactly like a fetch failure today —
  `failedAt.set(cid, Date.now())`, no cached URL → the UI renders the identicon.
- Reuses `assertDecodedDimsOk` / `AVATAR_MAX_DECODED_DIM=8192` from
  `avatar-normalize.ts`.

## Data flow & error handling

- Over-cap fetch → Rust `Err` → resolver `catch` → identicon fallback (reason logged
  via the existing `console.warn`).
- Over-dimension decode → resolver `catch` → identicon fallback.

Both are neutral user-facing fallbacks (an avatar that won't render shows the
identicon), consistent with the resolver's current failure handling.

## Honest limitation (parity, not superiority)

`createImageBitmap` **decodes** the image before `.width/.height` are readable, so
the dimension check runs *after* the decode allocation — it prevents the downstream
`<img>`/canvas amplification but not the initial decode. This is exactly the ingest
guard's behavior (`normalizeAvatar` does the same), so it achieves the **parity** the
ticket asks for. A stricter guard would parse the PNG/JPEG header for dimensions
*before* decoding — a cross-cutting improvement that should harden ingest **and**
receive together. That is filed as a separate **Low** follow-up rather than making
the receive path asymmetrically stricter here.

## Testing

- **Rust (`event_loop.rs` tests):** `fetch_recursive` with `Some(cap)` rejects once
  accumulated bytes exceed the cap (fixture: a multi-chunk root whose assembled size
  exceeds `cap` → `Err`); with the same root and `None` → full bytes (regression
  guard that existing callers stay unbounded); under-cap with `Some(cap)` → full
  bytes.
- **TS (`avatar-resolver` test):** a blob whose mocked `createImageBitmap` reports
  dims > `AVATAR_MAX_DECODED_DIM` → no cached URL, `failedAt` set (identicon); a
  normal blob → resolves and caches. Mock `createImageBitmap` (jsdom lacks it),
  mirroring the existing `avatar-normalize` tests. Assert the invoke targets
  `fetch_avatar`.

## Acceptance criteria (from ticket)

- A peer serving an in-cap-bytes but huge-decoded image is rejected on receive
  (identicon fallback) — covered by the TS decode-guard test.
- A peer advertising an over-cap `avatar_cid` does not cause an unbounded download;
  the fetch is rejected at/under the ceiling — covered by the bounded
  `fetch_recursive` test.
- No regression to the ZEB-343 happy path (valid ≤256² avatars still resolve +
  render cross-peer) — the `None`/under-cap tests + unchanged `fetch_content`.

## Out of scope

- **Header-only pre-decode dimension parse** (the parity-limitation follow-up above)
  — separate Low ticket; would harden ingest + receive together.
- **Stale-avatar CAS-object GC** — ticket says verify W-TinyLFU ages it out, don't
  build GC.
- **Bounded `AvatarResolver.cache` eviction** — session-scoped, cleared on
  `destroy()`; not this ticket.
