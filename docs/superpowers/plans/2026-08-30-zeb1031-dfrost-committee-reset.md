# ZEB-1031 D-FROST Committee-Reset Ceremony Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Governance-gated deactivation + re-DKG of a D-FROST committee under a new joint vk, with a membership-anchored vk-provenance chain, per the approved spec.

**Architecture:** Hybrid two-log design. The membership log carries the governance track — three new `MembershipEventKind` variants (`o`/`w`/`z`: proposal, cosign, threshold-signed committee response) with a pure derived lifecycle evaluator mirroring the ZEB-212 recovery family. The dfrost log gets one new committee-event kind (`rs`, the reset marker) whose apply performs deactivation, archives `vk_history`, and pins the successor DKG; membership state is the provenance anchor that joiner/straggler adoption verifies against.

**Tech Stack:** Rust (src-tauri workspace, `harmony-app` crate), canonical CBOR wire discipline, frost-ristretto255 group-signature verification, Svelte 5 frontend, headless `api`/`serve` e2e harness.

**Spec:** `docs/superpowers/specs/2026-08-30-zeb1031-dfrost-committee-reset-design.md` — read it before any task; every gate/phase name below is defined there. The anchor survey (line refs at main `36f470c1`) is summarized in the spec's §1 references.

## Global Constraints

- Wire invariants: 1-char variant tag **values**, 2-char inner-field keys, canonical CBOR, `bstr` byte serializers (`serialize_bytes_as_bstr`/`deserialize_bytes_from_bstr`). Never edit an existing byte-pin fixture — add new pins only.
- Constants (spec §8): `RESET_VETO_WINDOW_FLOOR_MS = 24 * 3_600_000`, `RESET_VETO_WINDOW_CEILING_MS = 30 * 86_400_000`, `RESET_FINALITY_MS = 48 * 3_600_000`, `RESET_AUTHORIZED_LAPSE_MS = 30 * 86_400_000`; proposal expiry reuses `ADMIN_PROPOSAL_EXPIRY_MS`.
- Domain tags (spec §3.3): `"harmony-dfrost-reset-endorse-v1"`, `"harmony-dfrost-reset-veto-v1"`, `"harmony-dfrost-reset-consumed-v1"`.
- All cargo commands run from `src-tauri/`; `scripts/test-select` runs from the repo root. Always `--locked --features test-fixtures`; clippy with `--all-targets`.
- Gate ladder per task: `cargo fmt --all` → targeted nextest (`--lib -E 'test(<module>)'`) → task gate `scripts/test-select --context task`. Full sweep is the final task only.
- Line refs in this plan are at main `36f470c1` and may drift a few lines — anchor by symbol name + compiler, not by line count.
- Commit trailers on every commit:
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` and
  `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`
- Branch: `zeblith/zeb-1031-dfrost-committee-reset` off latest `origin/main`, in the main repo (no worktrees).
- Verifier-mirror discipline: any check enforced at live ingest MUST be enforced identically on the catch-up/adoption path (and vice versa) — divergence here is the ZEB-1030 Tier-3 lesson.

## File structure

| File | Responsibility in this plan |
|---|---|
| `src-tauri/src/community_membership.rs` | `o`/`w`/`z` variants, `ResetVerdict`, digest fn, constants, verify gates, `ResetPhase`/`ResetProposalView`, `evaluate_reset_phases`, materialize arms, `reset_rejected_vks` helper |
| `src-tauri/src/community_dfrost_types.rs` | `ResetMarkerPayload` (`rs` kind), `VkLineageEntry`, `PendingReset` |
| `src-tauri/src/community_dfrost_log.rs` | `apply_reset_marker`, `CommitteeState.vk_history`/`pending_reset`, successor-DKG pin, adoption rejected-vk check |
| `src-tauri/src/community_dfrost_log_engine.rs` | membership-side marker admissibility (RS-M3/M4/M5), auto-drive (marker + `c` response), reset-purpose sign ceremonies, catch-up reset chain |
| `src-tauri/src/community_dfrost_catchup.rs` | catch-up response `reset_chain` field + serve side |
| `src-tauri/src/community_voting_tier3.rs` + `community_voting_log_engine.rs` | `Voided` poll disposition + void-on-reset hook + relaunch |
| `src-tauri/src/api/rpc.rs` + `src-tauri/src/lib.rs` | six IPCs (spec §9) |
| `src-tauri/tests/wire_format/…` | new byte pins for `o`/`w`/`z` and `rs` |
| `src/lib/components/community/…` (Svelte) | admin reset panel + voided-poll banner |
| `src-tauri/tests/…` (integration) | two-node e2e flows |

---

### Task 1: Membership wire events, digest, constants, verify gates

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (variants after `RecoveryVeto` ~`:502-510`; constants near `RECOVERY_VETO_WINDOW_FLOOR_MS` ~`:5750`; verify arms after `RecoveryVeto`'s ~`:4677`; `VerifyError` variants near the recovery ones ~`:1062-1097`)
- Test: inline `#[cfg(test)]` in the same file (module `reset_verify_tests`), plus new byte pins in the wire-format fixture file that pins `MembershipEventKind` (locate with `grep -rn "RecoveryProposal" src-tauri/tests/wire_format/` and follow that file's pattern exactly)

**Interfaces:**
- Consumes: existing `MembershipEventKind`, `EventId`, `OwnerAddr`, `SpaceId`, `Hlc`, `canonical_cbor_encode`, `is_joined_member`, bstr serializers.
- Produces (later tasks rely on these exact names):
  - `MembershipEventKind::DfrostResetProposal { target_vk: [u8;32], target_epoch: u64, new_members: Vec<OwnerAddr>, new_threshold: u16, veto_window_ms: u64 }` (tag `"o"`, keys `tv`/`te`/`nm`/`nt`/`vw`)
  - `MembershipEventKind::DfrostResetCosign { target_event_id: EventId }` (tag `"w"`, key `ti`)
  - `MembershipEventKind::DfrostResetResponse { target_event_id: EventId, verdict: ResetVerdict, group_sig: [u8;64], new_vk: Option<[u8;32]> }` (tag `"z"`, keys `ti`/`vd`/`sg`/`nv` with `nv` skip-if-none)
  - `pub enum ResetVerdict { Endorse, Veto, Consumed }` (1-char codes `"e"`/`"v"`/`"c"`, `Copy`)
  - `pub fn dfrost_reset_digest(space_id: &SpaceId, proposal_id: &EventId, target_vk: &[u8;32], target_epoch: u64, new_members: &[OwnerAddr], new_threshold: u16) -> Result<[u8;32], CryptoError>`
  - `pub fn dfrost_reset_message_hash(domain: &'static str, digest: &[u8;32], new_vk: Option<&[u8;32]>) -> [u8;32]`
  - Constants: `RESET_VETO_WINDOW_FLOOR_MS`, `RESET_VETO_WINDOW_CEILING_MS`, `RESET_FINALITY_MS`, `RESET_AUTHORIZED_LAPSE_MS`, `DFROST_RESET_ENDORSE_DOMAIN`, `DFROST_RESET_VETO_DOMAIN`, `DFROST_RESET_CONSUMED_DOMAIN`

- [ ] **Step 1: Write failing verify-gate tests.** In a new `mod reset_verify_tests` next to the existing recovery verify tests (find them with `grep -n "RecoveryProposalActorNotDesignate" src-tauri/src/community_membership.rs` and copy their fixture-building helpers). Cover, asserting the EXACT error variant (intent-pinning discipline — never bare `is_err`):
  - RS-P1: non-admin proposer → `DfrostResetProposalActorNotAdmin`; power-100 proposer passes.
  - RS-P2: unsorted / duplicated / len-1 `new_members`, and a member not Joined at the proposal HLC → `DfrostResetProposalBadMembers`.
  - RS-P3: `new_threshold` 1 and `new_members.len()+1` → `DfrostResetProposalBadThreshold`.
  - RS-P4: `veto_window_ms` floor−1 and ceiling+1 → `DfrostResetProposalBadWindow`; exactly floor and ceiling pass.
  - RS-P5: proposer with an open (Collecting) reset proposal → `DfrostResetProposalActorHasOpenProposal`. (This gate reads the derived view from Task 2; in THIS task assert the variant exists and the gate compiles against an empty view — the behavioural test lands in Task 2.)
  - RS-C1: non-admin cosigner → `DfrostResetCosignActorNotAdmin`; zero target id → `DfrostResetCosignTargetIdMalformed`.
  - RS-R1: `Consumed` verdict from an actor not in the target proposal's `new_members` → `DfrostResetResponseActorNotEligible`; `Endorse`/`Veto` from any Joined member passes the actor gate.
  - RS-R3: bad `group_sig` → `DfrostResetResponseSigInvalid` (build a valid sig with a locally-generated Schnorr keypair over the domain-tagged message; the verification helper is the same one `apply_vrf_beacon` uses at `community_dfrost_log.rs:2191-2200` — if it is not already a reusable `pub(crate) fn`, extract it as `community_dfrost_crypto::verify_group_signature(vk: &[u8;32], message: &[u8;32], sig: &[u8;64]) -> bool` and point the beacon path at it too).
  - RS-R4: `new_vk` present with `Endorse` / absent with `Consumed` → `DfrostResetResponseShapeInvalid`.
- [ ] **Step 2: Run to verify failure.** `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(reset_verify_tests)'` — expect compile failure (variants don't exist yet).
- [ ] **Step 3: Implement.** Add the three variants (mirror `RecoveryProposal`'s exact serde attribute style at `:458-509`, including doc comments citing spec §3.1–3.3), `ResetVerdict` (mirror `RecoveryPhase`'s 1-char-code style at `:2011`), the four constants (next to `RECOVERY_VETO_WINDOW_FLOOR_MS` `:5750`), the three domain-tag consts, and:

```rust
/// ZEB-1031 §3.3: the binding digest every reset response and marker
/// verifies. Recomputed by verifiers, never trusted from the wire.
/// Fail-closed on encode error (recovery_config_digest discipline).
pub fn dfrost_reset_digest(
    space_id: &SpaceId,
    proposal_id: &EventId,
    target_vk: &[u8; 32],
    target_epoch: u64,
    new_members: &[OwnerAddr],
    new_threshold: u16,
) -> Result<[u8; 32], CryptoError> {
    #[derive(Serialize)]
    struct ResetDigestInput<'a> {
        #[serde(rename = "sp")]
        space_id: &'a SpaceId,
        #[serde(rename = "pi", serialize_with = "serialize_bytes_as_bstr")]
        proposal_id: &'a EventId,
        #[serde(rename = "tv", serialize_with = "serialize_bytes_as_bstr")]
        target_vk: &'a [u8; 32],
        #[serde(rename = "te")]
        target_epoch: u64,
        #[serde(rename = "nm")]
        new_members: &'a [OwnerAddr],
        #[serde(rename = "nt")]
        new_threshold: u16,
    }
    let bytes = canonical_cbor_encode(&ResetDigestInput {
        space_id, proposal_id, target_vk, target_epoch, new_members, new_threshold,
    })?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

/// ZEB-1031 §3.3: 32-byte message handed to the threshold-sign ceremony
/// for a reset response. `new_vk` is appended for the consumed domain only.
pub fn dfrost_reset_message_hash(
    domain: &'static str,
    digest: &[u8; 32],
    new_vk: Option<&[u8; 32]>,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(domain.as_bytes());
    h.update(digest);
    if let Some(nv) = new_vk {
        h.update(nv);
    }
    *h.finalize().as_bytes()
}
```

  Verify arms (in `verify_event`'s match, after the `RecoveryVeto` arm `:4677`): RS-P1 power-100 check mirrors the `RecoveryVeto` power lookup (`:4684-4688`, WITHOUT the bootstrap exception — reset proposers are ordinary live admins); RS-P2 mirrors `check_ceremony_init_admissible`'s sorted/dedup/len logic (`community_dfrost_log.rs:1606`) plus per-member `is_joined_member`; RS-P4 clamps against the two constants; RS-R3 recomputes the digest via `dfrost_reset_digest` from the target proposal's fields — **lenient forward-ref**: when the target proposal is not yet in the log, skip RS-R3/R1's proposal-dependent halves (materialize re-checks; mirrors `RecoveryCosign`'s comment `:4659-4675`). For `group_sig`'s `[u8; 64]` serde, use the same serializer the membership event envelope signature uses (grep `"sg"` or `[u8; 64]` in this file and copy); for `new_vk`'s `Option<[u8;32]>`, mirror `RecoveryProposalView.vetoed_by`'s skip-if-none pattern with the bstr serializer used by `config_digest` `:471-476`.
- [ ] **Step 4: Run to green.** Same command as Step 2 — all `reset_verify_tests` pass.
- [ ] **Step 5: Byte pins.** In the membership wire-format fixture file found above, add pins for all three events following that file's existing recovery-event pattern exactly (synthetic bytes, hex-pinned canonical encoding, structural key-shape assertion). Run just that fixture test file to green.
- [ ] **Step 6: Gate + commit.** `cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, then from repo root `scripts/test-select --context task` (paste its `round=… bucket=…` line into your report). Commit: `feat(app): ZEB-1031 membership reset event family — o/w/z wire events, digest, verify gates`.

---

### Task 2: Lifecycle evaluator + materialize integration

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`ResetPhase` + `ResetProposalView` next to `RecoveryPhase`/`RecoveryProposalView` `:2011-2100`; `evaluate_reset_phases` next to `evaluate_recovery_phases` `:2535`; materialize arms next to the recovery arms `:3804-3869`; `CommunityState.reset_proposals` view field next to `recovery_proposals` `:1929`)
- Test: inline `mod reset_lifecycle_tests`

**Interfaces:**
- Consumes: Task 1's variants/constants/digest; `materialize_with_now`'s now-floor `T`; `ADMIN_PROPOSAL_EXPIRY_MS`; `quorum_signers` pre-pass pattern (`:2782`) for admin-cosign counting (proposer counts as 1, effective at `admin_quorum`).
- Produces:
  - `pub enum ResetPhase { Collecting, Window, Authorized, Consumed, Vetoed, Expired, Lapsed }` (codes `"c"/"w"/"a"/"n"/"v"/"x"/"l"`)
  - `pub struct ResetProposalView { id: EventId, proposer: OwnerAddr, target_vk: [u8;32], target_epoch: u64, new_members: Vec<OwnerAddr>, new_threshold: u16, veto_window_ms: u64, signers: BTreeSet<OwnerAddr>, proposed_at_wall_ms: u64, deadline_ms: Option<u64>, authorized_at_ms: Option<u64>, endorsed: bool, phase: ResetPhase, consumed_new_vk: Option<[u8;32]>, consumption_superseded: bool }` (2-char keys: `id`/`pr`/`tv`/`te`/`nm`/`nt`/`vw`/`sn`/`t0`/`dl`/`aa`/`en`/`ph`/`cv`/`cs`)
  - `CommunityState.reset_proposals: Vec<ResetProposalView>` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]` — old snapshots byte-identical)
  - `pub fn dfrost_reset_rejected_vks(views: &[ResetProposalView]) -> BTreeSet<[u8; 32]>`
  - `fn evaluate_reset_phases(work: &BTreeMap<EventId, ResetWork>, t: u64) -> BTreeMap<EventId, ResetOutcome>` (internal; `ResetWork` collects `t0_wall_ms`, proposal fields, admin `sig_walls: Vec<u64>`, and `responses: Vec<(u64, EventId, ResetVerdict, Option<[u8;32]>)>` — wall, response event id, verdict, `nv`)

- [ ] **Step 1: Write failing lifecycle tests** (build small event logs through the existing materialize test helpers — find them via the recovery lifecycle tests, `grep -n "RecoveryPhase::TimeLocked" …`). Required cases, each asserting `phase` and where relevant `deadline_ms`/`authorized_at_ms`/`endorsed`:
  1. Quorum=2 proposal + 1 cosign → `Window` with `deadline_ms = t_q + vw`; at `t = deadline + 1` still NOT Authorized (finality); at `t = deadline + RESET_FINALITY_MS + 1` → `Authorized` with `authorized_at_ms = deadline + RESET_FINALITY_MS`.
  2. Idle-community progression: same log, larger `now_ms` only (the now-floor makes phases advance with zero new events).
  3. Veto during Collecting (before quorum) → `Vetoed` terminal; a later endorse loses (first-response-wins asserts on the earlier `(wall, event_id)`).
  4. Endorse at wall `w1`, veto at `w2 > w1`, quorum after both → `Authorized` immediately at `max(w1, t_q)` with `endorsed: true` (endorse won; no window, no finality wait).
  5. Same-wall endorse and veto → lower `event_id` wins (deterministic tie-break).
  6. Response with wall > `t_q + vw` is inert (phase unaffected).
  7. No quorum within `ADMIN_PROPOSAL_EXPIRY_MS` → `Expired`.
  8. Authorized + `RESET_AUTHORIZED_LAPSE_MS` passes with no `c` → `Lapsed`; a `c` arriving after Lapsed does NOT resurrect (stays `Lapsed`).
  9. Authorized + valid `c` response → `Consumed` with `consumed_new_vk = Some(nv)`; `c` is excluded from the first-response contest (a `c` earlier than a veto does not "win" anything — case: c cannot exist pre-Authorized, assert it is ignored while Collecting).
  10. RS-P5 behavioural: proposer with a `Collecting`/`Window`/`Authorized` proposal is rejected; with only `Vetoed`/`Expired`/`Lapsed`/`Consumed` ones, accepted.
  11. `dfrost_reset_rejected_vks`: Authorized → tv in set; Consumed → tv in set; Lapsed/Vetoed/Expired → not in set; Consumed + LATER valid veto-response wall under same tv (on a second proposal) → not in set and first view's `consumption_superseded == true`.
  12. `checked_add` overflow on `t_q + vw` fails closed (mirror `:2559-2562`).
- [ ] **Step 2: Run to verify failure.** `-E 'test(reset_lifecycle_tests)'` — compile failure.
- [ ] **Step 3: Implement** `evaluate_reset_phases` mirroring `evaluate_recovery_phases`'s structure `:2535-2618` with these deltas: quorum from admin `sig_walls` sorted, `t_q = walls[admin_quorum-1]` (the pre-pass that feeds `ResetWork` reuses the `quorum_signers` OLD-quorum ordering rule `:2857-2861`); first-response-wins winner = min `(wall, event_id)` over `e`/`v` responses with `wall >= t0` and `wall <= deadline` (open-ended while no deadline, mirroring RV1 `:2567-2576`); phase ladder exactly as spec §4.1–4.2 (veto-winner → `Vetoed`; endorse-winner + quorum → Authorized at `max(w_endorse, t_q)`; else quorum → `Window` until `t > deadline + RESET_FINALITY_MS` → Authorized at `deadline + RESET_FINALITY_MS`; `c` valid only against an Authorized-reached proposal and only if its wall precedes lapse). Materialize arms: proposal → insert `ResetWork`; cosign → push admin sig wall (distinct actors only); response → push record. Post-pass builds `reset_proposals` views sorted by `(t0, id)`. `dfrost_reset_rejected_vks` per spec §6.1 including the supersession rule (later valid veto wall under same tv lifts a Consumed rejection).
- [ ] **Step 4: Run to green**, including re-running Task 1's `reset_verify_tests` (RS-P5 now behavioural).
- [ ] **Step 5: Gate + commit.** fmt/clippy/test-select as Task 1. Commit: `feat(app): ZEB-1031 reset lifecycle — phases, first-response-wins, finality, lapse, rejected-vk registry`.

---

### Task 3: Dfrost `rs` marker — wire type, state fields, apply

**Files:**
- Modify: `src-tauri/src/community_dfrost_types.rs` (payload next to `DkgCompletePayload` `:235`), `src-tauri/src/community_dfrost_log.rs` (`CommitteeState` `:283`, `CommitteeStateRaw` `:324`, apply dispatch, new `apply_reset_marker`)
- Test: inline dfrost log tests + new pins in `src-tauri/tests/wire_format/zeb303_dfrost_fixtures.rs`

**Interfaces:**
- Consumes: Task 1's digest fn (via `crate::community_membership::dfrost_reset_digest`) — NOT re-verified here (that is RS-M4, engine-side, Task 5); `SpaceId`; existing kind-code registration pattern (grep `"dk"` in community_dfrost_types.rs and mirror for `"rs"`).
- Produces:
  - `pub struct ResetMarkerPayload { reset_proposal_id: EventId, reset_digest: [u8;32], old_vk: [u8;32], old_epoch: u64, space_id: SpaceId }` (keys `ri`/`dg`/`ov`/`oe`/`sp`; `sp` MANDATORY — plain field, no Option)
  - `pub struct VkLineageEntry { old_vk: [u8;32], old_epoch: u64, reset_id: EventId, digest: [u8;32], at: Hlc }` (keys `ov`/`oe`/`ri`/`dg`/`at`)
  - `pub struct PendingReset { reset_id: EventId, new_members: Vec<OwnerAddr>, new_threshold: u16 }` (keys `ri`/`nm`/`nt`)
  - `CommitteeState.vk_history: Vec<VkLineageEntry>` and `CommitteeState.pending_reset: Option<PendingReset>` — both `#[serde(default)]` (+ `skip_serializing_if` empty/none) so pre-existing `dfrost.cbor` snapshots load unchanged; mirror `pending_repair` `:317` and add both to `CommitteeStateRaw` + its `From` impl.
  - `pub fn apply_reset_marker(&mut self, event: &SignedCommitteeEvent) -> Result<ResetMarkerApplied, ApplyError>` where `pub enum ResetMarkerApplied { Applied { old_epoch: u64, reset_id: EventId }, AlreadyMoved }`

- [ ] **Step 1: Write failing apply tests** in the dfrost log test module (fixture helpers exist — `zeb1034_space()` etc.):
  - Happy path: active committee at `(vk, epoch)`, marker with matching `ov`/`oe`/`sp` → `Applied`; afterwards `active == false`, `joint_verifying_key == None`, `vk_history.len() == 1` with the right entry, `pending_reset == Some(..)` (populate `nm`/`nt` from the marker-apply arguments below), `pending_dkg`/`pending_sign`/`pending_refresh`/`pending_repair` all cleared, `current_epoch` UNCHANGED at `oe`.
  - RS-M1: wrong `sp` → `ApplyError::InvariantViolation`, and the error occurs before any state change.
  - RS-M2 matrix, each failing on its own defect: not active; vk mismatch; epoch mismatch (simulate a mid-flight refresh by bumping `current_epoch`) → all `InvariantViolation`, state untouched.
  - RS-M6: re-apply the same marker after the first `Applied` → `Ok(AlreadyMoved)` (benign no-op, NOT an error — catch-up replay re-delivers).
- [ ] **Step 2: Run to failure** (`-E 'test(reset_marker)'`).
- [ ] **Step 3: Implement.** `ResetMarkerPayload` with full doc comments (spec §5); register kind code `"rs"` everywhere the existing kinds are matched (let the compiler find the dispatch sites: add the variant/code first, then chase exhaustiveness errors — scope-by-compiler). `apply_reset_marker` signature takes the successor pin as explicit arguments resolved by the ENGINE from membership state (`new_members: Vec<OwnerAddr>, new_threshold: u16`) so the log stays membership-blind:

```rust
pub fn apply_reset_marker(
    &mut self,
    event: &SignedCommitteeEvent,
    new_members: Vec<OwnerAddr>,
    new_threshold: u16,
) -> Result<ResetMarkerApplied, ApplyError> { … }
```

  Gate order: decode payload → RS-M1 space check (mirror `adopt_initial_quorum`'s `:1282-1299` unconditional style) → if the state has already moved (`!active`, or vk ≠ `ov`, or epoch ≠ `oe`) return `AlreadyMoved` **only when** `vk_history` already contains `reset_id == payload.ri` (a genuine re-delivery); otherwise `InvariantViolation` (RS-M2 — a marker for a state we never held is a defect, not a replay). Effects exactly as the happy-path test asserts; clear in-memory round secrets the way `abort_pending_dkg` `:1636` does.
- [ ] **Step 4: Run to green.**
- [ ] **Step 5: zeb303 pins.** Add `EXPECTED_RS_HEX` fixture + structural assertion for the 5-key `rs` payload in `zeb303_dfrost_fixtures.rs`, following the ZEB-1034 additive discipline (`:128-131`) — new pin, no existing pin touched. Run the wire_format test target to green.
- [ ] **Step 6: Gate + commit.** `feat(app): ZEB-1031 rs reset marker — deactivation, vk_history, pending_reset (RS-M1/M2/M6)`.

---

### Task 4: Successor-DKG pin

**Files:**
- Modify: `src-tauri/src/community_dfrost_log.rs` (`check_ceremony_init_admissible` `:1595`, `apply_dkg_complete` promotion block `:1904-1931`)
- Test: inline tests next to the existing ceremony-init tests

**Interfaces:**
- Consumes: Task 3's `PendingReset`.
- Produces: behavioural guarantee later tasks rely on — while `pending_reset` is `Some`, a `di` must claim exactly `new_members`/`new_threshold` (and `max_signers == new_members.len()`); promotion clears `pending_reset`.

- [ ] **Step 1: Failing tests.** After a marker apply (reuse Task 3's happy-path fixture): `di` with wrong members → `InvariantViolation`; wrong threshold → `InvariantViolation`; exact `nm`/`nt` at epoch `oe+1` → admitted; after the full DKG completes (drive with the existing dkg test helpers), `pending_reset == None`, `active == true`, new vk held, `current_epoch == oe+1`, and `vk_history` still carries the lineage entry.
- [ ] **Step 2: Run to failure.**
- [ ] **Step 3: Implement.** In `check_ceremony_init_admissible`, after the existing `!active` path's structural checks (`:1606-1620`): if `self.committee_state.pending_reset` is `Some(pin)`, require `members == pin.new_members && threshold == pin.new_threshold` else `InvariantViolation` with a message naming ZEB-1031. In the promotion block: `self.committee_state.pending_reset = None;`.
- [ ] **Step 4: Run to green. Step 5: Gate + commit.** `feat(app): ZEB-1031 successor-DKG pin — pending_reset constrains the post-reset di/dk`.

---

### Task 5: Engine-side marker admissibility + adoption/provenance + catch-up chain

**Files:**
- Modify: `src-tauri/src/community_dfrost_log_engine.rs` (ingest path for committee events; catch-up request/response handling; the two adopt call sites `:3257`/`:3470`), `src-tauri/src/community_dfrost_log.rs` (`adopt_initial_quorum` `:1258`), `src-tauri/src/community_dfrost_catchup.rs` (response struct + serve side)
- Test: dfrost log inline tests + `src-tauri/tests/community_voting/community_dfrost_integration.rs`

**Interfaces:**
- Consumes: Tasks 1–4; `dfrost_reset_rejected_vks` + `ResetProposalView` (Task 2); the engine's existing membership-state access used for at-event-HLC vouching (ZEB-1030 — find it at the `adopt_initial_quorum` call site `:3470`).
- Produces:
  - Engine fn `fn verify_reset_marker_admissible(payload: &ResetMarkerPayload, membership: &CommunityState /* materialized at the marker's envelope HLC */) -> Result<(Vec<OwnerAddr>, u16), String>` — RS-M3 (phase at HLC is `Authorized` or `Consumed`), RS-M4 (recomputed digest matches both marker `dg` and proposal content; proposal `tv`/`te` == marker `ov`/`oe`), RS-M5 (marker author is power-100 or ∈ `nm`, at the marker's HLC). Returns the successor pin `(nm, nt)` for `apply_reset_marker`. Used on BOTH live ingest and catch-up adoption (verifier-mirror).
  - `adopt_initial_quorum` gains a parameter: `rejected_vks: &BTreeSet<[u8; 32]>` with the new gate FIRST (before shape checks): payload vk ∈ set → `Err` naming ZEB-1031 provenance. Both call sites pass `dfrost_reset_rejected_vks(&membership.reset_proposals)`.
  - `CatchupResponse` gains `#[serde(rename = "rc", skip_serializing_if = "Option::is_none", default)] pub reset_chain: Option<Vec<ResetChainLink>>` with `pub struct ResetChainLink { #[serde(rename = "mk")] marker: SignedCommitteeEvent, #[serde(rename = "dk")] dk_events: Vec<SignedCommitteeEvent> }` — optional-key evolution, legacy responses byte-identical.

- [ ] **Step 1: Failing tests.**
  - Log-level: `adopt_initial_quorum` with the quorum's vk in `rejected_vks` → rejected with the provenance error; empty set → prior behaviour (run the existing 1030/1034 adoption tests against the extended signature to prove no regression).
  - Engine-level (integration file): build a community with an active committee, drive the full membership reset to Authorized (quorum + window + finality via `now_ms` control), ingest a marker → committee deactivates; drive successor DKG → active under new vk. Then: a straggler engine still at the OLD state receives a catch-up response carrying `reset_chain` → ends active at the new vk with `vk_history.len() == 1`. A fresh joiner offered ONLY the old quorum while the reset is Authorized → adoption rejected (stale-committee replay). Marker with a forged digest or a non-Authorized proposal → `verify_reset_marker_admissible` rejects (each case pinned to its own error string).
- [ ] **Step 2: Run to failure. Step 3: Implement.** Serve side: when the responder's `vk_history` is non-empty and the requester's claimed epoch ≤ a lineage entry's `old_epoch`, attach the chain links from that entry forward (marker event + the retained `dk` quorum events per successor epoch — the responder already retains dk events for `select_catchup`'s log scan; reuse that retrieval). Apply side (engine): for each link in order — `verify_reset_marker_admissible` at the marker's HLC → `apply_reset_marker(marker, nm, nt)` → `adopt_initial_quorum(dk_events, expected_space, rejected_vks)`. Live-ingest path for a lone `rs` event routes through the same admissibility fn (verifier-mirror).
- [ ] **Step 4: Run to green** (`-E 'test(reset)'` plus the touched integration file). **Step 5: Gate + commit.** `feat(app): ZEB-1031 provenance — marker admissibility mirror, rejected-vk adoption gate, catch-up reset chain`.

---

### Task 6: Reset-purpose sign ceremonies (endorse / veto / consumed signatures)

**Files:**
- Modify: `src-tauri/src/community_dfrost_log.rs` (`PendingSignSession` — add purpose), `src-tauri/src/lib.rs` (sign-ceremony contribute/aggregate cores — find via `grep -n "fn dfrost_contribute_sign" src-tauri/src/lib.rs` and the vb mint site near the ZEB-1032 fix comment `~:65362`), `src-tauri/src/community_dfrost_log_engine.rs` (ceremony driver)
- Test: inline + integration

**Interfaces:**
- Consumes: Task 1's `dfrost_reset_message_hash` + domain consts; existing threshold-sign ceremony machinery (`ts` events, `pending_sign` sessions, `derive_ceremony_id`).
- Produces:
  - `pub enum SignPurpose { #[default] Beacon, ResetResponse { proposal_id: EventId, verdict: ResetVerdict } }` on `PendingSignSession` (`#[serde(default)]` — pre-existing sessions decode as `Beacon`).
  - Engine entry `async fn initiate_reset_response_ceremony(&self, proposal_id: EventId, verdict: ResetVerdict) -> Result<(), String>` — computes the digest from the membership proposal, derives the message hash with the verdict's domain (for `Consumed`, `nv` = the CURRENT held vk), and initiates a `ts` ceremony whose session carries the purpose.
  - Completion sink: at aggregation, a `Beacon`-purpose session mints `vb` (unchanged); a `ResetResponse` session instead authors the membership `DfrostResetResponse` event with the 64-byte aggregate signature. NO `vb` is ever minted for reset purposes (the beacon index must not see these signatures).

- [ ] **Step 1: Failing tests.** Unit: a `ResetResponse`-purpose session that reaches aggregation produces a membership event with a `group_sig` that passes RS-R3 verification against the committee vk, and NO beacon-index change (`beacon_watermark` unchanged, no `vb` in the log). Endorse and veto domains produce DIFFERENT message hashes for the same digest (domain separation pinned). A `Beacon` session still mints `vb` (regression). Serde: a legacy session blob without the purpose field decodes as `Beacon`.
- [ ] **Step 2: Run to failure. Step 3: Implement** — the purpose field, the branch at the single aggregation/mint site (scope-by-compiler from the `vb` mint), the engine initiation fn (ceremony id derivation: reuse `derive_ceremony_id(&space_id, epoch, "sign-v1:" ‖ seed)` with seed = the reset message hash — deterministic, concurrent initiations converge). **Step 4: Green. Step 5: Gate + commit.** `feat(app): ZEB-1031 reset-purpose sign ceremonies — threshold endorse/veto/consumed signatures without beacon minting`.

---

### Task 7: Poll voiding + relaunch

**Files:**
- Modify: `src-tauri/src/community_voting_tier3.rs` (poll disposition), `src-tauri/src/community_voting_log_engine.rs` (void entry point), `src-tauri/src/community_dfrost_log_engine.rs` (hook call on marker apply — the ZEB-1033 Weak-hook pattern; find the existing hook installs via `grep -n "Weak" src-tauri/src/community_dfrost_log_engine.rs`)
- Test: `src-tauri/tests/community_voting/` integration

**Interfaces:**
- Consumes: Task 3's `ResetMarkerApplied::Applied { old_epoch, reset_id }`.
- Produces:
  - `Tier3PollState.voided: Option<VoidedInfo>` with `pub struct VoidedInfo { #[serde(rename = "ri", …bstr…)] reset_id: EventId, #[serde(rename = "oe")] old_epoch: u64 }` (`#[serde(default, skip_serializing_if = "Option::is_none")]`); a voided poll accepts no further ballots/beacons/tallies (guard every mutation entry the way the existing closed/finalized guards work — find them via the poll state machine's status checks).
  - Voting-engine fn `async fn void_tier3_polls_for_reset(&self, old_epoch: u64, reset_id: EventId) -> usize` voiding every open Tier-3 poll with `meta.community_epoch <= old_epoch`; idempotent (already-voided polls skipped); persisted via the existing poll-persist path (`community_voting_persist.rs` — add the field to the persisted shape with `serde(default)`).
  - Relaunch IPC core `fn relaunch_voided_poll_impl(…poll_id…) -> Result<NewPollId, String>`: authors a fresh Tier-3 PollCreate copying the voided poll's parameters, stamped at the CURRENT epoch by the existing pre-read (`community_voting_log_engine.rs:2179-2217` — untouched, it already does this), carrying a predecessor link (add `#[serde(rename = "pv", skip_serializing_if = "Option::is_none", default)] predecessor: Option<EventId>` to the Tier-3 poll-create payload — optional-key evolution, legacy byte-identical). Caller must be the original creator or power-100.

- [ ] **Step 1: Failing tests.** Open se-mode poll at epoch N; apply a reset marker for `oe = N` through the dfrost engine → poll is voided with the right `reset_id`; casting a ballot on it errors; `void…` re-run → 0 additional; a poll at epoch N+1 (created post-reset) untouched; relaunch produces a new poll with `community_epoch == N+1`, `predecessor == Some(old_id)`, and the old poll still voided; relaunch by a non-creator non-admin rejected.
- [ ] **Step 2: Run to failure. Step 3: Implement. Step 4: Green. Step 5: Gate + commit.** `feat(app): ZEB-1031 tier-3 poll voiding on reset + prompted relaunch`.

---

### Task 8: IPC surface + engine auto-drive

**Files:**
- Modify: `src-tauri/src/api/rpc.rs` (register next to the recovery IPCs `:973-1028`), `src-tauri/src/lib.rs` (`*_impl` fns), `src-tauri/src/community_dfrost_log_engine.rs` (auto-drive)
- Test: rpc registry-parity assertions (`rpc.rs:2591-2614` pattern) + engine integration

**Interfaces:**
- Consumes: everything above.
- Produces the six spec-§9 IPCs, mirroring the recovery IPC signatures (`get_recovery_state:973` takes `now_ms` — copy that as-of pattern):
  - `get_dfrost_reset_state(community_id, now_ms) -> Vec<ResetProposalView>` (serialized view)
  - `propose_dfrost_reset(community_id, target_vk_hex, target_epoch, new_members, new_threshold, veto_window_ms)`
  - `cosign_dfrost_reset(community_id, target_event_id)`
  - `respond_dfrost_reset(community_id, target_event_id, verdict)` → calls Task 6's `initiate_reset_response_ceremony` (verdict `e`/`v` only; `c` is auto-driven, not user-invocable)
  - `author_dfrost_reset_marker(community_id, target_event_id)` (manual fallback; normally auto-driven)
  - `relaunch_voided_poll(community_id, poll_id)`
  - Auto-drive (DkgDriver pattern — find the existing periodic drive loop): (a) when a reset the local engine can apply becomes Authorized-consumable, author the marker; (b) after successor promotion with `vk_history` back-pointer matching an unconsumed Authorized reset, a pinned successor member initiates the `Consumed` ceremony. Both idempotent (check for existing marker / existing `c` first).

- [ ] **Step 1: Failing tests** — registry parity (add the six names to the parity assertion and watch it fail before registration); engine integration: with auto-drive enabled, an Authorized reset progresses to deactivation + (after DKG) a `c` response with NO manual IPC calls beyond the propose/cosign.
- [ ] **Step 2–4: fail → implement → green.** **Step 5: Gate + commit.** `feat(app): ZEB-1031 reset IPC surface + auto-driven marker and consumption`.

---

### Task 9: UI — admin reset panel + voided-poll banner

**Files:**
- Create: `src/lib/components/community/DfrostResetPanel.svelte`
- Modify: the community admin/settings panel that hosts the recovery UI (locate via `grep -rn "get_recovery_state" src/` and mount alongside), the Tier-3 poll view component (locate via `grep -rn "ratification" src/lib/components/` or the poll status rendering) for the voided banner
- Test: vitest component tests next to the existing panel tests

**Interfaces:** Consumes Task 8's IPCs through the existing invoke adapter (camelCase params: `communityId`, `targetEventId`, `nowMs`, …; error extraction via `e instanceof Error ? e.message : String(e)`).

- [ ] **Step 1: Failing vitest tests** — panel renders proposal list with phase chips (Collecting/Window countdown from `deadline_ms`/Authorized/Vetoed/Consumed…), propose form (member multi-select, threshold + veto-window as slider PAIRED with a typeable number input, clamped 24h–30d, default 72h), cosign button for admins, veto/endorse buttons calling `respond_dfrost_reset`; voided poll shows the banner naming the reset and a Relaunch button (creator/admin only) calling `relaunch_voided_poll`. Mock the adapter per the existing component-test pattern.
- [ ] **Step 2–4: fail → implement → green** (`npx vitest run` + `npx tsc --noEmit` from repo root). **Step 5: Commit.** `feat(ui): ZEB-1031 dfrost reset admin panel + voided-poll relaunch banner`.

---

### Task 10: Two-node e2e + full sweep

**Files:**
- Create: `src-tauri/tests/community_dfrost_reset_e2e.rs` (follow the headless two-node harness used by the ZEB-1030 catch-up e2e — locate via `grep -rn "zeb1030" src-tauri/tests/`)
- Modify: nothing (pure test task; any product fix it forces goes through a review round)

- [ ] **Step 1: Write the four flows** (spec §11), driving both nodes through the headless surface with `now_ms` control and `poll_until` (camelCase assertion keys):
  1. **Disaster to completion**: 3-member committee live on both nodes → simulate share loss → propose + cosign to quorum → advance past window + finality → marker auto-applies on both → successor DKG under pinned members → both nodes active at new vk, `vk_history` length 1, `c` response present.
  2. **Disaster vetoed**: same until mid-window → committee runs the veto ceremony → both nodes converge on `Vetoed`, committee stays active, old vk intact.
  3. **Cooperative**: propose + cosign → endorse ceremony → Authorized immediately (no window wait) → reset completes.
  4. **Joiner bootstrap post-reset**: after flow 1, a third fresh node joins → adopts the successor committee; offered-old-quorum rejection asserted via the log-level test from Task 5 (e2e asserts only the positive: joiner lands on the NEW vk).
- [ ] **Step 2: Run the new e2e file to green** (rebuild the spawned binary first — stale-binary trap: `cargo build` the harness bin and pin `HARMONY_APP_BIN`).
- [ ] **Step 3: Full sweep.** `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` — ALL green (working tree clean before declaring it).
- [ ] **Step 4: Commit.** `test(app): ZEB-1031 two-node reset e2e — disaster, veto, cooperative, joiner flows`.

---

## Plan self-review (done at write time)

- **Spec coverage:** §3 → Tasks 1–2; §4 → Task 2; §5 → Tasks 3–4; §6 → Task 5; §3.3 ceremonies → Task 6; §7 → Task 7; §9 → Tasks 8–9; §11 → per-task tests + Task 10. §10 honesty-ledger items need no code (documented residuals). §6.3 request-side unchanged — confirmed no task touches the request struct.
- **Type consistency:** `ResetVerdict`, `ResetProposalView` fields, `dfrost_reset_digest`/`dfrost_reset_message_hash` signatures, `ResetMarkerApplied`, `SignPurpose`, `VoidedInfo`, and the `rejected_vks` parameter are named identically at their producer and every consumer above.
- **Placeholder scan:** no TBDs; the two "locate via grep" instructions are anchor-finding steps (the file to touch is named by responsibility and the grep is exact), not deferred content.
