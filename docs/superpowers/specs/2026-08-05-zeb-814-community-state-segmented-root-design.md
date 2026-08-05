# ZEB-814 — Segmented community-state root (manifest + immutable CAS segments)

**Status:** Design (approved shape 2026-08-05)
**Ticket:** ZEB-814 · branch `zeblith/zeb-814-community-state-root-scaling` (off `main` @ 2e2afffd)
**Scope tier (approved):** MVP — per-publisher O(delta). Cross-peer deterministic dedup and RBSR tail-reconcile are explicit non-goals (follow-ups).

---

## 1. Problem

`encode_root_packet` (`src-tauri/src/community_state_sync.rs:3106`) canonical-CBOR-encodes the **entire** `CommunityState`, encrypts it under the live epoch key, and derives **one** `ContentId::for_book` (`:3246`), hard-capped at `harmony_content::cid::MAX_PAYLOAD_SIZE` (1 MiB − 1). Both the publish path (`publish_root_now`, `:3320`) and the query-serve path (`:2869`) route through this single encoder. When the encoded state exceeds the cap, `for_book` rejects it and **root publish AND query-serve fail entirely** — the community can no longer converge or bootstrap new members.

### 1.1 Corrected premise (why the ticket's arithmetic is stale)

The ticket predates two merged siblings:

- **ZEB-815** (Done) moved `ReachabilityAnnounce` / `CommunityRelayAnnounce` out of the membership log into a bounded, eviction-based address book. Those announces were the ~640 B × members term that drove the ticket's "~1,500 members" figure. **That term is gone.**
- **ZEB-813** (Done) shipped only *observability* — the 50/80/100 % watermark warnings + `report_degraded` at `community_state_sync.rs:3185-3241`. The structural cliff remained.

Post-815, the root blob's only unbounded field is `CommunityState.log: VerifiedLog<MembershipPolicy>` (CBOR field `"ev"`, `community_state_crdt.rs:174`). This log is **append-only forever**: the engine's only compaction is supersession, and `MembershipPolicy::supersession_key` returns `Some` for exactly the two announce kinds ZEB-815 removed and `None` for every governance kind (`community_state_crdt.rs:425-447`, load-bearing comment: *"every other kind is durable community history and must never be compacted"*). So growth tracks **total lifetime governance churn**, not current membership size — a member who joins then leaves leaves two permanent events behind.

Corrected cliff arithmetic: ~150–260 B per bare governance event (Leave/Kick/SetPower), ~400–700 B per Join/PendingJoin (enrollment cert required, `community_membership.rs:626-631`). The 1 MiB cap therefore falls at **~2,000–6,000 lifetime membership events** — not imminent for a fleet-sized community, but on a clock for any long-lived one. The root was already observed at ~100 KB (10 % of cap) in the ZEB-805 incident (`community_state_sync.rs:8583`).

### 1.2 Success criterion (from the ticket)

> A community with 100k members can publish and serve its state root (or the equivalent bootstrap surface) without hitting a fixed-size cliff, and approaching any remaining bound is visible on a health surface well before failure.

---

## 2. Three blockers to naïve segmentation (all confirmed in-code)

A naïve "chunk the CBOR into content-addressed leaves and dedup" fails:

1. **Serialized log is random-`EventId`-ordered.** `CommunityState.log` serializes as a `BTreeMap<EventId, SignedMembershipEvent>` keyed by `EventId` ascending (`community_state_crdt.rs:174`, `verified_log.rs:296-298`); `EventId` is a random `[u8;16]` (`lib.rs:34587`). A new event lands at a *random* byte offset → no prefix stability → segmenting the existing CBOR gives no dedup. **Fix:** lay segments out in **time order** (`event_sort_key`), a serialization distinct from the legacy `"ev"` map.

2. **No ingest floor.** The clock hardening is a *future*-skew ceiling only (`clock_trust.rs`, `MAX_FORWARD_SKEW_MS = 5 min`); there is no past-floor. `insert_event` is order-independent (`community_state_crdt.rs:669`), so a backdated (low-HLC) event is always accepted and sorted into its historical position. No HLC range is ever provably "closed." **Fix:** immutability is *by convention with localized re-seal*, and segment boundaries are **absolute HLC ranges** (not positional counts) so a backdated event re-seals exactly one segment — no cascade.

3. **Epoch rotation re-keys the blob.** `encrypt_blob` mixes the live epoch key into both the deterministic nonce and the cipher (`community_state_sync.rs:167-187`); epoch rotates on every Kick/Leave (`live_epoch_key`, `:3027`). The same events under epoch N vs N+1 produce a different ContentId. **Fix:** decouple segment identity from the epoch key via **envelope encryption** (§4.2).

Two facts that make the design tractable:

- **Every member publishes its own view** — many-writers-converge on a community-keyed Zenoh topic (`harmony/community/{id}/state-root-v1`, `event_loop.rs:9680`), `ConsolidationMode::None`, 250 ms debounce. Not admin-gated, not a single publisher.
- **When two peers' materialized state matches, they already produce byte-identical root CIDs** — `encrypt_blob` is deterministic in (key, cleartext). So CAS dedup already works *when states converge*.

---

## 3. Scope decision: the cliff is CAS-only

The 1 MiB cap is a `ContentId`/CAS/wire limit. The **local** persistence (`community_state_persist.rs`, `communities/{id}/crdt.cbor`) is a plain atomic-rename file with **no** such cap. Therefore the MVP changes **only the publish / serve / receive-bootstrap path** and leaves local persistence as the monolithic `CommunityState`, plus one small new sidecar (the publisher's segment index, §4.4).

Consequences:
- `crdt.cbor` byte-pin fixtures (`tests/wire_format/community_fixtures.rs`, `zeb250_fixtures.rs`, `zeb285_fixtures.rs`) are **untouched**.
- The in-memory `CommunityState` still holds the full log (tens of MB at extreme scale — the same as today, and not the cliff this ticket addresses).
- This is one focused spec, not a storage-format migration.

---

## 4. Architecture

### 4.1 Objects

**Segment** — an immutable, content-addressed blob holding a contiguous (by `event_sort_key`) run of `SignedMembershipEvent`s.

```text
SegmentCleartext (canonical CBOR):
  "vn" : u16                        // segment format version (= 1)
  "ci" : SpaceId                    // community_id (misroute guard, mirrors CommunityState "ci")
  "ev" : Vec<SignedMembershipEvent> // events in event_sort_key ascending order
```

`segment_ciphertext = encrypt_blob(K_s, canonical_cbor(SegmentCleartext))`
`segment_cid = ContentId::for_book(segment_ciphertext, { encrypted: true })`

`K_s` is a random 32-byte key generated once when the segment is first sealed and persisted in the sidecar (§4.4). Because `encrypt_blob` is deterministic in (key, cleartext), a given (K_s, event-set) always yields the same `segment_cid` → the sealer's own segments are stable across its own republishes.

**Manifest** — the new target of `root_cid`; the small, bounded object a receiver fetches first.

```text
ManifestCleartext (canonical CBOR):
  "vn" : u16               // manifest format version (= 1)
  "ci" : SpaceId           // community_id (misroute guard)
  "sg" : Vec<SegmentRef>   // sealed segments, ascending by lo
  "tl" : Vec<SignedMembershipEvent>   // unsealed live tail, event_sort_key order

SegmentRef (canonical CBOR):
  "sc" : ContentId       // segment_cid
  "lo" : EventBoundary   // inclusive range low  (first event's boundary in the segment)
  "hi" : EventBoundary   // inclusive range high (last event's boundary)
  "nn" : u32             // event count
  "ks" : [u8; 32]        // K_s — plaintext INSIDE the epoch-encrypted manifest

EventBoundary (canonical CBOR) — an event_sort_key minus its `sig` tail:
  "wm" : u64      // wall_ms
  "lg" : u32      // logical
  "dv" : String   // device_id
  "id" : [u8; 16] // EventId
```

`manifest_ciphertext = encrypt_blob(current_epoch_key, canonical_cbor(ManifestCleartext))`  ← identical primitive/keying as today's root blob
`root_cid = ContentId::for_book(manifest_ciphertext, { encrypted: true })`

The manifest is bounded by segment count. A `SegmentRef` is ~175 B (`sc` ContentId ~34 B + two `EventBoundary`s ~45 B each incl. `device_id` and the 16-byte `EventId` + `nn` 4 B + `ks` 32 B + CBOR overhead), so 1 MiB ≈ **~5,000–6,000 segments**; at a ~512-event seal threshold that is **~2.5–3M lifetime events** before the manifest itself approaches the cap — a **~400–500× lift** over today's ~6k-event cliff, comfortably past 100k members. (Chunking the manifest — applying the deferred Approach-1 to the small manifest bytes — removes this residual bound entirely and is the documented trivial next lift if ever needed; §7.)

### 4.2 Crypto model — envelope via manifest epoch-encryption

The manifest is encrypted under the **current epoch key** (exactly like today's root). `K_s` therefore lives in plaintext *inside* the epoch-encrypted manifest — **no separate key-wrapping primitive is introduced.** This yields:

- **Epoch-stable segment CIDs.** `K_s` is independent of the epoch, so a segment's bytes/CID never change when the epoch rotates. Rotation re-encrypts only the tiny manifest; segments are untouched. Rotation cost goes from O(total history) to O(manifest).
- **Backward secrecy preserved.** A kicked member can't decrypt manifests published after the rotation → can't recover `K_s` for any segment referenced there. Future events land in new segments whose `K_s` only appears in post-kick manifests they can't read. (Old segments hold events the kicked member already saw — re-protecting them buys nothing, so nothing is lost by not re-keying them.)
- **No epoch key-chain for joiners.** A new joiner decrypts the *current* manifest with the *current* epoch key (which it already obtains today via `live_epoch_key` + the invite epoch snapshot) and thereby gets every segment's `K_s`. No historical epoch keys required.

Reused verbatim: `encrypt_blob` / `decrypt_blob` (`community_state_sync.rs:167-203`) for the manifest; the same primitive keyed by `K_s` for segments.

### 4.3 Seal policy (cascade-free under backdated events)

Boundaries are **absolute `EventBoundary` values** (an `event_sort_key` minus its `sig` tail), pinned to the segment's first and last events at seal time and persisted — never positional counts. Each sealed segment records an **inclusive** `[lo, hi]`.

- The **live tail** holds all events with sort key **>** the highest sealed `hi` (initially, before any seal, the whole log).
- When the tail reaches the seal threshold — `SEGMENT_SEAL_EVENTS` events **or** `SEGMENT_SEAL_BYTES` cleartext bytes, whichever first — cut a leading chunk of that size, seal it as a segment with `lo` = its first event's boundary and `hi` = its last event's boundary (generate `K_s`, encrypt, compute `segment_cid`, append the `SegmentRef` to the sidecar), and repeat on the remaining tail until it is below both thresholds.
- Sealed ranges are **contiguous and disjoint** by construction: each interval's `hi` is strictly below the next interval's `lo`, so an event joins the earliest interval whose `hi` it does not exceed.
- **Backdated event** (sort key ≤ some sealed `hi`): it falls into exactly one sealed interval → re-seal **only that interval's region** (recollect the interval's events, re-encode, new `segment_cid`, update its sidecar entry; the old CID is orphaned and GC-eligible). If the added event pushes that interval past a seal threshold, it is **split into bounded replacement segments** rather than re-sealed as one oversized blob — so no segment can grow past the threshold. Either way **no later segment shifts** (later intervals are matched by their own pinned `hi`) — that is the payoff of absolute-boundary intervals over positional counts.

Threshold constants (pinned): `SEGMENT_SEAL_EVENTS = 512`, `SEGMENT_SEAL_BYTES = 256 * 1024`. **Every** segment — first-sealed or backdated-re-sealed — respects both thresholds, so no single segment can ever approach 1 MiB. (The trade-off: heavy backdating into one already-sealed interval fragments it into extra small segments; those count toward the manifest's own segment cap, whose cliff sits ~400–500× further out. Compacting that fragmentation is a documented non-goal — §7.)

### 4.4 Sidecar (per-publisher stability)

`communities/{id}/segments.cbor` — the publisher's local segment index, written with the same atomic-rename + quarantine-on-corrupt discipline as `crdt.cbor` (`community_state_persist.rs:213-221`, `:110-113`).

```text
SegmentIndex (canonical CBOR):
  "vn" : u16
  "sg" : Vec<SealedEntry>   // sealed segments, ascending by lo

SealedEntry:
  "lo" : EventBoundary   // range low  (inclusive boundary)
  "hi" : EventBoundary   // range high (inclusive boundary)
  "nn" : u32             // event count
  "ks" : [u8; 32]        // K_s
  "sc" : ContentId       // segment_cid (cached; recomputable from the range's events + K_s)
```

On publish/serve the publisher loads the sidecar and reuses each `(K_s, segment_cid)` → its manifest keeps referencing the same segment CIDs → CAS dedups its own re-puts → **per-publisher O(delta) publish**. If the sidecar is lost/corrupt, the publisher regenerates `K_s` on next seal → different CIDs → one O(total) re-upload (a recoverable degradation, not a correctness loss; receivers still decode).

### 4.5 Data flow

**Publish / serve** (both derive identically, replacing the single `encode_root_packet` blob step):
1. Snapshot `CommunityState` under the existing TOCTOU epoch-recheck loop (`:3127-3173`) — unchanged.
2. Sort events by `event_sort_key`; split into sealed ranges (from the sidecar) + the current tail.
3. If the tail crosses the seal threshold, seal a new segment (§4.3) and persist the sidecar.
4. For every sealed segment not already present in CAS, `put_serveable` its ciphertext (idempotent; existing segments are skipped).
5. Build `ManifestCleartext { sealed SegmentRefs, tail events }`, `encrypt_blob` under the current epoch key, `put_serveable` → `root_cid`.
6. Wrap in the **unchanged** `CommunityRootPublishPayload` and sign (§5) — publish/serve exactly as today.

**Receive / bootstrap** (replacing the single root-blob fetch at `:4085` + decode at `:4152`):
1. Fetch `root_cid` (the manifest) from CAS; `decrypt_blob` with the epoch key that opened the wire packet.
2. Decode `ManifestCleartext`; verify `"ci"` matches (misroute guard, mirrors `:4158`).
3. For each `SegmentRef` whose `segment_cid` is not already held, fetch it, `decrypt_blob` with the ref's `K_s`, decode, verify `"ci"`.
4. Concatenate all sealed-segment events + the manifest tail, replay via the existing membership-gated `into_events()` / verify-on-receive pipeline (`:4180-4188`) — **unchanged** replay semantics.
5. Incremental re-sync fetches only segment CIDs not already held → **O(delta) bootstrap** when following a publisher.

---

## 5. Wire envelope & versioning

Reuse `CommunityRootPublishPayload` (`community_state_sync.rs:224-257`) and its signed sub-struct `CommunityRootSignedPayload` (`:270-281`) — `root_cid` now addresses a manifest. Add a format discriminator, byte-compatible with legacy fixtures:

- `CommunityRootPublishPayload` gains `"mf": Option<u8>` (manifest format; `skip_serializing_if Option::is_none`). Legacy monolithic root = `None`; segmented manifest = `Some(1)`.
- `CommunityRootSignedPayload` gains the **same** `"mf": Option<u8>` so the format is under the publisher signature — prevents a downgrade/confusion attack that reinterprets `root_cid`. Absent (`None`) → byte-identical to the pinned legacy fixtures; present → new fixtures for the manifest case.

Receiver dispatch: `"mf" == Some(1)` → parse `root_cid` as a manifest (§4.5); `None` → legacy monolithic decode (existing path, retained for **dual-read**).

---

## 6. Migration

- **Dual-read** both formats for a transition window: receivers understand legacy monolithic roots and new manifest roots (dispatched on `"mf"`).
- **Publish switches to manifest-only** after the update. No data migration: `crdt.cbor` loads unchanged, and the first manifest publish seals the community's current log into segments on the fly.
- **Flag-day assumption (justified at current scale):** a client that predates this change cannot decode a manifest root and would treat it as a corrupt monolithic blob. Given v0.2.x alpha with an effectively-zero external user base and a ≤3-node fleet, coordinated rollout (all clients updated before manifests are published) is acceptable and is the assumption here. A capability-negotiated dual-*publish* rollout (publish both formats during transition) is a documented fallback if a mixed-version fleet ever needs it, but is out of MVP scope.

---

## 7. Non-goals (explicit follow-ups)

Deferred by the approved MVP scope; each is additive on top of this design:

1. **Strict cross-peer segment dedup** — deterministic `K_s` derived from converged content + single-epoch sealing + an explicit seal watermark, so any peer serves any peer's segments and storage dedups. (The "Full" tier.) MVP uses random per-publisher `K_s`; different publishers store separate segment blobs for the same logical range — acceptable, and states still converge because replay is over events, not blobs.
2. **RBSR tail-reconcile** — range-based reconciliation of the live tail across peers (`channel_rbsr.rs` pattern). MVP re-fetches the bounded tail wholesale.
3. **Manifest chunking** — applying the ContentId-bundle chunker to the manifest bytes once segment count approaches the manifest's own ~13k-entry cap (millions of events out). Trivial when needed.
4. **Local-storage segmentation** — `crdt.cbor` stays monolithic; only the CAS/wire path segments.

---

## 8. Health surface

Re-point ZEB-813's watermark/`report_degraded` machinery (`community_state_sync.rs:3185-3241`, `classify_root_size` `:4715`) from monolithic-blob size to the two new bounds:

- **Manifest fill** — `segments.len()` against the manifest's `SegmentRef` capacity (warn at 50/80 %). This is the new "root approaching the cliff" signal, now sitting millions of events out.
- **Per-segment fill** — the live tail's cleartext bytes against `SEGMENT_SEAL_BYTES` / a single segment against `MAX_PAYLOAD_SIZE` (defense-in-depth; a segment should seal long before this).

Keep the `report_degraded` payload/plumbing; only the measured quantity and thresholds change.

---

## 9. Testing strategy

- **Per-publisher CID stability:** publish twice with no new events → identical `root_cid` and identical sealed `segment_cid`s (sidecar reuse).
- **O(delta) publish:** append events crossing one seal boundary → only the newly sealed segment + the manifest are `put`; prior segment CIDs unchanged and not re-put.
- **Backdated-event single-segment re-seal:** insert an event whose HLC falls in an already-sealed range → exactly that one `SegmentRef.sc` changes; all later segments' CIDs are byte-identical (no cascade).
- **Epoch rotation touches manifest only:** rotate the epoch, republish → every `segment_cid` unchanged; `root_cid` changes (manifest re-encrypted); a member with the new epoch decodes, a member without it cannot.
- **Backward secrecy:** a `K_s` obtained from a pre-rotation manifest does not appear in the post-rotation manifest for any *new* segment.
- **Bootstrap parity:** a joiner that fetches manifest + segments materializes the **byte-identical** `CommunityState` a monolithic-root joiner would (golden-parity against the current path on a fixture log).
- **Cross-peer convergence (not dedup):** two publishers with different arrival orders publish different segment blobs; a receiver replaying either converges to the same materialized state.
- **Dual-read:** a receiver decodes both a legacy monolithic root (`"mf"` = None) and a manifest root (`"mf"` = Some(1)).
- **Wire fixtures:** new byte-pins for `SegmentRef`, `ManifestCleartext`, `SegmentCleartext`, and the `"mf"`-present `CommunityRootPublishPayload`/`CommunityRootSignedPayload`; legacy fixtures (mf absent, `crdt.cbor`) proven unchanged.
- **Sidecar corruption recovery:** delete the sidecar → next publish regenerates keys and re-uploads all segments; receivers still decode.

---

## 10. File-change map (for the plan)

| File | Change |
|---|---|
| `src-tauri/src/community_state_sync.rs` | New `SegmentCleartext`, `ManifestCleartext`, `SegmentRef` types + encode/decode; segment seal logic; replace the single-blob step in `encode_root_packet`/`publish_root_now` with manifest+segment derivation; replace the single-fetch decode in the receive path with manifest→segments fetch/replay; add `"mf"` to publish/signed payloads; re-point watermarks. |
| `src-tauri/src/community_state_persist.rs` | New `segments.cbor` sidecar: `SegmentIndex` load/save (atomic-rename + quarantine), alongside `crdt.cbor`. |
| `src-tauri/src/community_state_crdt.rs` | Expose a stable `event_sort_key`-ordered iterator over the log for segment layout (if not already reachable), and `"ci"`/misroute helpers reused for segments. |
| `src-tauri/tests/wire_format/community_sync_fixtures.rs` (+ new) | Byte-pins for the new segment/manifest types and the `"mf"`-present envelope; assert legacy pins unchanged. |
| `src-tauri/src/event_loop.rs` | No topic change (`state-root-v1` reused); verify serve/put_serveable path handles the manifest CID and segment CIDs on the allowlist. |

---

## 11. Success check against the criterion

A 100k-member community with millions of lifetime events: the published **manifest** stays small (bounded by segment count, ~400–500× headroom vs today, and unbounded once the deferred manifest-chunking lands); publish re-uploads only the changed tail segment (per-publisher O(delta)); a joiner fetches only segments it lacks (O(delta) incremental bootstrap); epoch rotation re-encrypts only the manifest. The fixed-size cliff is removed for any realistic community, and the health surface warns at 50/80 % of the (far higher) manifest bound well before failure. ✔
