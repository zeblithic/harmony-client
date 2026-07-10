# ZEB-670: delete_vine — creator tombstone (signed retract + cache eviction) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A creator-signed delete verb for vines: publishing a tombstone removes the vine (record + reactions + viewed state + cached blob) from every online subscriber's cache and feed; other users' reshares survive as "Removed by creator" stubs; the composer's sovereign copy line is restored.

**Architecture:** A new `VineTombstonePayload` (Ed25519-signed, pubkey→address binding mirroring `SignedMembershipEvent`) published on `harmony/vines/{creator}/tombstones/{vine_id}`. The event loop routes it into `VineFeedCache::on_tombstone_sample`, which verifies, evicts, persists the tombstone (pre-arrival-proof), and reports an evict-candidate CID; the loop burns the blob (keep-set/pin-guarded) and emits `vine-removed`. Frontend `VineService` handles the event, marks reshare stubs, and exposes `deleteVine`; `VineCard`/`VineFeed` add the gated delete affordance behind a Tier-2 destructive confirm.

**Tech Stack:** Rust (tauri, serde, ed25519-dalek via harmony-identity), Svelte 5, TypeScript, vitest, cargo-nextest.

## Global Constraints

- Gates per CLAUDE.md: `npx tsc --noEmit`; `npx vitest run`; `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo fmt --all -- --check`. Iterative per-task Rust gates use `scripts/test-select --context task` (paste the emitted `round=… bucket=…` summary line into the task report so the selection is auditable, per CLAUDE.md); final sweep is the full `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Tombstone MUST be signature-verified on receive (pubkey→address binding + `verify_strict`); an unsigned delete verb is a censorship regression (ZEB-673 documents the wider unsigned-vine gap — out of scope here).
- Reshare semantics (pinned in ZEB-670 comment): creator tombstone of an original → other users' reshares become stubs (`originalRemoved`), own-record deletes remove outright. Chain reshares stub via `(origin creator, videoCid)` content match, suppressed when a live original by the same creator with the same CID exists (re-publish case).
- Copy must not overclaim: deletion is best-effort tombstone propagation (no backfill exists — offline nodes converge only if they later receive the tombstone).
- Blob eviction must not destroy pinned content (respect `pin_intent` + `compute_keep_set`) and must skip when another live descriptor still references the CID.
- Tauri IPC params: Rust `snake_case`, JS callers `camelCase`. Frontend error extraction: `e instanceof Error ? e.message : String(e)`.
- `vine_feed.json` stays `FILE_VERSION = 1`; the new `tombstones` field uses `#[serde(default)]` so old files load and old builds ignore it.

---

### Task 1: `vine_tombstone.rs` — signed payload, canonical bytes, sign/verify

**Files:**
- Create: `src-tauri/src/vine_tombstone.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod vine_tombstone;` next to the other module decls)

**Interfaces:**
- Produces: `VineTombstonePayload { vine_id, video_cid, creator_address, deleted_at, creator_identity_pub, sig }` (camelCase serde); `sign_tombstone(private, vine_id, video_cid, creator_address, deleted_at) -> VineTombstonePayload`; `verify_tombstone(&VineTombstonePayload) -> Result<(), String>`; `tombstone_key_expr(creator_address, vine_id) -> String`.

- [ ] **Step 1: Write failing tests** (in-module `#[cfg(test)]`): sign→verify roundtrip; `verify` rejects (a) address not matching pubkey hash, (b) tampered `vine_id`, (c) sig from a different identity. Generate identities the way `community_membership.rs` tests do (grep its test module for the identity fixture helper and reuse the idiom).
- [ ] **Step 2: Run to verify failure** — `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(vine_tombstone)'` (compile error: module missing).
- [ ] **Step 3: Implement**

```rust
//! ZEB-670: creator-signed vine tombstone (delete verb).
//!
//! The vine wire path is otherwise unsigned (ZEB-673); the tombstone is
//! signed anyway because it is the first REMOTELY DESTRUCTIVE vine verb —
//! unsigned, it would let any peer censor any creator's vines. The scheme
//! mirrors `community_membership::verify_signature`: the record carries the
//! signer's 64-byte identity pub; receivers require
//! `Identity::from_public_bytes(pub).address_hash == creator_address`
//! (hex) and then `verify_strict` over the canonical bytes.

use serde::{Deserialize, Serialize};

/// Domain-separation prefix + version for the signed byte string.
const CANONICAL_PREFIX: &str = "harmony-vine-tombstone-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VineTombstonePayload {
    pub vine_id: String,
    pub video_cid: String,
    pub creator_address: String,
    /// Unix seconds at signing time.
    pub deleted_at: u64,
    /// Hex-encoded 64-byte identity pub (X25519(32) || Ed25519(32)),
    /// per `harmony_identity::Identity::to_public_bytes`.
    pub creator_identity_pub: String,
    /// Hex-encoded 64-byte Ed25519 signature over `canonical_bytes`.
    pub sig: String,
}

/// Deterministic byte string the signature covers. `|` cannot appear in
/// any field (vine ids, hex CIDs, hex addresses, decimal timestamps), so
/// the encoding is unambiguous.
pub fn canonical_bytes(
    vine_id: &str,
    video_cid: &str,
    creator_address: &str,
    deleted_at: u64,
) -> Vec<u8> {
    format!("{CANONICAL_PREFIX}|{vine_id}|{video_cid}|{creator_address}|{deleted_at}").into_bytes()
}

pub fn tombstone_key_expr(creator_address: &str, vine_id: &str) -> String {
    format!("harmony/vines/{creator_address}/tombstones/{vine_id}")
}

pub fn sign_tombstone(
    private: &harmony_identity::PrivateIdentity,
    vine_id: String,
    video_cid: String,
    creator_address: String,
    deleted_at: u64,
) -> VineTombstonePayload {
    let bytes = canonical_bytes(&vine_id, &video_cid, &creator_address, deleted_at);
    let sig = private.sign(&bytes); // mirror community_membership.rs:618's call + byte extraction
    VineTombstonePayload {
        vine_id,
        video_cid,
        creator_address,
        deleted_at,
        creator_identity_pub: hex::encode(private.public_identity().to_public_bytes()),
        sig: hex::encode(sig /* adapt: .to_bytes() if Signature */),
    }
}

pub fn verify_tombstone(t: &VineTombstonePayload) -> Result<(), String> {
    let pub_bytes: [u8; 64] = hex::decode(&t.creator_identity_pub)
        .map_err(|e| format!("bad identity pub hex: {e}"))?
        .try_into()
        .map_err(|_| "identity pub must be 64 bytes".to_string())?;
    let identity = harmony_identity::Identity::from_public_bytes(&pub_bytes)
        .map_err(|_| "invalid identity pub".to_string())?;
    if hex::encode(identity.address_hash) != t.creator_address {
        return Err("tombstone pubkey does not match claimed creator address".into());
    }
    let sig_bytes: [u8; 64] = hex::decode(&t.sig)
        .map_err(|e| format!("bad sig hex: {e}"))?
        .try_into()
        .map_err(|_| "sig must be 64 bytes".to_string())?;
    let bytes = canonical_bytes(&t.vine_id, &t.video_cid, &t.creator_address, t.deleted_at);
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    identity
        .verifying_key
        .verify_strict(&bytes, &sig)
        .map_err(|_| "tombstone signature invalid".to_string())
}
```

Adapt the exact `private.sign(...)` return handling and identity-fixture helpers to what `community_membership.rs:585-625` and its tests actually use — do not invent new crypto plumbing.

- [ ] **Step 4: Run tests to pass** — same nextest filter; also `cargo clippy --locked -p harmony-app --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all`.
- [ ] **Step 5: Commit** — `git add src-tauri/src/vine_tombstone.rs src-tauri/src/lib.rs && git commit` (`ZEB-670 Task 1: vine_tombstone module — signed payload + verify`).

---

### Task 2: cache — tombstone state, eviction, guards, `originalRemoved`, persistence

**Files:**
- Modify: `src-tauri/src/vine_feed_cache.rs`
- Modify: `src-tauri/src/lib.rs` (`VineVideoDto` gains `original_removed: bool` — find the struct via `grep -n "struct VineVideoDto" src-tauri/src/lib.rs`)

**Interfaces:**
- Consumes: `vine_tombstone::{VineTombstonePayload, verify_tombstone}` (Task 1).
- Produces: `TombstoneOutcome { Applied { removed: Option<RemovedVine>, evict_cid: Option<String> }, AlreadyApplied, Rejected(String) }`; `RemovedVine { vine_id, video_cid, creator_address }` (camelCase Serialize — this is the `vine-removed` event payload); `on_tombstone_sample(&mut self, key_expr, payload) -> Option<TombstoneOutcome>`; `original_removed: bool` on `VineVideoDto` + `VineVideoDtoWithSource`.

- [ ] **Step 1: Write failing tests** (extend the existing `#[cfg(test)]` module; reuse `canonical_descriptor_bytes`/`followed_set_with` helpers and add a `signed_tombstone_for(identity, vine_id, video_cid, addr)` helper built on Task 1's `sign_tombstone`):
  1. `tombstone_removes_descriptor_reactions_viewed_and_reports_evict` — insert descriptor + 2 reactions + mark viewed; apply valid tombstone → `Applied { removed: Some(..), evict_cid: Some(cid) }`; `list_descriptors` empty, `list_reactions` empty, `is_viewed` false.
  2. `tombstone_keeps_blob_when_another_live_vine_references_cid` — two originals w/ same `video_cid` (different creators); tombstone one → `evict_cid: None`.
  3. `pre_arrival_tombstone_blocks_later_descriptor` — tombstone for unknown id → `Applied { removed: None, evict_cid: None }`; subsequent `on_descriptor_sample` for that id → `Rejected`.
  4. `tombstone_rejects_bad_signature_and_wrong_creator` — tampered sig → `Rejected`; valid-signed tombstone whose creator ≠ cached descriptor's creator → `Rejected` (vine stays).
  5. `tombstone_is_idempotent` — second apply → `AlreadyApplied`, no state change.
  6. `reshare_of_tombstoned_original_lists_original_removed` — original by A + reshare by B (`reshare_of` = original id); tombstone original → reshare row has `original_removed: true`, original gone; also a **chain** reshare (reshare-of-reshare carrying `original_creator_address = A`, same cid, `reshare_of` = middle id) flagged via content match.
  7. `republished_original_unsets_content_stub_for_new_reshares` — after tombstone, same creator publishes NEW original (new id, same cid); a reshare of the NEW id → `original_removed: false`; an old reshare pointing at the tombstoned id → still `true`.
  8. `tombstones_persist_and_block_after_reload` — apply tombstone, `save_for_test`/reload via `load()` → descriptor re-arrival still `Rejected`; disk file without the field (`version:1` legacy JSON) still loads (serde default).
- [ ] **Step 2: Run to verify failures** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(vine_feed_cache)'`.
- [ ] **Step 3: Implement** in `vine_feed_cache.rs`:

```rust
/// ZEB-670: applied tombstone record. Signature/pub are NOT retained —
/// verification happens once at ingest; persisted tombstones are trusted
/// local state (same posture as descriptors).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TombstoneOnDisk {
    vine_id: String,
    video_cid: String,
    creator_address: String,
    deleted_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedVine {
    pub vine_id: String,
    pub video_cid: String,
    pub creator_address: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TombstoneOutcome {
    Applied { removed: Option<RemovedVine>, evict_cid: Option<String> },
    AlreadyApplied,
    Rejected(String),
}
```

- Cache field: `tombstones: HashMap<String, TombstoneRecord>` (in-memory twin of `TombstoneOnDisk` minus `vine_id`; or key by `vine_id` and store `{video_cid, creator_address, deleted_at}`).
- `on_tombstone_sample(&mut self, key_expr: &str, payload: &[u8]) -> Option<TombstoneOutcome>`:
  key guard `starts_with("harmony/vines/") && contains("/tombstones/")` → parse `VineTombstonePayload` (parse fail → `Rejected`) → `verify_tombstone` (fail → `Rejected`) → topic `{addr}` segment must equal `t.creator_address` (`Rejected`) → if already in `tombstones` → `AlreadyApplied` → if a cached descriptor exists, require its `creator_address == t.creator_address` (`Rejected("tombstone creator does not own vine")`) → apply: insert tombstone; `removed = self.descriptors.remove(&t.vine_id).map(|cv| RemovedVine {..})`; `self.reactions.retain(|(vid, _), _| vid != &t.vine_id)`; `self.viewed.remove(&t.vine_id)`; `evict_cid = removed.as_ref().and_then(|r| (!self.descriptors.values().any(|cv| cv.descriptor.video_cid == r.video_cid)).then(|| r.video_cid.clone()))`; `self.save()`.
- Guard in `on_descriptor_sample` (two lines): early `if key_expr.contains("/tombstones/") { return None; }` next to the `/reactions/` guard; after parse, `if self.tombstones.contains_key(&descriptor.id) { return Some(DescriptorOutcome::Rejected(...)) }`.
- Stub computation, one private helper used by BOTH `list_descriptors` and `build_dto`:

```rust
/// A reshare renders as a "Removed by creator" stub when its direct
/// source was tombstoned, or (chain case) when the origin creator
/// retracted this content and has no live original for it (re-publish
/// of the same CID un-stubs FUTURE reshares; old ones stay stubbed via
/// the direct id match).
fn original_removed(&self, d: &VineDescriptorPayload) -> bool {
    let Some(src_id) = d.reshare_of.as_deref() else { return false };
    if self.tombstones.contains_key(src_id) {
        return true;
    }
    let origin = d.original_creator_address.as_deref().unwrap_or(&d.creator_address);
    let content_retracted = self.tombstones.values().any(|t| {
        t.creator_address == origin && t.video_cid == d.video_cid
    });
    content_retracted
        && !self.descriptors.values().any(|cv| {
            cv.descriptor.reshare_of.is_none()
                && cv.descriptor.creator_address == origin
                && cv.descriptor.video_cid == d.video_cid
        })
}
```

- `VineFeedDiskV1` gains `#[serde(default)] tombstones: Vec<TombstoneOnDisk>`; `save()` writes them; `populate_from_disk` loads them, age-pruning by `deleted_at < age_cutoff` (same 90-day window as descriptors).
- Add `original_removed: bool` to `VineVideoDto` (lib.rs) and `VineVideoDtoWithSource`, populated via the helper.
- [ ] **Step 4: Run tests to pass** — same filter, then `scripts/test-select --context task` (paste `round=… bucket=…` into the report), `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo fmt --all`.
- [ ] **Step 5: Commit** (`ZEB-670 Task 2: cache tombstone state, guards, originalRemoved, persistence`).

---

### Task 3: event loop — subscription, routing, blob burn, `vine-removed` emit

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (subs ~line 2876-2900; `emit_frontend_event` vine branch ~7553-7611; the pump call site ~3776; Burn-arm helpers `collect_descendants`/`compute_keep_set` ~6545)

**Interfaces:**
- Consumes: `on_tombstone_sample`, `TombstoneOutcome`, `RemovedVine` (Task 2).
- Produces: Tauri event `vine-removed` with `RemovedVine` payload (camelCase: `vineId`, `videoCid`, `creatorAddress`); blob eviction on the loop side.

- [ ] **Step 1: Failing test** — `emit_frontend_event` already has unit tests or a testable seam (check the `#[cfg(test)]` around line 7508 first and mirror its sink/fixture idiom). Test: a valid signed tombstone sample routed through `emit_frontend_event` (a) emits `vine-removed` exactly once with the right payload, (b) returns the evict candidate; a `Rejected` tombstone emits nothing. If `emit_frontend_event`'s current `()` return makes this awkward, change it to return `Option<String>` (evict CID) — all existing call sites ignore it with `let _ =`.
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement:**
  - New subscription after the reactions one (~2900): `RuntimeAction::Subscribe { key_expr: "harmony/vines/*/tombstones/*".to_string() }` via the same `dispatch_action` shape.
  - In `emit_frontend_event`'s vine branch, add a `/tombstones/` arm BEFORE the descriptor `else`: lock cache → `on_tombstone_sample` → on `Applied { removed, evict_cid }`: if `removed.is_some()`, `emit_ser(app, "vine-removed", &removed.unwrap())`; return `evict_cid` to the caller. (`AlreadyApplied`/`Rejected` → `tracing::debug!`, no emit.)
  - At the pump call site (~3776, inside `run`'s select scope where `runtime` and `pin_intent` live): if `Some(cid_hex)` comes back, decode to `ContentId` and burn EXACTLY like the `ContentVerbRequest::Burn` arm but WITHOUT touching `pin_intent` and skipping entirely when `pin_intent.contains(&cid)` (a deliberate local pin outlives a remote tombstone): `collect_descendants` → `compute_keep_set` → for non-kept: `runtime.unpin_content(&id); let _ = runtime.remove_content(&id);`. If the pump call site turns out NOT to have `runtime` in scope, fall back to sending `ContentVerbRequest::Burn` down a self-clone of the content-verb sender — but verify scope first; the direct path is preferred.
- [ ] **Step 4: Run tests to pass** + clippy + fmt (`scripts/test-select --context task`, paste summary line).
- [ ] **Step 5: Commit** (`ZEB-670 Task 3: tombstone subscription, routing, pin-guarded blob burn, vine-removed`).

---

### Task 4: `delete_vine` IPC — impl seam, GUI command, headless RPC, identity stash

**Files:**
- Modify: `src-tauri/src/lib.rs` (NodeState field; `delete_vine_impl` + `#[tauri::command]` after the `publish_vine_reaction` block ~13143; `generate_handler!` list ~53338-53343)
- Modify: `src-tauri/src/api/rpc.rs` (`rpc!` block after `mark_vine_viewed` ~690-700; curated inventory vec ~1587-1592 `// vines` block; surface-count doc comment ~393)

**Interfaces:**
- Consumes: `vine_tombstone::{sign_tombstone, tombstone_key_expr}`; NodeState's `publish_tx`, `vine_feed_cache`, `node_addr`.
- Produces: `delete_vine(vine_id)` → `Result<DeleteVineResult { published: bool }, String>`; NodeState field `owner_private_identity: Option<std::sync::Arc<harmony_identity::PrivateIdentity>>`.

- [ ] **Step 1: Failing tests** — (a) rpc.rs: `delete_vine_errs_not_connected_pre_node` mirroring `list_vine_reactions_errs_not_connected_pre_node` (`RpcError::Command(msg)` containing `"not connected"`); (b) inventory test gains `"delete_vine"`; (c) lib.rs unit test: `delete_vine_impl` rejects a vine whose cached `creator_address != node_addr` with "not your vine" and rejects unknown ids with "vine not found" (drive a `NodeState` fixture the way neighboring `*_impl` tests do — grep `mark_vine_viewed_impl` tests for the pattern).
- [ ] **Step 2: Run to verify failures.**
- [ ] **Step 3: Implement:**
  - NodeState: add `owner_private_identity: Option<std::sync::Arc<harmony_identity::PrivateIdentity>>` (doc comment: ZEB-670, signs vine tombstones; an Arc clone of `private_identity_arc`). Initialize `None` in the `Default`-ish init (~1620), set it in `start_node` where other `guard.*` handles install (search `guard.vine_feed_cache =` for the install block; clone `private_identity_arc`), clear it in the same place `node_addr`/handles clear on stop (~2069).
  - `delete_vine_impl(state: &Mutex<NodeState>, vine_id: String) -> Result<DeleteVineResult, String>` (async, like `publish_vine_impl`): under one lock take `publish_tx.clone()` (else `"not connected"`), `node_addr.clone()`, `owner_private_identity.clone()` (else `"not connected"`), `vine_feed_cache.clone()` (else `"not connected"`); drop lock; look up the vine in the cache — `"vine not found"` if absent; require `descriptor.creator_address == node_addr` — `"not your vine: only the creator can delete a vine"`; `deleted_at` = now-secs; `sign_tombstone(...)`; `serde_json::to_vec` → `PublishRequest { key_expr: tombstone_key_expr(&node_addr, &vine_id), payload, reply }` → await reply (mirror `publish_vine_descriptor` lib.rs:12829-12857). Local cache application arrives via the loopback echo of our own publish (same posture as `publish_vine` → `vine-received`).
  - `#[tauri::command] async fn delete_vine(state, vine_id: String)` → impl; add `delete_vine,` to `generate_handler!`.
  - rpc.rs: `VineIdArgs` already exists (~102-106) — `rpc!(m, "delete_vine", crate::VineIdArgs, |state, _sink, a| async move { crate::delete_vine_impl(state, a.vine_id).await });` + inventory entry + bump the count comment.
- [ ] **Step 4: Run tests to pass** + clippy (`--all-targets`) + fmt + `scripts/test-select --context task` (paste summary line).
- [ ] **Step 5: Commit** (`ZEB-670 Task 4: delete_vine IPC — signed publish seam, GUI + headless registration`).

---

### Task 5: frontend service — `vine-removed`, stubs, `deleteVine`

**Files:**
- Modify: `src/lib/vine-service.ts`
- Test: `src/lib/vine-service.test.ts`

**Interfaces:**
- Consumes: Tauri event `vine-removed` `{ vineId, videoCid, creatorAddress }`; IPC `delete_vine({ vineId })`; DTO field `originalRemoved`.
- Produces: `VineVideo.originalRemoved?: boolean`; `VineService.deleteVine(vine: VineVideo): Promise<void>`; third listener (destroy unlisten count 2 → 3 — update the pinned assertions at `vine-service.test.ts:335` and `:345`).

- [ ] **Step 1: Failing tests** (idioms: `createMockAdapter`, `emit`, command-dispatch `mockImplementation`):
  1. `vine-removed deletes the vine, its reactions, and notifies once` — seed via `vine-received` + `vine-reaction-received`; emit `vine-removed` → gone from both feeds, `getReaction` empty, single `onChange`.
  2. `vine-removed marks direct and chain reshares as originalRemoved` — original by A, reshare by B (`reshareOf` = original id), chain reshare by C (`reshareOf` = B's id, `originalCreatorAddress` = A, same `videoCid`); emit removal of the original → both B's and C's rows have `originalRemoved === true`.
  3. `deleteVine invokes delete_vine with the vine id` — `expect(adapter.invoke).toHaveBeenCalledWith('delete_vine', { vineId: 'v1' })`.
  4. `deleteVine falls back to local removal when not connected` — invoke rejects `'not connected'` → vine removed locally (mock-mode posture mirroring `publish`'s fallback; real errors re-throw).
  5. `hydrate carries originalRemoved through` — `mockHydrateInvoke` row with `originalRemoved: true` → surfaced on the feed row.
  6. Update destroy-count assertions 2 → 3.
- [ ] **Step 2: Run to verify failures** — `npx vitest run src/lib/vine-service.test.ts`.
- [ ] **Step 3: Implement:**
  - Types: `VineDescriptorEvent` + `VineVideo` gain `originalRemoved?: boolean`; new `VineRemovedEvent { vineId: string; videoCid: string; creatorAddress: string }`.
  - `connectAdapter`: third listener `vine-removed` → `applyRemoval(payload)`. `applyRemoval`: drop the vine from whichever feed holds it + `seenIds.delete` + `reactionMap.delete`; then for every remaining vine `v` with `v.reshareOf` set: `if (v.reshareOf === vineId || ((v.originalCreatorAddress ?? v.creatorAddress) === creatorAddress && v.videoCid === videoCid)) v.originalRemoved = true;` single `onChange` at the end (only if anything changed).
  - `deleteVine(vine)`: `await adapter.invoke('delete_vine', { vineId: vine.id })`; catch → `msg` extraction idiom; on `'not connected'`/`'event loop'` fall back to `applyRemoval({ vineId: vine.id, videoCid: vine.videoCid, creatorAddress: vine.creatorAddress })`, else re-throw. (Connected removal lands via the echoed tombstone → `vine-removed`.)
  - `wireToVine` passes `originalRemoved` through.
- [ ] **Step 4: Run tests to pass** + `npx tsc --noEmit`.
- [ ] **Step 5: Commit** (`ZEB-670 Task 5: vine-service — vine-removed handling, reshare stubs, deleteVine`).

---

### Task 6: UI — delete affordance + confirm, stub rendering, sovereign copy, App wiring

**Files:**
- Modify: `src/lib/components/VineCard.svelte` (props/handlers ~30-100; rail markup ~182-211)
- Modify: `src/lib/components/VineFeed.svelte` (state ~73; confirm fns ~294-315; card props ~382-402; dialogs ~409-415)
- Modify: `src/lib/components/VinePublishDialog.svelte` (sovereign note :232-235)
- Modify: `src/App.svelte` (VineFeed wiring ~3755-3774)
- Test: co-located component test files (follow the existing `VineFeed`/`VineCard` test files' render idioms)

**Interfaces:**
- Consumes: `vineService.deleteVine`, `vine.originalRemoved` (Task 5).
- Produces: `VineCard` props `canDelete?: boolean`, `deleting?: boolean`, `onDelete?: (vine: VineVideo) => void`; `VineFeed` prop `onDelete?: (vine: VineVideo) => Promise<void>`.

- [ ] **Step 1: Failing tests:**
  1. VineCard: delete rail-btn renders only when `canDelete && onDelete`; click calls `onDelete(vine)` and stops propagation; `deleting` disables it.
  2. VineCard stub: `vine.originalRemoved` → no `<video>` rendered even with a `videoUrl`; stub notice text `Removed by creator` visible; like/reshare buttons absent.
  3. VineFeed: `canDelete` true only for own vines (`creatorAddress === 'self'` or `=== ownAddress`); clicking delete opens `ConfirmDialog` (assert on its `Delete vine?` title + honest copy); confirm calls `onDelete`; rejection surfaces in the existing error strip (mirror the reshare-error tests).
  4. VinePublishDialog: sovereign note text updated (assert full sentence).
- [ ] **Step 2: Run to verify failures.**
- [ ] **Step 3: Implement:**
  - **VineCard**: add props `canDelete = false`, `deleting = false`, `onDelete`. Handler mirrors `handleReshareClick`. After the reshare block in `.action-rail`:

```svelte
      {#if canDelete && onDelete}
        <button
          type="button"
          class="rail-btn rail-btn-danger"
          onclick={handleDeleteClick}
          disabled={deleting}
          aria-label="Delete vine"
        >
          <span aria-hidden="true">🗑</span> {deleting ? 'Deleting…' : 'Delete'}
        </button>
      {/if}
```

  - Stub rendering: wrap the media area — when `vine.originalRemoved`, render `<div class="removed-stub" role="note"><span aria-hidden="true">🚫</span> Removed by creator</div>` instead of the `<video>`; suppress like/reshare rail buttons for stubs (keep the attribution row so "reshared by" context remains). `.rail-btn-danger`/`.removed-stub` styles follow the existing token idiom in the file (use existing `--gov-clay*`/danger tokens if present in the app's palette — grep before inventing).
  - **VineFeed**: `deleteTarget`/`deletingId`/`deleteError` state trio mirroring reshare's; `requestDelete`/`confirmDelete` mirroring `requestReshare`/`confirmReshare` (same error-extraction idiom, reuse the `reshare-error` strip or a sibling); pass to VineCard: `canDelete={!!onDelete && (vine.creatorAddress === 'self' || (ownAddress != null && vine.creatorAddress === ownAddress))}`, `deleting={deletingId === vine.id}`, `onDelete={requestDelete}`. Dialog (Tier-2 destructive, file-burn precedent — `ConfirmDialog.svelte`):

```svelte
{#if deleteTarget}
  <ConfirmDialog
    title="Delete vine?"
    message={`This deletes "${deleteTarget.title ?? 'this vine'}" from your feed and asks peers to drop their copies. Peers that are offline may keep it until they reconnect.`}
    confirmLabel="Delete"
    destructive={true}
    onConfirm={confirmDelete}
    onCancel={() => { deleteTarget = null; }}
  />
{/if}
```

  - **VinePublishDialog** :234 — restore the drawn line: `Publishes to your sovereign identity and replicates peer-to-peer. There's no central server to take it down — only you can delete it.`
  - **App.svelte**: pass `onDelete={(vine) => vineService.deleteVine(vine)}` to `<VineFeed …>`.
- [ ] **Step 4: Run tests to pass** — `npx vitest run` + `npx tsc --noEmit`.
- [ ] **Step 5: Commit** (`ZEB-670 Task 6: delete affordance + confirm, removed-by-creator stubs, sovereign copy`).

---

### Task 7: full gates + PR

- [ ] `npx tsc --noEmit` && `npx vitest run` (full).
- [ ] `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` && `cargo fmt --all -- --check`.
- [ ] Full sweep: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` (background, ~20 min, with wall-clock supervision).
- [ ] Open PR (body: premise correction, reshare-semantics decision, eviction posture, honesty framing, ZEB-673 link); fire `@coderabbitai review` once; converge.
