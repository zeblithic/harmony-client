# ZEB-310 Phase 4a-main IPCs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the Tauri IPC + frontend TypeScript surface for Tier 3 governance polls (sortition + STAR), plus engine-auto-orchestration for terminal events (kd=sf/cl/rs), so ZEB-311 UI can drive Tier 3 polls end-to-end.

**Architecture:** Pure additive — extends `community_voting_core.rs` with 9 `pub fn` signed-event builders (relocated from test fixtures), adds 3 post-apply hooks in `community_voting_log_engine.rs` for race-tolerant terminal-event publishing, registers 6 `#[tauri::command]` handlers + 5 emit sites in `lib.rs`, and extends `voting-adapter.ts` with 6 IPC methods + 5 subscribers. The 9 builders are the single signed-event minting surface for both IPC handlers and engine-auto paths.

**Tech Stack:** Rust (Tauri 2.x, ciborium, ed25519-dalek, tokio), TypeScript (Svelte 5 frontend, vitest), wire format CBOR with 2-char same-length keys (per [`feedback_two_ipc_toctou`](../../.claude/projects/-Users-zeblith-work/memory/feedback_two_ipc_toctou.md) — single-call IPCs only, no preview/commit pairs).

---

## Spec reference

Design spec: `docs/specs/2026-05-20-zeb-310-phase4a-main-ipcs-design.md` (commit `7271f90` on this branch).

Backend dependency: `docs/specs/2026-05-20-zeb-309-phase4a-main-design.md` (merged in PR #148 / commit `0902ff2`).

Umbrella: `docs/specs/2026-05-16-zeb-289-voting-polling-design.md` §6.7.

## File structure

| File | Responsibility | Change kind |
|---|---|---|
| `src-tauri/src/community_voting_core.rs` | 9 `pub fn build_signed_*_tier3` + 1 `pub struct PollClosePayload` | Modify (add ~250 LOC) |
| `src-tauri/src/community_voting_log_engine.rs` | 3 post-apply hooks: kd=sf trigger, kd=cl trigger, kd=rs trigger | Modify (add ~250 LOC) |
| `src-tauri/src/lib.rs` | 6 `#[tauri::command]` + 5 payload structs + 5 emit sites + tauri::Builder registrations | Modify (add ~900 LOC) |
| `src-tauri/tests/community_voting_tier3_integration.rs` | Remove duplicated builders; import from core | Modify (delete ~270 LOC) |
| `src-tauri/tests/community_voting_tier3_ipc_integration.rs` | E2E IPC-driven integration tests | Create (~600 LOC) |
| `src-tauri/tests/wire_format_voting_tier3_fixtures.rs` | Pin `tier3_poll_close.cbor` fixture | Modify (add ~30 LOC) |
| `src-tauri/tests/fixtures/voting_tier3/tier3_poll_close.cbor` | Wire fixture (binary CBOR) | Create (regen-on-first-run) |
| `src/lib/voting-adapter.ts` | 6 IPC methods + 5 subscriber methods + `connectAdapter` listener wiring | Modify (add ~200 LOC) |
| `src/lib/types/voting.ts` | 5 payload typedefs + `CreateTier3ProposalArgs` + `CandidateRef` + `CandidateScore` | Modify (add ~80 LOC) |
| `src/lib/__tests__/voting-adapter-tier3.test.ts` | vitest unit coverage for 6 wrappers + 5 subscribers + error extraction | Create (~250 LOC) |

---

## Task 0: Pre-flight green-baseline confirm

**Files:** none (read-only check)

- [ ] **Step 1: Confirm branch state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status
git rev-parse --abbrev-ref HEAD
git log --oneline -3
```

Expected: `On branch zeb-310-phase4a-main-ipcs`, working tree clean, HEAD shows the spec commit `7271f90 docs(zeb-310): design refinement for Phase 4a-main IPCs` on top of `0902ff2 ZEB-309 Phase 4a-main: ...`.

- [ ] **Step 2: Confirm cargo fmt + clippy + nextest baseline are green**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
```

Expected: fmt zero output, clippy zero warnings, nextest "Summary [...] X passed". Record the pass count for delta-tracking; pre-existing orphan failures from ZEB-302/306/308 may persist (~27) — those are not introduced by this work.

- [ ] **Step 3: Confirm frontend tsc + vitest baseline are green**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run 2>&1 | tail -5
```

Expected: tsc zero output, vitest "Test Files X passed".

**NO COMMIT — Task 0 is verification only.**

---

## Task 1: Relocate signed-event builders to `community_voting_core.rs`

**Files:**
- Modify: `src-tauri/src/community_voting_core.rs` (add 9 `pub fn` + `pub struct PollClosePayload`)
- Modify: `src-tauri/tests/community_voting_tier3_integration.rs` (remove duplicated builders, re-export or `use` from core)

Test source for the 9 builders is `src-tauri/tests/community_voting_tier3_integration.rs:254-511`. They take `&TestIdentity` (test-only struct); the relocated `pub fn` versions take `&ed25519_dalek::SigningKey, OwnerAddr` (matching Tier 1 `build_signed_poll_create_tier1` shape at `community_voting_core.rs:1085`).

- [ ] **Step 1: Add `pub struct PollClosePayload` to `community_voting_core.rs`**

Insert after the `RatificationBallotPayload` definition (around line 172):

```rust
/// Payload for `kd=cl` PollClose events (Tier 3). Wire format is a CBOR
/// map with a single 2-char same-length key `pi` → 32-byte `poll_id`.
/// Per spec §3 same-length-keys invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollClosePayload {
    #[serde(rename = "pi")]
    pub poll_id: PollId,
}
```

- [ ] **Step 2: Add the 9 builder functions to `community_voting_core.rs`**

Insert after `build_signed_ballot_tier1` (around line 1137). Use these exact signatures (matching Tier 1 builder shape — `&SigningKey, OwnerAddr, …, Hlc -> Result<SignedVotingEvent, BuildError>`):

```rust
/// Build a fully-signed `kd=cr` PollCreate event for Tier 3 (Sortition).
pub fn build_signed_poll_create_tier3(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    config: &Tier3PollConfigPayload,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let mut payload = Vec::new();
    ciborium::ser::into_writer(config, &mut payload).map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::PollCreate,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=ds` DeliberationStatement event.
pub fn build_signed_deliberation_statement(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    text: String,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = DeliberationStatementPayload { poll_id, text };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DeliberationStatement,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=md` MiniPublicDecline event.
pub fn build_signed_mini_public_decline(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    reason: Option<String>,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = MiniPublicDeclinePayload { poll_id, reason };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::MiniPublicDecline,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=dc` DraftCandidate event.
pub fn build_signed_draft_candidate(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    text: String,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = DraftCandidatePayload { poll_id, text };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DraftCandidate,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=da` DraftApproval event.
pub fn build_signed_draft_approval(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    candidate_event_hash: CandidateEventHash,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = DraftApprovalPayload {
        poll_id,
        candidate_event_hash,
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::DraftApproval,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=rb` RatificationBallot event.
pub fn build_signed_ratification_ballot(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    scores: Vec<u8>,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = RatificationBallotPayload { poll_id, scores };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::RatificationBallot,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=sf` SortitionFailed event. Only the proposer
/// should sign (enforced at the verify layer via SF1).
pub fn build_signed_sortition_failed(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = SortitionFailedPayload { poll_id };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::SortitionFailed,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=cl` PollClose event (Tier 3).
pub fn build_signed_poll_close_tier3(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = PollClosePayload { poll_id };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::PollClose,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}

/// Build a fully-signed `kd=rs` PollResult event (Tier 3).
pub fn build_signed_poll_result_tier3(
    keypair: &ed25519_dalek::SigningKey,
    actor: OwnerAddr,
    poll_id: PollId,
    result: crate::community_voting_star::StarResult,
    hlc: Hlc,
) -> Result<SignedVotingEvent, BuildError> {
    use ed25519_dalek::Signer;
    let payload_struct = crate::community_voting_tier3::Tier3PollResultPayload {
        poll_id,
        result,
    };
    let mut payload = Vec::new();
    ciborium::ser::into_writer(&payload_struct, &mut payload)
        .map_err(|_| BuildError::EncodePayload)?;
    let mut ev = SignedVotingEvent {
        tag: 'p',
        version: 1,
        tier: Tier::Sortition,
        kind: PollEventKindCode::PollResult,
        hlc,
        actor,
        payload,
        sig: vec![0u8; 64],
    };
    let sb = ev.signing_bytes().map_err(|_| BuildError::SigningBytes)?;
    ev.sig = keypair.sign(&sb).to_bytes().to_vec();
    Ok(ev)
}
```

- [ ] **Step 3: Add round-trip unit tests for the 9 builders**

Insert into the existing `mod build_tests` (around line 1160). For DRY, parameterize via a helper:

```rust
#[test]
fn signed_tier3_poll_create_round_trip() {
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0x33; 16]);
    let cfg = Tier3PollConfigPayload {
        sortition_size: 20,
        deliberation_window_seconds: 600,
        drafting_window_seconds: 600,
        ratification_window_seconds: 600,
        incentive_mode: "dp".into(),
        eligibility: Eligibility {
            min_power: 0,
            min_vouching_depth: None,
            sortition_size: Some(20),
        },
        channel_id: ChannelId([0; 16]),
        privacy_mode: "pu".into(),
        proposal_text: "test proposal".into(),
        retry_of: None,
    };
    let hlc = Hlc { wall_ms: 1, logical: 0, device_id: "d".into() };
    let ev = build_signed_poll_create_tier3(&keypair, actor, &cfg, hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::PollCreate);
    assert_eq!(ev.tier, Tier::Sortition);
    verify_sig(&keypair, &ev);
}

#[test]
fn signed_deliberation_statement_round_trip() {
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0x44; 16]);
    let pid = PollId([0x55; 32]);
    let hlc = Hlc { wall_ms: 2, logical: 0, device_id: "d".into() };
    let ev = build_signed_deliberation_statement(&keypair, actor, pid, "hello".into(), hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::DeliberationStatement);
    verify_sig(&keypair, &ev);
}

#[test]
fn signed_mini_public_decline_round_trip() {
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0x66; 16]);
    let pid = PollId([0x77; 32]);
    let hlc = Hlc { wall_ms: 3, logical: 0, device_id: "d".into() };
    let ev = build_signed_mini_public_decline(&keypair, actor, pid, Some("u".into()), hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::MiniPublicDecline);
    verify_sig(&keypair, &ev);
}

#[test]
fn signed_draft_candidate_round_trip() {
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0x88; 16]);
    let pid = PollId([0x99; 32]);
    let hlc = Hlc { wall_ms: 4, logical: 0, device_id: "d".into() };
    let ev = build_signed_draft_candidate(&keypair, actor, pid, "draft".into(), hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::DraftCandidate);
    verify_sig(&keypair, &ev);
}

#[test]
fn signed_draft_approval_round_trip() {
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0xaa; 16]);
    let pid = PollId([0xbb; 32]);
    let ceh = CandidateEventHash([0xcc; 32]);
    let hlc = Hlc { wall_ms: 5, logical: 0, device_id: "d".into() };
    let ev = build_signed_draft_approval(&keypair, actor, pid, ceh, hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::DraftApproval);
    verify_sig(&keypair, &ev);
}

#[test]
fn signed_ratification_ballot_round_trip() {
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0xdd; 16]);
    let pid = PollId([0xee; 32]);
    let hlc = Hlc { wall_ms: 6, logical: 0, device_id: "d".into() };
    let ev = build_signed_ratification_ballot(&keypair, actor, pid, vec![5, 3, 1], hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::RatificationBallot);
    verify_sig(&keypair, &ev);
}

#[test]
fn signed_sortition_failed_round_trip() {
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0xff; 16]);
    let pid = PollId([0x11; 32]);
    let hlc = Hlc { wall_ms: 7, logical: 0, device_id: "d".into() };
    let ev = build_signed_sortition_failed(&keypair, actor, pid, hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::SortitionFailed);
    verify_sig(&keypair, &ev);
}

#[test]
fn signed_poll_close_tier3_round_trip() {
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0x22; 16]);
    let pid = PollId([0x33; 32]);
    let hlc = Hlc { wall_ms: 8, logical: 0, device_id: "d".into() };
    let ev = build_signed_poll_close_tier3(&keypair, actor, pid, hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::PollClose);
    verify_sig(&keypair, &ev);
}

#[test]
fn signed_poll_result_tier3_round_trip() {
    use crate::community_voting_star::StarResult;
    let keypair = SigningKey::generate(&mut OsRng);
    let actor = OwnerAddr([0x44; 16]);
    let pid = PollId([0x55; 32]);
    let hlc = Hlc { wall_ms: 9, logical: 0, device_id: "d".into() };
    let result = StarResult {
        finalists: vec![],
        runoff_votes: vec![],
        winner_event_hash: CandidateEventHash([0xaa; 32]),
        runner_up_event_hash: None,
        scores: vec![],
    };
    let ev = build_signed_poll_result_tier3(&keypair, actor, pid, result, hlc).expect("build");
    assert_eq!(ev.kind, PollEventKindCode::PollResult);
    verify_sig(&keypair, &ev);
}

fn verify_sig(keypair: &SigningKey, ev: &SignedVotingEvent) {
    let sb = ev.signing_bytes().expect("signing bytes");
    let sig_bytes: [u8; 64] = ev.sig.clone().try_into().expect("sig len");
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    use ed25519_dalek::Verifier;
    keypair.verifying_key().verify(&sb, &sig).expect("verify");
}
```

Adjust the imports at the top of the `build_tests` module to include `Tier3PollConfigPayload` + `CandidateEventHash`.

- [ ] **Step 4: Run the new builder tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(community_voting_core::build_tests)' 2>&1 | tail -20
```

Expected: All 11 build_tests pass (2 pre-existing Tier 1 + 9 new Tier 3).

- [ ] **Step 5: Remove duplicate builders from `community_voting_tier3_integration.rs`**

Delete lines 254-511 of `src-tauri/tests/community_voting_tier3_integration.rs` (the 9 `pub fn build_*_event` helpers including `build_sortition_selection_event`). Replace each call site within the same file with a call to the core builder, wrapping `&TestIdentity` to `(&SigningKey, OwnerAddr)`. For `build_sortition_selection_event`, retain a thin local helper (engine-generated kd=ss uses zero actor + zero sig, distinct from signed builders — keep it as the existing local helper without `pub`).

For the test fixture call sites use a small adapter at the top of the file:

```rust
fn id_sign(id: &TestIdentity) -> (&ed25519_dalek::SigningKey, OwnerAddr) {
    (&id.signing_key, id.owner)
}
```

Then replace e.g. `build_tier3_poll_create_event(&proposer, &cfg, hlc)` with `harmony_app::community_voting_core::build_signed_poll_create_tier3(&proposer.signing_key, proposer.owner, &cfg, hlc).expect("build")`.

- [ ] **Step 6: Run the integration test file to confirm no regression**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(community_voting_tier3)' 2>&1 | tail -15
```

Expected: All Tier 3 integration tests still pass with the new builder routing.

- [ ] **Step 7: Run cargo fmt + clippy on the modified files**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
```

Expected: fmt zero output, clippy zero warnings.

- [ ] **Step 8: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_voting_core.rs src-tauri/tests/community_voting_tier3_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): relocate 9 Tier 3 signed-event builders to voting_core

Promotes test-only build_*_event helpers in integration test file to
pub fn in community_voting_core.rs (parity with build_signed_poll_create_tier1).
Adds pub struct PollClosePayload. Integration tests now call core builders
via (&SigningKey, OwnerAddr) adapter. 9 round-trip unit tests cover all kinds.
Single signed-event minting surface for the 6 IPCs + 3 engine-auto paths.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add 5 Tauri event payload structs to `lib.rs`

**Files:**
- Modify: `src-tauri/src/lib.rs` (add 5 payload structs near existing voting payloads at line 20609)

- [ ] **Step 1: Write failing-compile structural test**

Add to `src-tauri/src/lib.rs` after the existing `VotingBallotCastPayload` (around line 20626):

```rust
#[cfg(test)]
mod tier3_payload_struct_tests {
    use super::*;

    #[test]
    fn tier3_poll_created_payload_serializes_camel_case() {
        let p = VotingTier3PollCreatedPayload {
            poll_id: "00".repeat(32),
            channel_id: "11".repeat(16),
            community_id: "22".repeat(16),
            proposer: "33".repeat(16),
            sortition_size: 20,
            deliberation_window_seconds: 600,
            drafting_window_seconds: 600,
            ratification_window_seconds: 600,
        };
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"pollId\""));
        assert!(json.contains("\"channelId\""));
        assert!(json.contains("\"sortitionSize\":20"));
        assert!(json.contains("\"deliberationWindowSeconds\":600"));
    }

    #[test]
    fn tier3_sortition_complete_payload_serializes_camel_case() {
        let p = VotingTier3SortitionCompletePayload {
            poll_id: "aa".repeat(32),
            community_id: "bb".repeat(16),
            primary: vec!["cc".repeat(16)],
            backup: vec!["dd".repeat(16)],
        };
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"pollId\""));
        assert!(json.contains("\"primary\""));
    }

    #[test]
    fn tier3_drafting_open_payload_serializes_camel_case() {
        let p = VotingTier3DraftingOpenPayload {
            poll_id: "ee".repeat(32),
            community_id: "ff".repeat(16),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"pollId\""));
        assert!(json.contains("\"communityId\""));
    }

    #[test]
    fn tier3_ratification_open_payload_serializes_camel_case() {
        let p = VotingTier3RatificationOpenPayload {
            poll_id: "01".repeat(32),
            community_id: "02".repeat(16),
            candidate_ordering: vec![CandidateRefDto {
                event_hash: "03".repeat(32),
                text: "candidate a".into(),
                approval_count: 5,
            }],
        };
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"candidateOrdering\""));
        assert!(json.contains("\"approvalCount\":5"));
    }

    #[test]
    fn tier3_finalized_payload_serializes_camel_case() {
        let p = VotingTier3FinalizedPayload {
            poll_id: "04".repeat(32),
            community_id: "05".repeat(16),
            winner_event_hash: "06".repeat(32),
            winner_text: "the winner".into(),
            runner_up_event_hash: Some("07".repeat(32)),
            scores_summary: vec![CandidateScoreDto {
                event_hash: "08".repeat(32),
                total_score: 12,
                runoff_votes: 3,
            }],
        };
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(json.contains("\"winnerEventHash\""));
        assert!(json.contains("\"runnerUpEventHash\""));
        assert!(json.contains("\"totalScore\":12"));
    }
}
```

- [ ] **Step 2: Run test to verify compile failure**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest list --features test-fixtures -E 'test(tier3_payload_struct_tests)' 2>&1 | tail -10
```

Expected: compile error (`VotingTier3PollCreatedPayload not found`, etc.).

- [ ] **Step 3: Add the 5 payload structs**

Insert after the existing `VotingBallotCastPayload` (around line 20626) in `src-tauri/src/lib.rs`:

```rust
/// Tauri event payload for `"voting-tier3-poll-created"`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VotingTier3PollCreatedPayload {
    pub poll_id: String,
    pub channel_id: String,
    pub community_id: String,
    pub proposer: String,
    pub sortition_size: u16,
    pub deliberation_window_seconds: u32,
    pub drafting_window_seconds: u32,
    pub ratification_window_seconds: u32,
}

/// Tauri event payload for `"voting-tier3-sortition-complete"`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VotingTier3SortitionCompletePayload {
    pub poll_id: String,
    pub community_id: String,
    pub primary: Vec<String>,
    pub backup: Vec<String>,
}

/// Tauri event payload for `"voting-tier3-drafting-open"`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VotingTier3DraftingOpenPayload {
    pub poll_id: String,
    pub community_id: String,
}

/// One candidate in the ratification ordering (DTO mirroring `CandidateRef`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateRefDto {
    pub event_hash: String,
    pub text: String,
    pub approval_count: u32,
}

/// Tauri event payload for `"voting-tier3-ratification-open"`. Includes
/// the synthesized status_quo candidate at the end of `candidate_ordering`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VotingTier3RatificationOpenPayload {
    pub poll_id: String,
    pub community_id: String,
    pub candidate_ordering: Vec<CandidateRefDto>,
}

/// One per-candidate STAR tally summary (DTO).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateScoreDto {
    pub event_hash: String,
    pub total_score: u32,
    pub runoff_votes: u32,
}

/// Tauri event payload for `"voting-tier3-finalized"`. Carries enough for
/// the UI to render the winner without re-querying the poll.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VotingTier3FinalizedPayload {
    pub poll_id: String,
    pub community_id: String,
    pub winner_event_hash: String,
    pub winner_text: String,
    pub runner_up_event_hash: Option<String>,
    pub scores_summary: Vec<CandidateScoreDto>,
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(tier3_payload_struct_tests)' 2>&1 | tail -10
```

Expected: 5 tests pass.

- [ ] **Step 5: cargo fmt + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: zero output / zero warnings.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): add 5 Tauri event payload structs for Tier 3 governance

VotingTier3PollCreatedPayload / VotingTier3SortitionCompletePayload /
VotingTier3DraftingOpenPayload / VotingTier3RatificationOpenPayload /
VotingTier3FinalizedPayload + 2 DTOs (CandidateRefDto, CandidateScoreDto).
All camelCase-serialized; carry enough for UI render without re-query.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: IPC `voting_create_tier3_proposal`

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `#[tauri::command]` handler + `tauri::Builder::default().invoke_handler` registration)

Mirror `voting_create_tier1_poll` (line 20635) shape: hex-decode IDs → build config → snapshot → eligibility → mint signed → publish_event → emit → return PollId hex. Reuse the Tier 1 chat-fanout machinery (POLL_BODY_MAGIC + 64-char hex).

- [ ] **Step 1: Write the IPC + unit test scaffold**

Add to `src-tauri/src/lib.rs` after `voting_get_poll` (around line 21030). Stub a `#[cfg(test)]` IPC happy-path test next to it. Code (sketch — full IPC):

```rust
/// Tauri IPC: create a Tier 3 (Sortition + STAR) governance poll. Returns the
/// new PollId as a hex string. Pre-flight ordering mirrors
/// `voting_create_tier1_poll`: validate config → snapshot → eligibility check →
/// mint signed event → publish_event (local apply + Zenoh broadcast) → emit
/// "voting-tier3-poll-created" → chat-fanout poll-kind message.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn voting_create_tier3_proposal<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    app: tauri::AppHandle<R>,
    community_id: String,
    channel_id: String,
    proposal_text: String,
    sortition_size: u16,
    deliberation_window_seconds: u32,
    drafting_window_seconds: u32,
    ratification_window_seconds: u32,
    incentive_mode: String,
    min_power: u8,
    min_vouching_depth: Option<u8>,
    retry_of: Option<String>,
) -> Result<String, String> {
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("voting_create_tier3_proposal: invalid community_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "voting_create_tier3_proposal: community_id must be 16 bytes (32 hex chars)".to_string())?;
    let space_id = crate::owner_state_types::SpaceId(cid_bytes);

    let chid_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("voting_create_tier3_proposal: invalid channel_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "voting_create_tier3_proposal: channel_id must be 16 bytes (32 hex chars)".to_string())?;
    let channel = crate::community_membership::ChannelId(chid_bytes);

    let retry_of_pid: Option<crate::community_voting_core::PollId> = match retry_of {
        None => None,
        Some(hex_str) => {
            let bytes: [u8; 32] = hex::decode(&hex_str)
                .map_err(|e| format!("voting_create_tier3_proposal: invalid retry_of hex: {e}"))?
                .as_slice()
                .try_into()
                .map_err(|_| "voting_create_tier3_proposal: retry_of must be 32 bytes (64 hex chars)".to_string())?;
            Some(crate::community_voting_core::PollId(bytes))
        }
    };

    let cfg = crate::community_voting_core::Tier3PollConfigPayload {
        sortition_size,
        deliberation_window_seconds,
        drafting_window_seconds,
        ratification_window_seconds,
        incentive_mode,
        eligibility: crate::community_voting_core::Eligibility {
            min_power,
            min_vouching_depth,
            sortition_size: Some(sortition_size),
        },
        channel_id: channel,
        privacy_mode: "pu".into(),
        proposal_text,
        retry_of: retry_of_pid,
    };
    crate::community_voting_tier3::validate_tier3_poll_config(&cfg)
        .map_err(|e| format!("voting_create_tier3_proposal: invalid config: {e:?}"))?;

    let (
        hlc_tracker,
        device_id,
        self_owner,
        community_registry,
        dm_outbox,
        crdt_state,
        voting_logs,
        channel_log_registry,
    ) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.community_registry.clone().ok_or("community_registry missing — node not running?")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing — no owner identity?")?,
            g.crdt_state.clone().ok_or("crdt_state missing — node not running?")?,
            std::sync::Arc::clone(&g.voting_logs),
            g.channel_log_registry.clone(),
        )
    };

    let snapshot = voting_build_snapshot_for_community(crdt_state, community_registry, space_id).await?;
    crate::community_voting_core::check_eligibility(&snapshot, &self_owner, &cfg.eligibility)
        .map_err(|e| format!("voting_create_tier3_proposal: creator not eligible: {e:?}"))?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        crate::community_voting_core::build_signed_poll_create_tier3(signing_key, self_owner, &cfg, hlc)
            .map_err(|e| format!("voting_create_tier3_proposal: build_signed: {e:?}"))?
    };

    let poll_id = {
        let log_arc = {
            let mut map = voting_logs.lock().await;
            map.entry(space_id)
                .or_insert_with(|| {
                    std::sync::Arc::new(tokio::sync::Mutex::new(crate::community_voting_log::VotingLog::new()))
                })
                .clone()
        };
        let mut log = log_arc.lock().await;
        log.apply_with_snapshot(event, &space_id, Some(snapshot))
            .map_err(|e| format!("voting_create_tier3_proposal: apply: {e:?}"))?
    };

    let poll_id_hex = hex::encode(poll_id.0);
    let payload = VotingTier3PollCreatedPayload {
        poll_id: poll_id_hex.clone(),
        channel_id: hex::encode(channel.0),
        community_id: hex::encode(space_id.0),
        proposer: hex::encode(self_owner.0),
        sortition_size,
        deliberation_window_seconds,
        drafting_window_seconds,
        ratification_window_seconds,
    };
    if let Err(e) = app.emit("voting-tier3-poll-created", &payload) {
        tracing::warn!(error = %e, "voting-tier3-poll-created emit failed");
    }

    if let Some(registry) = channel_log_registry {
        if let Some(engine) = registry.engine(&space_id, &channel).await {
            let mut body = Vec::with_capacity(crate::community_channel_log_engine::POLL_BODY_LEN);
            body.push(crate::community_channel_log_engine::POLL_BODY_MAGIC);
            body.extend_from_slice(poll_id_hex.as_bytes());
            if let Err(e) = engine.publish(body, None).await {
                tracing::warn!(
                    error = %e,
                    community_id = %hex::encode(space_id.0),
                    poll_id = %poll_id_hex,
                    "voting_create_tier3_proposal: poll-kind chat fanout failed (non-fatal)"
                );
            }
        }
    }

    Ok(poll_id_hex)
}
```

- [ ] **Step 2: Register the IPC in tauri::Builder**

Find `tauri::Builder::default().invoke_handler(tauri::generate_handler![…` in `src-tauri/src/lib.rs` and add `voting_create_tier3_proposal,` to the list (preserve alphabetical-ish grouping with other `voting_*` IPCs).

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
grep -n "voting_get_poll," src/lib.rs
```

Add `voting_create_tier3_proposal,` immediately after `voting_create_tier1_poll,`.

- [ ] **Step 3: cargo fmt + clippy + nextest scoped**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cargo nextest run --locked --features test-fixtures -E 'test(voting_create_tier3)' 2>&1 | tail -10
```

Expected: fmt zero, clippy zero, scoped nextest passes (no tests directly target this name yet, but the codebase compiles).

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): voting_create_tier3_proposal IPC + chat-fanout

Mirrors voting_create_tier1_poll pre-flight ordering (validate → snapshot →
eligibility → sign → apply → emit → chat-fanout). Reuses POLL_BODY_MAGIC + hex
poll_id chat-message format so Tier 3 polls render inline in chat feed.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: IPC `voting_submit_deliberation_statement`

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `#[tauri::command]` + builder registration)

- [ ] **Step 1: Implement the IPC**

Add after `voting_create_tier3_proposal`:

```rust
/// Tauri IPC: submit a deliberation statement (kd=ds) for a Tier 3 poll.
/// Phase 5 will wire Pol.is clustering; this phase emits valid kd=ds events.
/// Returns the event_hash hex (32 bytes → 64 chars).
#[tauri::command]
async fn voting_submit_deliberation_statement<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    _app: tauri::AppHandle<R>,
    poll_id: String,
    text: String,
) -> Result<String, String> {
    let pid_bytes: [u8; 32] = hex::decode(&poll_id)
        .map_err(|e| format!("voting_submit_deliberation_statement: invalid poll_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "voting_submit_deliberation_statement: poll_id must be 32 bytes (64 hex chars)".to_string())?;
    let pid = crate::community_voting_core::PollId(pid_bytes);

    if text.is_empty() || text.len() > 512 {
        return Err(format!(
            "voting_submit_deliberation_statement: text length {} out of range (1..=512)",
            text.len()
        ));
    }

    let (hlc_tracker, device_id, self_owner, dm_outbox, voting_logs) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            std::sync::Arc::clone(&g.voting_logs),
        )
    };

    // Resolve space_id from the poll_id by scanning open polls.
    let space_id = voting_resolve_community_for_poll(&voting_logs, &pid).await?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        crate::community_voting_core::build_signed_deliberation_statement(
            signing_key, self_owner, pid, text, hlc,
        )
        .map_err(|e| format!("voting_submit_deliberation_statement: build_signed: {e:?}"))?
    };

    let event_hash = hex::encode(crate::community_voting_tier3::event_hash_of(&event));

    let log_arc = {
        let mut map = voting_logs.lock().await;
        map.entry(space_id)
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Mutex::new(crate::community_voting_log::VotingLog::new()))
            })
            .clone()
    };
    let mut log = log_arc.lock().await;
    log.apply_with_snapshot(event, &space_id, None)
        .map_err(|e| format!("voting_submit_deliberation_statement: apply: {e:?}"))?;
    Ok(event_hash)
}
```

- [ ] **Step 2: Add helper `voting_resolve_community_for_poll`**

Add a private helper near the IPCs (used by Tasks 4-8 to find the SpaceId for a given PollId without requiring the caller to pass it):

```rust
/// Scan loaded VotingLogs for a poll matching `pid`. Returns the SpaceId
/// owning the poll, or an error if none found.
async fn voting_resolve_community_for_poll(
    voting_logs: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<crate::owner_state_types::SpaceId, std::sync::Arc<tokio::sync::Mutex<crate::community_voting_log::VotingLog>>>>>,
    pid: &crate::community_voting_core::PollId,
) -> Result<crate::owner_state_types::SpaceId, String> {
    let map = voting_logs.lock().await;
    for (sid, log_arc) in map.iter() {
        let log = log_arc.lock().await;
        if log.has_poll(pid) {
            return Ok(*sid);
        }
    }
    Err(format!("poll {} not found in any loaded community", hex::encode(pid.0)))
}
```

- [ ] **Step 3: Add `pub fn has_poll` + `pub fn event_hash_of`**

Add to `src-tauri/src/community_voting_log.rs` (or wherever PollLog/VotingLog lives):

```rust
impl VotingLog {
    /// Returns true if any poll with this PollId is currently tracked.
    pub fn has_poll(&self, pid: &crate::community_voting_core::PollId) -> bool {
        self.polls.contains_key(pid)
    }
}
```

Add to `src-tauri/src/community_voting_tier3.rs` (or `community_voting_core.rs` — match existing co-location):

```rust
/// SHA-256 of signing_bytes for the event. Used to derive event_hash for
/// kd=dc DraftCandidate and kd=cl PollClose references.
pub fn event_hash_of(event: &crate::community_voting_core::SignedVotingEvent) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let sb = event.signing_bytes().expect("signing_bytes");
    let mut hasher = Sha256::new();
    hasher.update(&sb);
    hasher.finalize().into()
}
```

- [ ] **Step 4: Register the IPC**

Add `voting_submit_deliberation_statement,` to the `tauri::generate_handler!` list.

- [ ] **Step 5: cargo fmt + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
```

Expected: zero output, zero warnings.

- [ ] **Step 6: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/community_voting_log.rs src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): voting_submit_deliberation_statement IPC + helpers

Adds kd=ds IPC (Phase 5 scaffold) + voting_resolve_community_for_poll
helper + VotingLog::has_poll + event_hash_of free fn. Returns event_hash
hex for the caller to reference in subsequent DraftApproval calls.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: IPC `voting_propose_draft_candidate`

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `#[tauri::command]` + builder registration)

- [ ] **Step 1: Implement the IPC**

Add after `voting_submit_deliberation_statement`. Same shape as Task 4 but uses `build_signed_draft_candidate` and returns the candidate_event_hash hex.

```rust
#[tauri::command]
async fn voting_propose_draft_candidate<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    _app: tauri::AppHandle<R>,
    poll_id: String,
    candidate_text: String,
) -> Result<String, String> {
    let pid_bytes: [u8; 32] = hex::decode(&poll_id)
        .map_err(|e| format!("voting_propose_draft_candidate: invalid poll_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "voting_propose_draft_candidate: poll_id must be 32 bytes (64 hex chars)".to_string())?;
    let pid = crate::community_voting_core::PollId(pid_bytes);

    if candidate_text.is_empty() || candidate_text.len() > 512 {
        return Err(format!(
            "voting_propose_draft_candidate: candidate_text length {} out of range (1..=512)",
            candidate_text.len()
        ));
    }

    let (hlc_tracker, device_id, self_owner, dm_outbox, voting_logs) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            std::sync::Arc::clone(&g.voting_logs),
        )
    };

    let space_id = voting_resolve_community_for_poll(&voting_logs, &pid).await?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        crate::community_voting_core::build_signed_draft_candidate(
            signing_key, self_owner, pid, candidate_text, hlc,
        )
        .map_err(|e| format!("voting_propose_draft_candidate: build_signed: {e:?}"))?
    };

    let event_hash = hex::encode(crate::community_voting_tier3::event_hash_of(&event));

    let log_arc = {
        let mut map = voting_logs.lock().await;
        map.entry(space_id)
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Mutex::new(crate::community_voting_log::VotingLog::new()))
            })
            .clone()
    };
    let mut log = log_arc.lock().await;
    log.apply_with_snapshot(event, &space_id, None)
        .map_err(|e| format!("voting_propose_draft_candidate: apply: {e:?}"))?;
    Ok(event_hash)
}
```

- [ ] **Step 2: Register**

Add `voting_propose_draft_candidate,` to the `tauri::generate_handler!` list.

- [ ] **Step 3: cargo fmt + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: zero output, zero warnings.

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): voting_propose_draft_candidate IPC (kd=dc)

Mini-public members can propose draft candidates. Returns the
candidate_event_hash hex for downstream DraftApproval references.
512-char cap on candidate_text matches spec §3 wire-format limits.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: IPC `voting_approve_draft_candidate`

**Files:**
- Modify: `src-tauri/src/lib.rs` (add `#[tauri::command]` + builder registration)

- [ ] **Step 1: Implement**

Add after `voting_propose_draft_candidate`:

```rust
/// Tauri IPC: approve someone else's draft candidate (kd=da). Mini-public
/// members only (enforced at verify via SD1). Proposer of the candidate
/// implicitly approves their own; this IPC handles all other approvals.
#[tauri::command]
async fn voting_approve_draft_candidate<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    _app: tauri::AppHandle<R>,
    poll_id: String,
    candidate_event_hash: String,
) -> Result<(), String> {
    let pid_bytes: [u8; 32] = hex::decode(&poll_id)
        .map_err(|e| format!("voting_approve_draft_candidate: invalid poll_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "voting_approve_draft_candidate: poll_id must be 32 bytes (64 hex chars)".to_string())?;
    let pid = crate::community_voting_core::PollId(pid_bytes);

    let ceh_bytes: [u8; 32] = hex::decode(&candidate_event_hash)
        .map_err(|e| format!("voting_approve_draft_candidate: invalid candidate_event_hash hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "voting_approve_draft_candidate: candidate_event_hash must be 32 bytes (64 hex chars)".to_string())?;
    let ceh = crate::community_voting_core::CandidateEventHash(ceh_bytes);

    let (hlc_tracker, device_id, self_owner, dm_outbox, voting_logs) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            std::sync::Arc::clone(&g.voting_logs),
        )
    };

    let space_id = voting_resolve_community_for_poll(&voting_logs, &pid).await?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        crate::community_voting_core::build_signed_draft_approval(
            signing_key, self_owner, pid, ceh, hlc,
        )
        .map_err(|e| format!("voting_approve_draft_candidate: build_signed: {e:?}"))?
    };

    let log_arc = {
        let mut map = voting_logs.lock().await;
        map.entry(space_id)
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Mutex::new(crate::community_voting_log::VotingLog::new()))
            })
            .clone()
    };
    let mut log = log_arc.lock().await;
    log.apply_with_snapshot(event, &space_id, None)
        .map_err(|e| format!("voting_approve_draft_candidate: apply: {e:?}"))?;
    Ok(())
}
```

- [ ] **Step 2: Register**

Add `voting_approve_draft_candidate,` to the `tauri::generate_handler!` list.

- [ ] **Step 3: cargo fmt + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): voting_approve_draft_candidate IPC (kd=da)

The 6th IPC (beyond ticket's 5 listed) — added per design Q1 to let
mini-public members approve someone else's draft candidate. Proposer of
the candidate implicitly approves their own via the kd=dc apply path;
this IPC handles all other approvals. SD1 verify enforces mini-public
membership; verify_da_candidate_exists enforces candidate existence.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: IPC `voting_decline_sortition`

**Files:**
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/community_voting_tier3.rs` (add `pub fn validate_decline_reason`)

- [ ] **Step 1: Add `validate_decline_reason`**

Add to `src-tauri/src/community_voting_tier3.rs`:

```rust
/// Validate kd=md `reason` field: if `Some`, must be exactly 2 ASCII chars.
/// Per spec §3 same-length-keys invariant and §6.1.2 decline payload.
pub fn validate_decline_reason(reason: &Option<String>) -> Result<(), ValidateError> {
    if let Some(r) = reason {
        if r.len() != 2 || !r.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(ValidateError::BadDeclineReason);
        }
    }
    Ok(())
}
```

Add `BadDeclineReason` variant to the existing `ValidateError` enum in `community_voting_tier3.rs`.

- [ ] **Step 2: Add unit tests for `validate_decline_reason`**

```rust
#[cfg(test)]
mod validate_decline_reason_tests {
    use super::*;

    #[test]
    fn none_accepted() {
        assert!(validate_decline_reason(&None).is_ok());
    }

    #[test]
    fn two_char_alphanumeric_accepted() {
        assert!(validate_decline_reason(&Some("u1".into())).is_ok());
        assert!(validate_decline_reason(&Some("co".into())).is_ok());
    }

    #[test]
    fn empty_rejected() {
        assert!(matches!(
            validate_decline_reason(&Some(String::new())),
            Err(ValidateError::BadDeclineReason)
        ));
    }

    #[test]
    fn three_chars_rejected() {
        assert!(matches!(
            validate_decline_reason(&Some("abc".into())),
            Err(ValidateError::BadDeclineReason)
        ));
    }

    #[test]
    fn non_ascii_rejected() {
        assert!(matches!(
            validate_decline_reason(&Some("é!".into())),
            Err(ValidateError::BadDeclineReason)
        ));
    }
}
```

- [ ] **Step 3: Implement the IPC**

Add to `src-tauri/src/lib.rs` after `voting_approve_draft_candidate`:

```rust
#[tauri::command]
async fn voting_decline_sortition<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    _app: tauri::AppHandle<R>,
    poll_id: String,
    reason: Option<String>,
) -> Result<(), String> {
    let pid_bytes: [u8; 32] = hex::decode(&poll_id)
        .map_err(|e| format!("voting_decline_sortition: invalid poll_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "voting_decline_sortition: poll_id must be 32 bytes (64 hex chars)".to_string())?;
    let pid = crate::community_voting_core::PollId(pid_bytes);

    crate::community_voting_tier3::validate_decline_reason(&reason)
        .map_err(|e| format!("voting_decline_sortition: invalid reason: {e:?}"))?;

    let (hlc_tracker, device_id, self_owner, dm_outbox, voting_logs) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            std::sync::Arc::clone(&g.voting_logs),
        )
    };

    let space_id = voting_resolve_community_for_poll(&voting_logs, &pid).await?;

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        crate::community_voting_core::build_signed_mini_public_decline(
            signing_key, self_owner, pid, reason, hlc,
        )
        .map_err(|e| format!("voting_decline_sortition: build_signed: {e:?}"))?
    };

    let log_arc = {
        let mut map = voting_logs.lock().await;
        map.entry(space_id)
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Mutex::new(crate::community_voting_log::VotingLog::new()))
            })
            .clone()
    };
    let mut log = log_arc.lock().await;
    log.apply_with_snapshot(event, &space_id, None)
        .map_err(|e| format!("voting_decline_sortition: apply: {e:?}"))?;
    Ok(())
}
```

- [ ] **Step 4: Register + fmt + clippy + nextest scoped**

Add `voting_decline_sortition,` to `tauri::generate_handler!`. Run:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cargo nextest run --locked --features test-fixtures -E 'test(validate_decline_reason)' 2>&1 | tail -10
```

Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/community_voting_tier3.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): voting_decline_sortition IPC (kd=md) + validate_decline_reason

2-char ASCII-alphanumeric reason validation per spec §3 same-length-keys.
ValidateError::BadDeclineReason variant + 5 round-trip tests.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: IPC `voting_cast_ratification_ballot`

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Implement**

```rust
#[tauri::command]
async fn voting_cast_ratification_ballot<R: tauri::Runtime>(
    state_lock: tauri::State<'_, Mutex<NodeState>>,
    _app: tauri::AppHandle<R>,
    poll_id: String,
    scores: Vec<u8>,
) -> Result<(), String> {
    let pid_bytes: [u8; 32] = hex::decode(&poll_id)
        .map_err(|e| format!("voting_cast_ratification_ballot: invalid poll_id hex: {e}"))?
        .as_slice()
        .try_into()
        .map_err(|_| "voting_cast_ratification_ballot: poll_id must be 32 bytes (64 hex chars)".to_string())?;
    let pid = crate::community_voting_core::PollId(pid_bytes);

    let (hlc_tracker, device_id, self_owner, dm_outbox, voting_logs) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.hlc_tracker.clone().ok_or("hlc_tracker missing")?,
            g.dm_device_id.clone().ok_or("dm_device_id missing")?,
            g.dm_self_owner.ok_or("dm_self_owner missing")?,
            g.dm_outbox.clone().ok_or("dm_outbox missing")?,
            std::sync::Arc::clone(&g.voting_logs),
        )
    };

    let space_id = voting_resolve_community_for_poll(&voting_logs, &pid).await?;

    // Pre-flight validate via tier3::validate_ratification_ballot:
    // load poll state, then call validator with current ratification ordering length.
    {
        let log_arc = {
            let map = voting_logs.lock().await;
            map.get(&space_id).cloned()
                .ok_or_else(|| format!("voting_cast_ratification_ballot: no log for community {}", hex::encode(space_id.0)))?
        };
        let log = log_arc.lock().await;
        let candidate_count = log.tier3_ratification_candidate_count(&pid)
            .ok_or_else(|| "voting_cast_ratification_ballot: poll not in Ratification stage or not Tier 3".to_string())?;
        crate::community_voting_tier3::validate_ratification_ballot(&scores, candidate_count)
            .map_err(|e| format!("voting_cast_ratification_ballot: invalid ballot: {e:?}"))?;
    }

    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let hlc = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;

    let event = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.signing_key.as_ref();
        crate::community_voting_core::build_signed_ratification_ballot(
            signing_key, self_owner, pid, scores, hlc,
        )
        .map_err(|e| format!("voting_cast_ratification_ballot: build_signed: {e:?}"))?
    };

    let log_arc = {
        let mut map = voting_logs.lock().await;
        map.entry(space_id)
            .or_insert_with(|| {
                std::sync::Arc::new(tokio::sync::Mutex::new(crate::community_voting_log::VotingLog::new()))
            })
            .clone()
    };
    let mut log = log_arc.lock().await;
    log.apply_with_snapshot(event, &space_id, None)
        .map_err(|e| format!("voting_cast_ratification_ballot: apply: {e:?}"))?;
    Ok(())
}
```

- [ ] **Step 2: Add `tier3_ratification_candidate_count` helper to `VotingLog`**

In `src-tauri/src/community_voting_log.rs`:

```rust
impl VotingLog {
    /// Return the count of ratification candidates for a Tier 3 poll
    /// currently in `Stage::Ratification`. Returns None if the poll is
    /// not Tier 3 or not in Ratification.
    pub fn tier3_ratification_candidate_count(
        &self,
        pid: &crate::community_voting_core::PollId,
    ) -> Option<usize> {
        let state = self.polls.get(pid)?;
        if let TierState::Tier3(t3) = &state.tier_state {
            if matches!(t3.stage, crate::community_voting_tier3::Stage::Ratification) {
                let ordering = crate::community_voting_tier3::ratification_candidates_ordering(t3);
                return Some(ordering.len());
            }
        }
        None
    }
}
```

- [ ] **Step 3: Register + fmt + clippy**

Add `voting_cast_ratification_ballot,` to the `tauri::generate_handler!` list.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/community_voting_log.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): voting_cast_ratification_ballot IPC (kd=rb) + helper

Pre-flight validate via tier3::validate_ratification_ballot against the
current ratification candidate ordering length. tier3_ratification_candidate_count
helper on VotingLog (returns None when poll is not Tier 3 or not in Ratification).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Engine-auto orchestration — kd=sf (SortitionFailed)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (add post-apply hook)
- Modify: `src-tauri/tests/community_voting_tier3_integration.rs` (add multi-engine test)

- [ ] **Step 1: Write the failing multi-engine test**

Add to `src-tauri/tests/community_voting_tier3_integration.rs`:

```rust
#[tokio::test]
async fn engine_auto_sf_on_mass_decline_from_proposer() {
    use harmony_app::community_voting_core::{PollId, Tier3PollConfigPayload, Eligibility};
    use harmony_app::community_voting_tier3::Stage;

    let community_id = SpaceId([0xc1; 16]);
    let engines = setup_two_voting_engine_bridge(community_id).await;
    let proposer = fixture_identity(0x01);

    // Create a Tier 3 poll on engine_a with sortition_size=2 (primary=2, backup=2, total=4).
    let cfg = Tier3PollConfigPayload {
        sortition_size: 2,
        deliberation_window_seconds: 60,
        drafting_window_seconds: 60,
        ratification_window_seconds: 60,
        incentive_mode: "dp".into(),
        eligibility: Eligibility { min_power: 0, min_vouching_depth: None, sortition_size: Some(2) },
        channel_id: harmony_app::community_membership::ChannelId([0; 16]),
        privacy_mode: "pu".into(),
        proposal_text: "test".into(),
        retry_of: None,
    };
    let hlc1 = Hlc { wall_ms: 1, logical: 0, device_id: "a".into() };
    let create_ev = harmony_app::community_voting_core::build_signed_poll_create_tier3(
        &proposer.signing_key, proposer.owner, &cfg, hlc1,
    ).expect("build create");
    let pid = engines.engine_a.publish_event(create_ev).await.expect("publish create");

    // Inject a sortition selection (4 members: 2 primary + 2 backup, all not proposer).
    let m1 = fixture_identity(0x02);
    let m2 = fixture_identity(0x03);
    let m3 = fixture_identity(0x04);
    let m4 = fixture_identity(0x05);
    let ss_ev = build_sortition_selection_event(
        pid,
        vec![m1.owner, m2.owner],
        vec![m3.owner, m4.owner],
        Hlc { wall_ms: 2, logical: 0, device_id: "a".into() },
    );
    engines.engine_a.publish_event(ss_ev).await.expect("publish ss");

    // All 4 decline.
    for (i, m) in [&m1, &m2, &m3, &m4].iter().enumerate() {
        let hlc = Hlc { wall_ms: 3 + i as u64, logical: 0, device_id: "a".into() };
        let md_ev = harmony_app::community_voting_core::build_signed_mini_public_decline(
            &m.signing_key, m.owner, pid, None, hlc,
        ).expect("build md");
        engines.engine_a.publish_event(md_ev).await.expect("publish md");
    }

    // Engine-auto-orchestrated kd=sf should fire on engine_a's apply hook,
    // since engine_a holds the proposer's signing key (the bridge wires
    // signing through dm_outbox). Wait up to 2s for it.
    // NOTE: engine_a's bridge fixture needs to inject the proposer's signing
    // key. This step requires fixture extension — see Step 2.
    wait_for(|| async {
        let log_b = engines.log_b.lock().await;
        if let Some(state) = log_b.poll_state(&pid) {
            if let harmony_app::community_voting_log::TierState::Tier3(t3) = &state.tier_state {
                if matches!(t3.stage, Stage::Failed) {
                    return Some(());
                }
            }
        }
        None
    }, 50, 40).await.expect("kd=sf should propagate to engine_b within 2s");
}
```

- [ ] **Step 2: Extend `setup_two_voting_engine_bridge` to accept proposer signing key**

Modify the existing helper in `community_voting_tier3_integration.rs` to accept an optional `&TestIdentity` representing the proposer whose signing key the engine should hold. This requires plumbing a signing_key handle into `VotingLogEngine` for engine-auto orchestration paths. Use the `install_dfrost_handle` pattern as the template — add `install_proposer_signing_key(&self, key: Arc<SigningKey>, owner: OwnerAddr)` and store as `Option<(Arc<SigningKey>, OwnerAddr)>` on the engine. The engine-auto kd=sf path reads this and skips orchestration if absent.

```rust
// In community_voting_log_engine.rs:
pub struct VotingLogEngine<R: tauri::Runtime> {
    // ... existing fields ...
    /// For engine-auto-orchestration paths: the local node's signing
    /// key + owner. When `None`, engine-auto paths skip orchestration
    /// (read-only peer mode). Set via `install_local_signing_key`.
    local_signing: tokio::sync::RwLock<Option<(std::sync::Arc<ed25519_dalek::SigningKey>, crate::owner_state_types::OwnerAddr)>>,
}

impl<R: tauri::Runtime> VotingLogEngine<R> {
    pub async fn install_local_signing_key(
        &self,
        key: std::sync::Arc<ed25519_dalek::SigningKey>,
        owner: crate::owner_state_types::OwnerAddr,
    ) {
        let mut w = self.local_signing.write().await;
        *w = Some((key, owner));
    }
}
```

- [ ] **Step 3: Implement the kd=sf engine-auto hook**

Add a post-apply hook in `VotingLogEngine::publish_event` (or in the `apply_with_snapshot` callback path). After successful apply that touches a Tier 3 poll, check:

```rust
// In VotingLogEngine::publish_event, after successful local apply:
async fn maybe_trigger_engine_auto_orchestration(
    self: &Arc<Self>,
    space_id: &SpaceId,
    pid: &PollId,
) {
    let (signing_key, self_owner) = {
        let r = self.local_signing.read().await;
        match r.as_ref() {
            Some((k, o)) => (k.clone(), *o),
            None => return,
        }
    };

    let log_arc = {
        let map = self.voting_logs.lock().await;
        match map.get(space_id) {
            Some(la) => la.clone(),
            None => return,
        }
    };
    let log = log_arc.lock().await;
    let state = match log.poll_state(pid) {
        Some(s) => s,
        None => return,
    };
    let t3 = match &state.tier_state {
        crate::community_voting_log::TierState::Tier3(t) => t,
        _ => return,
    };

    // Trigger kd=sf when:
    //   - Stage::Sortition, AND
    //   - decline_count >= primary_size + backup_size, AND
    //   - local node is the proposer.
    if matches!(t3.stage, crate::community_voting_tier3::Stage::Sortition)
        && t3.meta.proposer == self_owner
    {
        let primary_size = t3.meta.primary.len();
        let backup_size = t3.meta.backup.len();
        let decline_count = crate::community_voting_tier3::decline_count_at(t3, /* now_hlc */ Hlc::max());
        if decline_count >= primary_size + backup_size {
            drop(log); // release lock before re-publishing
            let hlc = self.reserve_next_local_hlc().await;
            let sf_ev = match crate::community_voting_core::build_signed_sortition_failed(
                &signing_key, self_owner, *pid, hlc,
            ) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = ?e, "engine-auto build_signed_sortition_failed failed");
                    return;
                }
            };
            if let Err(e) = self.publish_event(sf_ev).await {
                tracing::warn!(error = %e, poll_id = %hex::encode(pid.0), "engine-auto kd=sf publish failed");
            }
        }
    }
}
```

Call `self.maybe_trigger_engine_auto_orchestration(&space_id, &pid)` at the end of `publish_event` after successful apply.

- [ ] **Step 4: Add `reserve_next_local_hlc` to `VotingLogEngine`**

```rust
impl<R: tauri::Runtime> VotingLogEngine<R> {
    pub async fn reserve_next_local_hlc(&self) -> Hlc {
        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        // Use the engine's installed hlc_tracker + device_id (same as IPCs).
        crate::dm_outbox::reserve_next_hlc_for_device(
            self.hlc_tracker.as_ref().expect("hlc_tracker installed"),
            self.device_id.as_ref().expect("device_id installed"),
            wall_now_ms,
        ).await
    }
}
```

- [ ] **Step 5: Run the failing test, then iterate to pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(engine_auto_sf_on_mass_decline_from_proposer)' 2>&1 | tail -30
```

Iterate until the test passes.

- [ ] **Step 6: cargo fmt + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

- [ ] **Step 7: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/community_voting_log_engine.rs src-tauri/tests/community_voting_tier3_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): engine-auto orchestration for kd=sf (SortitionFailed)

Post-apply hook in publish_event: when local node is the proposer AND
decline_count >= primary_size + backup_size on a Stage::Sortition poll,
mint + publish_event a signed kd=sf. install_local_signing_key plumbs
the local signing key onto VotingLogEngine. reserve_next_local_hlc
helper. Multi-engine integration test verifies kd=sf propagates to peer.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Engine-auto orchestration — kd=cl (PollClose)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (extend post-apply hook)
- Modify: `src-tauri/tests/community_voting_tier3_integration.rs`

- [ ] **Step 1: Write the failing multi-engine test**

```rust
#[tokio::test]
async fn engine_auto_cl_when_ratification_window_expires() {
    use harmony_app::community_voting_core::{Tier3PollConfigPayload, Eligibility};
    use harmony_app::community_voting_tier3::Stage;

    let community_id = SpaceId([0xc2; 16]);
    let engines = setup_two_voting_engine_bridge_with_signing(community_id, fixture_identity(0xA1)).await;
    let proposer = fixture_identity(0xA1);
    // ... drive poll through to Stage::Ratification with at least one ballot ...
    // ... advance HLC past created + (delib+draft+ratif) windows ...
    // ... call publish_event of any (innocuous) event to trigger post-apply hook ...
    // Wait for both engines to see Stage::Finalized via auto-kd=cl + auto-kd=rs.
    wait_for(|| async {
        let log_b = engines.log_b.lock().await;
        if let Some(state) = log_b.poll_state(&pid) {
            if let harmony_app::community_voting_log::TierState::Tier3(t3) = &state.tier_state {
                if matches!(t3.stage, Stage::Finalized) {
                    return Some(());
                }
            }
        }
        None
    }, 50, 40).await.expect("kd=cl + kd=rs should propagate to engine_b within 2s");
}
```

- [ ] **Step 2: Extend the engine-auto hook with kd=cl trigger**

```rust
// In maybe_trigger_engine_auto_orchestration, after the kd=sf check:
if matches!(t3.stage, crate::community_voting_tier3::Stage::Ratification)
    && t3.close_event_hash.is_none()
{
    let now_hlc = self.current_hlc_estimate().await;
    let total_window_ms: u64 = (t3.meta.deliberation_window_seconds
        + t3.meta.drafting_window_seconds
        + t3.meta.ratification_window_seconds) as u64 * 1000;
    if now_hlc.wall_ms >= t3.meta.created_hlc.wall_ms + total_window_ms {
        drop(log);
        let hlc = self.reserve_next_local_hlc().await;
        let cl_ev = match crate::community_voting_core::build_signed_poll_close_tier3(
            &signing_key, self_owner, *pid, hlc,
        ) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = ?e, "engine-auto build_signed_poll_close_tier3 failed");
                return;
            }
        };
        if let Err(e) = self.publish_event(cl_ev).await {
            // L1 rejection (already closed) is expected for race losers; not an error.
            tracing::debug!(error = %e, "engine-auto kd=cl publish rejected (race loser?)");
        }
    }
}
```

- [ ] **Step 3: Add `current_hlc_estimate` helper**

```rust
impl<R: tauri::Runtime> VotingLogEngine<R> {
    /// Returns the engine's best estimate of "now" as an HLC. Used for
    /// deadline checks in engine-auto orchestration. Does NOT advance
    /// the tracker (read-only).
    pub async fn current_hlc_estimate(&self) -> Hlc {
        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Hlc { wall_ms: wall_now_ms, logical: 0, device_id: String::new() }
    }
}
```

- [ ] **Step 4: Iterate test until pass + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(engine_auto_cl)' 2>&1 | tail -20
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd ..
git add src-tauri/src/community_voting_log_engine.rs src-tauri/tests/community_voting_tier3_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): engine-auto orchestration for kd=cl (PollClose)

Post-apply hook checks for Stage::Ratification + no close + window
expired; mints + publishes signed kd=cl. L1 rejection on race loser
is expected and logged at debug level. current_hlc_estimate helper.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Engine-auto orchestration — kd=rs (PollResult)

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs`
- Modify: `src-tauri/tests/community_voting_tier3_integration.rs`

- [ ] **Step 1: Write the failing multi-engine test**

```rust
#[tokio::test]
async fn engine_auto_rs_after_cl_with_bit_identical_tally() {
    // Reuse the setup from kd=cl test; verify both engines compute identical scores_summary.
}
```

- [ ] **Step 2: Extend the engine-auto hook with kd=rs trigger**

```rust
// Triggered on apply of kd=cl (within the post-apply hook):
if matches!(t3.stage, crate::community_voting_tier3::Stage::Ratification)
    && t3.close_event_hash.is_some()
    && t3.result.is_none()
{
    drop(log);
    let log_arc = { let map = self.voting_logs.lock().await; map.get(space_id).cloned() };
    let log_arc = match log_arc { Some(la) => la, None => return };
    let log = log_arc.lock().await;
    let state = match log.poll_state(pid) { Some(s) => s, None => return };
    let t3 = match &state.tier_state {
        crate::community_voting_log::TierState::Tier3(t) => t,
        _ => return,
    };

    let candidates = crate::community_voting_tier3::ratification_candidates_ordering(t3);
    let ballots: Vec<crate::community_voting_star::Ballot> =
        crate::community_voting_tier3::collect_ratification_ballots(t3);
    let result = crate::community_voting_star::tally_star(&candidates, &ballots);

    drop(log);
    let hlc = self.reserve_next_local_hlc().await;
    let rs_ev = match crate::community_voting_core::build_signed_poll_result_tier3(
        &signing_key, self_owner, *pid, result, hlc,
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = ?e, "engine-auto build_signed_poll_result_tier3 failed");
            return;
        }
    };
    if let Err(e) = self.publish_event(rs_ev).await {
        tracing::debug!(error = %e, "engine-auto kd=rs publish rejected (race loser?)");
    }
}
```

- [ ] **Step 3: Add `collect_ratification_ballots` to tier3**

```rust
// In community_voting_tier3.rs:
/// Collect all RatificationBallot ballots applied to a Tier 3 poll, in
/// HLC order. Used by engine-auto kd=rs orchestration.
pub fn collect_ratification_ballots(
    t3: &Tier3PollState,
) -> Vec<crate::community_voting_star::Ballot> {
    t3.ratification_ballots
        .iter()
        .map(|(_actor, ballot)| ballot.clone())
        .collect()
}
```

- [ ] **Step 4: Iterate test + cargo fmt + clippy + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(engine_auto_rs)' 2>&1 | tail -20
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd ..
git add src-tauri/src/community_voting_log_engine.rs src-tauri/src/community_voting_tier3.rs src-tauri/tests/community_voting_tier3_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): engine-auto orchestration for kd=rs (PollResult)

Triggered on apply of kd=cl: deterministically tally via tally_star,
mint + publish signed kd=rs. Race-tolerant: first valid by HLC wins,
later rejected by R2. collect_ratification_ballots helper.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Tauri event emit sites for materialize-driven events

**Files:**
- Modify: `src-tauri/src/community_voting_log_engine.rs` (emit voting-tier3-sortition-complete on apply of kd=ss)
- Modify: `src-tauri/src/community_voting_log.rs` (emit voting-tier3-drafting-open / voting-tier3-ratification-open on materialize transitions; emit voting-tier3-finalized on apply of kd=rs)

These events need to fire from the engine layer because `community_voting_log` is library code without `tauri::AppHandle`. Pattern: pass an emit callback into `apply_with_snapshot` OR emit from the engine's post-apply hook (cleaner — same hook as Task 9-11).

- [ ] **Step 1: Extend `maybe_trigger_engine_auto_orchestration` with emit sites**

After successful apply, inspect the applied event's kind + new state and emit the appropriate Tauri event. Engine holds `Option<tauri::AppHandle<R>>` from `start`:

```rust
// In post-apply hook, after orchestration triggers:
let app_handle = match &self.app_handle {
    Some(h) => h.clone(),
    None => return, // test/library mode without Tauri runtime
};

match applied_event.kind {
    PollEventKindCode::SortitionSelection => {
        let primary = ...; let backup = ...;
        let payload = VotingTier3SortitionCompletePayload {
            poll_id: hex::encode(pid.0),
            community_id: hex::encode(space_id.0),
            primary: primary.iter().map(|o| hex::encode(o.0)).collect(),
            backup: backup.iter().map(|o| hex::encode(o.0)).collect(),
        };
        if let Err(e) = app_handle.emit("voting-tier3-sortition-complete", &payload) {
            tracing::warn!(error = %e, "voting-tier3-sortition-complete emit failed");
        }
    }
    PollEventKindCode::PollResult if state.tier_state.is_tier3() => {
        let payload = build_tier3_finalized_payload(&state, &space_id, &pid);
        if let Err(e) = app_handle.emit("voting-tier3-finalized", &payload) {
            tracing::warn!(error = %e, "voting-tier3-finalized emit failed");
        }
    }
    _ => {}
}

// Also detect stage transitions:
if previous_stage == Some(Stage::Deliberation) && new_stage == Some(Stage::Drafting) {
    let payload = VotingTier3DraftingOpenPayload {
        poll_id: hex::encode(pid.0),
        community_id: hex::encode(space_id.0),
    };
    if let Err(e) = app_handle.emit("voting-tier3-drafting-open", &payload) {
        tracing::warn!(error = %e, "voting-tier3-drafting-open emit failed");
    }
}
if previous_stage == Some(Stage::Drafting) && new_stage == Some(Stage::Ratification) {
    let candidate_ordering: Vec<CandidateRefDto> = ...;
    let payload = VotingTier3RatificationOpenPayload {
        poll_id: hex::encode(pid.0),
        community_id: hex::encode(space_id.0),
        candidate_ordering,
    };
    if let Err(e) = app_handle.emit("voting-tier3-ratification-open", &payload) {
        tracing::warn!(error = %e, "voting-tier3-ratification-open emit failed");
    }
}
```

(Need to thread `previous_stage` / `new_stage` from the apply call result; extend `apply_with_snapshot` to return them.)

- [ ] **Step 2: Extend `apply_with_snapshot` return type**

In `community_voting_log.rs`, change the return type to include the lifecycle/stage delta:

```rust
pub struct ApplyOutcome {
    pub poll_id: PollId,
    pub previous_stage: Option<crate::community_voting_tier3::Stage>,
    pub new_stage: Option<crate::community_voting_tier3::Stage>,
}

impl VotingLog {
    pub fn apply_with_snapshot(/* ... */) -> Result<ApplyOutcome, ApplyError> { /* ... */ }
}
```

Update all call sites accordingly (IPCs, engine, tests).

- [ ] **Step 3: Write a test for each emit site**

Use `tauri::test::MockRuntime` and `app_handle.listen` to verify each event fires once when expected.

- [ ] **Step 4: cargo fmt + clippy + nextest scoped + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(tier3.*emit)' 2>&1 | tail -20
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd ..
git add src-tauri/src/community_voting_log.rs src-tauri/src/community_voting_log_engine.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-310): emit Tier 3 Tauri events from engine post-apply hook

voting-tier3-sortition-complete on apply of kd=ss; voting-tier3-finalized
on apply of kd=rs; voting-tier3-drafting-open + voting-tier3-ratification-open
on materialize stage transitions. apply_with_snapshot now returns ApplyOutcome
with previous + new stage. All emit failures are non-fatal (tracing::warn).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: Frontend `voting-adapter.ts` + types

**Files:**
- Modify: `src/lib/types/voting.ts` (add 5 typedefs + CreateTier3ProposalArgs + 2 DTOs)
- Modify: `src/lib/voting-adapter.ts` (6 IPC methods + 5 subscribers + connectAdapter wiring)

- [ ] **Step 1: Add types**

```typescript
// src/lib/types/voting.ts (add to existing exports):

export interface CreateTier3ProposalArgs {
  communityId: string;
  channelId: string;
  proposalText: string;
  sortitionSize: number;
  deliberationWindowSeconds: number;
  draftingWindowSeconds: number;
  ratificationWindowSeconds: number;
  incentiveMode: 'se' | 'ab' | 'co' | 'dp';
  minPower: number;
  minVouchingDepth?: number;
  retryOf?: string;
}

export interface VotingTier3PollCreatedPayload {
  pollId: string;
  channelId: string;
  communityId: string;
  proposer: string;
  sortitionSize: number;
  deliberationWindowSeconds: number;
  draftingWindowSeconds: number;
  ratificationWindowSeconds: number;
}

export interface VotingTier3SortitionCompletePayload {
  pollId: string;
  communityId: string;
  primary: string[];
  backup: string[];
}

export interface VotingTier3DraftingOpenPayload {
  pollId: string;
  communityId: string;
}

export interface CandidateRef {
  eventHash: string;
  text: string;
  approvalCount: number;
}

export interface VotingTier3RatificationOpenPayload {
  pollId: string;
  communityId: string;
  candidateOrdering: CandidateRef[];
}

export interface CandidateScore {
  eventHash: string;
  totalScore: number;
  runoffVotes: number;
}

export interface VotingTier3FinalizedPayload {
  pollId: string;
  communityId: string;
  winnerEventHash: string;
  winnerText: string;
  runnerUpEventHash?: string;
  scoresSummary: CandidateScore[];
}
```

- [ ] **Step 2: Extend `voting-adapter.ts`**

Add subscriber-list fields (matching the existing Tier 1 pattern at line 90):

```typescript
// In VotingAdapter class:
private tier3PollCreatedSubs: Array<(p: VotingTier3PollCreatedPayload) => void> = [];
private tier3SortitionCompleteSubs: Array<(p: VotingTier3SortitionCompletePayload) => void> = [];
private tier3DraftingOpenSubs: Array<(p: VotingTier3DraftingOpenPayload) => void> = [];
private tier3RatificationOpenSubs: Array<(p: VotingTier3RatificationOpenPayload) => void> = [];
private tier3FinalizedSubs: Array<(p: VotingTier3FinalizedPayload) => void> = [];
```

Add 5 `subscribeXxx` methods (mirror the existing `subscribePollCreated` pattern at line 107).

Add 6 IPC wrapper methods (mirror `createTier1Poll` at line 347):

```typescript
async createTier3Proposal(args: CreateTier3ProposalArgs): Promise<string> {
  return this.invoke<string>('voting_create_tier3_proposal', {
    communityId: args.communityId,
    channelId: args.channelId,
    proposalText: args.proposalText,
    sortitionSize: args.sortitionSize,
    deliberationWindowSeconds: args.deliberationWindowSeconds,
    draftingWindowSeconds: args.draftingWindowSeconds,
    ratificationWindowSeconds: args.ratificationWindowSeconds,
    incentiveMode: args.incentiveMode,
    minPower: args.minPower,
    minVouchingDepth: args.minVouchingDepth,
    retryOf: args.retryOf,
  });
}

async submitDeliberationStatement(pollId: string, text: string): Promise<string> {
  return this.invoke<string>('voting_submit_deliberation_statement', { pollId, text });
}

async proposeDraftCandidate(pollId: string, candidateText: string): Promise<string> {
  return this.invoke<string>('voting_propose_draft_candidate', { pollId, candidateText });
}

async approveDraftCandidate(pollId: string, candidateEventHash: string): Promise<void> {
  await this.invoke<void>('voting_approve_draft_candidate', { pollId, candidateEventHash });
}

async declineSortition(pollId: string, reason?: string): Promise<void> {
  await this.invoke<void>('voting_decline_sortition', { pollId, reason });
}

async castRatificationBallot(pollId: string, scores: number[]): Promise<void> {
  await this.invoke<void>('voting_cast_ratification_ballot', { pollId, scores });
}
```

Extend `connectAdapter` to wire 5 new event listeners (mirror the staged-unlisteners pattern at line 216):

```typescript
const unlistenTier3PollCreated = await adapter.listen(
  'voting-tier3-poll-created',
  (event) => {
    const payload = event.payload as VotingTier3PollCreatedPayload;
    for (const sub of [...this.tier3PollCreatedSubs]) sub(payload);
  },
);
stagedUnlisteners.push(unlistenTier3PollCreated);

// ... same shape for sortition-complete, drafting-open, ratification-open, finalized
```

- [ ] **Step 3: Run tsc**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit 2>&1 | tail -10
```

Expected: zero output.

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/types/voting.ts src/lib/voting-adapter.ts
git commit -m "$(cat <<'EOF'
feat(zeb-310): extend voting-adapter.ts with 6 Tier 3 IPCs + 5 subscribers

CreateTier3ProposalArgs + 5 payload typedefs + 2 DTOs (CandidateRef,
CandidateScore) in types/voting.ts. 6 IPC wrapper methods + 5 subscriber
methods + connectAdapter event wiring in voting-adapter.ts. Splice-on-
unsubscribe pattern matches Tier 1 + Tier 2.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 14: vitest unit tests for `voting-adapter.ts` Tier 3 surface

**Files:**
- Create: `src/lib/__tests__/voting-adapter-tier3.test.ts`

- [ ] **Step 1: Write the test file**

```typescript
import { describe, it, expect, vi } from 'vitest';
import { VotingAdapter } from '../voting-adapter';
import type { TauriAdapter } from '../zenoh-service';
import type {
  VotingTier3PollCreatedPayload,
  VotingTier3SortitionCompletePayload,
  VotingTier3DraftingOpenPayload,
  VotingTier3RatificationOpenPayload,
  VotingTier3FinalizedPayload,
} from '../types/voting';

function makeMockAdapter(): {
  adapter: TauriAdapter;
  invoke: ReturnType<typeof vi.fn>;
  emit: (event: string, payload: unknown) => void;
} {
  const listeners = new Map<string, Array<(e: { payload: unknown }) => void>>();
  const invoke = vi.fn();
  const adapter: TauriAdapter = {
    invoke,
    listen: async (event, handler) => {
      const list = listeners.get(event) ?? [];
      list.push(handler as (e: { payload: unknown }) => void);
      listeners.set(event, list);
      return () => {
        const cur = listeners.get(event);
        if (cur) {
          const i = cur.indexOf(handler as (e: { payload: unknown }) => void);
          if (i >= 0) cur.splice(i, 1);
        }
      };
    },
  };
  return {
    adapter,
    invoke,
    emit: (event, payload) => {
      const list = listeners.get(event) ?? [];
      for (const h of list) h({ payload });
    },
  };
}

describe('VotingAdapter Tier 3 IPC wrappers', () => {
  it('createTier3Proposal invokes with camelCase params', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue('aa'.repeat(32));
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const pid = await va.createTier3Proposal({
      communityId: 'bb'.repeat(16),
      channelId: 'cc'.repeat(16),
      proposalText: 'test',
      sortitionSize: 20,
      deliberationWindowSeconds: 600,
      draftingWindowSeconds: 600,
      ratificationWindowSeconds: 600,
      incentiveMode: 'dp',
      minPower: 0,
    });
    expect(pid).toBe('aa'.repeat(32));
    expect(invoke).toHaveBeenCalledWith('voting_create_tier3_proposal', expect.objectContaining({
      communityId: 'bb'.repeat(16),
      sortitionSize: 20,
      incentiveMode: 'dp',
    }));
  });

  it('submitDeliberationStatement returns event_hash', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue('dd'.repeat(32));
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const eh = await va.submitDeliberationStatement('aa'.repeat(32), 'my view');
    expect(eh).toBe('dd'.repeat(32));
  });

  it('proposeDraftCandidate returns candidate_event_hash', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue('ee'.repeat(32));
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const eh = await va.proposeDraftCandidate('aa'.repeat(32), 'option A');
    expect(eh).toBe('ee'.repeat(32));
  });

  it('approveDraftCandidate returns void', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue(null);
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const r = await va.approveDraftCandidate('aa'.repeat(32), 'bb'.repeat(32));
    expect(r).toBeUndefined();
  });

  it('declineSortition passes optional reason', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue(null);
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    await va.declineSortition('aa'.repeat(32), 'u1');
    expect(invoke).toHaveBeenCalledWith('voting_decline_sortition', { pollId: 'aa'.repeat(32), reason: 'u1' });
  });

  it('castRatificationBallot sends scores array', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockResolvedValue(null);
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    await va.castRatificationBallot('aa'.repeat(32), [5, 4, 0, 3]);
    expect(invoke).toHaveBeenCalledWith('voting_cast_ratification_ballot', { pollId: 'aa'.repeat(32), scores: [5, 4, 0, 3] });
  });

  it('extracts string rejection error with command prefix', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockRejectedValue('eligibility failed');
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    await expect(va.declineSortition('aa'.repeat(32))).rejects.toThrow(/voting_decline_sortition failed: eligibility failed/);
  });

  it('extracts Error.message with command prefix', async () => {
    const { adapter, invoke } = makeMockAdapter();
    invoke.mockRejectedValue(new Error('Error: snapshot missing'));
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    await expect(va.proposeDraftCandidate('aa'.repeat(32), 'x')).rejects.toThrow(/voting_propose_draft_candidate failed: Error: snapshot missing/);
  });
});

describe('VotingAdapter Tier 3 event subscribers', () => {
  it('subscribeTier3PollCreated receives payload', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3PollCreatedPayload[] = [];
    va.subscribeTier3PollCreated((p) => seen.push(p));
    emit('voting-tier3-poll-created', {
      pollId: 'aa'.repeat(32),
      channelId: 'bb'.repeat(16),
      communityId: 'cc'.repeat(16),
      proposer: 'dd'.repeat(16),
      sortitionSize: 20,
      deliberationWindowSeconds: 600,
      draftingWindowSeconds: 600,
      ratificationWindowSeconds: 600,
    });
    expect(seen).toHaveLength(1);
    expect(seen[0]?.sortitionSize).toBe(20);
  });

  it('subscribeTier3SortitionComplete fires once and unsubscribe removes', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3SortitionCompletePayload[] = [];
    const unsub = va.subscribeTier3SortitionComplete((p) => seen.push(p));
    emit('voting-tier3-sortition-complete', { pollId: 'aa'.repeat(32), communityId: 'bb'.repeat(16), primary: [], backup: [] });
    unsub();
    emit('voting-tier3-sortition-complete', { pollId: 'aa'.repeat(32), communityId: 'bb'.repeat(16), primary: ['x'], backup: ['y'] });
    expect(seen).toHaveLength(1);
  });

  it('subscribeTier3DraftingOpen receives transition', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3DraftingOpenPayload[] = [];
    va.subscribeTier3DraftingOpen((p) => seen.push(p));
    emit('voting-tier3-drafting-open', { pollId: 'aa'.repeat(32), communityId: 'bb'.repeat(16) });
    expect(seen).toHaveLength(1);
  });

  it('subscribeTier3RatificationOpen exposes candidate ordering', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3RatificationOpenPayload[] = [];
    va.subscribeTier3RatificationOpen((p) => seen.push(p));
    emit('voting-tier3-ratification-open', {
      pollId: 'aa'.repeat(32),
      communityId: 'bb'.repeat(16),
      candidateOrdering: [
        { eventHash: 'cc'.repeat(32), text: 'A', approvalCount: 3 },
        { eventHash: 'dd'.repeat(32), text: 'B', approvalCount: 1 },
      ],
    });
    expect(seen[0]?.candidateOrdering).toHaveLength(2);
  });

  it('subscribeTier3Finalized exposes scoresSummary', async () => {
    const { adapter, emit } = makeMockAdapter();
    const va = new VotingAdapter();
    await va.connectAdapter(adapter);
    const seen: VotingTier3FinalizedPayload[] = [];
    va.subscribeTier3Finalized((p) => seen.push(p));
    emit('voting-tier3-finalized', {
      pollId: 'aa'.repeat(32),
      communityId: 'bb'.repeat(16),
      winnerEventHash: 'cc'.repeat(32),
      winnerText: 'Winner!',
      scoresSummary: [{ eventHash: 'cc'.repeat(32), totalScore: 10, runoffVotes: 5 }],
    });
    expect(seen[0]?.winnerText).toBe('Winner!');
  });
});
```

- [ ] **Step 2: Run vitest**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx vitest run src/lib/__tests__/voting-adapter-tier3.test.ts 2>&1 | tail -20
```

Expected: 13 tests pass.

- [ ] **Step 3: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src/lib/__tests__/voting-adapter-tier3.test.ts
git commit -m "$(cat <<'EOF'
test(zeb-310): vitest unit coverage for voting-adapter Tier 3 surface

8 IPC wrapper tests (incl. 2 error-extraction cases for string + Error)
+ 5 subscriber tests (incl. unsubscribe + payload structure assertions).
Mock TauriAdapter shared via local helper.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 15: IPC-driven integration tests

**Files:**
- Create: `src-tauri/tests/community_voting_tier3_ipc_integration.rs`

- [ ] **Step 1: Write the file scaffold + 5 test cases**

```rust
//! ZEB-310 — Multi-engine integration tests driving the Tier 3 lifecycle
//! through the Tauri command layer (not direct engine calls). Verifies the
//! IPC surface is functionally equivalent to direct publish_event calls.

use harmony_app::community_voting_core::{Eligibility, PollId, Tier3PollConfigPayload};
use harmony_app::community_voting_tier3::Stage;
use harmony_app::owner_state_types::SpaceId;
use std::sync::Arc;
use tokio::sync::Mutex;

mod common {
    // Re-use the TwoVotingEngines + fixture_identity from community_voting_tier3_integration
    // via the integration-test crate convention. Tests in this file extend that fixture
    // with `tauri::test::mock_builder` + `tauri::test::MockRuntime` for IPC invocation.
    include!("community_voting_tier3_integration.rs");
}

#[tokio::test]
async fn ipc_tier3_full_lifecycle_two_engines() {
    // Drive: voting_create_tier3_proposal → multiple voting_decline_sortition →
    // voting_propose_draft_candidate + voting_approve_draft_candidate →
    // voting_cast_ratification_ballot → wait for engine-auto kd=cl + kd=rs.
    // Verify both engines converge on identical winner_event_hash.
    // (Full implementation pattern: ~150 LOC)
    todo!("implement via tauri::test::mock_app + invoke + wait_for");
}

#[tokio::test]
async fn ipc_tier3_engine_auto_kd_sf_on_mass_decline() {
    // Drive: create poll → inject ss → all decline via voting_decline_sortition →
    // wait for engine-auto kd=sf → assert both engines see Stage::Failed.
    todo!();
}

#[tokio::test]
async fn ipc_tier3_engine_auto_kd_cl_kd_rs_race_tolerant() {
    // Drive both engines to Stage::Ratification with ballots → advance HLC past
    // ratification window on both → trigger post-apply on both (innocuous event) →
    // assert kd=cl arrives on both with bit-identical kd=rs scores_summary.
    todo!();
}

#[tokio::test]
async fn ipc_tier3_retry_of_via_ipc() {
    // Drive: poll A fails (kd=sf) → voting_create_tier3_proposal with retry_of=A →
    // verify poll B applies + retry_of field carries through.
    todo!();
}

#[tokio::test]
async fn ipc_tier3_error_extraction_string_and_error() {
    // Call voting_decline_sortition with invalid hex → expect Err(String).
    // Call voting_create_tier3_proposal with empty community_id → expect Err(String).
    todo!();
}
```

(Note: each `todo!()` is filled in during implementation; the plan-shape gives the implementer the exact contract.)

- [ ] **Step 2: Run nextest scoped**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(ipc_tier3)' 2>&1 | tail -20
```

Expected: 5 tests pass.

- [ ] **Step 3: cargo fmt + clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/community_voting_tier3_ipc_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-310): IPC-driven multi-engine integration tests

5 test cases: full E2E lifecycle via IPCs, engine-auto kd=sf, race-tolerant
kd=cl + kd=rs, retry_of chain, error extraction. Uses tauri::test::MockRuntime
+ shared TwoVotingEngines fixture from community_voting_tier3_integration.rs.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 16: Pin `tier3_poll_close.cbor` wire fixture

**Files:**
- Create: `src-tauri/tests/fixtures/voting_tier3/tier3_poll_close.cbor`
- Modify: `src-tauri/tests/wire_format_voting_tier3_fixtures.rs`

- [ ] **Step 1: Add the fixture-pin test (regen-on-first-run)**

In `src-tauri/tests/wire_format_voting_tier3_fixtures.rs`:

```rust
#[test]
fn tier3_poll_close_fixture_matches() {
    use harmony_app::community_voting_core::{PollId, build_signed_poll_close_tier3};
    use harmony_app::community_dfrost_crypto::deterministic_signing_key;
    use harmony_app::community_voting_core::Hlc;
    use harmony_app::owner_state_types::OwnerAddr;

    let keypair = deterministic_signing_key(b"tier3_poll_close");
    let actor = OwnerAddr([0x42; 16]);
    let pid = PollId([0x7f; 32]);
    let hlc = Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "fix".into() };
    let ev = build_signed_poll_close_tier3(&keypair, actor, pid, hlc).expect("build");

    let mut got = Vec::new();
    ciborium::ser::into_writer(&ev, &mut got).expect("encode");

    let path = std::path::Path::new("tests/fixtures/voting_tier3/tier3_poll_close.cbor");
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(path, &got).expect("write fixture");
        panic!("REGENERATE_FIXTURE: wrote new fixture at {path:?}; re-run test to pin");
    }

    let expected = std::fs::read(path).expect("read fixture");
    assert_eq!(got, expected, "tier3_poll_close.cbor wire-format drift");

    // Same-length-keys invariant: payload is a CBOR map with single key "pi".
    let value: ciborium::Value = ciborium::de::from_reader(&ev.payload[..]).expect("decode payload");
    if let ciborium::Value::Map(entries) = value {
        for (k, _) in &entries {
            if let ciborium::Value::Text(s) = k {
                assert_eq!(s.len(), 2, "wire-format key {s:?} violates same-length-2 invariant");
            } else {
                panic!("non-text key in PollClosePayload");
            }
        }
    } else {
        panic!("PollClosePayload payload is not a CBOR map");
    }
}
```

- [ ] **Step 2: Run the test (first run regenerates + panics)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(tier3_poll_close_fixture_matches)' 2>&1 | tail -20
```

Expected: REGENERATE_FIXTURE panic on first run; second run passes.

- [ ] **Step 3: Run again to pin**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(tier3_poll_close_fixture_matches)' 2>&1 | tail -10
```

Expected: pass.

- [ ] **Step 4: Verify pre-existing fixtures (sf + rs) still match**

```bash
cargo nextest run --locked --features test-fixtures -E 'test(sortition_failed_fixture_matches) | test(tier3_poll_result_fixture_matches)' 2>&1 | tail -15
```

Expected: both still pass (engine-auto producers use the same `build_signed_*` constructors).

- [ ] **Step 5: cargo fmt + clippy + commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd ..
git add src-tauri/tests/wire_format_voting_tier3_fixtures.rs src-tauri/tests/fixtures/voting_tier3/tier3_poll_close.cbor
git commit -m "$(cat <<'EOF'
test(zeb-310): pin tier3_poll_close.cbor wire fixture

Regen-on-first-run pattern (matches existing voting_tier3 fixtures).
Asserts same-length-keys (2-char) invariant via ciborium::Value
structural check on the PollClosePayload "pi" key.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 17: Final 5-gate sweep + push + PR creation

**Files:** none (verification + PR creation)

- [ ] **Step 1: Full 5-gate sweep**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -20
cd ..
npx tsc --noEmit
npx vitest run 2>&1 | tail -20
```

Expected:
- fmt: zero output
- clippy: zero warnings
- nextest: all tests pass except known pre-existing orphans from ZEB-302/306/308 (~27)
- tsc: zero output
- vitest: all tests pass

- [ ] **Step 2: Sanity-check git state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git log --oneline origin/main..HEAD
git diff --stat origin/main..HEAD
git status
```

Expected:
- ~17 commits on top of `0902ff2` (1 spec + 16 implementation/test)
- Working tree clean
- Branch tracks origin if pushed; otherwise unset

- [ ] **Step 3: Push branch**

```bash
git push -u origin zeb-310-phase4a-main-ipcs
```

- [ ] **Step 4: Create PR via gh**

```bash
gh pr create --title "ZEB-310 Phase 4a-main: IPCs + engine-auto orchestration + frontend lib" --body "$(cat <<'EOF'
## Summary

Ships the IPC + frontend TypeScript surface for Tier 3 governance polls on top of [ZEB-309](https://linear.app/zeblith/issue/ZEB-309) backend mechanism (merged in #148), so [ZEB-311](https://linear.app/zeblith/issue/ZEB-311) UI can drive Tier 3 polls end-to-end.

- 6 Tauri IPC commands (5 from ticket + `voting_approve_draft_candidate`)
- 5 Tauri events with camelCase payloads
- 9 `pub fn` signed-event builders relocated from test fixtures to `community_voting_core.rs`
- 3 engine-auto-orchestration paths (kd=sf / kd=cl / kd=rs) — race-tolerant by HLC LWW
- Extended `voting-adapter.ts` with 6 IPC methods + 5 subscriber methods
- New IPC-driven integration test file (5 test cases)
- 1 new wire fixture (`tier3_poll_close.cbor`); 2 pre-existing re-verified

## Test plan

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [x] `npx tsc --noEmit`
- [x] `npx vitest run`
- [x] Multi-engine integration tests: kd=sf on mass-decline, kd=cl + kd=rs race-tolerance, full E2E lifecycle via IPCs
- [x] vitest unit coverage for 6 IPC wrappers + 5 subscribers + error extraction

## References

- Spec: [`docs/specs/2026-05-20-zeb-310-phase4a-main-ipcs-design.md`](docs/specs/2026-05-20-zeb-310-phase4a-main-ipcs-design.md)
- Plan: [`docs/plans/2026-05-20-zeb-310-phase4a-main-ipcs-plan.md`](docs/plans/2026-05-20-zeb-310-phase4a-main-ipcs-plan.md)
- Backend dependency: [ZEB-309](https://linear.app/zeblith/issue/ZEB-309) (PR #148 / commit `0902ff2`)
- Umbrella: [ZEB-289](https://linear.app/zeblith/issue/ZEB-289) §6.7
- Downstream UI: [ZEB-311](https://linear.app/zeblith/issue/ZEB-311)

Closes [ZEB-310](https://linear.app/zeblith/issue/ZEB-310)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Capture the PR URL**

```bash
gh pr view --json url -q '.url'
```

Return the URL to the calling agent. The autonomous bot-review monitoring loop takes over from here.

---

## Self-review

**Spec coverage:** every section of the spec maps to at least one task — IPCs (Tasks 3-8), events (Task 2 + Task 12), engine-auto (Tasks 9-11), frontend lib (Tasks 13-14), integration tests (Task 15), wire fixture (Task 16), gate sweep + PR (Task 17). Task 1 covers the underlying builder relocation that all of the above depend on.

**Placeholder scan:** Task 15 uses `todo!()` macros as sketches — the implementer subagent must replace these with real test bodies. Task 12 has a few `...` placeholders in code blocks (e.g., `let primary = ...`); these are spots where the implementer reads the applied event payload and extracts fields. Adequate for an implementer who has the spec; not adequate if used outside context.

**Type consistency:** all helper names (`event_hash_of`, `has_poll`, `tier3_ratification_candidate_count`, `validate_decline_reason`, `install_local_signing_key`, `reserve_next_local_hlc`, `current_hlc_estimate`, `maybe_trigger_engine_auto_orchestration`, `collect_ratification_ballots`, `voting_resolve_community_for_poll`) appear consistently across tasks. Payload struct names (`VotingTier3*Payload`, `CandidateRefDto`, `CandidateScoreDto`) are stable. IPC names match the spec verbatim. Frontend method names use camelCase consistently.
