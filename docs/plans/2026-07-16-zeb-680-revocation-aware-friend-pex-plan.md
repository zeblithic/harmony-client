# ZEB-680 Revocation-Aware Friend/PEX Verification — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Friend/PEX verifiers reject revoked devices (consume `RevokedDeviceProjection`), and
the friend-link handshake carries the sender's own fleet revocations so new friends learn of
past revocations at link time.

**Architecture:** Two layers per `docs/specs/2026-07-16-zeb-680-revocation-aware-friend-pex-design.md`
(read it first — it holds the threat model and all rationale). §1 enforcement: one new
parameter on `verify_enrolled_device` threaded to every friend/PEX call site. §2 carry: an
additive `revocations` field on the two friend-link frames, verified pre-consent (pure),
applied post-establishment via the existing `handle_revocation_push` machinery.

**Tech stack:** Rust (src-tauri, single crate `harmony-app`), serde/ciborium CBOR wire,
Svelte 5 + vitest frontend.

## Global Constraints

- zeb375/zeb376 wire fixtures stay **byte-identical** — `IntroduceRequest`, `Introduction`,
  `PexFrame`, catalog frames gain NO fields. Only `FriendLinkRequest`/`FriendLinkAccepted`
  change. **zeb370_fixtures byte-pins BOTH friend-link frames** (T1 review finding): the pins
  hold because empty `revocations` skips the `"v"` key — fixture struct literals must stay
  `Vec::new()`; back-compat via `#[serde(default)]`.
- `enrollment_verify::verify_enrollment_any_issuer` stays pure and untouched; no async added
  to any verifier.
- Every local `OwnerState` mutation pairs with the owner-state dirty notification
  (ZEB-248/#473 invariant) — copy the `dm_inbox_ingest.rs:605-640` pattern: apply under
  lock, drop lock, notify only on genuine insert.
- Over-cap decode is a **hard error** (frame rejected), never truncation — the
  `MAX_DEVICES_PER_OWNER` visitor convention (`iroh_friend_acceptor.rs:160-185`).
- Error strings/variants: additive only; existing variants unchanged.
- Gates per task: `scripts/test-select --context task`; final: `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`,
  `cargo nextest run --locked --workspace --all-targets --features test-fixtures`,
  `npx tsc --noEmit` + `npx vitest run` from repo root.
- Commit after each task; never stage `docs/plans/2026-07-14-zeb-690-dial-seeding-hardening-plan.md`.

**Key shared references** (verified 2026-07-16, main @ eaa717cf):
- Wrapper: `verify_enrolled_device` `src-tauri/src/iroh_friend_acceptor.rs:783`.
- Projection: `src-tauri/src/revoked_device_projection.rs` — `is_revoked(&OwnerAddr, &[u8;32]) -> bool`
  (`:46`), `RevokedDeviceProjection::new()`, `union_from_members` (`:32`); NodeState accessor
  `lib.rs:1747`.
- Trust-bind verifier to reuse: `dm_outbox.rs:2388` `pub(crate) fn verify_revocation_push(
  expected_owner, &RevocationCert, &EnrollmentCert) -> Result<[u8;32], DmReceiveError>`.
- Apply machinery to reuse: `dm_outbox.rs:2442` `handle_revocation_push(&mut OwnerState,
  expected_owner, &RevocationCert, &EnrollmentCert, &RevokedDeviceProjection) -> Result<bool, _>`
  (returns `inserted`).
- Cert test factory precedent: `dm_outbox.rs` tests `sample_revocation_case()` (RevCase:
  owner, revocation, enrollment, revoked_ed); `dm_inbox_ingest.rs` tests build certs via
  `EnrollmentCert::sign_master` / `RevocationCert::sign_master` (see `:2495-2530`).
- Friend-link production sites: request build `lib.rs:51793` (token driver
  `connectivity_link_friend_iroh_inner`, imports at `:51626`) and `lib.rs:56013` (token-less
  driver, imports at `:55966`); acceptor core `process_friend_request` (auth at
  `iroh_friend_acceptor.rs:1046`, friendship apply `:1114`, Accepted build `:1218`); inbound
  auth `authenticate_friend_request` `:980`; serve dispatch `:1655`; dialer-side accept
  verification `lib.rs:51638` region / `:51920` / `:56138`; dialer friendship apply `:51550`.
- Intro/PEX sites: `friend_intro.rs:148` `authenticate_introduce_request` (verify at `:157`),
  `:288` `verify_introduction` (voucher verify `:313`, subject verify `:336`); callers
  `iroh_pex_acceptor.rs:575` and `:720`; catalog `referral_catalog.rs:273` + `:329`.

---

### Task 1: Wire — `RevocationAttestation` + capped `revocations` field on both friend-link frames

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (structs at `:268` and `:379`, serde helper
  mods near `:50-190`)
- Test: same file, `#[cfg(test)]` mod (starts `:2003`)

**Interfaces:**
- Produces: `pub struct RevocationAttestation { pub revocation: RevocationCert (rename "r"),
  pub enrollment: Box<EnrollmentCert> (rename "e") }`; `pub const MAX_CARRIED_REVOCATIONS:
  usize = 32`; field `pub revocations: Vec<RevocationAttestation>` (rename `"v"`, `default`,
  `skip_serializing_if = "Vec::is_empty"`, capped deserialize) on BOTH `FriendLinkRequest`
  and `FriendLinkAccepted`.

- [ ] **Step 1: Write failing tests** (in the existing test mod; follow neighbouring tests'
  cert-mint helpers, e.g. `signed_request_no_token` `:3378`):
  - `revocations_field_round_trips`: build a request via an existing helper, set
    `revocations` to one valid attestation (mint via `EnrollmentCert::sign_master` +
    `RevocationCert::sign_master` as in `dm_inbox_ingest.rs:2495-2530`), encode
    (`encode_friend_request` or the existing encode path used by neighbouring tests), decode,
    assert equality including the pair.
  - `revocations_absent_decodes_empty`: encode a request with `revocations: vec![]`, assert
    the encoded CBOR map has NO `"v"` key (skip_serializing_if), decode, assert empty — this
    is the pre-ZEB-680 back-compat proof.
  - `revocations_over_cap_is_decode_error`: 33 attestations → decode returns
    `Err(FriendHandshakeError::Decode(_))` (the capped visitor errors inside ciborium).
  - Same trio for `FriendLinkAccepted`.
- [ ] **Step 2: Run to verify failure** — `cargo nextest run -p harmony-app
  revocations_field -E 'test(/revocations_/)'` (expect: compile error, field missing).
- [ ] **Step 3: Implement** — struct + const + a `vec_revocation_capped` serde mod cloned
  from the `MAX_DEVICES_PER_OWNER` visitor shape (`:160-185`: size_hint pre-check + per-item
  cap check, both `Error::custom`), field on both structs with doc comment from the spec §2
  ("Not signature-bound — ZEB-677 signer_certs precedent…").
- [ ] **Step 4: Run tests to green**, plus the existing friend-link round-trip tests
  (unchanged frames must still pass).
- [ ] **Step 5: Commit** `ZEB-680 T1: RevocationAttestation carry field on friend-link frames`.

### Task 2: Enforcement — `DeviceRevoked` + projection param threaded through every friend/PEX verifier

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (error enum `:659`, wrapper `:783`,
  `authenticate_friend_request` `:980`, `process_friend_request` `:1046`, serve dispatch
  `:1655`, acceptor struct fields near `:1240`), `src-tauri/src/friend_intro.rs` (`:148`,
  `:157`, `:288`, `:313`, `:336`), `src-tauri/src/referral_catalog.rs` (`:273`, `:329`),
  `src-tauri/src/iroh_pex_acceptor.rs` (`:575`, `:720`, struct + construction site),
  `src-tauri/src/lib.rs` (drivers `:51638`/`:51920`/`:56138`, acceptor spawn sites).
- Test: `iroh_friend_acceptor.rs` test mod.

**Interfaces:**
- Produces: `FriendHandshakeError::DeviceRevoked` and
  `FriendHandshakeError::RevocationAttestationInvalid(String)` (used by Task 5 — add both
  now so the enum changes once); `verify_enrolled_device(cert, signer_certs, claimed_owner,
  revoked: &crate::revoked_device_projection::RevokedDeviceProjection, now_secs)`.
- Consumes: `RevokedDeviceProjection::is_revoked`.

- [ ] **Step 1: Write failing wrapper tests**:

```rust
#[test]
fn verify_enrolled_device_rejects_revoked_device() {
    // mint owner + cert via the existing helpers used by signed_request_real_owner
    let (owner, cert, device_ed) = /* existing helper or inline sign_master mint */;
    let revoked = crate::revoked_device_projection::RevokedDeviceProjection::new();
    revoked.union_from_members(/* seed owner -> {device_ed} — see revoked_device_projection tests for the seeding shape */);
    let err = verify_enrolled_device(&cert, &[], owner, &revoked, 1_700_000_100).unwrap_err();
    assert!(matches!(err, FriendHandshakeError::DeviceRevoked));
}

#[test]
fn verify_enrolled_device_passes_with_empty_projection() { /* same mint, new(), assert Ok */ }
```

- [ ] **Step 2: Run to verify failure** (compile error: unknown param/variant).
- [ ] **Step 3: Implement** — variants + consult in the wrapper (spec §1 code block:
  consult AFTER chokepoint success, against `v.device_ed25519` and `claimed_owner`), then
  mechanically thread a `&RevokedDeviceProjection` through EVERY caller:
  - `authenticate_friend_request` + `process_friend_request` + `serve()` gain the param /
    read a new acceptor-struct field `revoked: RevokedDeviceProjection` (clone of the
    NodeState handle, seeded at every acceptor construction site — find them via
    `grep -n "IrohFriendAcceptor\|FriendAcceptor::new" src-tauri/src/lib.rs`); tests pass
    `RevokedDeviceProjection::new()`.
  - `friend_intro.rs`: `authenticate_introduce_request` and `verify_introduction` gain a
    `revoked: &RevokedDeviceProjection` param, passed to their inner
    `verify_enrolled_device` calls. `verify_introduction` consults for BOTH certs: voucher
    (`:313`, against the voucher owner) and subject (`:336`, against the subject owner the
    fn already binds).
  - `referral_catalog.rs`: same param on `:273`/`:329` fns.
  - `iroh_pex_acceptor.rs`: struct gains the field (like ZEB-694's limiter field), passed at
    `:575`/`:720`; seeded at its construction site in lib.rs.
  - lib.rs drivers `:51638`/`:51920`/`:56138`: pass the NodeState projection accessor
    (`lib.rs:1747`).
  - Byte-pinned zeb375/376 fixture tests call intro constructors/verifiers — update those
    call sites with `RevokedDeviceProjection::new()`; the FIXTURE BYTES must not change.
- [ ] **Step 4: Green** — new tests + `scripts/test-select --context task` (friend/PEX/intro
  suites + wire_format_tests).
- [ ] **Step 5: Commit** `ZEB-680 T2: friend/PEX verifiers consult RevokedDeviceProjection`.

### Task 3: Per-site enforcement regression tests

**Files:**
- Test only: `iroh_friend_acceptor.rs`, `friend_intro.rs`, `referral_catalog.rs` test mods.

**Interfaces:** consumes Task 2's signatures; no production change expected (fix threading
gaps if a test exposes one).

- [ ] **Step 1: Write the five failing-by-revocation tests** (each: mint the actor, seed the
  projection with its device key, assert the typed rejection; then assert the SAME call with
  an empty projection succeeds — paired positive/negative in one test fn is fine):
  - `authenticate_friend_request_rejects_revoked_requester`
  - `authenticate_introduce_request_rejects_revoked_requester`
  - `verify_introduction_rejects_revoked_voucher`
  - `verify_introduction_rejects_revoked_subject`
  - `authenticate_catalog_request_rejects_revoked_requester` (+ author-verify sibling if the
    `:329` site has a separable seam)
  Reuse each module's existing signed-fixture helpers (`signed_request_real_owner` `:3515`,
  the zeb376 intro builders, catalog test mints).
- [ ] **Step 2: Run — each must fail only if Task 2 missed a site** (expected: all pass
  immediately; that is the acceptance — this task pins the behavior against regression).
- [ ] **Step 3: Commit** `ZEB-680 T3: per-site revoked-device rejection pins`.

### Task 4: Send side — attestation builder + attach at all three frame-construction sites

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (builder fn + `process_friend_request`
  param + Accepted literal `:1218`), `src-tauri/src/lib.rs` (`:51793`, `:56013`, acceptor
  spawn/dispatch wiring).
- Test: `iroh_friend_acceptor.rs` test mod.

**Interfaces:**
- Produces: `pub fn build_revocation_attestations(trust: &harmony_owner::state::OwnerState)
  -> Vec<RevocationAttestation>`; `process_friend_request` gains
  `self_revocations: Vec<RevocationAttestation>` placed verbatim into the Accepted literal.
- Consumes: trust snapshot exactly like `push_revocation_to_friends`
  (`owner_commands.rs:~1214`): iterate `trust.revocations`, keep Master-issued only
  (match the `RevocationIssuer::Master` arm), pair with
  `trust.enrollments.get(&rc.target)` (skip + `tracing::warn!` on miss, same message shape
  as the push path), cap at `MAX_CARRIED_REVOCATIONS` keeping smallest-N by
  `(rc.target, …)` byte order for determinism.

- [ ] **Step 1: Failing builder tests**: `builder_pairs_and_caps` (3 revocations, 1 missing
  enrollment → 2 pairs; 33 with enrollments → 32, smallest-N kept),
  `builder_skips_non_master_issued` (a SelfDevice-issued cert → excluded). Mint certs as in
  Task 1.
- [ ] **Step 2: Run to verify failure** (fn not found).
- [ ] **Step 3: Implement builder**; thread `self_revocations` through
  `process_friend_request` into the Accepted literal (tests pass `vec![]`); attach at the
  two lib.rs request builders using a fresh trust snapshot obtained the same way the
  `revoke_device` path reaches its `trust_snapshot` (grep `trust_snapshot` /
  `owner_commands.rs:3808` caller); the serve dispatch computes the acceptor's list fresh
  per handshake (ZEB-621 `home_relay_url` fresh-read precedent — a provider closure/field on
  the acceptor, NOT a frozen boot snapshot).
- [ ] **Step 4: Green** + task-scoped test-select.
- [ ] **Step 5: Commit** `ZEB-680 T4: carry own-fleet revocation attestations on friend-link frames`.

### Task 5: Receive phase 1 — pure verification of carried attestations (fail-closed)

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (new fn + `authenticate_friend_request`),
  `src-tauri/src/lib.rs` (dialer-side accept verification `:51638` region + `:56138`).
- Test: `iroh_friend_acceptor.rs` test mod.

**Interfaces:**
- Produces: `pub fn verify_carried_revocations(peer_owner: OwnerAddr,
  attestations: &[RevocationAttestation]) -> Result<(), FriendHandshakeError>` — per pair,
  `dm_outbox::verify_revocation_push(peer_owner, &att.revocation, &att.enrollment)`; any
  error maps to `FriendHandshakeError::RevocationAttestationInvalid(e.to_string())`; empty
  slice → `Ok(())`.
- Wire-in: `authenticate_friend_request` calls it after the handshake-sig verify (inbound
  request), and each dialer driver calls it after verifying the Accepted frame — a present
  invalid attestation REJECTS the handshake (spec §2 fail-closed).

- [ ] **Step 1: Failing tests**: `carried_revocations_valid_pass` (own-fleet pair → Ok),
  `carried_revocations_third_party_owner_rejected` (revocation.owner != peer → Err, mirrors
  `handle_revocation_push_rejects_third_party_owner` `dm_outbox.rs:3669`),
  `carried_revocations_target_enrollment_mismatch_rejected` (mirrors `dm_outbox.rs:3689`),
  `authenticate_friend_request_rejects_invalid_attestation` (valid request + bogus
  attestation → `RevocationAttestationInvalid`).
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement + wire into the three verification points.**
- [ ] **Step 4: Green** + task-scoped test-select.
- [ ] **Step 5: Commit** `ZEB-680 T5: fail-closed verification of carried revocation attestations`.

### Task 6: Receive phase 2 — apply at establishment + dirty notification

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (`process_friend_request` after the
  `apply_friend_update` success `:1114`), `src-tauri/src/lib.rs` (dialer path after `:51550`
  apply; both drivers).
- Test: `iroh_friend_acceptor.rs` + `lib.rs` test mods.

**Interfaces:**
- Consumes: `dm_outbox::handle_revocation_push(&mut state, req.from_addr /* or peer owner on
  the dialer side */, &att.revocation, &att.enrollment, revoked)` per verified pair — state
  is already `&mut` inside `process_friend_request`; collect `inserted_any: bool`.
- Produces: the dispatch/driver notifies the owner-state sync engine dirty when
  `inserted_any` (find the existing post-`process_friend_request` dirty notification the
  friendship apply already triggers and OR into its condition; pattern reference
  `dm_inbox_ingest.rs:605-640` — apply under lock, drop lock, notify on genuine insert only).

- [ ] **Step 1: Failing tests**:
  - `accepted_handshake_applies_carried_revocations`: full `process_friend_request` with one
    valid attestation → `state.revoked_dm_devices[peer]` contains the key AND
    `projection.is_revoked(peer, key)`.
  - `carried_revocation_apply_is_establishment_gated`: an auth-failing request (bad sig) with
    a valid attestation → state untouched (`revoked_dm_devices` empty).
  - `duplicate_carried_revocation_reports_no_insert`: apply twice → second reports
    `inserted_any == false` (dirty not re-fired; assert via the callback-counting pattern in
    `dm_inbox_ingest.rs` tests, `AtomicUsize` dirty_cb).
- [ ] **Step 2: Run to verify failure.**
- [ ] **Step 3: Implement** both sides (acceptor + two dialer drivers).
- [ ] **Step 4: Green** + task-scoped test-select.
- [ ] **Step 5: Commit** `ZEB-680 T6: carried revocations land in the DM store + projection at establishment`.

### Task 7: End-to-end handshake regressions

**Files:**
- Test only: `iroh_friend_acceptor.rs` (serve-level tests exist from `:2560` on — reuse
  their harness).

- [ ] **Step 1: Three tests**: (a) requester's device pre-seeded revoked in the acceptor's
  projection → handshake refused with `DeviceRevoked`; (b) handshake whose request carries a
  valid attestation → after accept, acceptor's store + projection contain it; (c) a
  pre-ZEB-680 frame (no `"v"` key — encode with empty vec) completes the handshake unchanged.
- [ ] **Step 2: Run — expected pass** (pins behavior; fix any integration gap found).
- [ ] **Step 3: Commit** `ZEB-680 T7: handshake-level revocation regressions`.

### Task 8: Honesty copy + ledger + vitest pin

**Files:**
- Modify: `src/lib/components/RemoveDeviceDialog.svelte` (honesty paragraph, lines ~97-101),
  `docs/specs/2026-07-11-zeb-668-device-management-design.md` (§8 row `:298`).
- Test: co-located or existing component test location (follow the repo's vitest layout;
  `git grep -l "RemoveDeviceDialog" src` for an existing spec file).

- [ ] **Step 1: Rewrite the honesty paragraph** — it must now say: vine feeds stop accepting
  new posts (unchanged clause); direct messages stop being accepted once the removal syncs —
  including contacts you only DM directly (ZEB-685 shipped; DELETE the stale "lands in
  follow-up work" clause); and the device can no longer friend-link, introduce itself, or
  vouch for introductions against peers who have learned of the removal — new friends learn
  at link time; someone it has never met and who shares no community may not know (spec §4
  stranger residual, stated honestly).
- [ ] **Step 2: vitest pin** on the paragraph's key phrases (render the component, assert the
  honesty text contains "no longer" + "introduce" and does NOT contain "follow-up work").
- [ ] **Step 3: Update the ZEB-668 spec §8 row** per design spec §3.
- [ ] **Step 4: `npx tsc --noEmit && npx vitest run` from repo root — green.**
- [ ] **Step 5: Commit** `ZEB-680 T8: revoke-dialog honesty copy — friends/PEX cutoff + ZEB-685 staleness fix`.

### Task 9: Final gates

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  (full sweep; zeb375/zeb376 fixture suites byte-identical is asserted by their own tests)
- [ ] `npx tsc --noEmit && npx vitest run` (repo root)
- [ ] Commit any stragglers; do NOT push (bundle per converge protocol).
