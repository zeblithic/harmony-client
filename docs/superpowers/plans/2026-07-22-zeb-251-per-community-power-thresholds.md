# Per-community power thresholds (ZEB-251) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a community's `invite`/`kick`/`set_power` power thresholds configurable per community via a signed, quorum-gated `AdminProposal{ChangeThresholds}` event that every member materializes identically, replacing the single global `POWER_THRESHOLDS` const read inside `verify_event`.

**Architecture:** Mirror the existing `ChangeQuorum` governance path end-to-end. Thresholds become a materialized field on `MaterializedMembership` (member-agreed, at-event-HLC), defaulting to today's `POWER_THRESHOLDS` for byte-compat. A new `ProposalKind::ChangeThresholds` rides the existing `AdminProposal`/`AdminCountersign` envelope (inheriting the whole quorum/pending/countersign machinery). `verify_event` reads thresholds from `prior_state`; the IPC + service + a `ChangeThresholdsDialog.svelte` (clone of `ChangeQuorumDialog`) expose it.

**Tech Stack:** Rust (`community_membership.rs` CRDT, `lib.rs` Tauri IPC), TypeScript (`community-service.ts`, `types.ts`), Svelte 5 runes (`src/lib/components/`).

## Global Constraints

- **Rust gates** (run from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- **Frontend gates** (run from repo root): `npx tsc --noEmit`; `npx vitest run`.
- **Iterative Rust test scoping:** the `community_membership.rs` unit tests are inline `#[cfg(test)]` in the lib, so during dev run `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(<name>)'` — this avoids relinking ~97 integration binaries (~50 min). The **final** per-PR gate is the full `--workspace --all-targets` sweep above.
- **CBOR wire:** `ProposalKind` is `#[serde(tag = "kd", content = "bd")]` with **1-char variant tags** (`s`/`k`/`c`/`r` taken → use **`t`**) and **2-char inner field keys**. `MaterializedMembership` is snapshot-persisted: any new field MUST use `#[serde(rename="..", default="..", skip_serializing_if="..")]` seeding to `POWER_THRESHOLDS`, so existing snapshots and never-customized communities stay byte-identical.
- **The ceiling is fixed:** `max == 100` always. Customizable = `invite`, `kick`, `set_power`. The validity invariant `0 ≤ invite ≤ kick ≤ set_power ≤ max == 100` is enforced **authoritatively at `verify_event`** (so every member rejects an invalid change identically), redundantly cheap in the IPC command, and live in the dialog.
- **IPC naming:** Rust `snake_case` ↔ JS `camelCase` (`set_power` ↔ `setPower`).
- **Tauri error extraction:** `const msg = e instanceof Error ? e.message : String(e)`.
- **Second-order correctness:** thresholds are read from `prior_state`/running-`m` (at-event-HLC), never a mutable "current" snapshot; the change event is quorum-gated exactly like `ChangeQuorum` (no weaker path); NO anti-backdating guard is added (would break backfill).

---

## File Structure

- **Modify** `src-tauri/src/community_membership.rs` — `PowerThresholds` serde derives; `MaterializedMembership.power_thresholds` field + default fns; `ProposalKind::ChangeThresholds`; `VerifyError::AdminProposalThresholdsInvalid` (+ `Display` arm); `verify_event` read-swap + validity gate; `apply_admin_proposal_effect` arm; the 2 `materialize` read-swaps; unit tests.
- **Modify** `src-tauri/src/lib.rs` — `ProposalKindDto::ChangeThresholds` + projection arm; `mint_admin_proposal_change_thresholds_event`; `propose_change_thresholds` command; `CommunityGovernanceDto` threshold fields + `compute_community_governance`; `generate_handler!` registration.
- **Modify** `src/lib/types.ts` — governance/threshold TS types; fix the stale `:1108` comment.
- **Modify** `src/lib/community-service.ts` — extend `getCommunityGovernance` return type.
- **Create** `src/lib/components/ChangeThresholdsDialog.svelte` (clone of `ChangeQuorumDialog.svelte`) + `src/lib/components/__tests__/ChangeThresholdsDialog.test.ts`.
- **Modify** `src/lib/components/CommunitySettingsPanel.svelte` — button + dialog mount + per-community gate consumption.
- **Modify** `src/lib/components/PendingAdminProposalsPanel.svelte` — `ProposalKindDto` union member + `proposalSummary` arm.

---

## Task 1: `power_thresholds` becomes a materialized field `verify_event` reads (behavior-preserving)

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (PowerThresholds derives ~:5259; MaterializedMembership ~:1761–1861; verify_event reads per Item-8 table; materialize reads :3110/:3217)
- Test: inline `#[cfg(test)] mod` in the same file

**Interfaces:**
- Produces: `MaterializedMembership.power_thresholds: PowerThresholds` (pub field, defaults to `POWER_THRESHOLDS`); `default_power_thresholds() -> PowerThresholds`; `is_default_power_thresholds(&PowerThresholds) -> bool`. `PowerThresholds` now derives `Serialize, Deserialize, PartialEq, Eq`.

- [ ] **Step 1: Write the failing test** — a hand-built `MaterializedMembership` with a custom `power_thresholds` governs `verify_event` (proves the read comes from the field, not the const). Add to the membership test module (mirror the existing `ChangeQuorum`/`verify_event` tests — search `fn ` + `verify_event(` in `#[cfg(test)]`):

```rust
#[test]
fn verify_event_reads_invite_threshold_from_materialized_field() {
    // A power-10 member inviting: allowed under default invite=0, but
    // REFUSED once the community's materialized invite threshold is 25.
    let (admin, admin_key) = test_admin_pair(); // existing helper style
    let (low, low_key) = test_member_pair();
    let mut prior = base_membership_with_admin(&admin); // existing helper style
    prior.members.insert(low, joined_member());
    prior.power_levels.insert(low, 10);

    // Baseline: invite=0 (default) → a power-10 invite verifies OK.
    let ev = mint_invite_event(&low, &low_key, /*target*/ new_addr());
    prior.power_thresholds = POWER_THRESHOLDS; // invite = 0
    assert!(verify_event(&ev, &prior, &vctx()).is_ok());

    // Raise the community's invite threshold to 25 in the materialized
    // state → the SAME event must now fail on insufficient power.
    prior.power_thresholds = PowerThresholds { invite: 25, ..POWER_THRESHOLDS };
    let err = verify_event(&ev, &prior, &vctx()).unwrap_err();
    assert!(matches!(err, VerifyError::ActorPowerInsufficient), "got {err:?}");
}
```

*(Adapt helper names to the existing test module's conventions — reuse whatever the current `verify_event` tests use to build members/events. Do NOT invent a new fixture harness.)*

- [ ] **Step 2: Run it to verify it fails** — `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(verify_event_reads_invite_threshold_from_materialized_field)'`. Expected: FAIL to compile (`no field power_thresholds on MaterializedMembership`).

- [ ] **Step 3: Add serde derives + field renames to `PowerThresholds`** (~:5259):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerThresholds {
    #[serde(rename = "iv")]
    pub invite: u8,
    #[serde(rename = "kk")]
    pub kick: u8,
    #[serde(rename = "sp")]
    pub set_power: u8,
    #[serde(rename = "mx")]
    pub max: u8,
}
```

*(The `POWER_THRESHOLDS` const and every Rust `.invite`/`.kick`/… access are unchanged — serde renames only affect CBOR keys.)*

- [ ] **Step 4: Add the materialized field + default helpers.** In `MaterializedMembership` (place next to `admin_quorum` ~:1804, mirroring its serde attrs):

```rust
    /// ZEB-251: per-community power thresholds, materialized from
    /// AdminProposal{ChangeThresholds} events (Task 2). Default =
    /// POWER_THRESHOLDS (Sub-C v1 hardcoded). Byte-compat with pre-ZEB-251
    /// cached snapshots — the `default`/`skip_serializing_if` pair means a
    /// never-customized community serializes no "pt" key and decodes to the
    /// hardcoded defaults, exactly as before.
    #[serde(
        rename = "pt",
        default = "default_power_thresholds",
        skip_serializing_if = "is_default_power_thresholds"
    )]
    pub power_thresholds: PowerThresholds,
```

Add the free fns next to `default_admin_quorum`/`is_default_admin_quorum` (~:1855):

```rust
pub(crate) fn default_power_thresholds() -> PowerThresholds {
    POWER_THRESHOLDS
}

pub(crate) fn is_default_power_thresholds(t: &PowerThresholds) -> bool {
    *t == POWER_THRESHOLDS
}
```

And in the `impl Default for MaterializedMembership` (~:1839), add `power_thresholds: POWER_THRESHOLDS,`. (Every other construction site of `MaterializedMembership` that names all fields must also add `power_thresholds: POWER_THRESHOLDS` — the compiler will list them; use `default_power_thresholds()` or the const.)

- [ ] **Step 5: Swap the 17 `verify_event` reads** from `POWER_THRESHOLDS.<field>` to `prior_state.power_thresholds.<field>`, at these lines (per the extraction; `verify_event` spans ~:3894–:4915): 4010, 4136, 4492, 4506, 4536, 4539, 4554, 4584, 4592, 4608, 4625, 4643, 4685, 4707, 4726, 4757. Each is a mechanical `POWER_THRESHOLDS.` → `prior_state.power_thresholds.` replacement. **Do NOT** blind `replace_all` (the const is read in many other fns) — scope edits to the `verify_event` body. After the swap, **remove the `#[allow(clippy::absurd_extreme_comparisons)]`** at ~:3888: `prior_state.power_thresholds.invite` is a runtime value, so `power < …invite` is no longer an absurd-comparison-against-0 and the lint won't fire.

- [ ] **Step 6: Swap the 2 `materialize` reads** at :3110 and :3217 (`issuer_power >= POWER_THRESHOLDS.kick`) to the **running** materialized value `m.power_thresholds.kick` (materialize has no `prior_state`; `m` is the state folded so far, which is the correct at-fold-point value — identical to the const while no `ChangeThresholds` event exists, so behavior is preserved). Read the local context first to confirm the binding name is `m` at those sites.

- [ ] **Step 7: Run the test to verify it passes** — same command as Step 2. Expected: PASS. Then run the broader membership suite to confirm the refactor preserved behavior: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(community_membership)'`. Expected: all green (default `power_thresholds == POWER_THRESHOLDS` ⇒ no behavior change).

- [ ] **Step 8: Commit** — `git add -A && git commit` with message `feat(zeb-251): materialize power_thresholds on membership; verify_event reads per-community` + the standard Co-Authored-By/Claude-Session trailers.

---

## Task 2: `ProposalKind::ChangeThresholds` — the quorum-gated change event

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`ProposalKind` ~:39; `verify_event` AdminProposal match ~:4112–:4258; `apply_admin_proposal_effect` ~:5441; `VerifyError` enum + `Display`)
- Modify: `src-tauri/src/lib.rs` (`ProposalKindDto` ~:44420 + projection ~:44736 — exhaustive matches; must update or lib.rs won't compile)
- Test: inline membership tests

**Interfaces:**
- Consumes: `MaterializedMembership.power_thresholds` (Task 1).
- Produces: `ProposalKind::ChangeThresholds { new_thresholds: PowerThresholds }` (CBOR tag `"t"`, field `"th"`); `VerifyError::AdminProposalThresholdsInvalid`; `apply_admin_proposal_effect` sets `m.power_thresholds`; `ProposalKindDto::ChangeThresholds { invite, kick, set_power }`.

- [ ] **Step 1: Write the failing tests** — mirror the existing `ChangeQuorum` verify/materialize tests (search `ChangeQuorum` in the `#[cfg(test)]` module and clone their structure). Three cases:

```rust
#[test]
fn change_thresholds_at_quorum1_materializes_and_governs() {
    // Single admin (admin_quorum defaults to 1) proposes invite=25.
    // After materialize, m.power_thresholds.invite == 25 and a power-10
    // invite now fails.
    let log = /* build: bootstrap admin, then AdminProposal{ChangeThresholds{invite:25,kick:50,set_power:100,max:100}} */;
    let m = materialize(&log, admin_addr());
    assert_eq!(m.power_thresholds.invite, 25);
}

#[test]
fn change_thresholds_invalid_ordering_is_rejected_at_verify() {
    // kick(40) < invite(50) violates the ordering invariant → rejected.
    let ev = mint_admin_proposal_change_thresholds(
        &admin, &admin_key,
        PowerThresholds { invite: 50, kick: 40, set_power: 100, max: 100 },
    );
    let err = verify_event(&ev, &prior_with_admin(), &vctx()).unwrap_err();
    assert!(matches!(err, VerifyError::AdminProposalThresholdsInvalid), "got {err:?}");
}

#[test]
fn change_thresholds_max_not_100_is_rejected_at_verify() {
    let ev = mint_admin_proposal_change_thresholds(
        &admin, &admin_key,
        PowerThresholds { invite: 0, kick: 50, set_power: 100, max: 99 },
    );
    assert!(matches!(
        verify_event(&ev, &prior_with_admin(), &vctx()).unwrap_err(),
        VerifyError::AdminProposalThresholdsInvalid
    ));
}
```

*(Also add, if the existing ChangeQuorum tests have analogues to mirror: a below-quorum-stays-pending test with `admin_quorum == 2`, and an at-HLC test where an `Invite` ordered before the threshold-raise verifies against the old threshold and one after against the new. Reuse the exact multi-admin log-building helpers the ChangeQuorum tests use.)*

- [ ] **Step 2: Run to verify failure** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(change_thresholds)'`. Expected: FAIL to compile (`no variant ChangeThresholds`).

- [ ] **Step 3: Add the `ProposalKind` variant** (after `SetRecoveryDesignates`, ~:104), tag `"t"`, field `"th"`:

```rust
    /// ZEB-251: change the community's per-action power thresholds
    /// (invite/kick/set_power). Routed through the AdminProposal quorum
    /// machinery (AP1–AP5 unchanged; always admin-affecting, like
    /// ChangeQuorum). Validity gate AT1 at verify_event. Materializes as
    /// `MaterializedMembership.power_thresholds`. Variant tag "t" (1-char,
    /// unused before this); inner field key "th" (2-char).
    #[serde(rename = "t")]
    ChangeThresholds {
        #[serde(rename = "th")]
        new_thresholds: PowerThresholds,
    },
```

- [ ] **Step 4: Add the `VerifyError` variant + `Display` arm.** In `enum VerifyError` (next to the ZEB-250 admin block ~:989):

```rust
    /// ZEB-251 AT1: ChangeThresholds new_thresholds violate the invariant
    /// 0 <= invite <= kick <= set_power <= max, or max != 100.
    AdminProposalThresholdsInvalid,
```

In the hand-written `Display` impl (next to the other `AdminProposal*` arms ~:1275):

```rust
            VerifyError::AdminProposalThresholdsInvalid => write!(
                f,
                "ZEB-251 AdminProposal ChangeThresholds invariant violated (need 0 <= invite <= kick <= set_power <= max == 100)"
            ),
```

- [ ] **Step 5: Add the verify validity gate.** Inside `verify_event`'s `match proposal_kind` (the `AdminProposal` arm, alongside `ChangeQuorum` ~:4238), add:

```rust
                ProposalKind::ChangeThresholds { new_thresholds } => {
                    // AT1: ordering invariant + fixed ceiling. Authoritative —
                    // every member rejects an invalid change identically.
                    let t = new_thresholds;
                    if !(t.invite <= t.kick && t.kick <= t.set_power && t.set_power <= t.max)
                        || t.max != POWER_THRESHOLDS.max
                    {
                        return Err(VerifyError::AdminProposalThresholdsInvalid);
                    }
                    // ChangeThresholds is always admin-affecting; no AP4 distinction.
                }
```

- [ ] **Step 6: Add the apply arm** in `apply_admin_proposal_effect` (alongside `ChangeQuorum` ~:5471):

```rust
        ProposalKind::ChangeThresholds { new_thresholds } => {
            // Mutates running power_thresholds so subsequent events in the
            // same replay verify against the updated values (single-pass-
            // with-running-state, mirrors ChangeQuorum).
            m.power_thresholds = *new_thresholds;
        }
```

*(The pending/countersign materialize arms at ~:3373–:3481 are kind-agnostic — no change. `ChangeThresholds` is not a recovery-config generation change, so do NOT add it to the `matches!(kind, ProposalKind::SetRecoveryDesignates …)` guards.)*

- [ ] **Step 7: Update the `ProposalKindDto` exhaustive matches in `lib.rs`.** (Also: adding a `ProposalKind` variant breaks compilation at *every* exhaustive `match proposal_kind` / `match kind` in the workspace — `verify_event` and `apply_admin_proposal_effect` are handled in Steps 5–6; the compiler will list any others, e.g. a `Display`/debug helper or `list_pending_admin_proposals` — add a `ChangeThresholds` arm to each the compiler flags.) Add the DTO variant (~:44420):

```rust
    ChangeThresholds {
        invite: u8,
        kick: u8,
        set_power: u8,
    },
```

And the projection arm (~:44736):

```rust
            ProposalKind::ChangeThresholds { new_thresholds } => ProposalKindDto::ChangeThresholds {
                invite: new_thresholds.invite,
                kick: new_thresholds.kick,
                set_power: new_thresholds.set_power,
            },
```

- [ ] **Step 8: Run tests to verify pass** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(change_thresholds)'`. Expected: PASS. Then `-E 'test(community_membership)'` to confirm no regressions.

- [ ] **Step 9: Commit** — `feat(zeb-251): ProposalKind::ChangeThresholds quorum-gated change event + verify/apply`.

---

## Task 3: IPC — `propose_change_thresholds` + governance DTO

**Files:**
- Modify: `src-tauri/src/lib.rs` (`mint_admin_proposal_change_quorum_event` neighbor ~:42860; `propose_change_quorum` neighbor ~:45040; `CommunityGovernanceDto` + `compute_community_governance` ~:44309; `generate_handler!` ~:65094)
- Test: inline `#[cfg(test)]` in lib.rs for `compute_community_governance`

**Interfaces:**
- Consumes: Task 2's `ProposalKind::ChangeThresholds`, `PowerThresholds`.
- Produces: `#[tauri::command] propose_change_thresholds(community_id, invite, kick, set_power) -> Result<AdminActionResult, String>`; `CommunityGovernanceDto` gains `invite/kick/set_power/max`.

- [ ] **Step 1: Write the failing test** — `compute_community_governance` surfaces the materialized thresholds:

```rust
#[test]
fn compute_governance_surfaces_power_thresholds() {
    let mut m = crate::community_membership::MaterializedMembership::default();
    let me = test_owner_addr();
    m.members.insert(me, joined_member());
    m.power_thresholds = crate::community_membership::PowerThresholds {
        invite: 25, kick: 60, set_power: 100, max: 100,
    };
    let dto = compute_community_governance(&m, me).unwrap();
    assert_eq!((dto.invite, dto.kick, dto.set_power), (25, 60, 100));
}
```

- [ ] **Step 2: Run to verify failure** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(compute_governance_surfaces_power_thresholds)'`. Expected: FAIL (`no field invite on CommunityGovernanceDto`).

- [ ] **Step 3: Extend `CommunityGovernanceDto` + `compute_community_governance`** (~:44317):

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommunityGovernanceDto {
    /// Current materialized admin quorum (ZEB-250). Default 1.
    pub admin_quorum: u8,
    /// ZEB-251: current materialized power thresholds. Default {0,50,100,100}.
    pub invite: u8,
    pub kick: u8,
    pub set_power: u8,
    pub max: u8,
}
```

In `compute_community_governance`, populate from `materialized.power_thresholds`:

```rust
    Ok(CommunityGovernanceDto {
        admin_quorum: materialized.admin_quorum,
        invite: materialized.power_thresholds.invite,
        kick: materialized.power_thresholds.kick,
        set_power: materialized.power_thresholds.set_power,
        max: materialized.power_thresholds.max,
    })
```

- [ ] **Step 4: Add the mint helper** next to `mint_admin_proposal_change_quorum_event` (~:42860):

```rust
/// ZEB-251: mint a signed AdminProposal carrying a ChangeThresholds proposal_kind.
pub fn mint_admin_proposal_change_thresholds_event(
    community_id: crate::owner_state_types::SpaceId,
    self_owner: crate::owner_state_types::OwnerAddr,
    new_thresholds: crate::community_membership::PowerThresholds,
    signing_key: &ed25519_dalek::SigningKey,
    hlc: crate::owner_state_types::Hlc,
) -> Result<crate::community_membership::SignedMembershipEvent, String> {
    use crate::community_membership::{sign_event, EventPayload, MembershipEventKind, ProposalKind};
    use rand::RngCore;
    let mut rng = rand::thread_rng();
    let mut event_id_bytes = [0u8; 16];
    rng.fill_bytes(&mut event_id_bytes);
    let payload = EventPayload {
        id: event_id_bytes,
        community_id,
        kind: MembershipEventKind::AdminProposal {
            proposal_kind: ProposalKind::ChangeThresholds { new_thresholds },
        },
        actor: self_owner,
        at: hlc,
    };
    sign_event(&payload, signing_key)
        .map_err(|e| format!("sign admin_proposal_change_thresholds: {e}"))
}
```

- [ ] **Step 5: Add the `propose_change_thresholds` command** — clone `propose_change_quorum` (~:45040) verbatim, changing: the signature params to `(community_id, invite, kick, set_power)`; the up-front validation to the ordering invariant; drop the admin-count check (irrelevant to thresholds) and keep the caller-Joined + caller-power≥100 auth block; build `PowerThresholds { invite, kick, set_power, max: 100 }`; call the new mint helper. Skeleton (reuse the exact HLC-reservation / generation-fence / engine-lookup / outbox blocks from `propose_change_quorum`):

```rust
#[tauri::command]
async fn propose_change_thresholds(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    invite: u8,
    kick: u8,
    set_power: u8,
) -> Result<AdminActionResult, String> {
    // Cheap client-side invariant (verify_event is authoritative).
    if !(invite <= kick && kick <= set_power && set_power <= 100) {
        return Err(
            "propose_change_thresholds: require 0 <= invite <= kick <= set_power <= 100".to_string(),
        );
    }
    let new_thresholds = crate::community_membership::PowerThresholds {
        invite, kick, set_power, max: 100,
    };
    // ── identical to propose_change_quorum from here: hex-decode community_id,
    //    snapshot (hlc_tracker, device_id, self_owner, community_registry,
    //    dm_outbox, generation), reserve_next_hlc_for_device, generation+
    //    registry fence, engine_arc lookup, admin_addr ──
    // ... (copy verbatim) ...

    // Auth: caller Joined + power >= 100, and read current admin_quorum.
    let admin_quorum = {
        let state = engine_arc.state();
        let state_g = state.lock().await;
        let m = state_g.materialize_now(admin_addr);
        let caller_status = m.members.get(&self_owner).map(|ms| ms.status);
        if !matches!(caller_status, Some(crate::community_membership::MemberStatus::Joined)) {
            return Err("propose_change_thresholds: caller is not a Joined member".to_string());
        }
        let caller_power = m.power_levels.get(&self_owner).copied().unwrap_or(0);
        if caller_power < 100 {
            return Err(format!(
                "propose_change_thresholds: caller power {caller_power} below admin threshold 100"
            ));
        }
        m.admin_quorum
    };

    // Mint AdminProposal{ChangeThresholds}.
    let proposal = {
        let outbox_g = dm_outbox.lock().await;
        let signing_key = outbox_g.community_signing_key.as_ref();
        mint_admin_proposal_change_thresholds_event(
            space_id, self_owner, new_thresholds, signing_key, event_hlc,
        )?
    };
    let proposal_id_hex = hex::encode(proposal.id);
    let outcome = engine_arc.insert_local_event(proposal).await
        .map_err(|e| format!("engine.insert_local_event (AdminProposal change_thresholds): {e}"))?;
    if matches!(outcome, crate::community_state_crdt::InsertOutcome::Rejected(_)) {
        return Err(membership_outcome_err("propose_change_thresholds (AdminProposal)", &outcome));
    }
    if admin_quorum == 1 {
        Ok(AdminActionResult::Completed)
    } else {
        Ok(AdminActionResult::Pending {
            proposal_event_id: proposal_id_hex,
            signers_so_far: 1,
            quorum_required: admin_quorum,
        })
    }
}
```

- [ ] **Step 6: Register** `propose_change_thresholds,` in `generate_handler!` next to `propose_change_quorum,` (~:65101).

- [ ] **Step 7: Run tests + compile** — `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(compute_governance_surfaces_power_thresholds)'` (PASS), and a scoped clippy on the lib to catch the new command's warnings: `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`.

- [ ] **Step 8: Commit** — `feat(zeb-251): propose_change_thresholds IPC + governance DTO thresholds`.

---

## Task 4: TypeScript types + service getter

**Files:**
- Modify: `src/lib/types.ts` (~:485–:520)
- Modify: `src/lib/community-service.ts` (`getCommunityGovernance` ~:618)
- Test: `src/lib/__tests__/community-service.test.ts` (or the existing service test file — mirror an existing `getCommunityGovernance`/governance test if present)

**Interfaces:**
- Produces: `CommunityGovernance` TS type `{ adminQuorum, invite, kick, setPower, max }`; `getCommunityGovernance(): Promise<CommunityGovernance>`.

- [ ] **Step 1: Write the failing test** — the mocked IPC returns thresholds and the service passes them through:

```ts
it('getCommunityGovernance returns per-community thresholds', async () => {
  mockInvoke.mockResolvedValueOnce({ adminQuorum: 1, invite: 25, kick: 60, setPower: 100, max: 100 });
  const gov = await service.getCommunityGovernance('00'.repeat(16));
  expect(gov).toEqual({ adminQuorum: 1, invite: 25, kick: 60, setPower: 100, max: 100 });
});
```

*(Match the existing service-test harness for mocking `invoke` — reuse its `mockInvoke` setup, do not introduce a new one.)*

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/__tests__/community-service.test.ts`. Expected: FAIL (type/shape mismatch).

- [ ] **Step 3: Add the TS type** in `types.ts` (near `POWER_THRESHOLDS` ~:485; also fix the stale `:1108` → `:5259` comment on the const):

```ts
/** ZEB-608/ZEB-251: read-only governance snapshot for a community. */
export interface CommunityGovernance {
  adminQuorum: number;
  invite: number;
  kick: number;
  setPower: number;
  max: number;
}
```

- [ ] **Step 4: Widen `getCommunityGovernance`** in `community-service.ts` (~:618) to return `Promise<CommunityGovernance>` (import the type), body otherwise unchanged:

```ts
  async getCommunityGovernance(communityId: string): Promise<CommunityGovernance> {
    try {
      return await this.invoke<CommunityGovernance>('get_community_governance', { communityId });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(`getCommunityGovernance: ${msg}`);
    }
  }
```

- [ ] **Step 5: Run test to verify pass** — `npx vitest run src/lib/__tests__/community-service.test.ts` (PASS) + `npx tsc --noEmit` (clean; fix any callers the widened return type breaks — e.g. a caller destructuring only `adminQuorum` still works).

- [ ] **Step 6: Commit** — `feat(zeb-251): CommunityGovernance TS type carries per-community thresholds`.

---

## Task 5: `ChangeThresholdsDialog.svelte`

**Files:**
- Create: `src/lib/components/ChangeThresholdsDialog.svelte`
- Create: `src/lib/components/__tests__/ChangeThresholdsDialog.test.ts`

**Interfaces:**
- Consumes: `propose_change_thresholds` IPC (Task 3); `AdminActionResult` from `../types`.
- Produces: `<ChangeThresholdsDialog communityId currentThresholds={{invite,kick,setPower}} onClose />`.

- [ ] **Step 1: Write the failing test** — clone `src/lib/components/__tests__/ChangeQuorumDialog.test.ts` and adapt: render with `currentThresholds={{ invite: 0, kick: 50, setPower: 100 }}`, edit the invite input to 25, click "Propose change", assert `invoke` was called with `('propose_change_thresholds', { communityId, invite: 25, kick: 50, setPower: 100 })`; and a validation test that an ordering-invalid entry (kick < invite) disables submit. Reuse ChangeQuorumDialog.test.ts's mock/render scaffolding.

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/components/__tests__/ChangeThresholdsDialog.test.ts`. Expected: FAIL (component missing).

- [ ] **Step 3: Create the dialog** by cloning `src/lib/components/ChangeQuorumDialog.svelte` and applying these changes (keep the `<dialog>`/`showModal`/`handleUserCancel`/submitting/error structure, the `.actions` buttons, and the `<style>` block **verbatim** — only the props, the three inputs, the validation, and the `invoke` change):
  - **Props:** replace `currentQuorum`/`currentAdminCount` with `currentThresholds: { invite: number; kick: number; setPower: number }`.
  - **State:** `let invite = $state(untrack(() => currentThresholds.invite))`, same for `kick`, `setPower`.
  - **Derived validity:** `let orderingOk = $derived(invite <= kick && kick <= setPower && setPower <= 100)`.
  - **Body:** three `control-row`s, each a paired `range` + `number` input (`min=0 max=100`) bound to `invite`/`kick`/`setPower` respectively, with `aria-label`s "Invite threshold", "Kick threshold", "Set-power threshold" (the paired slider+number pattern matches the quorum dialog and the repo's slider+number-input convention).
  - **Warning copy** (one line, mirroring the quorum dialog's `⚖` note): "⚖ This change is itself an admin action — it needs the current quorum to take effect."
  - **`propose()`:** guard `if (!orderingOk) { errorMessage = 'Require 0 ≤ invite ≤ kick ≤ set power ≤ 100.'; return; }` then `const result = await invoke<AdminActionResult>('propose_change_thresholds', { communityId, invite, kick, setPower });` then `handleClose()` on Completed **or** Pending (same as quorum).
  - **Submit button** `disabled={submitting || !orderingOk || (invite === currentThresholds.invite && kick === currentThresholds.kick && setPower === currentThresholds.setPower)}`.

- [ ] **Step 4: Run test to verify pass** — `npx vitest run src/lib/components/__tests__/ChangeThresholdsDialog.test.ts` (PASS) + `npx tsc --noEmit` (clean).

- [ ] **Step 5: Commit** — `feat(zeb-251): ChangeThresholdsDialog admin UI`.

---

## Task 6: Wire the dialog into `CommunitySettingsPanel` + per-community gates + pending rendering

**Files:**
- Modify: `src/lib/components/CommunitySettingsPanel.svelte` (imports ~:1; gates ~:135–:198,:357–:366,:557; Admin-governance section ~:571; dialog mount ~:648; `$state` ~:114)
- Modify: `src/lib/components/PendingAdminProposalsPanel.svelte` (`ProposalKindDto` union ~:6; `proposalSummary` ~:140)
- Test: extend the existing `CommunitySettingsPanel` and `PendingAdminProposalsPanel` test files

**Interfaces:**
- Consumes: `ChangeThresholdsDialog` (Task 5), `getCommunityGovernance` (Task 4), `ProposalKindDto::ChangeThresholds` shape (Task 2/3).

- [ ] **Step 1: Write the failing tests** — (a) in the PendingAdminProposalsPanel test file, a pending `{ kind: 'ChangeThresholds', invite: 25, kick: 60, set_power: 100 }` renders a summary like `"Change thresholds to invite 25 / kick 60 / set-power 100"`; (b) in the CommunitySettingsPanel test file, an admin sees a "Change thresholds…" button in the Admin-governance section. Mirror the existing `ChangeQuorum`/"Change quorum…" tests in those files.

- [ ] **Step 2: Run to verify failure** — `npx vitest run src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts src/lib/components/__tests__/CommunitySettingsPanel.test.ts`. Expected: FAIL.

- [ ] **Step 3: PendingAdminProposalsPanel** — add the union member (~:6, matching the Rust `ProposalKindDto`, snake_case fields per `#[serde(tag="kind")]`):

```ts
    | { kind: 'ChangeThresholds'; invite: number; kick: number; set_power: number }
```

and a `proposalSummary` case (~:162):

```ts
      case 'ChangeThresholds':
        return `Change thresholds to invite ${kind.invite} / kick ${kind.kick} / set-power ${kind.set_power}`;
```

- [ ] **Step 4: CommunitySettingsPanel — source per-community thresholds.** `adminQuorum` is a **prop** on this component (extraction: `CommunitySettingsPanel.svelte:57`, default `1`). Locate the parent that renders `<CommunitySettingsPanel adminQuorum={…}>` (grep `adminQuorum=` and `getCommunityGovernance` in `src/`) — it obtains `adminQuorum` from a `getCommunityGovernance` call. Extend that same call site to also read the new `invite`/`kick`/`setPower` fields and pass a new `thresholds` prop into `CommunitySettingsPanel`. In `CommunitySettingsPanel`, declare the prop with a default equal to the const so nothing regresses before governance loads:

```svelte
  let { /* …existing props… */ thresholds = { invite: POWER_THRESHOLDS.invite, kick: POWER_THRESHOLDS.kick, setPower: POWER_THRESHOLDS.setPower } } = $props();
```

Then swap the const reads in the derived gates to `thresholds.*`: `adminCount`/`amOnlyAdmin`/`canModerate`/`canAdmin` (~:187–:198), `crossesAdminThreshold` (~:135), `canKick`/`canSetPower` (~:357–:366), and `{#if myPower >= POWER_THRESHOLDS.invite}` (~:557). (Note the field name is `setPower` on the prop, matching the camelCase governance DTO.) Because the default equals the const, an un-customized community behaves exactly as today.

- [ ] **Step 5: CommunitySettingsPanel — add the dialog.** Add `let showChangeThresholdsDialog = $state(false)` (~:114); in the Admin-governance section (~:585, after the "Change quorum…" button) add:

```svelte
        <button class="change-quorum-btn" onclick={() => (showChangeThresholdsDialog = true)}>
          Change thresholds…
        </button>
```

import `ChangeThresholdsDialog`, and mount it near the quorum dialog (~:655):

```svelte
    {#if showChangeThresholdsDialog && canAdmin}
      <ChangeThresholdsDialog
        {communityId}
        currentThresholds={{ invite: thresholds.invite, kick: thresholds.kick, setPower: thresholds.setPower }}
        onClose={() => (showChangeThresholdsDialog = false)}
      />
    {/if}
```

- [ ] **Step 6: Run tests to verify pass** — `npx vitest run src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts src/lib/components/__tests__/CommunitySettingsPanel.test.ts` (PASS) + `npx tsc --noEmit` (clean).

- [ ] **Step 7: Commit** — `feat(zeb-251): wire ChangeThresholdsDialog + per-community gates + pending summary`.

---

## Final gate (after all tasks)

- [ ] **Rust full sweep** (from `src-tauri/`): `cargo fmt --all -- --check` && `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` && `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. (This relinks integration binaries — budget ~50 min; run once, at the end.)
- [ ] **Frontend full sweep** (from repo root): `npx tsc --noEmit` && `npx vitest run`.
- [ ] Independent whole-branch code review (superpowers:requesting-code-review), then PR.
