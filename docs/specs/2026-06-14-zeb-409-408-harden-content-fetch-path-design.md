# Harden the content-fetch path (ZEB-409 + ZEB-408)

**Date:** 2026-06-14
**Tickets:** ZEB-409 (Rust, Medium) + ZEB-408 (frontend, Low) — both ZEB-344 follow-ups, bundled into one PR (one bot-review round).
**Branch:** `zeb-409-408-harden-content-fetch` off `main` `15b8e00a`.

## Problem

Two residual decode/fetch memory-amplification gaps left open by ZEB-344 (avatar/CAS receive caps):

- **ZEB-409 (Rust):** `FetchRequest.max_bytes` is enforced only in `fetch_recursive` *after* `out.extend_from_slice(&bytes)` — the **assembled total** across the content DAG. A single oversized leaf served by a hostile peer under an `avatar_cid` is fully materialized by `fetch_via_zenoh` (`sample.payload().to_bytes().to_vec()`) before the post-extend cap rejects it. Transient Rust-side RAM spike, bounded only by what the peer serves. (In-community griefing threat model; non-hot path.)
- **ZEB-408 (frontend):** both the ingest guard (`normalizeAvatar`) and receive guard (`AvatarResolver.fetchCid`) call `createImageBitmap(...)` and *then* `assertDecodedDimsOk(bmp.width, bmp.height)`. `createImageBitmap` decodes the full image (allocates the bitmap) before `.width/.height` are readable, so the dimension check fires *after* the decode-bomb allocation.

## Decision

### ZEB-409 — reject a leaf before the contiguous copy

Honest scope: by the time we hold a Zenoh `Sample`, the transport has already received the full payload into its own `ZBytes` buffer. Preventing *that* requires a transport-level streaming fork of `fetch_via_zenoh` — the heavy, cross-cutting change ZEB-344 explicitly deferred and which is **out of scope** here. What we do instead (the ticket's "check a declared size before draining"):

- Read `sample.payload().len()` (zenoh 1.9.0 `ZBytes::len() -> usize`, no copy) and reject **before** `.to_bytes().to_vec()` — and therefore before `fetch_recursive`'s `out.extend_from_slice`. This eliminates the two contiguous copies our own code makes (the `Vec` from `.to_vec()` and the `out` extend), which *are* the "Rust-side memory spike" the ticket targets.
- Thread `max_bytes: Option<usize>` into `fetch_via_zenoh`. Only the content-fetch `fetch_one` closure (event_loop.rs:3360, where avatar passes `Some(AVATAR_MAX_BYTES)`) passes the cap; the three other callers (`CasOp::GetOrFetch` 3514, `RuntimeAction::FetchContent` 5792, `FetchModule` 5810) pass `None` — unchanged behavior.
- Keep `fetch_recursive`'s assembled-total check **unchanged**. The two bounds are complementary defense-in-depth: per-leaf catches one giant leaf early; assembled-total catches a bundle of many small leaves summing over cap.
- Boundary mirrors the existing check: reject when `len > cap` (allow `== cap`).
- Extract a pure helper `leaf_cap_exceeded(payload_len, max_bytes) -> Option<usize>` (returns `Some(cap)` when over) so the threshold logic is unit-testable without a Zenoh session — same philosophy as the frontend's pure `assertDecodedDimsOk`.

### ZEB-408 — parse header dims before decode

- Add a shared, DOM-free helper in `src/lib/avatar-normalize.ts`:
  - `parseImageHeaderDims(bytes: Uint8Array): { width, height } | null` — PNG IHDR (width @16, height @20, big-endian u32) and JPEG SOFn marker walk (dims at `[precision(1)][height(2)][width(2)]`). Returns `null` for GIF/WebP/unknown/truncated headers.
  - `assertHeaderDimsOk(bytes)` — runs `assertDecodedDimsOk` on parsed dims; **no-op when unparseable** (the existing post-decode guard still applies — harden, don't weaken).
- Call `assertHeaderDimsOk(bytes)` before `createImageBitmap` in **both** `normalizeAvatar` (ingest) and `AvatarResolver.fetchCid` (receive). Keep the post-decode `assertDecodedDimsOk` as the fallback for formats we don't header-parse.

## Testing

- **Rust:** unit-test `leaf_cap_exceeded` — `None` unbounded, under/at cap ok, over-cap reports the cap. (`fetch_recursive`'s existing assembled-total tests are untouched.)
- **Frontend (vitest):** unit-test `parseImageHeaderDims`/`assertHeaderDimsOk` — crafted small-on-wire / huge-header PNG and JPEG are rejected; small valid PNG/JPEG and unknown/truncated headers do not throw. Extend `avatar-resolver.test.ts` to assert a header-bomb is rejected **without** `createImageBitmap` being called (the mock records zero calls).

## Risk

Low. Per-leaf bound is strictly tighter-or-equal to the existing assembled-total bound for the one bounded caller; `None` callers are byte-identical. Frontend helper only *adds* an earlier rejection and degrades to the current behavior on any unparseable header, so it cannot falsely reject a legitimate image whose header it fails to parse (it returns `null` → no throw).
