# ZEB-673: Vine wire signing — descriptors + reactions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the ZEB-670 tombstone signing scheme to `VineDescriptorPayload` and `VineReactionPayload` so vine creator/reactor attribution is cryptographically real: records carry `identityPub` + `sig`, receivers verify pubkey→address binding + `verify_strict`, and unsigned/invalid wire arrivals are rejected.

**Architecture:** New `vine_signing.rs` module with an injective length-prefixed canonical encoding (free-text-safe, unlike the tombstone's pipe scheme). Signing happens at the two publish choke points (`publish_vine_descriptor` shared core; extracted `publish_vine_reaction_impl`), verification at the two cache admission choke points (`on_descriptor_sample`, `on_reaction_sample`). Migration: strict on wire, tolerant on disk (Option fields, legacy records age out via existing 90-day/5000-cap bounds).

**Tech Stack:** Rust (ed25519-dalek `verify_strict`, `harmony_identity`), serde JSON wire format. No frontend changes (extra JSON fields are invisible to TS).

## Global Constraints

- Signature fields are `Option<String>` + `#[serde(default, skip_serializing_if = "Option::is_none")]`. (Survey correction: `VineFeedDiskV1` persists separate `DescriptorOnDisk`/`ReactionOnDisk` structs, NOT the wire payloads verbatim — and `TombstoneOnDisk` already pins the posture "signature/pub are NOT retained; verification happens once at ingest; persisted records are trusted local state". So the disk format is untouched; loads reconstruct in-memory payloads with `None` sig fields, which is why the fields must be `Option`.)
- Wire arrivals missing or failing signature/topic-binding → `Rejected` (strict). Disk records without signatures load unchanged (tolerant).
- Domain prefixes: `harmony-vine-descriptor-v1`, `harmony-vine-reaction-v1`.
- Canonical encoding: length-prefixed (`u32-LE len ‖ bytes` per field, fixed field order), `Option` = 1 presence byte then value, `bool` = 1 byte, `u64` = 8-byte LE. NOT pipe-separated (free-text fields), NOT `canonical_cbor_encode` (its same-length-field-name contract is violated by these structs).
- Verification mirrors `vine_tombstone::verify_tombstone`: hex-decode 64-byte pub → `Identity::from_public_bytes` → `hex(address_hash)` must equal `creator_address` (descriptor) / `reactor_address` (reaction) → `verify_strict` over canonical bytes.
- Task order is load-bearing: publish-side signing (Task 2) MUST land before receive-side strictness (Task 3), or publish→echo roundtrip tests break between commits.
- Gates per task: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `scripts/test-select --context task` (paste `round=… bucket=…`). Final: full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` + `npx tsc --noEmit` + `npx vitest run`.
- Commit trailers: `Co-Authored-By` + `Claude-Session` per session convention.

---

### Task 1: `vine_signing.rs` + payload signature fields

**Files:**
- Create: `src-tauri/src/vine_signing.rs`
- Modify: `src-tauri/src/lib.rs` (module decl next to `mod vine_tombstone;`; `VineDescriptorPayload` + `VineReactionPayload` gain fields)

**Interfaces (Produces):**
```rust
pub fn descriptor_canonical_bytes(d: &VineDescriptorPayload) -> Vec<u8>;
pub fn reaction_canonical_bytes(r: &VineReactionPayload) -> Vec<u8>;
pub fn sign_descriptor(private: &harmony_identity::PrivateIdentity, d: &mut VineDescriptorPayload); // sets identity_pub + sig
pub fn sign_reaction(private: &harmony_identity::PrivateIdentity, r: &mut VineReactionPayload);
pub fn verify_descriptor(d: &VineDescriptorPayload) -> Result<(), String>; // Err("descriptor is unsigned…") when fields absent
pub fn verify_reaction(r: &VineReactionPayload) -> Result<(), String>;
```

- [ ] **Step 1: Add signature fields to both payload structs** (`lib.rs:12745`, `12818`):
```rust
/// ZEB-673: hex 64-byte identity pub (X25519‖Ed25519) of the signer.
/// Option for disk back-compat — wire receivers reject None.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub identity_pub: Option<String>,
/// ZEB-673: hex 64-byte Ed25519 signature over the canonical bytes.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub sig: Option<String>,
```
  Fix every struct-literal construction site (`publish_vine`, `publish_vine_reaction`, tests) with `identity_pub: None, sig: None` to compile.
- [ ] **Step 2: Write failing tests in `vine_signing.rs`** — roundtrip sign→verify (both types); tamper each semantic field → "signature invalid"; forged signer (victim's pubkey, attacker's key) → "signature invalid"; pubkey/address mismatch → "does not match"; unsigned → "unsigned"; canonical-bytes injectivity: `title: None` ≠ `title: Some("")`, and free text containing `|`, newlines, emoji roundtrips; field-shift attack (`creator_name: "ab", title: Some("c")` vs `creator_name: "a", title: Some("bc")`) produces different bytes; serde pin: None fields absent from JSON, Some fields camelCase (`identityPub`, `sig`), legacy JSON without the keys parses.
- [ ] **Step 3: Implement** — encoding helpers (`push_str`, `push_opt_str`, `push_u64`, `push_bool`), canonical byte builders over semantic fields ONLY (never `identity_pub`/`sig`), sign/verify mirroring `vine_tombstone.rs`. Descriptor field order: `id, creator_address, creator_name, created_at, video_cid, title, reshare_of, original_creator_address, original_creator_name`. Reaction field order: `vine_id, reactor_address, reactor_name, liked, timestamp`.
- [ ] **Step 4: Gates + commit** (`git add` new module; `feat(zeb-673): vine_signing module + optional sig fields on wire payloads`).

### Task 2: Publish side signs (descriptor core + reaction `_impl`)

**Files:**
- Modify: `src-tauri/src/lib.rs` (`publish_vine_descriptor` ~12844, `publish_vine_reaction` ~13106)

**Interfaces (Produces):** `pub(crate) async fn publish_vine_reaction_impl(state: &Mutex<NodeState>, reaction: PublishReactionPayload) -> Result<(), String>` — the `#[tauri::command]` delegates to it (pattern: `delete_vine_impl`).

- [ ] **Step 1: Failing tests** (mirror `delete_vine_tests` NodeState fixture with `owner_private_identity: Some(...)` + capture channel): published descriptor payload deserializes and `verify_descriptor` passes; same for reaction via `publish_vine_reaction_impl`; identity absent (`owner_private_identity: None`, publish_tx present) → `Err` containing "cannot sign".
- [ ] **Step 2: Implement** — `publish_vine_descriptor` pulls `owner_private_identity` Arc in the same lock as `publish_tx`; `None` → `Err("identity unavailable: cannot sign vine publish")`; `vine_signing::sign_descriptor(&identity, &mut descriptor)` before serialize. Extract `publish_vine_reaction_impl` from the command body verbatim, add identical identity pull + `sign_reaction`. `reshare_vine`/headless `publish_vine` RPCs route through the shared core — signed for free.
- [ ] **Step 3: Gates + commit** (`feat(zeb-673): sign vine descriptors + reactions at publish`).

### Task 3: Receive side strict + fixture sweep

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs` (`on_descriptor_sample:428`, `on_reaction_sample:598`, `ReactionOutcome`), `src-tauri/src/event_loop.rs` (reaction match ~7619; routing tests), `src-tauri/src/lib.rs` (delete_vine test seeds ~13284/13340)

- [ ] **Step 1: Failing tests** — unsigned wire descriptor → `Rejected("…unsigned…")`; tampered sig → Rejected; topic creator segment ≠ payload `creator_address` → Rejected; signed+bound → Inserted. Reactions: unsigned → Rejected; topic `{vine_id}`/`{reactor}` segments ≠ payload → Rejected; signed+bound → Inserted/UpdatedNewer. Disk legacy: `VineFeedDiskV1` fixture whose descriptors lack `identityPub`/`sig` loads and lists (extend the existing legacy-fixture test ~2910).
- [ ] **Step 2: Implement `on_descriptor_sample`** — immediately after parse (before dedup/tombstone/age; fail fast, no state effects): `vine_signing::verify_descriptor(&descriptor)?` → `Rejected(reason)`; then topic binding: segment after `harmony/vines/` must equal `descriptor.creator_address` (reuse the tombstone's segment-parse idiom).
- [ ] **Step 3: Implement `on_reaction_sample`** — after parse: `verify_reaction` → `Rejected(reason)` (lift `ReactionOutcome::Rejected` to `Rejected(String)` — its doc comment reserves this); topic binding: `/reactions/{vine_id}/{reactor}` segments must equal payload `vine_id`/`reactor_address`. Update every exhaustive `ReactionOutcome` match (event_loop `matches!` on Inserted|UpdatedNewer needs no change; test asserts do).
- [ ] **Step 4: Fixture sweep** — cache unit tests, event_loop routing tests, lib.rs delete_vine seeds: generate a real `PrivateIdentity` per test (or shared helper `fn test_signer() -> (PrivateIdentity, String)` returning identity + derived hex address), build payloads with the DERIVED address (literal fake addresses like `"aabb"` can no longer verify), sign via `sign_descriptor`/`sign_reaction`, publish on the matching topic. `followed_set` entries switch to derived addresses.
- [ ] **Step 5: Gates + commit** (`feat(zeb-673): strict signature + topic-binding verification at vine cache admission`).

### Task 4: Integration tests + final sweep

**Files:**
- Modify: `src-tauri/tests/content/vine_feed_cache_integration.rs`, `vine_feed_persistence_integration.rs`, `vine_content_roundtrip_integration.rs`

- [ ] **Step 1:** Update the three integration files' payload builders to sign (public API: `harmony_app::vine_signing::sign_descriptor` etc. + `harmony_identity::PrivateIdentity::generate`); derived addresses replace literals; persistence test keeps one deliberately-unsigned DISK fixture proving tolerant load.
- [ ] **Step 2:** Full final gates: `cargo fmt --all -- --check`; clippy `--all-targets`; full `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; `npx tsc --noEmit`; `npx vitest run`.
- [ ] **Step 3:** Commit (`test(zeb-673): signed fixtures for vine integration tests`), push, open PR, fire CodeRabbit once.

## Non-goals / notes

- No frontend changes: `vine-reaction-received` re-emits the raw wire payload, so signed records carry two extra JSON fields — TS interfaces ignore unknown fields.
- No re-signing of legacy cached records (impossible for others' vines; unnecessary for own — no anti-entropy exists that would re-serve them).
- No `verified` UI flag (decision: strict wire cut instead — see Linear design comment).
- Tombstone path (`vine_tombstone.rs`) unchanged — already signed.
