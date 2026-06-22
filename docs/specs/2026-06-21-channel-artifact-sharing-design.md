# Channel Artifact Sharing via CAS — Design Spec

**Ticket:** ZEB-535 (parent epic ZEB-533 "Harmony fleet collaboration features"; 2nd child after ZEB-534 mentions).
**Status:** Approved design — implementation plan to follow.
**Date:** 2026-06-21.
**Author:** Koya (autonomous), with Jake.

## 1. Goal

Let a channel message reference one or more **artifacts** (logs / diffs / files) stored in the content-addressed store (CAS) instead of pasting large blobs as message text. A message carries a small, signed list of attachment references (CID + name/mime/size); recipients fetch the bytes over the existing CAS-serve path and get a local file. Artifacts are **members-only confidential by default** (encrypted), with an explicit public option that reuses already-public CAS content without redundant re-encryption.

This is the **data layer** only. GUI rendering/preview widgets are a follow-up (same "data-first, UI-later" shape as ZEB-534 mentions).

## 2. Background — existing primitives we build on (verified)

The chunking/CAS system already exists in **harmony core** (`harmony-content`, pinned at `8b870ae0`) and is reused, not rebuilt. Verified facts:

- **Chunker:** FastCDC content-defined chunking (`harmony-content/src/chunker.rs`); books up to `MAX_PAYLOAD_SIZE = 0xF_FFFF = 1,048,575 B` (~1 MB, 20-bit size field, `cid.rs:12`). Default targets: min 256 KiB / avg 512 KiB / max ~1 MB.
- **DAG / bundle:** B-tree Merkle DAG. Bundle node = flat array of 32-byte child CIDs; fanout `MAX_BUNDLE_ENTRIES = 32,767` (`bundle.rs:9`); depth to `MAX_BUNDLE_DEPTH = 62` (`cid.rs:21`).
  - **Single indirection layer ≈ 32,767 × 1,048,575 B ≈ 34.4 GB (≈ 32 GiB).** Multi-layer supported to depth 62 → effectively unbounded (practical ingest sub-limit ~4 PiB from a `u32` chunk count). **The structure is not the bottleneck.**
- **CID:** 32 bytes = 4-byte header (`mode|depth|size|checksum`) + 28-byte SHA-256-truncated hash (`cid.rs`). **Deterministic** over `(bytes, flags)` — no nonce/timestamp. `cid.cid_type()` distinguishes `Book` (depth 0) / `Bundle(1..=62)` / `Stream` (63) / `InlineData` from the header alone (no store lookup).
- **`ContentFlags`:** `encrypted` (0x80), `ephemeral` (0x40), `sha224` (0x20), `lsb_mode` (0x10). The `encrypted` flag is **part of CID identity** (header byte), so encrypted vs. public copies of identical bytes are two distinct, coexisting CIDs. The 28-byte hash itself does not depend on `encrypted`.
- **harmony-content does zero encryption** — the caller encrypts then content-addresses the ciphertext. Precedent: `community_state_sync::encode_root_packet` (`community_state_sync.rs:2630-2663`) does `encrypt_blob → ContentId::for_book(_, encrypted:true) → content_store.put_serveable(root, ciphertext)`. We generalize this from one book to a chunked tree.
- **`encrypt_blob` / `decrypt_blob`** (`community_state_sync.rs:156`): ChaCha20-Poly1305 with a **deterministic nonce = SHA-256(prefix ‖ key ‖ plaintext)[..12]** so two members encrypting identical plaintext under the same epoch key derive identical ciphertext → identical CID → CAS dedup/convergence. Nonce is embedded in the blob output (decrypt recovers it).
- **Client ingest:** `streaming_ingest` (`harmony-client src-tauri/src/lib.rs:448`) + `build_bundle_tree` (`lib.rs:558`) — a streaming, IPC-routed re-implementation of `dag::ingest`. **Currently hardcodes `ContentFlags::default()` (unencrypted) at every `for_book`/bundle** (`lib.rs:502,519,533,602`). This is the one place that needs an `encrypted`-flags parameter threaded through.
- **Client fetch:** `fetch_recursive` (`event_loop.rs:5631`) — async, network-fetched, `max_bytes`-bounded DAG assembly that admits each fetched book to the local `StorageTier` cache (`wrap_fetch_one_with_admission`, `event_loop.rs:5710`). Legitimately client-only (harmony-content's `dag::walk`/`reassemble` are sync/in-memory).
- **Serve gate:** `content_cid_servable(cid) = !cid.flags().encrypted || allowlist.contains(cid)` (`event_loop.rs:7466`); `CommunityServeAllowlist` is a shared `Arc<RwLock<HashSet<ContentId>>>` (`content_store.rs:31`), populated by `put_serveable`. An encrypted CID not in the allowlist is silently not served (the serve queryable `continue`s).
- **StorageTier policy:** `handle_publish` silently drops `EncryptedEphemeral` always and `EncryptedDurable` unless `encrypted_durable_persist == true`. The client's `production_content_policy` (`lib.rs:14-32`) sets `encrypted_durable_persist: true` (keeps `encrypted_durable_announce: false`). Artifacts must be **durable** (not ephemeral) so they persist.
- **Book-granularity dedup:** yes — cache/disk/archive indexes are CID-keyed; the same book referenced by many files is stored once.
- **Available-but-not-required-for-v1:** `delta.rs` (CID-aligned bundle deltas — re-share only changed children), `flatpack.rs` (child→bundle reverse index for GC safety), `bloom.rs`/`cuckoo.rs`/`sketch.rs` (set reconciliation / discovery). Noted for Future Work.

## 3. Design decisions (approved)

1. **Members-only encrypted by default** (Model A — community **epoch key** via `encrypt_blob`). Maximal reuse of audited crypto; an artifact is exactly as confidential as the channel's own messages/state. Per-artifact keys (Model B, Signal-style) are deferred to Future Work.
2. **Large files via chunking** — reuse the existing FastCDC + Merkle-DAG path. **Do not add a third ingest path:** thread an `encrypted`-flags parameter through the existing `streaming_ingest`/`build_bundle_tree`.
3. **Channel artifacts only** in v1 (no DM attachments).
4. **Configurable cap** `max_artifact_bytes`, **default 1 GiB** (fits a compiled app bundle with headroom; ~32× the "rare > 100 MB" case; well under the ~32 GiB structural ceiling). Community/operator-settable.
5. **Subtree serve-authorization** for chunked encrypted artifacts: at ingest, allowlist **every** CID of the artifact (leaves + bundles); on fetch, re-serve fetched books so the original sharer isn't a single point of failure.
6. **Public-copy reuse**: never silently downgrade a private share. Default = encrypt. The public path (deterministic-CID existence probe → reference the existing public CID instead of re-encrypting) triggers only when the user explicitly chooses public **or** is forwarding content that is already a public CAS CID.

## 4. Data model — `ChannelAttachment` and the message field

A new optional, signed list of attachment references on the channel `Post` event, structurally mirroring `mentions` (ZEB-534).

```rust
// New nested struct (signed; rides inside the channel Post event).
pub struct ChannelAttachment {
    pub cid: ContentId,   // root CID (Book or Bundle) of the stored artifact.
                          //   cid.flags().encrypted drives decrypt-or-not on fetch.
    pub mime: String,     // declared/detected MIME type.
    pub name: String,     // original filename (sensitive → signed + packet-encrypted).
    pub size: u64,        // PLAINTEXT byte length (UI + bound + post-fetch cross-check).
}
```

**Nested CBOR keys (own map, canonical order):** `cid`→`"cd"` (0x6364), `mime`→`"mi"` (0x6d69), `name`→`"nm"` (0x6e6d), `size`→`"sz"` (0x737a). Declaration order **cid, mime, name, size** matches RFC 8949 §4.2.1 bytewise order (`6364 < 6d69 < 6e6d < 737a`). All fields required (no `skip_serializing_if`). `cid` serializes as a fixed 32-byte byte string.

**Canonical `cid` representation:** `cid` is the 32-byte `ContentId` everywhere internally. It is serialized as a 32-byte CBOR byte string on the wire (under `pa`→`cd`) and surfaced as a 64-char lowercase hex string in the `ChannelAttachmentDto` that crosses the IPC boundary to the frontend.

**Post-level field** added to `ChannelPostPayload`, `SignedChannelEvent::Post`, and `ChannelPostSignedSet`:

```rust
#[serde(rename = "pa", skip_serializing_if = "Option::is_none", default)]
attachments: Option<Vec<ChannelAttachment>>,
```

**Canonical CBOR ordering (load-bearing — same discipline as ZEB-534):** the post inner-map keys must be declared in bytewise order. With `attachments`→`"pa"` (0x7061) inserted between `mentions`→`"mn"` (0x6d6e) and `reply_to`→`"rt"` (0x7274), the full order is:

```
at(6174) au(6175) bd(6264) ch(6368) ci(6369) id(6964) kd(6b64) mn(6d6e) pa(7061) rt(7274) sg(7367)
```

`attachments` is declared between `mentions` and `reply_to` in **both** `SignedChannelEvent::Post` and `ChannelPostSignedSet`.

**No-flag-day guarantee:** `attachments: None` (with `skip_serializing_if = Option::is_none`) omits the `pa` key entirely ⇒ canonical CBOR byte-identical to a pre-feature post ⇒ identical signature. Both existing wire pins (mention-less and mention-bearing) must stay byte-for-byte unchanged.

**`MAX_ATTACHMENTS = 16`**, enforced at the **three** entry points (mirroring `MAX_MENTIONS`): local mint (`publish()`), the IPC boundary (`post_channel_message_impl`), and inbound verification (`verify_channel_event`) — so a remote peer cannot bypass it with a signed event carrying an oversized `pa` array.

## 5. Encryption model

**Model A — community epoch key.** No key field on `ChannelAttachment`; the receiver already holds the epoch key. `cid.flags().encrypted` is the single signal that drives decrypt-or-not.

**Encrypt-whole → chunk-ciphertext → decrypt-after-assemble** (the clean layering): encrypt the whole file once with `encrypt_blob(epoch_key, plaintext)`; chunk the **ciphertext** through `streaming_ingest` with `ContentFlags{ encrypted: true }` on every leaf and bundle; the receiver's `fetch_recursive` reassembles the opaque ciphertext, then a single `decrypt_blob(epoch_key, ciphertext)` yields plaintext. The hardened fetch/size-cap/bundle-depth path is reused verbatim — encryption sits cleanly above it.

**v1 practical ceiling for *encrypted* artifacts is memory-bound, not structure-bound.** `encrypt_blob` operates on the whole plaintext in memory (peak ≈ plaintext + ciphertext). With the 1 GiB default cap this is acceptable on the fleet's machines. **Public (unencrypted) artifacts stream natively** through `streaming_ingest` and get the full chunking headroom now. Streaming **per-book AEAD** (removing the in-memory bound for multi-GB *encrypted* artifacts) is explicit Future Work; the configurable cap is the honest knob until we have real multi-GB transfer data.

**Epoch-key source:** ingest is scoped to `(community_id, channel_id)`; it uses the community's current epoch key via the same accessor `encode_root_packet` uses. Decryption on fetch uses the epoch key the receiver holds for that community. (Old-epoch artifacts remain decryptable by whoever held that epoch — identical semantics to existing channel content; this is intended.)

## 6. Subtree serve-authorization (the one net-new primitive)

The serve gate + `put_serveable` authorize **one CID at a time** (designed for a single community-root book). A chunked encrypted artifact is many CIDs (leaves + interior bundles); if **any** is not allowlisted, a fetch **stalls silently** on the first un-served child.

**Approach (no new gate semantics):**
- **At ingest (sharer):** after building the encrypted DAG, enumerate every CID in the artifact's subtree (reuse `collect_descendants` / `compute_keep_set` over the local store, `event_loop.rs:5554/5604`) and `put_serveable` each — leaves and bundles — into the `CommunityServeAllowlist`. Because we just produced the tree, all CIDs are local; this is a bounded loop, not new allowlist semantics.
- **On fetch (receiver):** extend the existing fetch-admission hook (`wrap_fetch_one_with_admission`) so that, for encrypted artifact CIDs, each fetched book is also `put_serveable`'d locally. This makes every fetcher a re-server, so the original sharer is not a single point of failure.

A new helper `put_serveable_subtree(root_cid)` (allowlist + persist all reachable CIDs) wraps this for both call sites. We deliberately do **not** build an ancestor-aware serve gate (it would require a parent reverse-index the serve path lacks, for no benefit).

**Single-book artifacts (≤ 1 book) are the trivial sub-case** — one `put_serveable`, identical to the community-root path, no subtree walk.

## 7. Public vs. encrypted policy & deterministic-CID reuse

**PR1 scope: default members-only encryption.** Every share in PR1 is encrypted (members-only):

- **Default: encrypt (members-only), always.** Even if an identical public copy exists, an explicit-private share produces/serves the encrypted copy (different CID); we never leak that a public copy exists.
- **Honest dedup caveat:** cross-sender dedup for *encrypted* artifacts converges only among members sharing the epoch key + the deterministic-nonce scheme (which fleet members do). Cross-key encrypted dedup is impossible by design (and correct).

**Deferred to fast-follow (not in PR1).** The public path and deterministic-CID reuse below are designed but deferred; PR1 ingests every share as encrypted (the public branch in `ingest_channel_artifact_impl` re-ingests rather than dedup-probing):

- **Public path** triggers only on explicit user choice **or** when the user forwards content that is already a public CAS CID. Implementation: compute the deterministic **unencrypted** root CID for the plaintext (via `streaming_ingest` with default flags, or an in-memory `dag::ingest`), probe CAS (`ContentStore::get` / a bounded Zenoh GET) for existence; if present, the `ChannelAttachment.cid` references that public CID (zero re-upload, zero re-encryption, dedup); if absent, publish the unencrypted tree (public class — served freely by the gate, no allowlist needed).

## 8. Share path (end to end)

1. Frontend opens a file dialog → local `sourcePath`.
2. `ingest_channel_artifact(communityId, channelId, sourcePath, name?, mime?, encrypt = true)`:
   - Stat the file; reject if `size > max_artifact_bytes` (configurable, default 1 GiB) **before** reading.
   - Detect `mime` (from extension/magic bytes) and `name` (from path) if not provided.
   - **encrypt = true:** read plaintext → `encrypt_blob(epoch_key, plaintext)` → `streaming_ingest(ciphertext, flags = encrypted)` → root CID → `put_serveable_subtree(root)`.
   - **encrypt = false (public):** compute deterministic unencrypted root CID; if it already exists on CAS, return that CID; else `streaming_ingest(plaintext, flags = default)` → root CID (public class; no allowlist needed).
   - Return `ChannelAttachmentDto { cid: hex, mime, name, size }`.
3. `post_channel_message(communityId, channelId, body, replyTo?, mentions?, attachments?)` — the existing IPC gains an `attachments` param; the signed event carries the `pa` list (bounded by `MAX_ATTACHMENTS`, all CIDs validated as 32-byte hex).

## 9. Receive / fetch path (end to end)

1. Inbound packet decrypts → `verify_channel_event` verifies the signed event (which now covers `pa`); the cap is re-checked here.
2. `attachments` ride the `ChannelMessageDto` (hex cid + mime/name/size); the frontend lists them (no auto-download).
3. On user action: `download_channel_artifact(communityId, cid, destPath, expectedSize, maxBytes?)` (`communityId` scopes the epoch key; `expectedSize` is the attachment's declared plaintext size):
   - `fetch_recursive(root_cid, cap)` reassembles the (cipher-or-plain) bytes, bounded by `min(maxBytes, max_artifact_bytes)`; per-book + assembled caps + `MAX_BUNDLE_DEPTH` enforced by the existing path.
   - If `cid.flags().encrypted`: `decrypt_blob(epoch_key, bytes)` → plaintext; else use bytes directly.
   - Verify assembled **plaintext** length == `expectedSize`; reject on mismatch.
   - Write plaintext to `destPath` (streamed to a temp file, atomic rename on success); return `bytesWritten` (== `expectedSize`).
   - Each fetched book is admitted + (for encrypted artifacts) re-served via the extended admission hook.

## 10. IPC surface (new / changed)

- `ingest_channel_artifact(communityId, channelId, sourcePath, name?, mime?, encrypt?) -> ChannelAttachmentDto` (new; streams from path; `encrypt` defaults true).
- `post_channel_message(..., attachments?: ChannelAttachmentDto[])` (extend existing — add trailing optional param, mirroring how `mentions` was added).
- `download_channel_artifact(communityId, cid, destPath, expectedSize, maxBytes?) -> bytesWritten` (new; `communityId` scopes the epoch key; fetch → decrypt-if-encrypted → size-verify against `expectedSize` → write to path).

All new IPCs normalize rejections per the ZEB-534 lesson: callers extract `e instanceof Error ? e.message : String(e)`; service methods rethrow `new Error(msg)`.

## 11. Frontend service

Extend `ChannelMessageService` (`src/lib/channel-message-service.ts`):

- `ChannelMessageDto` gains `attachments?: ChannelAttachmentDto[]` (`{ cid, mime, name, size }`).
- `postMessage(..., attachments?: ChannelAttachmentDto[])` — forwards an empty list as `undefined` (so the backend never emits `pa: []`, which would change signed bytes — same normalization as `mentions`).
- New thin facades `ingestArtifact(...)` and `downloadArtifact(...)` over the new IPCs, with rejection normalization.
- No GUI widgets in v1 (the agents/CLI are the first consumers).

## 12. Bounds, limits, validation

- `max_artifact_bytes`: configurable policy, default 1 GiB; enforced at ingest (pre-read) and as the fetch ceiling.
- `MAX_ATTACHMENTS = 16` per message at all three gates.
- Per-attachment: `cid` must be 32-byte hex; `name` length-bounded to 255 bytes and `mime` to 255 bytes, validated at the IPC boundary and inbound (`verify_channel_event`).
- Existing per-leaf (~1 MB), assembled (`max_bytes`), and `MAX_BUNDLE_DEPTH = 62` caps are inherited from `fetch_recursive`.
- Empty `attachments` normalized `Some([]) -> None` at mint, IPC, and frontend (no-flag-day).

## 13. Error handling

- Ingest: file too large → reject before read; encrypt failure → surfaced `Err`; epoch key unavailable → surfaced `Err`.
- Post: oversized `attachments` / bad CID hex / over-length name/mime → reject at IPC and inbound.
- Fetch: cap exceeded / hash mismatch / missing child / decrypt failure (wrong epoch / tamper) / plaintext-size mismatch vs declared → surfaced `Err`; partial output never written to `destPath` on failure (write to a temp file, atomic rename on success).

## 14. Testing strategy

- **Wire pins:** `attachments: None` byte-identical to a pre-feature post (both prior pins unchanged); a new pin for a populated `pa` post (single attachment).
- **Round-trip (unit):** encrypt → chunk → `put_serveable_subtree` → `fetch_recursive` → decrypt == original, for (a) a single-book artifact and (b) a multi-book artifact spanning ≥ 2 leaves and ≥ 1 bundle.
- **Cap enforcement:** `MAX_ATTACHMENTS` rejected at all three gates; `max_artifact_bytes` rejected at ingest; declared-size-vs-assembled mismatch rejected on fetch.
- **Subtree authorization (integration):** two-node test (extend `cas_serve_two_node_integration.rs`) — node A shares a multi-book **encrypted** artifact, node B fetches + decrypts the full file; assert it stalls/ errors cleanly if an interior CID is *not* allowlisted (negative test) and succeeds when `put_serveable_subtree` ran.
- **Public reuse:** ingest plaintext public → compute deterministic CID → re-ingest identical bytes returns the same CID without re-upload; encrypted vs public copies of identical bytes yield distinct CIDs.
- **DTO projection + empty normalization;** frontend service forwards/lists/ingests/downloads (mocked adapter).

## 15. Security considerations

- **Confidentiality** rests on actual encryption (CID over ciphertext), not gate-only access control: a non-member who obtains encrypted bytes cannot read them without the epoch key.
- **Tamper-evidence:** `name`/`mime`/`size`/`cid` are inside the signed event (the `pa` list is covered by `sg`), and packet-encrypted in transit, so they are both confidential and unforgeable. The fetched plaintext is bound to the attachment by `cid` (hash verify) and `size` (length cross-check).
- **No silent private→public downgrade:** the public path is opt-in or already-public-only.
- **Serve-gate completeness:** the negative subtree-authorization test guards against the silent-stall failure mode where a missing interior allowlist entry breaks fetch with no error.

## 16. Out of scope (v1)

GUI render/preview/thumbnail widgets; DM attachments; per-artifact keys (Model B); streaming per-book AEAD for multi-GB *encrypted* artifacts; orphan/GC of unreferenced artifact CIDs; `delta.rs`/`bloom.rs` replication optimizations; cross-community artifact references.

## 17. Future work

- **Streaming per-book AEAD** to lift the in-memory encryption ceiling toward the structural ~32 GiB / multi-GB range (with real transfer-testing data to set the recommended cap).
- **Per-artifact keys (Model B)** if cross-community sharing or finer revocation is needed.
- **GC / retention** for unreferenced artifact CIDs (reuse `flatpack.rs` reverse index; `compute_keep_set` for "pin this whole DAG").
- **Replication efficiency** via `delta.rs` (re-share only changed children) and `bloom.rs`/`cuckoo.rs` (peer availability discovery).
- **GUI** attachment rendering/preview.
- **Codebase refactor (separate track, ZEB-533-adjacent):** evaluate moving platform-wide CAS pieces from harmony-client into harmony core, and modularizing harmony-client into smaller components. Track as its own issue; not part of this feature.

## 18. Open questions / risks

1. **Whole-file in-memory encryption** caps practical *encrypted* artifacts at the configurable limit (default 1 GiB). Accepted for v1; streaming AEAD is the future unlock. (Confirm 1 GiB default at spec review.)
2. **Allowlist growth:** a chunked artifact inserts thousands of CIDs into the unbounded `CommunityServeAllowlist` HashSet. Functionally fine; revisit if memory pressure appears at scale (bulk/subtree allowlist representation is a future optimization).
3. **`encrypt_blob` nonce recovery on decrypt** — confirm `decrypt_blob` recovers the embedded nonce for the artifact ciphertext path (it does for community roots; verify the same call shape works here).
4. **Ingest drift:** `streaming_ingest` vs core `dag::ingest` are parallel implementations; threading flags must keep them behaviorally identical. Consider a shared test asserting identical root CIDs for the same input.
