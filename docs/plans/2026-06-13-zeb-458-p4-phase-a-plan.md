# ZEB-458 P4 Phase A — Community Sealed Relay (mechanism) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Plan-doc checkboxes are the execution-tracking format, not a Linear substitute.

**Goal:** Build the community sealed-relay *mechanism* — a co-community volunteer that accepts a DM deposit it cannot read, holds it opaque keyed by recipient, and serves it back when the recipient pulls — proven end-to-end by a direct-connect integration test. Production discovery + sender rung + pull driver are Phase B.

**Architecture:** P4 is a P1 deposit whose transport is "hold-and-pull via a community volunteer." The sender seals the **same `DepositPayload`** to **R's butler-set device key(s)** (P1 crypto: `seal_to_owner_with_info(birational(device_vk), …)`), a co-membership-gated relay holds it **opaque** in a new `RelayHoldDoc`, and R's butler device **pulls + opens** it (`open_from_owner_with_info(ed25519_priv_to_x25519(device_sk), …)`) and ingests through the **normal receive path**. The relay verifies only co-membership (S, R both `Joined` in C) — it cannot open the blob.

**Tech Stack:** Rust, `async_trait`, tokio `Mutex`, `FleetSyncEngine` CRDT datasets, canonical CBOR (`harmony_owner::cbor`), the `iroh_butler_acceptor` ctx-injection test pattern, `cargo nextest`.

**Spec:** `docs/specs/2026-06-13-zeb-458-community-sealed-relay-design.md` (D35–D45). **Branch:** `zeb-458-community-sealed-relay` (off main `5ceb7f7b`, spec committed `0f7ab626`).

**Reuse anchors (read these — most tasks mirror them):**
- Butler deposit constants/frame/seal: `src/butler_deposit.rs` (`DepositFrame` L107, `DepositPayload` L144, `BUTLER_DEPOSIT_SEAL_INFO` L45, `build_deposit_frame` L318–362, caps L71–99).
- Deposit acceptor: `src/iroh_butler_acceptor.rs` (`handle_deposit_core` L424+, `ButlerDepositCtx` trait L151, `ProdButlerDepositCtx` L239–281, `persist_entry` + `DepositPersistVerdict` L139, iroh shell `IrohButlerDepositAcceptor` L659+).
- Inbox CRDT: `src/dm_inbox_crdt.rs` (`DmInboxEntry` L14, `DmInboxDoc` L33, `key` L47, `merge_from` L57).
- Ingest + receive-path verify: `src/dm_inbox_ingest.rs` (`ingest_pending`, `DmInboxIngestCtx::verify` L362–419 → `dm_outbox::verify_cidnotify_admission`, `decrypt_and_bind_dm_blob`, `apply_inbox`).
- Seal/open + birational: `src/dm_signing.rs` (`seal_to_owner_with_info` L79, `open_from_owner_with_info` L128, `ed25519_pub_to_x25519` L167, `ed25519_priv_to_x25519` L189).
- Community membership read: `src/community_membership.rs` (`MaterializedMembership.members` L1310, `MemberState` L1396, `MemberStatus::Joined` L1412); co-membership scan pattern: `src/iroh_butler_acceptor.rs:shares_live_group_dm_in` L405.
- start_node install: `src/lib.rs` L6255–6289 (ProdButlerDepositCtx build + `install_butler_deposit_acceptor`); ALPN routing `src/zenoh_iroh_transport.rs` L173–213, `src/iroh_endpoint.rs` L393.
- Opt-in pattern: `set_butler_pin` `src/lib.rs` L42189–42290; Tauri handler list L42503–42737.
- Wire fixtures: `tests/wire_format_*_fixtures.rs` (deterministic helpers behind `test-fixtures` feature).

**House rules (every task):** no worktrees; `set -o pipefail`; commit BEFORE running gates; 10-min wall-clock kill switch on any cargo command (Bash `timeout` param — macOS has no `timeout` binary); `--locked` always. Per-task gates unless a task says otherwise:

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

Reserve `--all-targets` for the final sweep (Task 7) — it relinks ~97 integration binaries (~25min compile / ~27min clippy). Per-task `--lib` scoping is deliberate relink-cost management; the final sweep + CI's `--all-targets` catch integration-target breakage.

**Crypto invariant:** the relay NEVER opens the blob. Seal target is R's butler-set device key; only an R device opens it. The relay's only checks are co-membership + cert + frame-sig over the *opaque* blob.

---

### Task 1: Relay wire module — constants, frames, build/seal/sign helpers, co-membership predicate

**Files:**
- Create: `src-tauri/src/community_relay.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod community_relay;`)
- Test: inline `#[cfg(test)]` in `community_relay.rs`

Mirror `butler_deposit.rs`. Define:

```rust
// ALPNs (distinct from butler)
pub const COMMUNITY_RELAY_DEPOSIT_ALPN: &[u8] = b"harmony/community-relay-deposit/v1";
pub const COMMUNITY_RELAY_PULL_ALPN: &[u8] = b"harmony/community-relay-pull/v1";
// HKDF info + sig domains (distinct strings — no cross-protocol confusion)
pub const COMMUNITY_RELAY_SEAL_INFO: &[u8] = b"harmony-zeb-458-community-relay-v1";
pub const COMMUNITY_RELAY_DEPOSIT_SIG_DOMAIN: &[u8] = b"harmony-zeb-458-community-relay-deposit-v1";
pub const COMMUNITY_RELAY_PULL_SIG_DOMAIN: &[u8] = b"harmony-zeb-458-community-relay-pull-v1";
// Caps + TTL (reuse butler magnitudes)
pub const RELAY_HOLD_PER_SENDER_CAP: usize = 64;
pub const RELAY_HOLD_GLOBAL_CAP: usize = 1024;
pub const RELAY_HOLD_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

// Deposit frame: P1 DepositFrame + community_id + recipient_device. The
// sealed_blob is a DepositPayload sealed to birational(recipient_device_vk).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RelayDepositFrame {
    pub recipient_owner: [u8; 16],
    pub recipient_device: [u8; 16],     // which R device the blob is sealed to
    pub sender_owner: [u8; 16],
    pub community_id: SpaceId,
    #[serde(with = "serde_bytes")] pub sender_enrollment_cert: Vec<u8>,
    #[serde(with = "serde_bytes")] pub sig: Vec<u8>,
    #[serde(with = "serde_bytes")] pub sealed_blob: Vec<u8>,
}
pub struct RelayDepositAck { pub content_id: [u8; 32] } // ContentId(sealed_blob)

// Pull protocol
pub struct RelayPullQuery {
    pub recipient_owner: [u8; 16],
    pub community_id: SpaceId,
    #[serde(with="serde_bytes")] pub requester_enrollment_cert: Vec<u8>,
    #[serde(with="serde_bytes")] pub sig: Vec<u8>, // over PULL_SIG_DOMAIN ‖ recipient_owner ‖ community_id
}
pub struct RelayHeldBlob { pub sender_owner: [u8; 16], #[serde(with="serde_bytes")] pub sealed_blob: Vec<u8> }
pub struct RelayPullResponse { pub entries: Vec<RelayHeldBlob> }
pub struct RelayPullAck { pub content_ids: Vec<[u8; 32]> }
```

- Encode/decode via `harmony_owner::cbor::{to_canonical, from_strict}` (strict decode rejects unknown/trailing — mirror `decode_enrollment_cert_strict`).
- `pub fn relay_deposit_sig_payload(recipient_owner, community_id, sealed_blob) -> Vec<u8>` (domain ‖ ro ‖ cid_bytes ‖ sealed_blob); `pub fn relay_pull_sig_payload(recipient_owner, community_id) -> Vec<u8>`.
- `pub fn build_relay_deposit_frame(recipient_owner, recipient_device_ed25519_verify, sender_owner, community_id, sender_cert_bytes, sender_device_key: &SigningKey, payload: &DepositPayload) -> Result<RelayDepositFrame, _>` — mirror `build_deposit_frame` L318: `seal_pub = ed25519_pub_to_x25519(recipient_device_ed25519_verify)`; `sealed_blob = seal_to_owner_with_info(seal_pub, encode_deposit_payload(payload), COMMUNITY_RELAY_SEAL_INFO)`; `sig = sender_device_key.sign(relay_deposit_sig_payload(...))`. **Reuse `DepositPayload` + `encode_deposit_payload` from `butler_deposit.rs` (do NOT redefine).**
- `pub fn both_joined_members(membership: &MaterializedMembership, a: &OwnerAddr, b: &OwnerAddr) -> bool` — both present with `status == MemberStatus::Joined` (mirror `shares_live_group_dm_in` containment style).

- [ ] **Step 1: Write failing tests** — round-trip encode/decode for `RelayDepositFrame` + `RelayPullQuery`/`Response`/`Ack`; strict-decode rejects trailing bytes; `build_relay_deposit_frame` produces a blob that `open_from_owner_with_info(ed25519_priv_to_x25519(recipient_device_sk), blob, COMMUNITY_RELAY_SEAL_INFO)` opens to the original `DepositPayload`, and that opening with a DIFFERENT key fails; `relay_deposit_sig_payload` verifies under the sender device vk; `both_joined_members` true only when both `Joined` (false for Left/Banned/Invited/absent).
- [ ] **Step 2: Run — expect compile-fail / FAIL.**
- [ ] **Step 3: Implement `community_relay.rs` + `mod community_relay;`.**
- [ ] **Step 4: Run tests — PASS.**
- [ ] **Step 5: Commit** `feat(zeb-458): relay wire module (frames, seal/sign, co-membership predicate)`.

---

### Task 2: `RelayHoldDoc` CRDT — opaque holding store with caps + TTL + coverage GC

**Files:**
- Create: `src-tauri/src/community_relay_hold_crdt.rs`
- Modify: `src-tauri/src/lib.rs` (`mod community_relay_hold_crdt;`)
- Test: inline `#[cfg(test)]`

Mirror `dm_inbox_crdt.rs` exactly (same `merge_from`/`MergeOutcome` shape so it satisfies the `FleetSyncEngine` merger bound):

```rust
pub struct RelayHoldEntry {
    pub recipient_owner: [u8; 16],
    pub recipient_device: [u8; 16],
    pub sender_owner: [u8; 16],
    pub community_id: SpaceId,
    #[serde(with = "serde_bytes")] pub sealed_blob: Vec<u8>, // opaque
    pub held_at: Hlc,
    pub held_by: String,                 // relay device id (64-hex)
    pub pulled_by: BTreeSet<String>,     // grow-only: R device ids that pulled+acked
}
pub struct RelayHoldDoc { pub entries: BTreeMap<String, RelayHoldEntry> }
impl RelayHoldDoc {
    // key = "{recipient_owner_hex}:{recipient_device_hex}:{content_id_hex}"
    pub fn key(recipient_owner: &[u8;16], recipient_device: &[u8;16], content_id: &[u8;32]) -> String { ... }
    pub fn merge_from(&mut self, remote: RelayHoldDoc) -> MergeOutcome { ... } // new entries insert; existing → pulled_by union (grow-only), metadata first-writer-wins by held_at; Changed only on new/ig-growth — mirror dm_inbox_crdt L57
}
```

- Caps + GC are **pure helpers** here (the persist critical section lives in the Prod ctx, Task 3, mirroring `persist_entry`): `pub fn count_for_sender(&self, community_id, sender_owner) -> usize`, `pub fn live_count(&self) -> usize`. GC helper `pub fn gc(&mut self, now_ms: u64) -> bool` removing entries that are **covered** (`recipient_device ∈ pulled_by`) OR TTL-expired (`held_at.wall_ms + RELAY_HOLD_TTL_MS < now_ms`), with one-sweep deferral (mirror dm_inbox_ingest GC L247) — defer removal of an entry that became covered DURING this sweep so `pulled_by` replicates first.

- [ ] **Step 1: Failing tests** — key determinism; merge unions `pulled_by` + keeps first-writer metadata + Changed-flag semantics; `count_for_sender`/`live_count`; `gc` removes covered + TTL-expired, defers freshly-covered one sweep, keeps uncovered/unexpired.
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(zeb-458): RelayHoldDoc opaque holding CRDT (caps + TTL + coverage GC)`.

---

### Task 3: Relay deposit acceptor — admission + opaque persist-with-caps

**Files:**
- Create: `src-tauri/src/iroh_community_relay_acceptor.rs`
- Modify: `src-tauri/src/lib.rs` (`mod iroh_community_relay_acceptor;`)
- Test: inline `#[cfg(test)]` (ctx-injection mock, mirror `iroh_butler_acceptor.rs` test module)

Mirror `handle_deposit_core`. Define `RelayDepositReject` (WrongCommunity, NotCoMember, BadCert, BadSig, CapExceeded, PersistFailed) and:

```rust
#[async_trait] pub trait RelayDepositCtx: Send + Sync {
    fn relay_device_id(&self) -> String;
    /// opt-in + membership: is this relay a Joined member of `community_id` AND opted-in to relay for it?
    async fn serves_community(&self, community_id: &SpaceId) -> bool;
    /// both sender & recipient Joined members of community_id (local check vs replicated C-membership)
    async fn both_co_members(&self, community_id: &SpaceId, sender_owner: &[u8;16], recipient_owner: &[u8;16]) -> bool;
    fn now_secs(&self) -> u64;
    async fn mint_hlc(&self) -> Hlc;
    /// atomic persist-with-caps over RelayHoldDoc (mirror persist_entry); key built by caller
    async fn persist_hold(&self, key: String, entry: RelayHoldEntry) -> Result<RelayPersistVerdict, String>;
}
pub async fn handle_relay_deposit_core(frame: &RelayDepositFrame, ctx: &dyn RelayDepositCtx) -> Result<RelayDepositAck, RelayDepositReject>;
```

Admission order (spec D36):
0. `serves_community(frame.community_id)` else `WrongCommunity` (cheapest local).
1. `both_co_members(community_id, sender_owner, recipient_owner)` else `NotCoMember` (uniform; no oracle).
2. Decode+verify `sender_enrollment_cert` (strict), `cert.owner_id == sender_owner`, Master-issued, owner-id-derived anchor `owner_id_from_master_ed25519(master) == sender_owner` (reuse `friend_graph::owner_id_from_master_ed25519`); else `BadCert`.
3. Frame sig: `cert.device_pubkeys…ed25519_verify` verifies `relay_deposit_sig_payload(recipient_owner, community_id, sealed_blob)` over `frame.sig`; else `BadSig`.
4. **NO decrypt** — compute `content_id = ContentId::for_book(sealed_blob, ContentFlags{encrypted:true,..})`; build `RelayHoldEntry { recipient_owner, recipient_device, sender_owner, community_id, sealed_blob, held_at: mint_hlc(), held_by: relay_device_id(), pulled_by: empty }`; `key = RelayHoldDoc::key(recipient_owner, recipient_device, &content_id)`; `persist_hold(key, entry)` → map `CapExceeded`→reject, `Inserted|Duplicate`→ack `RelayDepositAck{content_id}`.

`ProdRelayDepositCtx` (build in Task 6) + a `#[cfg(test)] TestRelayDepositCtx` mock (call-order event probe like `TestCtx`).

- [ ] **Step 1: Failing tests** — co-member deposit accepted+held (assert opaque blob stored, ack carries content_id); non-served community → WrongCommunity (no persist); sender or recipient not Joined → NotCoMember **before any persist**; cert owner-id/master-anchor mismatch → BadCert; frame-sig mismatch → BadSig; per-sender cap exceeded → CapExceeded (nothing stored); duplicate redelivery → idempotent ack, caps bypassed. **Assert the ctx NEVER decrypts (there is no decrypt hook — the blob is stored verbatim).**
- [ ] **Step 2: Run — FAIL.**
- [ ] **Step 3: Implement `handle_relay_deposit_core` + trait + mock.**
- [ ] **Step 4: Run — PASS.**
- [ ] **Step 5: Commit** `feat(zeb-458): relay deposit acceptor (co-membership admission, opaque persist)`.

---

### Task 4: Relay pull acceptor — requester auth + serve + ack→pulled_by→GC

**Files:**
- Modify: `src-tauri/src/iroh_community_relay_acceptor.rs` (add pull core + ctx + mock)
- Test: inline `#[cfg(test)]`

```rust
#[async_trait] pub trait RelayPullCtx: Send + Sync {
    async fn serves_community(&self, community_id: &SpaceId) -> bool;
    async fn is_joined_member(&self, community_id: &SpaceId, owner: &[u8;16]) -> bool;
    fn now_secs(&self) -> u64;
    /// all held blobs for recipient_owner (any device) — returns (key, RelayHeldBlob)
    async fn held_for(&self, recipient_owner: &[u8;16]) -> Vec<(String, RelayHeldBlob)>;
    /// record that requester_device pulled+acked these keys; runs GC
    async fn mark_pulled(&self, keys: &[String], requester_device: String) -> Result<(), String>;
    /// resolve the requester device id from the validated cert (for pulled_by)
    fn requester_device_id_from_cert(&self, cert: &EnrollmentCert) -> Option<String>;
}
pub async fn handle_relay_pull_query(query: &RelayPullQuery, ctx: &dyn RelayPullCtx) -> Result<RelayPullResponse, RelayPullReject>;
pub async fn handle_relay_pull_ack(recipient_owner: &[u8;16], ack: &RelayPullAck, requester_device: String, ctx: &dyn RelayPullCtx) -> Result<(), RelayPullReject>;
```

Query flow (spec D39 step 1–2): `serves_community` else reject; decode+verify requester cert (strict, owner-id == `query.recipient_owner`, Master-issued, owner-id-derived anchor); `is_joined_member(community_id, recipient_owner)` else reject; verify `query.sig` over `relay_pull_sig_payload(recipient_owner, community_id)` against the cert device key; then return `held_for(recipient_owner)` as `RelayPullResponse`. Ack flow: validate the same auth, then `mark_pulled(content_id→keys, requester_device)` (translate acked content_ids to stored keys for this recipient), which unions `pulled_by` + runs `gc`.

`ProdRelayPullCtx` (Task 6) + `#[cfg(test)] TestRelayPullCtx`.

- [ ] **Step 1: Failing tests** — authed R pull returns exactly R's held blobs (opaque); wrong-owner cert → reject (returns nothing); non-served community → reject; bad sig → reject; ack marks `pulled_by` + GCs the covered entry; ack for a content_id not held is a no-op (no error).
- [ ] **Step 2: Run — FAIL.** [ ] **Step 3: Implement.** [ ] **Step 4: PASS.**
- [ ] **Step 5: Commit** `feat(zeb-458): relay pull acceptor (requester auth, serve, ack→GC)`.

---

### Task 5: Recipient open-and-ingest core — open R's copies, ingest via the normal receive path

**Files:**
- Create: `src-tauri/src/community_relay_pull.rs`
- Modify: `src-tauri/src/lib.rs` (`mod community_relay_pull;`)
- Test: inline `#[cfg(test)]`

A pure core that, given pulled `RelayHeldBlob`s, opens those sealed to one of this owner's enrolled devices and ingests each via the SAME receive path P1 deposits use — NOT a forked verify (spec D39 step 3). The background polling driver is Phase B; this is the open+ingest unit the integration test (Task 7) and the Phase-B driver both call.

```rust
#[async_trait] pub trait RelayIngestCtx: Send + Sync {
    /// X25519 privs for each of this owner's enrolled devices (to try-open each blob)
    fn device_x25519_privs(&self) -> Vec<Zeroizing<[u8;32]>>;
    /// ingest a recovered DepositPayload via the normal receive path; Ok(content_id) on success.
    /// Reuses dm_inbox_ingest verify+apply: verify_cidnotify_admission, decrypt_and_bind_dm_blob,
    /// apply_inbox, emit. Returns Err(reason) on any verify failure (R drops it).
    async fn ingest_recovered(&self, payload: DepositPayload) -> Result<(), String>;
}
/// Open each blob (try each device priv with COMMUNITY_RELAY_SEAL_INFO); on success decode
/// DepositPayload and ingest. Returns content_ids successfully ingested (to ack to the relay).
pub async fn open_and_ingest(blobs: &[RelayHeldBlob], ctx: &dyn RelayIngestCtx) -> Vec<[u8;32]>;
```

`open_and_ingest`: for each blob, `content_id = ContentId::for_book(blob.sealed_blob, encrypted:true)`; try `open_from_owner_with_info(priv, blob.sealed_blob, COMMUNITY_RELAY_SEAL_INFO)` over each device priv; first success → `decode_deposit_payload` → `ingest_recovered(payload)`; on Ok push content_id. Blobs that no device opens, or that fail ingest, are skipped (dropped). **Note:** `ingest_recovered`'s production impl reuses the existing `DmInboxIngestCtx::verify`/`apply_inbox`/emit primitives (do NOT fork the receive path) — Task 6 wires it; the test mock asserts only opened-and-decoded payloads reach it.

- [ ] **Step 1: Failing tests** — `open_and_ingest` opens a blob sealed to one of the owner's devices and calls `ingest_recovered` with the exact `DepositPayload`, returning its content_id; a blob sealed to a NON-owner device is skipped (never reaches `ingest_recovered`); an `ingest_recovered` Err drops that blob (content_id not returned); multiple blobs handled independently.
- [ ] **Step 2: Run — FAIL.** [ ] **Step 3: Implement.** [ ] **Step 4: PASS.**
- [ ] **Step 5: Commit** `feat(zeb-458): recipient open-and-ingest core (reuses normal receive path)`.

---

### Task 6: Prod ctx impls, iroh shells, start_node install, opt-in toggle

**Files:**
- Modify: `src-tauri/src/iroh_community_relay_acceptor.rs` (`ProdRelayDepositCtx`, `ProdRelayPullCtx`, iroh shells `IrohCommunityRelayDepositAcceptor` / `IrohCommunityRelayPullAcceptor` mirroring `IrohButlerDepositAcceptor` L659+)
- Modify: `src-tauri/src/community_relay_pull.rs` (`ProdRelayIngestCtx`)
- Modify: `src-tauri/src/zenoh_iroh_transport.rs` (+ `iroh_endpoint.rs`): register the two new ALPNs + `install_community_relay_*_acceptor` (mirror `install_butler_deposit_acceptor` L173–213, each behind its own `OnceLock`)
- Modify: `src-tauri/src/lib.rs`: build the Prod ctxs + install the acceptors (mirror L6255–6289); add the relay `RelayHoldDoc` `FleetSyncEngine` (`lookup_key_tag: b"relay-hold-v1"`, merger `|l,r| l.merge_from(r)`, mirror dm-inbox engine wiring) + handles on `NodeState`; per-community opt-in flag storage + `set_community_relay_optin` IPC command (mirror `set_butler_pin` L42189–42290) + register in the handler list (L42503–42737)
- Test: a focused `#[cfg(test)]` for the opt-in core (`set_community_relay_optin_inner`) — toggling persists + flips `serves_community`

Opt-in storage: a per-community `BTreeSet<SpaceId>` of opted-in communities, persisted in the relay node's own state (simplest: a small dedicated CBOR settings doc on disk + an in-memory `Arc<Mutex<BTreeSet<SpaceId>>>` on `NodeState`, loaded at start_node, mutated by the IPC). `serves_community(C)` = `opted_in.contains(C) && self is a Joined member of C` (read C-membership from `crdt_state`/community state). Default empty (opt-out). `persist_hold`/`held_for`/`mark_pulled` wrap the `RelayHoldDoc` + its engine with the atomic-cap critical section (mirror `ProdButlerDepositCtx::persist_entry` caps logic). `ProdRelayIngestCtx::ingest_recovered` delegates to the existing dm-inbox ingest primitives.

- [ ] **Step 1: Failing test** — `set_community_relay_optin_inner(community_id, true)` then `serves_community` true for that C (member) + persists across reload; `false` reverts. (Keychain-hermetic; inject via `*_inner` seams per CLAUDE.md.)
- [ ] **Step 2: Run — FAIL.** [ ] **Step 3: Implement Prod ctxs + shells + ALPN install + start_node wiring + opt-in IPC.**
- [ ] **Step 4: Gates** (lib clippy may surface integration wiring — keep lib-scoped here).
- [ ] **Step 5: Commit** `feat(zeb-458): prod relay ctxs + iroh shells + start_node install + opt-in toggle`.

---

### Task 7: Direct-connect integration test + final `--all-targets` sweep

**Files:**
- Create: `src-tauri/tests/community_relay_integration.rs`
- Create: `src-tauri/tests/wire_format_community_relay_fixtures.rs` (byte-pin `RelayDepositFrame` + `RelayPullQuery`/`Response` canonical CBOR)

E2E with three engines (relay + recipient fleet; a test sender builds the frame). Mirror the `butler_deposit_integration.rs` harness. Scenarios:

1. **Happy path (deposit → hold opaque → pull → open → ingest → ack → GC):** test sender `build_relay_deposit_frame` sealing the `DepositPayload` to R's device vk → `handle_relay_deposit_core` admits + holds (assert the held `sealed_blob` is byte-identical to the frame's and the relay never decrypted) → R issues `RelayPullQuery` (authed) → `handle_relay_pull_query` returns the opaque blob → `open_and_ingest` opens it (R's device priv) + ingests via the real receive path (assert the `dm-received` inbox entry matches the original message) → `RelayPullAck` → `mark_pulled` GCs the entry (assert the hold doc is empty).
2. **Non-member sender rejected, nothing held** (sender not `Joined` in C).
3. **Wrong-owner pull rejected** (a cert for a different owner gets nothing).
4. **Restart durability:** a held blob survives recreating the relay engine from its persisted doc and is still pullable.
5. **Relay opacity:** assert the relay holds only `sealed_blob` and never reconstructs the `DepositPayload` (no decrypt path exists on the relay ctxs).

Wire fixtures: pin `RelayDepositFrame` + `RelayPullQuery` + `RelayPullResponse` canonical bytes (deterministic helpers behind `test-fixtures`).

- [ ] **Step 1: Write the integration + fixture tests.**
- [ ] **Step 2: Run** `cargo nextest run --locked -p harmony-app --features test-fixtures --test community_relay_integration --test wire_format_community_relay_fixtures` — PASS.
- [ ] **Step 3: Final sweep** `set -o pipefail && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --all-targets --features test-fixtures` (the load-bearing full gate; budget ~50 min; known-unrelated api_server contention flake `serve_core_drives_full_flow_over_http_and_ws` = ZEB-374, re-run if it's the only failure).
- [ ] **Step 4: Commit** `test(zeb-458): community-relay E2E + wire fixtures (Phase A)`.

---

## Self-review checklist (run before execution)
- **Spec coverage:** D35 seal target (Task 1 build_relay_deposit_frame to R device vk ✓); D36 admission (Task 3 ✓); D38 store/caps/TTL/GC (Task 2 + Task 6 persist ✓); D39 pull + open+ingest via receive path (Task 4 + Task 5 ✓); D41 wire + fixtures (Task 1 + Task 7 ✓); D42 DM-scope (DepositPayload reuse ✓); D43 opt-in (Task 6 ✓). **Deferred to Phase B:** D37 discovery/advertisement, D40 sender rung, the background pull driver — explicitly out of Phase A.
- **Type consistency:** `DepositPayload` + `encode/decode_deposit_payload` reused from `butler_deposit.rs` (not redefined); `RelayHoldDoc::merge_from` returns the same `MergeOutcome` the FleetSync merger bound needs; ContentId flags `{encrypted:true}` match between build (Task 1), persist key (Task 3), and open (Task 5).
- **No placeholders:** every task names exact files + the mirror anchor + the new signatures + the test assertions.
