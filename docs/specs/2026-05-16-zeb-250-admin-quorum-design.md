# ZEB-250: M-of-N admin quorum for community governance

**Branch:** `zeb-250-admin-quorum`
**Linear:** [ZEB-250](https://linear.app/zeblith/issue/ZEB-250) (M-of-N admin recovery for communities)
**Parent epic:** [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) (Sub-C v1)
**Related:** [ZEB-254](https://linear.app/zeblith/issue/ZEB-254) (PendingJoin+JoinCountersign pattern this spec generalizes), [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) (community forking — the universal escape-hatch per §1.4)

## 1. Context

### 1.1 Origin

Surfaced during ZEB-217 (Sub-C v1) brainstorm as an explicit governance non-goal of v1. Sub-C v1 ships with a single-point-of-failure: if the sole power-100 admin loses their private key, no one can promote new admins. Community is functional but ungovernable.

ZEB-254 (offline counter-signer queue, merged 2026-05-16) established the **PendingJoin + JoinCountersign** event-pair pattern — the joiner proposes via a self-signed event, an admin contributes one countersignature, materialize pairs them. ZEB-250 generalizes this pattern to N-of-M admin approval.

### 1.2 The actual failure mode

| Admin configuration | Status under v1 | Recovery |
|---|---|---|
| 1 admin, loses keys | Community ungovernable — no one can promote replacement admins | Fork (ZEB-285) |
| 2 admins, both reachable | Healthy — either can govern | n/a |
| 2 admins, 1 loses keys | Same as 1-admin-loses-keys (the other still has full power but is a SPOF) | (ZEB-250 introduces survival path via quorum) |
| N admins with no quorum mechanism | Any single rogue admin can demote / kick / change-quorum others | (ZEB-250 introduces multi-sig guard) |

### 1.3 What ZEB-250 lands

1. A new **per-community `admin_quorum: u8`** field on `CommunityState`. Default `1` (current single-admin behavior). Communities opt into multi-sig by raising it.
2. A new **CRDT event pair**: `MembershipEventKind::AdminProposal { proposal_kind }` + `MembershipEventKind::AdminCountersign { target_event_id }`. Generalizes ZEB-254's PendingJoin+JoinCountersign.
3. **Materialize pre-pass** collects per-proposal admin signatures (proposer counts as 1; AdminCountersign actors add). Proposal becomes effective when `unique_signers >= admin_quorum`.
4. **30-day expiry** on AdminProposals (pure-function, mirrors PendingJoin).
5. **Modified verify rules** on existing `SetPower` and `Kick` events: rejected at verify when `admin_quorum > 1` AND the action is admin-affecting (promotes to admin, demotes an admin, or kicks an admin). They must arrive as AdminProposal instead.
6. **No new IPCs for action proposal** — existing `set_power_level` and `kick_from_community` auto-route to AdminProposal vs direct event based on community's `admin_quorum`. New IPCs: `list_pending_admin_proposals`, `countersign_admin_proposal`, `propose_change_quorum`.
7. **`PendingAdminProposalsPanel.svelte`** mounted in CommunitySettingsPanel (admin-only, parallel to ZEB-254's PendingJoinsPanel). New `ChangeQuorumDialog.svelte` for raising/lowering quorum.

### 1.4 Recovery model

The ticket headline says "M-of-N admin recovery" but Sub-C v1's true recovery primitive is **forking** (ZEB-285). Concrete cases:

| Case | Recovery |
|---|---|
| `M-of-N` with `N > M`, 1 admin unreachable | Remaining N-1 admins form quorum normally. ✓ Works without special protocol. |
| `M-of-M` with 1 admin unreachable | Intentionally strict; community partially-stuck until either remaining admin reduces quorum (can't if quorum=M) or community forks. |
| 1 admin, lost keys | Fork is the only recovery (creator of fork becomes power-100 admin of the new community). |

ZEB-250 does NOT introduce a time-based admin-inactivity fallback. Documentation will recommend **N ≥ M+1** for survivability. The polycentric-governance memory frames forking as the universal escape-hatch.

## 2. Decisions (with rationale)

| Decision | Rationale |
|---|---|
| Accretive countersignatures (not threshold-sig crypto) | Generalizes proven ZEB-254 pattern. No new cryptographic primitive. Each countersign is a normal signed CRDT event. |
| Admin-affecting scope only: `SetPower{level: 100}`, `SetPower{target: admin, level: <100}`, `Kick{target: admin}`, change of `admin_quorum` | Quorum protects governance, not day-to-day moderation. A single mod's spam-kick doesn't need to wait for N signatures. Smallest surface that fixes the actual failure mode. |
| Default `admin_quorum = 1`; opt-in raise | Backwards compat with all existing communities. First raise from 1 to 2 happens under quorum=1 with single signature (no chicken-and-egg). |
| Forking is the universal escape-hatch (no time-based fallback) | Per `project_harmony_polycentric_governance` memory — communities are sovereign; secession is always available. ZEB-285 already covers it. |
| Proposer's signature counts toward quorum | `admin_quorum=2` means proposer + 1 countersigner. Mirrors ZEB-254's PendingJoin (joiner self-signs + 1 admin countersign). |
| Direct admin-affecting events REJECTED when `admin_quorum > 1` | Forces the protocol path. Backwards compat preserved for `admin_quorum=1` communities. |
| Lenient forward-ref AdminCountersign | Verify doesn't require target AdminProposal to be present yet. Pairing at materialize time. Mirrors ZEB-254. |
| 30-day expiry on AdminProposals | Pure-function check at materialize. Late countersigns to expired proposals are no-ops. Matches ZEB-254's PendingJoin expiry. |
| No withdrawal — propose inverse to undo | Append-only CRDT. To revoke an effective action, propose the opposite. |
| Leave is self-determined (no quorum) | Per polycentric memory — personal liberty + free association. Documented: don't set `admin_quorum > current_admin_count - 1` if you want survivability. |
| Fork: forker is sole admin with quorum=1 (fresh start) | Per ZEB-285 — fork is fresh sovereign entity, not a governance continuation. Avoids the trap of inheriting an unreachable quorum. |
| New `PendingAdminProposalsPanel.svelte` + `ChangeQuorumDialog.svelte` (admin-only) | Parallel to ZEB-254's PendingJoinsPanel. Separate from PendingJoinsPanel for v1 (consolidation deferred). |
| Existing `set_power_level` / `kick_from_community` auto-route based on `admin_quorum` | Frontend doesn't need to know which path. IPC returns discriminated `AdminActionResult { Completed \| Pending { ... } }`. |
| No auto-countersign hook for admin proposals | Deliberate UX. Admin actions are governance, not bot-driven. Admins explicitly countersign via panel. |

## 3. Wire surface

### 3.1 New `CommunityState.admin_quorum` field

Added to `CommunityState` in `src-tauri/src/community_state_crdt.rs`:

```rust
/// ZEB-250: M-of-N admin quorum. Number of admin-tier signatures
/// required for admin-affecting actions (SetPower to/from 100, Kick of
/// an admin, change of admin_quorum itself).
///
/// Default 1 (single-admin governance — the proposer's signature alone
/// suffices). When raised >= 2, admin-affecting actions must arrive as
/// AdminProposal (with >= N-1 AdminCountersigns) instead of direct
/// SetPower/Kick events. Backwards-compatible: pre-ZEB-250 blobs lack
/// this field and decode as default 1.
#[serde(
    rename = "aq",
    default = "default_admin_quorum",
    skip_serializing_if = "is_default_admin_quorum"
)]
pub admin_quorum: u8,

// In the impl module:
fn default_admin_quorum() -> u8 { 1 }
fn is_default_admin_quorum(q: &u8) -> bool { *q == 1 }
```

CBOR key `"aq"`. Skip-if-default-1 → byte-compatible with pre-ZEB-250 blobs. Manual `Clone` and `PartialEq` impls on CommunityState extended to include the new field.

### 3.2 New `MembershipEventKind::AdminProposal` variant

Added to the enum in `src-tauri/src/community_membership.rs`:

```rust
/// ZEB-250: a power-100 admin proposes an admin-affecting action.
/// Becomes effective only when the proposal accumulates >= admin_quorum
/// total admin signatures (proposer counts as 1; remainder come from
/// AdminCountersign events targeting this event_id).
///
/// 30-day expiry: if quorum isn't reached within 30 days of the
/// proposal's HLC wall_ms, the proposal is dead (pure-function check
/// at materialize time). Late countersigns to expired proposals are
/// no-ops.
///
/// Variant tag "q" (1-char value, lowercase, unused before this).
/// Inner field key "pk" (proposal_kind) per same-length-keys invariant.
#[serde(rename = "q")]
AdminProposal {
    #[serde(rename = "pk")]
    proposal_kind: ProposalKind,
},
```

### 3.3 New `ProposalKind` nested enum

Defined alongside `MembershipEventKind`:

```rust
/// ZEB-250: shape of the proposed admin-affecting action. Mirrors
/// existing single-signed event variants but wrapped for quorum
/// approval. Same-length-keys invariant: 1-char variant tags
/// `s` / `k` / `c`, 2-char inner field keys.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kd", content = "bd")]
pub enum ProposalKind {
    /// SetPower whose target IS currently an admin (level was 100) OR
    /// whose new level IS 100 (promoting to admin).
    #[serde(rename = "s")]
    SetPower {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "lv")]
        level: u8,
    },
    /// Kick of a target who is currently an admin (level == 100).
    #[serde(rename = "k")]
    Kick {
        #[serde(rename = "tg")]
        target: OwnerAddr,
        #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    /// Change `CommunityState.admin_quorum`. New value must be >= 1.
    /// Practical cap: <= current admin count (enforced at verify_event AP5).
    #[serde(rename = "c")]
    ChangeQuorum {
        #[serde(rename = "nq")]
        new_quorum: u8,
    },
}
```

Tagged-union representation (`#[serde(tag = "kd", content = "bd")]`) so the CBOR encoding has explicit discriminator + body keys at the ProposalKind level.

### 3.4 New `MembershipEventKind::AdminCountersign` variant

```rust
/// ZEB-250: admin-tier countersignature on a target AdminProposal.
/// Lenient forward-ref — verify_event doesn't require target to be
/// present yet. Pairing happens at materialize time.
///
/// Variant tag "n" (1-char value, lowercase, unused before this).
#[serde(rename = "n")]
AdminCountersign {
    #[serde(rename = "ti")]
    target_event_id: EventId,
},
```

### 3.5 Variant tag table (after this change)

| Tag | Variant |
|-----|---------|
| `j` | Join |
| `l` | Leave |
| `i` | Invite |
| `k` | Kick |
| `p` | SetPower |
| `u` | Unban |
| `c` | ChannelCreate |
| `m` | ChannelModify |
| `d` | ChannelDelete |
| `r` | EpochRotation |
| `f` | EpochCatchup |
| `x` | Fork (ZEB-285) |
| `g` | PendingJoin (ZEB-254) |
| `y` | JoinCountersign (ZEB-254) |
| **`q`** | **AdminProposal (new — ZEB-250)** |
| **`n`** | **AdminCountersign (new — ZEB-250)** |

### 3.6 Backwards compatibility

- Pre-ZEB-250 `CommunityState` blobs (lacking `aq` key) decode as `admin_quorum = 1` via `serde(default)`.
- Pre-ZEB-250 communities encode byte-identically post-this-change (default-value skip).
- Pre-ZEB-250 clients receiving `q` / `n` events: `MembershipEventKind` does not use `#[serde(deny_unknown_fields)]` (preserves forward-compat); old clients silently drop unknown variants. Old clients can't see new admin-quorum activity but their own behavior is preserved.

## 4. Verify rules

### 4.1 AdminProposal verify (5 gates — AP1 through AP5)

| Gate | Description | Failure mode |
|---|---|---|
| **AP1** | Actor is Joined at proposal HLC | `AdminProposalActorNotJoined` |
| **AP2** | Actor's power at proposal HLC ≥ 100 | `AdminProposalActorNotAdmin` |
| **AP3** | `proposal_kind` is well-formed — see §4.2 | `AdminProposalKindInvalid` |
| **AP4** | `proposal_kind` matches admin-affecting criteria — see §4.3 | `AdminProposalNotAdminAffecting` |
| **AP5** | If `proposal_kind == ChangeQuorum`: `new_quorum >= 1` AND `new_quorum <= count_of_current_admins` | `AdminProposalQuorumOutOfRange` |

### 4.2 ProposalKind well-formedness (AP3)

```text
SetPower { target, level }:
  - target exists in prior_state.members (any status)
  - level is in [0, 100]

Kick { target, reason }:
  - target exists in prior_state.members
  - target.status is Joined (banned/left targets don't make sense to kick)
  - reason is None OR a non-empty UTF-8 string

ChangeQuorum { new_quorum }:
  - new_quorum >= 1
  - (range check against admin count happens in AP5)
```

### 4.3 Admin-affecting criteria (AP4)

A `proposal_kind` IS admin-affecting iff:

- **`SetPower`**: `level == 100` (promoting to admin) **OR** `prior_state.power_levels[target] == 100` (demoting an admin)
- **`Kick`**: `prior_state.power_levels[target] == 100` (kicking an admin)
- **`ChangeQuorum`**: always admin-affecting

Wrapping a non-admin-affecting action in AdminProposal is a category error and rejected at verify.

### 4.4 AdminCountersign verify (3 gates — AC1 through AC3)

| Gate | Description | Failure mode |
|---|---|---|
| **AC1** | Actor is Joined at countersign HLC | `AdminCountersignActorNotJoined` |
| **AC2** | Actor's power at countersign HLC ≥ 100 | `AdminCountersignActorNotAdmin` |
| **AC3** | `target_event_id` is non-zero / well-formed | `AdminCountersignTargetIdMalformed` |

**Lenient forward-ref**: AC verify does NOT require the target AdminProposal to be present in the event log yet. Out-of-order CRDT delivery is normal. Pairing happens at materialize time. Mirrors ZEB-254's JoinCountersign semantics.

**No P4 "actor != proposer" check**: even if a malicious admin tries to double-count, materialize deduplicates by actor in the HashSet (§5.1). Verify-side check would require resolving the target event, which is forward-ref-unfriendly.

### 4.5 Modified verify for direct `SetPower`

Existing rule (single-admin, current v1): actor is Joined + power ≥ POWER_THRESHOLDS.set_power (=100).

**Added in ZEB-250**: if `prior_state.admin_quorum > 1` AND the action is admin-affecting per §4.3:

- REJECT with `SetPowerRequiresQuorum` — must arrive as AdminProposal instead.

If `admin_quorum == 1` (default), existing rule applies — direct SetPower events accepted as before. Backwards-compat.

### 4.6 Modified verify for direct `Kick`

Existing rule: actor is Joined + power ≥ POWER_THRESHOLDS.kick (=50).

**Added in ZEB-250**: if `prior_state.admin_quorum > 1` AND `prior_state.power_levels[target] == 100`:

- REJECT with `KickRequiresQuorum` — kicking an admin must arrive as AdminProposal.

Kicking a non-admin (mod or regular member) remains a single-signed action regardless of `admin_quorum`.

### 4.7 Backwards-compat for historical events

Verify operates at-time-of-event using `prior_state`. Historical SetPower/Kick events from before `admin_quorum` was raised verify under the then-current `admin_quorum` value (which was implicitly 1 for all pre-ZEB-250 history). They remain valid forever — no retroactive invalidation.

### 4.8 Bootstrap edge case: first raise from quorum=1 to quorum=2

The very first `ChangeQuorum { new_quorum: 2 }` proposal happens under `admin_quorum == 1`. Per AP5, `new_quorum (2) <= current_admin_count`, so the community needs ≥ 2 admins before this raise is valid.

When only 1 admin exists and they propose `ChangeQuorum { new_quorum: 2 }`, AP5 fails. The natural bootstrap path:

1. Lone admin promotes a second admin via direct `SetPower { level: 100 }` (allowed under quorum=1).
2. Now there are 2 admins.
3. Either admin proposes `ChangeQuorum { new_quorum: 2 }`. AP5 passes (2 ≤ 2). The proposer's own signature counts as 1 → quorum=1 satisfied → proposal effective immediately.
4. After this, all admin-affecting actions require 2 signatures.

## 5. Materialize semantics

### 5.1 Pre-pass: collect raw signature data

ZEB-254 introduced a pre-pass over the event log to collect `countersigned_pending_ids: HashSet<EventId>` (PendingJoin events with ≥ 1 JoinCountersign). ZEB-250 extends the pre-pass with structures for admin quorum — **raw signature collection only**. The quorum-reached evaluation happens in the main pass (§5.2) because `admin_quorum` itself changes over time (via ChangeQuorum proposals), creating a recursive dependency that's cleanly resolved by single-pass-with-running-state.

**New pre-pass state:**

```rust
// EventId of an AdminProposal → set of admin OwnerAddrs who have
// signed it (proposer auto-included; AdminCountersign actors add).
quorum_signers: HashMap<EventId, HashSet<OwnerAddr>>,

// EventId → (proposal_kind, proposer_actor, proposer_wall_ms).
proposals_index: HashMap<EventId, (ProposalKind, OwnerAddr, u64)>,

// EventId of a proposal → list of (wall_ms, actor) for each signing event
// (proposer + each countersign). Used by the main pass to determine when
// the N-th signature was contributed.
proposal_signing_hlcs: HashMap<EventId, Vec<(u64, OwnerAddr)>>,
```

**Pre-pass walk (arbitrary order):**

```text
for each signed_event in events.values():
  match signed_event.kind {
    AdminProposal { proposal_kind } => {
      proposals_index.insert(
        signed_event.event_id,
        (proposal_kind.clone(), signed_event.actor, signed_event.at.wall_ms),
      );
      // Proposer's own signature counts.
      quorum_signers.entry(signed_event.event_id)
        .or_insert_with(HashSet::new)
        .insert(signed_event.actor);
      proposal_signing_hlcs.entry(signed_event.event_id)
        .or_insert_with(Vec::new)
        .push((signed_event.at.wall_ms, signed_event.actor));
    }
    AdminCountersign { target_event_id } => {
      quorum_signers.entry(*target_event_id)
        .or_insert_with(HashSet::new)
        .insert(signed_event.actor);
      proposal_signing_hlcs.entry(*target_event_id)
        .or_insert_with(Vec::new)
        .push((signed_event.at.wall_ms, signed_event.actor));
    }
    _ => {}
  }
```

No quorum-reached evaluation here. The pre-pass is purely about indexing.

### 5.2 Main pass — apply admin-affecting effects with running `admin_quorum`

The main pass iterates events in HLC ascending order, maintaining the running materialized state. `admin_quorum` is a field on the running state, initialized to 1 (or whatever the pre-ZEB-250 CommunityState carries via `serde(default)`). It mutates via applied ChangeQuorum effects.

When the main pass encounters an AdminProposal:

```text
on AdminProposal event:
  let admin_quorum_now = materialized_state.admin_quorum;
    // = the running value AT THIS POINT in the main pass.

  let signers = quorum_signers.get(&event_id).map(|s| s.len()).unwrap_or(0);
  let signing_hlcs = proposal_signing_hlcs.get(&event_id).cloned().unwrap_or_default();

  // Determine if quorum was reached, and at which HLC:
  if signers >= admin_quorum_now {
    // Sort signing events by wall_ms ascending; the (admin_quorum_now)-th
    // entry is the one that pushed the count over the threshold.
    // (1-indexed: 1st, 2nd, ... so index = admin_quorum_now - 1.)
    let mut sorted = signing_hlcs.clone();
    sorted.sort_by_key(|(wall_ms, _)| *wall_ms);
    let nth_signer_wall_ms = sorted[admin_quorum_now as usize - 1].0;

    // 30-day expiry check: quorum must have been reached within 30
    // days of the proposal's HLC.
    let age_when_reached = nth_signer_wall_ms - signed_event.at.wall_ms;
    if age_when_reached <= 30 * DAYS_MS {
      apply_proposal_effect(materialized_state, proposal_kind);
      // For ChangeQuorum, this updates materialized_state.admin_quorum,
      // which the running state propagates to subsequent iterations.
    }
    // else: quorum reached too late; proposal is dead, no effect.
  }
  // else: insufficient signatures; proposal stays pending. No state mutation.
```

`apply_proposal_effect` translates the `ProposalKind` into the same state mutation that direct `SetPower` / `Kick` would produce: updates `power_levels[target]`, removes target from `members` or sets status, or updates the `admin_quorum` field for ChangeQuorum proposals. The materialize is HLC-deterministic across replicas.

**Recursive-dependency note**: a quorum-reached ChangeQuorum at HLC X updates `admin_quorum`. Subsequent AdminProposals at HLC > X are evaluated against the NEW admin_quorum. The single-pass-with-running-state algorithm handles this cleanly because the main pass observes mutations in iteration order.

### 5.3 Late-quorum semantics + expiry permanence

Two subtle cases:

**Case A — countersign lands AFTER 30-day window**: Pre-pass adds the late countersigner to `quorum_signers`, but `quorum_reached_at` records the HLC of the N-th signer (which is now past expiry). Main pass sees `age_when_reached > 30d` → no effect. UI surfaces "Expired — propose again". Consistent with ZEB-254's countersign-doesn't-revive-expired semantics.

**Case B — quorum reached before expiry, then more events arrive**: `quorum_reached_at[event_id]` is fixed at the N-th-signer's HLC. Subsequent events advancing `current_max_wall_ms` don't change the recorded "when quorum was reached". Effect remains applied. Permanence guaranteed.

### 5.4 Idempotency and dedup

`quorum_signers` uses `HashSet<OwnerAddr>` — duplicate AdminCountersign events from the same actor are silently deduped. Materialize is HLC-deterministic; replicas computing the same materialize see the same effect-application order.

### 5.5 No auto-countersign hook

ZEB-254's `maybe_spawn_auto_counter_sign` hook auto-countersigns PendingJoin events when an admin engine first sees one. ZEB-250 does NOT add a parallel hook for AdminProposal.

Rationale: admin proposals are deliberate governance actions. Requiring an admin to consciously approve in the UI is the right UX. Auto-countersigning would defeat the multi-sig purpose — a single rogue admin could rapid-fire proposals and rely on auto-countersign from another.

Admins discover pending proposals via `PendingAdminProposalsPanel` (§7) and explicitly countersign.

### 5.6 ChangeQuorum effect application

When a `ChangeQuorum { new_quorum }` proposal reaches quorum and is within the 30-day window, the effect is:

```text
materialized_state.admin_quorum = new_quorum
```

This change affects verify rules for SUBSEQUENT events at HLCs after the change. It does NOT retroactively invalidate prior events (verify is at-time-of-event per §4.7).

## 6. IPC surface

### 6.1 Auto-routing of existing IPCs

`set_power_level(community_id, target_addr, level)` and `kick_from_community(community_id, target_addr, reason?)` already exist. ZEB-250 extends their backend implementations:

```text
set_power_level(community_id, target_addr, level):
  - Read community's current admin_quorum.
  - Determine whether action is admin-affecting per §4.3.
  - If admin_quorum > 1 AND admin-affecting:
      mint AdminProposal { proposal_kind: SetPower { target, level } }
      return AdminActionResult::Pending { proposal_event_id, signers_so_far: 1, quorum_required }
  - Else:
      mint direct SetPower event (existing behavior)
      return AdminActionResult::Completed
```

Similarly for `kick_from_community`.

```rust
#[derive(serde::Serialize)]
#[serde(tag = "kind")]
pub enum AdminActionResult {
    /// Action completed immediately (admin_quorum == 1 OR action not admin-affecting).
    Completed,
    /// Action proposed; awaiting countersignatures.
    Pending {
        proposal_event_id: String,    // hex EventId
        signers_so_far: u8,           // 1 (proposer's signature)
        quorum_required: u8,
    },
}
```

Frontend UI uses the discriminator to choose between "Done" toast vs "Pending — 1 of N signatures collected" feedback.

### 6.2 New IPC: `list_pending_admin_proposals`

```rust
#[tauri::command]
async fn list_pending_admin_proposals(
    community_id: String,
    state: tauri::State<'_, ...>,
) -> Result<Vec<PendingAdminProposalDto>, String>
```

**Authorization**: caller must be Joined + power ≥ 100.

**Behavior**:

- Walk membership event log filtering for AdminProposal variants.
- Per proposal, compute: `signers_so_far`, `quorum_required`, `signers_remaining`, `expired`, `effective`, `self_has_signed`, `signer_display_names`.
- Sort: pending first (chronological), then effective (chronological), then expired (chronological).

**DTO**:

```rust
#[derive(serde::Serialize)]
pub struct PendingAdminProposalDto {
    pub event_id: String,                       // hex
    pub proposer_addr: String,                  // hex
    pub proposer_display_name: Option<String>,
    pub proposal_kind: ProposalKindDto,         // friendly tagged-union
    pub proposed_at_wall_ms: u64,
    pub signers_so_far: u8,
    pub quorum_required: u8,
    pub expired: bool,
    pub effective: bool,
    pub self_has_signed: bool,
    pub signer_display_names: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind")]
pub enum ProposalKindDto {
    SetPower {
        target_addr: String,
        target_display_name: Option<String>,
        level: u8,
    },
    Kick {
        target_addr: String,
        target_display_name: Option<String>,
        reason: Option<String>,
    },
    ChangeQuorum {
        new_quorum: u8,
    },
}
```

### 6.3 New IPC: `countersign_admin_proposal`

```rust
#[tauri::command]
async fn countersign_admin_proposal(
    community_id: String,
    proposal_event_id: String,
    state: tauri::State<'_, ...>,
) -> Result<CountersignResult, String>
```

**Authorization**: caller is Joined + power ≥ 100.

**Behavior**:

- Validate: proposal exists, not already quorum-reached, not expired.
- Check: caller hasn't already signed (idempotent — re-signing returns Ok no-op).
- Mint AdminCountersign event with `target_event_id`.
- Insert via existing engine path. Triggers materialize side-effects (e.g., `CommunityMembershipDelta` emit).

**Return**:

```rust
#[derive(serde::Serialize)]
pub struct CountersignResult {
    pub signers_after: u8,
    pub quorum_required: u8,
    pub reached_quorum: bool,
}
```

Frontend uses `reached_quorum` to choose between "Approved (1 more needed)" toast and "Action approved — effective now" toast.

### 6.4 New IPC: `propose_change_quorum`

```rust
#[tauri::command]
async fn propose_change_quorum(
    community_id: String,
    new_quorum: u8,
    state: tauri::State<'_, ...>,
) -> Result<AdminActionResult, String>
```

**Authorization**: caller is Joined + power ≥ 100.

**Behavior**:

- Validate `new_quorum` per AP5 (≥ 1, ≤ current admin count).
- If current `admin_quorum == 1`: mint a single-signed (proposer's signature suffices) ChangeQuorum event via AdminProposal mechanism — quorum is met immediately on insert.
- Else: mint AdminProposal, returns Pending.

Returns the same `AdminActionResult` enum as §6.1.

### 6.5 No withdrawal IPC

Per Q7 / §2: append-only CRDT, no withdrawal. To revoke an effective action, propose the inverse.

### 6.6 No separate "list recent admin actions" IPC

`list_pending_admin_proposals` already surfaces effective and expired proposals via the `effective` and `expired` flags. Frontend filters into sections client-side.

## 7. UI surface

### 7.1 `PendingAdminProposalsPanel.svelte` (new)

Mounted in `CommunitySettingsPanel.svelte`, admin-only (gated on `power >= 100`).

**Inputs** (Svelte 5 props):

```typescript
{
  communityId: string;
  canAdmin: boolean;   // power >= 100 check from parent
  selfOwnerAddr: string;
}
```

**Internal state**: `proposals: PendingAdminProposalDto[]`, `loading`, `errorMessage`.

**Mount behavior** (`$effect` per ZEB-287 R3 + R4-1 lessons):

- If `!canAdmin`: skip fetch entirely; render minimal section header. **No leaked privileged data.** Bump `latestCallId` to discard any in-flight refresh from before canAdmin flipped to false.
- If `canAdmin`: fetch `list_pending_admin_proposals` on mount AND on `community-state-sync-converged` event.
- Per-call token (`latestCallId`) to discard stale async responses (per ZEB-287 R4-7).
- Per-watch token (`latestWatchId`) for re-running effects when `communityId` or `canAdmin` change (per ZEB-287 R3-3).

**Render shape**:

```text
Admin actions
─────────────

[ Pending — N more signatures needed ]

  ┌─ Promote @alice to admin ─────────────────┐
  │  Proposed by @bob · 2026-05-16            │
  │  Signed: 1 of 2  (you · bob)              │
  │  Status: 28 days remaining                │
  │  [ Countersign ]  or "Already signed ✓"   │
  └────────────────────────────────────────────┘

  ┌─ Change quorum to 3 ──────────────────────┐
  │  Proposed by @bob · 2026-05-15            │
  │  Signed: 1 of 2  (bob)                    │
  │  Status: 29 days remaining                │
  │  [ Countersign ]                          │
  └────────────────────────────────────────────┘

[ Recently approved ]                          (collapsed by default)
[ Expired without quorum ]                     (collapsed by default)
```

**Per-proposal card content (by `proposal_kind.kind`)**:

- `SetPower` with `level == 100`: "Promote @{target_name} to admin"
- `SetPower` with `level == 0`: "Demote @{target_name} from admin"
- `SetPower` with other level: "Change @{target_name}'s power to {level}"
- `Kick`: "Kick @{target_name}" + reason (if present) on a subline
- `ChangeQuorum`: "Change quorum to {new_quorum}"

**Countersign button** enabled when `canAdmin && !self_has_signed && !expired && !effective`. Disabled with tooltip otherwise. Click → invokes `countersign_admin_proposal`. Optimistic local update on success.

**Confirmation tier** (per `feedback_severe_action_confirmation`): countersigning is severe-but-reversible (via counter-proposal) → secondary-position click-confirm. The "Countersign" button itself is the confirm step.

### 7.2 Proposal-creation flow — extending existing UI

Existing member-list UI in `CommunitySettingsPanel.svelte` (Phase ZEB-284) has "promote to admin" / "demote" / "kick" affordances. ZEB-250 doesn't change the click affordance — clicking those still invokes `set_power_level` / `kick_from_community`.

**What changes**: the IPC response is now a discriminated `AdminActionResult`. UI handling:

- `Completed` → existing "Done" toast or member-list re-render
- `Pending { signers_so_far, quorum_required }` → toast: "Proposal submitted — {signers_so_far} of {quorum_required} signatures. Awaiting {quorum_required - signers_so_far} more."

The moderation-action dialogs themselves are unchanged.

### 7.3 New "Raise admin quorum" flow

A new "Admin governance" section in `CommunitySettingsPanel.svelte`, just above PendingAdminProposalsPanel:

```text
Admin governance
────────────────

Current admin quorum:  1 of N admins required for admin-affecting actions
                       [ Change quorum… ]

[ Pending admin proposals ]    ← PendingAdminProposalsPanel
```

**"Change quorum…" button** opens `ChangeQuorumDialog.svelte` (new):

- Slider + paired number input (per `feedback_slider_pair_with_number_input` memory) — `min: 1`, `max: current_admin_count`
- Explainer copy: "With quorum of {N}, admin actions need {N} signatures from current admins. Recommended N+1 admins for survivability."
- "Propose" button → invokes `propose_change_quorum(communityId, new_quorum)`.

**Decision**: dedicated `propose_change_quorum` IPC, not over-loading `set_power_level`. Simpler typing.

### 7.4 Member-list pending-state badge

Member-list rows show pending-state badges for members targeted by an active AdminProposal:

```text
alice — Member  ⏳ pending promotion to admin
charlie — Admin ⏳ pending demotion
```

**Implementation**: member-list iterates the already-fetched `proposals` array and computes per-target overlays. Avoids a new IPC. Click goes to PendingAdminProposalsPanel for the action affordance.

### 7.5 Hide rules

- **Admin governance section** (raise-quorum affordance + PendingAdminProposalsPanel) — rendered only when `canAdmin: true` (power ≥ 100). Non-admins don't need to see governance internals.
- Single-admin communities (`admin_quorum == 1`) — Admin governance section still renders (so the admin can opt into multi-sig). PendingAdminProposalsPanel will show empty in this case.

### 7.6 Accessibility

- PendingAdminProposalsPanel uses `<ul role="list">` of proposal cards
- Each card has `aria-label="Pending admin proposal: {summary}"`
- Countersign buttons have descriptive `aria-label`s including the proposal summary
- ChangeQuorumDialog uses focus trap + Escape-to-close (per existing modal conventions)

## 8. Testing strategy

### 8.1 Wire-format pinning (`tests/wire_format_zeb250_fixtures.rs`, new)

1. `admin_proposal_setpower_canonical_cbor`
2. `admin_proposal_kick_canonical_cbor`
3. `admin_proposal_change_quorum_canonical_cbor`
4. `admin_countersign_canonical_cbor`
5. `community_state_with_admin_quorum_canonical_cbor`
6. `community_state_default_quorum_omits_aq_key` (byte-compat with pre-ZEB-250)

Use regen-on-first-run pattern + structural CBOR-key checks via `ciborium::Value` (per ZEB-287 R1 lessons).

### 8.2 Unit tests (`community_membership.rs`)

**AdminProposal verify (10 tests)**:

1. `admin_proposal_accepted_when_actor_admin`
2. `admin_proposal_rejected_when_actor_not_joined`
3. `admin_proposal_rejected_when_actor_power_below_100`
4. `admin_proposal_setpower_rejected_when_target_not_in_members`
5. `admin_proposal_setpower_rejected_when_level_out_of_range`
6. `admin_proposal_setpower_rejected_when_not_admin_affecting`
7. `admin_proposal_kick_rejected_when_target_not_admin`
8. `admin_proposal_change_quorum_rejected_when_below_one`
9. `admin_proposal_change_quorum_rejected_when_exceeds_admin_count`
10. `admin_proposal_change_quorum_accepted_when_equals_admin_count` (boundary)

**AdminCountersign verify (4 tests)**:

11. `admin_countersign_accepted_when_actor_admin`
12. `admin_countersign_rejected_when_actor_not_joined`
13. `admin_countersign_rejected_when_actor_power_below_100`
14. `admin_countersign_accepted_when_target_not_present_yet` (forward-ref)

**Modified verify for existing variants (6 tests)**:

15. `direct_setpower_to_100_rejected_when_admin_quorum_above_1`
16. `direct_setpower_demote_admin_rejected_when_admin_quorum_above_1`
17. `direct_setpower_to_non_admin_accepted_regardless_of_quorum`
18. `direct_kick_of_admin_rejected_when_admin_quorum_above_1`
19. `direct_kick_of_mod_accepted_regardless_of_quorum`
20. `direct_setpower_admin_actions_accepted_when_admin_quorum_equals_1` (backwards-compat)

**Materialize (9 tests)**:

21. `materialize_proposal_without_countersigns_pending_when_quorum_above_1`
22. `materialize_proposal_effective_when_one_countersign_reaches_quorum_2`
23. `materialize_proposal_effective_when_two_countersigns_reach_quorum_3`
24. `materialize_proposal_dedups_duplicate_countersigns_by_same_actor`
25. `materialize_proposal_expires_at_30_days_without_quorum`
26. `materialize_proposal_late_countersign_after_expiry_is_noop`
27. `materialize_quorum_reached_within_30d_then_aged_past_30d_remains_effective`
28. `materialize_change_quorum_proposal_updates_admin_quorum_field`
29. `materialize_setpower_via_quorum_matches_direct_setpower_effect_at_quorum_1`

### 8.3 Integration tests (`tests/community_admin_quorum_integration.rs`, new)

1. `single_admin_community_unaffected_by_zeb250` — full backwards-compat regression
2. `two_admin_community_set_power_requires_countersign`
3. `three_admin_community_kick_admin_requires_two_signatures`
4. `change_quorum_bootstrap_path` (lone admin → 2 admins → raise quorum)
5. `lone_admin_loses_keys_community_unrecoverable_except_via_fork`
6. `quorum_reached_late_countersign_is_noop`
7. `quorum_reached_within_30d_then_aged_past_remains_effective`
8. `two_admin_community_admin_leaves_drops_below_quorum`
9. `fork_of_quorum_community_resets_to_quorum_1`

### 8.4 IPC unit tests

1. `list_pending_admin_proposals_rejects_non_admin_caller`
2. `list_pending_admin_proposals_returns_pending_and_recent_sections`
3. `list_pending_admin_proposals_resolves_proposer_and_signer_names`
4. `countersign_admin_proposal_idempotent_when_already_signed`
5. `countersign_admin_proposal_rejects_non_admin_caller`
6. `countersign_admin_proposal_rejects_expired_proposal`
7. `countersign_admin_proposal_returns_reached_quorum_true_on_threshold_tip`
8. `set_power_level_routes_to_proposal_when_quorum_above_1_and_target_becomes_admin`
9. `set_power_level_returns_completed_when_quorum_1` (backwards-compat)
10. `propose_change_quorum_rejects_out_of_range_values`

### 8.5 Frontend vitest tests

**`src/lib/components/__tests__/PendingAdminProposalsPanel.test.ts`** (new):

1. `non_admin_skips_fetch_and_listen_registration`
2. `renders_pending_proposal_cards_with_signers_count`
3. `countersign_button_disabled_when_self_already_signed`
4. `countersign_button_disabled_for_expired_proposals`
5. `recently_approved_section_renders_separately_when_collapsed_by_default`
6. `countersign_click_invokes_ipc_and_updates_optimistically`
7. `community_state_sync_converged_event_triggers_refresh`
8. `stale_async_response_after_communityid_change_is_discarded`

**`src/lib/components/__tests__/ChangeQuorumDialog.test.ts`** (new):

1. `slider_and_number_input_sync_bidirectionally`
2. `propose_button_disabled_when_quorum_outside_valid_range`
3. `propose_invokes_propose_change_quorum_ipc_with_new_value`
4. `explainer_text_present_for_survivability_recommendation`

**Augment `CommunitySettingsPanel.test.ts`**:

1. `admin_governance_section_renders_for_admin`
2. `admin_governance_section_hidden_for_non_admin`
3. `pending_promotion_badge_renders_on_target_member_row`

### 8.6 Manual smoke test (PR body)

Two-engine local run per ZEB-254/287 pattern:

1. Engine A creates community C, invites Engine B
2. A promotes B to admin (quorum=1; direct event; immediate)
3. A proposes raising quorum to 2 → proposer's signature = quorum=1 satisfied → quorum=2 effective
4. A proposes promoting D to admin → "Pending: 1 of 2 signatures"
5. B opens PendingAdminProposalsPanel → countersigns → D is promoted
6. A tries to directly demote B → IPC returns Pending (proposal route now mandatory)
7. B refuses to countersign → after 30 days the proposal shows "Expired"

### 8.7 CI gate target

- Rust: 1401 baseline (post-ZEB-287) → ~1430-1445 (+29-44 new tests)
- Frontend: 1755 → ~1770-1780 (+15-25 new tests)
- All 5 gates (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo nextest run`, `npx tsc --noEmit`, `npx vitest run`) green before PR opens.

## 9. Backwards compatibility

| Surface | Behavior |
|---|---|
| Pre-ZEB-250 CommunityState blobs | Decode as `admin_quorum = 1` via `serde(default)`. Byte-identical re-encode (default-value skip). |
| Pre-ZEB-250 SetPower/Kick events in historical log | Verify at-time-of-event under then-current `admin_quorum` (implicitly 1 for all pre-ZEB-250 history). Remain valid forever. |
| Pre-ZEB-250 clients receiving `q` / `n` events | Unknown-variant decode silently drops the event. Old clients can't see new admin-quorum activity but their own behavior preserved. |
| Communities at `admin_quorum = 1` | Zero behavioral change. Section UI hidden for non-admins. |
| Fork inheritance | Fork starts at default `admin_quorum = 1`. Does NOT inherit parent's quorum. |

## 10. Acceptance criteria

1. New CRDT event variants `AdminProposal` + `AdminCountersign` land with full verify rules + materialize semantics + 30-day expiry.
2. `CommunityState.admin_quorum` field added (default 1, byte-compatible).
3. Direct `SetPower{level: 100}` / `SetPower{target: admin, level: <100}` / `Kick{target: admin}` rejected at verify when `admin_quorum > 1`.
4. ProposalKind well-formedness (AP3) + admin-affecting check (AP4) + ChangeQuorum range (AP5) all enforced.
5. Materialize single-pass-with-running-state algorithm collects per-proposal signer state and applies effects once quorum is reached.
6. Quorum-reached-then-aged proposals remain effective (permanence per §5.3).
7. Late countersigns to expired proposals are no-ops.
8. ChangeQuorum updates `admin_quorum` field on materialize.
9. Existing `set_power_level` + `kick_from_community` IPCs auto-route based on community's `admin_quorum`; new IPCs `list_pending_admin_proposals`, `countersign_admin_proposal`, `propose_change_quorum`.
10. `PendingAdminProposalsPanel.svelte` mounted in CommunitySettingsPanel (admin-only); pending + recent + expired sections; countersign button with appropriate disabled states.
11. `ChangeQuorumDialog.svelte` for raising/lowering quorum with slider + number-input.
12. Member-list rendering shows pending-promotion / pending-demotion / pending-kick badges.
13. Fork-of-quorum-community correctly resets fork to quorum=1.
14. Five CI gates green; new wire-format fixtures pin canonical CBOR.

## 11. Out of scope (explicit deferrals)

File as Phase 2/3 follow-ups if/when needed:

- Time-based admin-inactivity fallback — forking is the universal escape-hatch per Q4
- Member-quorum recovery flow — forking covers it
- Withdrawal of countersignatures or proposer-cancellation — propose inverse to undo per Q7
- Quorum-gated Leave — Leave is self-determined per Q9
- Fork inheriting quorum — fresh start per Q10
- Threshold-signature cryptography (BLS/Schnorr aggregation) — no new crypto, generalize ZEB-254 instead
- Configurable per-action quorum policy — admin-affecting only per Q2
- Auto-countersign hook for admin proposals — deliberate UX per §5.5
- Notification system for "pending proposals require your attention" — no Harmony notification surface yet
- Per-action quorum overrides (e.g., "ChangeQuorum needs supermajority") — uniform quorum for all admin-affecting actions in v1
- Bulk-countersign UI (sign multiple pending proposals at once) — one at a time in v1
- Consolidation with [ZEB-254](https://linear.app/zeblith/issue/ZEB-254)'s PendingJoinsPanel — ship parallel panels in v1, consolidate in a future refactor
- Inline action affordances on member rows — panel-only in v1
- i18n / multi-language UI copy

## 12. References

- Spec: this document
- Sub-C v1 parent: [ZEB-217](https://linear.app/zeblith/issue/ZEB-217)
- Pattern source: [ZEB-254](https://linear.app/zeblith/issue/ZEB-254) (PendingJoin + JoinCountersign) — `docs/specs/2026-05-15-zeb-254-pending-join-crdt-design.md`
- Recovery primitive: [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) forking — `docs/specs/2026-05-14-zeb-285-phase1-community-forking-design.md`
- Memory: `project_harmony_polycentric_governance.md` — communities-only governance, communities are sovereign
- Memory: `feedback_severe_action_confirmation.md` — confirmation tier for countersign button
- Memory: `feedback_slider_pair_with_number_input.md` — ChangeQuorumDialog UI requirement
- Source: [`src-tauri/src/community_membership.rs`](../../src-tauri/src/community_membership.rs) — `MembershipEventKind` host
- Source: [`src-tauri/src/community_state_crdt.rs`](../../src-tauri/src/community_state_crdt.rs) — `CommunityState` host
- Source: [`src/lib/components/CommunitySettingsPanel.svelte`](../../src/lib/components/CommunitySettingsPanel.svelte) — host for new Admin governance section
- Source: [`src/lib/components/PendingJoinsPanel.svelte`](../../src/lib/components/PendingJoinsPanel.svelte) — ZEB-254 pattern reference
