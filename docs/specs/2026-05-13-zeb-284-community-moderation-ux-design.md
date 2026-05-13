# ZEB-284: Community moderation UX — kick / unban / set-power / member-list surface

**Date:** 2026-05-13
**Branch:** `zeb-284-community-moderation-ux`
**Parent:** [ZEB-284](https://linear.app/zeblith/issue/ZEB-284) (standalone; no parent epic — Sub-C v2 [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) is the predecessor that shipped channel-config but not member-moderation)

## 1. Goal

Surface the already-wired community moderation primitives to end-users by building the frontend UX that consumes the existing backend IPCs (`kick_from_community`, `set_power_level`, `list_community_members`) plus one new CRDT primitive (`MembershipEventKind::Unban`) and one new IPC (`unban_from_community`).

Concretely, after this PR ships:

- Admins and moderators can act on community members from a dedicated members panel.
- Kicked members can be reinstated via a new admin-tier Unban action.
- Power changes use a Member/Moderator/Admin vocabulary (raw u8 levels stay backend-internal).
- The community's recent moderation actions are visible inline to every member.
- Self-demote and self-leave when you are the last admin are gated by a typed-confirm dialog that forward-points to the (future) forking feature for orphan recovery.

## 2. Context — current state

### 2.1 Backend CRDT layer is complete

`src-tauri/src/community_membership.rs` already defines the full power-gated event model:

- `MembershipEventKind::Kick { target, reason: Option<String> }` — mod-tier (power ≥ `POWER_THRESHOLDS.kick` = 50)
- `MembershipEventKind::SetPower { target, level }` — admin-tier (power ≥ `POWER_THRESHOLDS.set_power` = 100)
- `MembershipEventKind::EpochRotation` — auto-fires on Kick / Leave to exclude target from new epoch ciphertexts
- Channel-config events (`ChannelCreate` / `ChannelModify` / `ChannelDelete`) — all gated at mod-tier per [ZEB-248](https://linear.app/zeblith/issue/ZEB-248)

A code comment at [`community_membership.rs:415-430`](../../src-tauri/src/community_membership.rs) explicitly documents the current shipped semantics:

> Kick = effective ban until a dedicated unban flow exists, so a kicked actor cannot replay Leave (no power required, would create a member admin can no longer kick — admin's own kick already triggered EpochRotation that left target behind, and target is no longer Banned-blocked) to rejoin — defeating Kick-as-ban.

This PR delivers the "dedicated unban flow" the comment foreshadows.

### 2.2 Backend IPC layer is wired

- `list_community_members(communityId) -> Vec<MemberInfoDto>` at [`lib.rs:6412`](../../src-tauri/src/lib.rs). Returns rows sorted by power desc, then `joined_at` asc, then addr bytes. `MemberInfoDto` already exposes `power: u8`, `status: MemberStatusDto`, `joined_at: Hlc`.
- `kick_from_community(communityId, targetAddr) -> Result<(), String>` at [`lib.rs:11614`](../../src-tauri/src/lib.rs). Currently does **not** accept a reason parameter even though the CRDT event variant carries `reason: Option<String>`.
- `set_power_level(communityId, targetAddr, level: u8) -> Result<(), String>` at [`lib.rs:11917`](../../src-tauri/src/lib.rs).

### 2.3 Service layer is wired

`src/lib/community-service.ts:185` already has `kickFromCommunity(communityId, targetAddr)` and `:195` has `setPowerLevel(communityId, targetAddr, level)`.

### 2.4 The gap is entirely frontend UX

Grep for any Svelte component calling `kickFromCommunity` or `setPowerLevel`: zero matches. `ChannelMembersPanel.svelte` lists channel members but exposes no moderation actions. `CommunitySettingsPanel.svelte` exists but does not host a member-management surface.

### 2.5 Power thresholds (current)

```rust
pub const POWER_THRESHOLDS: PowerThresholds = PowerThresholds {
    invite: 0,
    kick: 50,
    set_power: 100,
    max: 100,
};
```

This PR does **not** change thresholds. [ZEB-251](https://linear.app/zeblith/issue/ZEB-251) (per-community customization) remains a separate future ticket.

## 3. Design decisions resolved in brainstorm

Seven design questions surfaced during brainstorming; this section documents the resolution of each.

### 3.1 Kick / ban model — add Unban (admin-tier)

**Decision:** Add a new `MembershipEventKind::Unban` variant. Admin-tier action that transitions a Banned member to Left status, re-enabling them to accept a fresh invite. Does **not** auto-rejoin — they must accept an Invite afterward.

**Alternatives rejected:**
- Ship over existing primitives (kick = permanent ban with no recourse). Honest about current semantics but lacks the mistake-correction path. Doesn't match the CRDT author's documented intent.
- Separate Kick + Ban as distinct primitives (Discord/Matrix model). Two new variants, two new IPCs, doubles UI surface. Locks dual-vocabulary forever.

### 3.2 Last-admin guard — no hard guard; soft typed-confirm warning

**Decision:** Do **not** block self-demote or self-leave at the IPC or CRDT layer. Show a typed-confirm warning dialog when the action would orphan the community, with a forward-pointing breadcrumb to the future forking feature.

**Reasoning:** A hard guard on self-demote / self-leave does not prevent the underlying failure mode (admin loses identity via death, passphrase loss, or device loss — all unguardable from the UI). Blocking self-leave also conflicts with personal-liberty / free-association principles. The genuine recovery path is **community forking** — tracked as a separate ticket so the design surface gets the attention it deserves rather than being squeezed into this PR.

The typed-confirm dialog gives the user one clear chance to abort an irreversible-via-self action without being theater. Tokens: `DEMOTE` for self-demote, `LEAVE` for self-leave.

### 3.3 Audit surface — inline recent-actions badge

**Decision:** A collapsible "Recent moderation actions" section at the top of the new members panel, showing the last ~5-10 events with human-readable rendering. Reads from a new IPC `list_recent_moderation_events(communityId, limit)`.

**Alternatives rejected:**
- No audit surface in this PR. Risk: ships moderation without accountability; wrong first impression of the polycentric governance model.
- Dedicated audit tab with filtering. Bigger surface; deferred to a future ticket as the eventual UX target.

### 3.4 Reason capture — optional, visible to target + mods

**Decision:** Optional free-text reason field on both Kick and Unban dialogs. If filled, the reason is signed into the CRDT event. Visibility: the kicked member sees the reason in their last-visible community state (before epoch rotation excludes them); other mods see it in the recent-actions badge and (future) audit tab.

**Alternatives rejected:**
- Required, visible to all members. Force-leaks private investigation context to everyone; loses the internal-note use case.
- Optional, mods-only. Reduces accountability to the kicked party.
- No reason capture. Field stays vestigial; abandons the social-design opportunity.

### 3.5 Power-tier vocabulary — fixed labels

**Decision:** `0..49 → Member`, `50..99 → Moderator`, `100 → Admin`. UI shows the label only; raw u8 is backend-internal. Set-power UI is a dropdown of named actions (`Promote to Moderator`, `Promote to Admin`, `Demote to Member`, `Demote to Moderator`).

**Alternatives rejected:**
- Fixed labels + visible numeric. Cluttered without commensurate benefit.
- Raw numeric slider + paired input. Defers the vocabulary decision; exposes implementation details.
- Discord-style custom roles per community. Big surface (`RoleDefine` CRDT event, per-role color, etc.); deferred.

### 3.6 Member-row interaction pattern — always-visible kebab

**Decision:** Each member row shows a kebab (⋮) icon at row end. Click opens a dropdown with the actions available to the viewer given their power and the target's power/status. Touch-target sized ≥44×44px so the same pattern works on mobile Tauri later.

**Alternatives rejected:**
- Right-click context menu + long-press. Native idiom but no visible affordance; newcomers don't discover.
- Selection-mode (tap row → actions appear). Adds selection state; useful for batch actions but premature here.

### 3.7 Confirmation tiers

Per the [`feedback_severe_action_confirmation`](../../~/.claude/projects/-Users-zeblith-work/memory/feedback_severe_action_confirmation.md) memory:

| Action | Tier | Mechanism |
|---|---|---|
| Promote (any) | Low-risk | Inline kebab click; no dialog |
| Demote (someone else) | Severe, reversible | Click-confirm dialog (Cancel + Confirm at non-adjacent positions) |
| Kick | Severe, reversible (via Unban) | `ModerationReasonDialog` with confirm at secondary position |
| Unban | Low-risk (undoes a prior Kick) | `ModerationReasonDialog`; same dialog as Kick parameterized by action |
| Self-demote (last admin) | Severe, irreversible-via-self | `LastAdminWarningDialog` typed-confirm with token `DEMOTE` |
| Self-leave (last admin) | Severe, irreversible-via-self | `LastAdminWarningDialog` typed-confirm with token `LEAVE` |
| Self-demote (not last admin) | Severe, reversible | Standard click-confirm |
| Self-leave (not last admin) | Severe, reversible | Existing leave-community flow (unchanged by this PR) |

## 4. CRDT layer changes

### 4.1 New `MembershipEventKind::Unban` variant

In `src-tauri/src/community_membership.rs`, add to the `MembershipEventKind` enum:

```rust
/// Admin-tier action: lifts a prior Kick-as-effective-ban so the target
/// can be re-invited. Does NOT auto-rejoin — target must accept a fresh
/// Invite. Transitions MemberStatus::Banned → MemberStatus::Left.
///
/// Variant code "u". Inner field keys are 2-char (tg, rs).
#[serde(rename = "u")]
Unban {
    #[serde(rename = "tg")]
    target: OwnerAddr,
    #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
    reason: Option<String>,
}
```

### 4.2 New `VerifyError::UnbanTargetNotBanned` variant

```rust
/// Unban event targets an addr that is not currently Banned. Reject so
/// the IPC layer can surface "target is not currently banned" rather
/// than silently no-op.
UnbanTargetNotBanned,
```

### 4.3 `verify_event` gates

In `community_membership.rs::verify_event`, add the Unban arm:

```rust
MembershipEventKind::Unban { target } => {
    if actor_power < POWER_THRESHOLDS.set_power {
        return Err(VerifyError::ActorPowerInsufficient);
    }
    let Some(target_state) = membership.members.get(target) else {
        return Err(VerifyError::TargetNotMember);
    };
    if target_state.status != MemberStatus::Banned {
        return Err(VerifyError::UnbanTargetNotBanned);
    }
    Ok(())
}
```

### 4.4 `apply_membership_event` Unban arm

```rust
MembershipEventKind::Unban { target, .. } => {
    if let Some(state) = self.members.get_mut(target) {
        state.status = MemberStatus::Left;
        state.joined_at = event.hlc.clone();
        // Power level is preserved (banned admins retain their power
        // until a future SetPower event modifies it). On re-invite,
        // they re-Join at the preserved power level.
    }
    // No EpochRotation auto-trigger — Unban is additive (re-opens
    // invite eligibility); re-Join handles its own epoch via the
    // existing Invite → Join flow.
}
```

### 4.5 Wire-format invariants

- Variant code is 1 char (`"u"`), matching the existing same-length-keys invariant.
- Inner field keys are 2 char (`"tg"`, `"rs"`), matching every other inner-field pattern in the enum.
- Pin a canonical CBOR fixture in `src-tauri/tests/wire_format_membership.rs` (or wherever ZEB-248 added its fixtures) for `Unban { target, reason: Some("test") }` and `Unban { target, reason: None }`.

## 5. IPC layer changes

### 5.1 New `unban_from_community` Tauri command

In `src-tauri/src/lib.rs`, mirror the existing `kick_from_community` pattern (NodeState snapshot + lock-drop discipline):

```rust
#[tauri::command]
async fn unban_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<(), String> {
    // Same hex-decode + NodeState snapshot + community-registry lookup
    // pattern as kick_from_community at lib.rs:11614+
    // Mint via new mint_unban_event helper
    // Apply via existing apply_membership_event
    // Broadcast via existing broadcast path
}
```

Register in `tauri::generate_handler!`.

### 5.2 New `mint_unban_event` helper

```rust
pub fn mint_unban_event(
    actor: OwnerAddr,
    target: OwnerAddr,
    reason: Option<String>,
    prev_hlc: Option<&Hlc>,
    wall_now_ms: u64,
    device_id: &DeviceId,
    signing_key: &SigningKey,
) -> SignedMembershipEvent {
    // Mirror mint_kick_event at lib.rs:11452+
}
```

### 5.3 `kick_from_community` extended with optional reason

Extend signature:

```rust
async fn kick_from_community(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    target_addr: String,
    reason: Option<String>,  // NEW
) -> Result<(), String>
```

Backwards-compatible: existing callers pass `None`. The new frontend wires `reason: Some(...)` from the dialog.

### 5.4 New `list_recent_moderation_events` IPC

For the recent-actions badge. Reads from the existing community event log:

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerationEventDto {
    pub event_id: String,         // hex
    pub kind: ModerationEventKindDto, // "kick" | "unban" | "set_power"
    pub actor_addr: String,       // hex
    pub target_addr: String,      // hex
    pub reason: Option<String>,   // only on kick/unban
    pub new_power: Option<u8>,    // only on set_power
    pub hlc: Hlc,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModerationEventKindDto { Kick, Unban, SetPower }

#[tauri::command]
async fn list_recent_moderation_events(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    limit: u32,
) -> Result<Vec<ModerationEventDto>, String> {
    // Read community event log → filter to Kick/Unban/SetPower variants →
    // sort by HLC desc → take(limit) → map to DTO
}
```

`limit` is clamped to 1..=100 in the handler.

## 6. Frontend service-layer changes (`src/lib/community-service.ts`)

### 6.1 Extend `kickFromCommunity` with optional reason

```ts
async kickFromCommunity(
    communityId: string,
    targetAddr: string,
    reason?: string
): Promise<void> {
    await this.invoke<void>('kick_from_community', {
        communityId,
        targetAddr,
        reason: reason ?? null,
    });
}
```

### 6.2 New `unbanFromCommunity`

```ts
async unbanFromCommunity(
    communityId: string,
    targetAddr: string,
    reason?: string
): Promise<void> {
    await this.invoke<void>('unban_from_community', {
        communityId,
        targetAddr,
        reason: reason ?? null,
    });
}
```

### 6.3 New `listRecentModerationEvents`

```ts
async listRecentModerationEvents(
    communityId: string,
    limit: number = 10
): Promise<ModerationEvent[]> {
    return this.invoke<ModerationEvent[]>('list_recent_moderation_events', {
        communityId,
        limit,
    });
}
```

`ModerationEvent` type added to `src/lib/types.ts`.

## 7. Frontend components

### 7.1 `CommunityMembersPanel.svelte` (new)

**Path:** `src/lib/components/CommunityMembersPanel.svelte`
**Props:** `communityId: string`
**State:**
- `members: MemberInfoDto[]` — fetched on mount + on `community-state-updated` event
- `recentEvents: ModerationEvent[]` — fetched on mount + on `community-state-updated`
- `viewerPower: number` — derived from `members.find(m => m.addr === ownProfileAddr)?.power ?? 0`
- `viewerIsLastAdmin: boolean` — derived: `viewerPower === 100 && members.filter(m => m.power === 100 && m.status === 'joined').length === 1`

**Layout:**

```
┌─ Community Members ──────────────────────────┐
│ [RecentActionsBadge — collapsible]           │
│ ──────────────────────────────────────────── │
│ [Search input (in-memory filter)]             │
│                                               │
│ [Joined section — sorted power desc]          │
│ <MemberRow ... /> ×N                          │
│                                               │
│ [Banned section — collapsed by default,       │
│  shown only if banned.length > 0]             │
│ <MemberRow ... /> ×N                          │
└──────────────────────────────────────────────┘
```

Banned members are sectioned separately because their action set is different (only Unban) and visually segregating them keeps the active-member list clean.

### 7.2 `MemberRow.svelte` (new)

**Path:** `src/lib/components/MemberRow.svelte`
**Props:** `member: MemberInfoDto`, `viewer: { addr: string, power: number, isLastAdmin: boolean }`, `communityId: string`
**Output:**

```
👤  {displayName}  {tierLabel}  joined {date}  ⋮
```

`tierLabel` = `Admin` if power=100, `Moderator` if 50≤power<100, `Member` if power<50, `Banned` if status=Banned (overrides power label).

Kebab content (computed pure-function from viewer power, target power, target status):

```ts
function kebabActions(viewerPower, targetPower, targetStatus, isSelf, isLastAdmin) {
    if (targetStatus === 'banned') {
        return viewerPower >= 100 ? ['Unban'] : [];
    }
    if (isSelf) {
        const actions = [];
        if (viewerPower === 100) actions.push(isLastAdmin ? 'Demote to Moderator (last admin)' : 'Demote to Moderator');
        if (viewerPower >= 50) actions.push('Demote to Member');
        // Self-kick is not a valid action; Leave is a separate community-leave flow
        return actions;
    }
    // Acting on another member
    const actions = [];
    if (viewerPower > targetPower) {
        if (viewerPower >= 100 && targetPower < 100) actions.push('Promote to Admin');
        if (viewerPower >= 100 && targetPower < 50) actions.push('Promote to Moderator');
        if (viewerPower >= 100 && targetPower >= 50) actions.push('Demote to Member');
        if (viewerPower >= 100 && targetPower === 100) actions.push('Demote to Moderator');
        if (viewerPower >= 50) actions.push('Kick');
    }
    return actions;
}
```

Empty action list → kebab icon is hidden. No dead UI affordance.

### 7.3 `ModerationReasonDialog.svelte` (new, parameterized)

**Path:** `src/lib/components/ModerationReasonDialog.svelte`
**Props:** `action: 'kick' | 'unban'`, `targetName: string`, `communityName: string`, `onConfirm: (reason: string | null) => Promise<void>`, `onCancel: () => void`
**Layout:**

```
{Kick|Unban} {targetName} from "{communityName}"?

Optional: reason (visible to {targetName} and other mods)
[textarea, max 280 chars]

[Cancel]                              [{Kick|Unban} (right)]
```

Submit triggers `onConfirm(reason || null)` and disables both buttons + shows a spinner until promise resolves. Error from promise surfaces in a toast: `e instanceof Error ? e.message : String(e)`.

### 7.4 `LastAdminWarningDialog.svelte` (new)

**Path:** `src/lib/components/LastAdminWarningDialog.svelte`
**Props:** `action: 'demote' | 'leave'`, `communityName: string`, `onConfirm: () => Promise<void>`, `onCancel: () => void`
**Layout:**

```
⚠ You are the last admin of "{communityName}"

After this action, the community will be locked: no one
will be able to issue moderation actions, including
restoring admin tier. Recovery is possible by forking
the community (coming soon — [ZEB-285](https://linear.app/zeblith/issue/ZEB-285)).

To proceed, type {DEMOTE|LEAVE} below:
[input field — validates exact match, case-sensitive]

[Cancel]                              [Proceed (disabled until match)]
```

Token: `DEMOTE` for `action='demote'`, `LEAVE` for `action='leave'`. Validates on every keystroke; `Proceed` button enabled only when input exactly equals the required token (no trim, case-sensitive).

### 7.5 `RecentActionsBadge.svelte` (new)

**Path:** `src/lib/components/RecentActionsBadge.svelte`
**Props:** `events: ModerationEvent[]`
**Layout:**

```
▾ Recent moderation actions ({count})
   • {relative-time} — {actorName} {action-verb} {targetName} {reason-quoted-if-present}
   ...
```

Action verbs:
- `kick` → "kicked"
- `unban` → "unbanned"
- `set_power` → "promoted to {tier-label}" or "demoted to {tier-label}" (computed from `new_power`)

Empty state: `No recent moderation actions.` Collapsed-by-default; expandable via the ▸/▾ chevron.

### 7.6 `CommunitySettingsPanel.svelte` (modified)

Add an entry that opens `CommunityMembersPanel` for the current `communityId`. Either:
- A new tab in the existing tab strip, OR
- A button/link in the panel body (lower-cost change)

Implementer picks based on what's cleanest given the current tab structure. Spec-compliance review will accept either.

## 8. Data flow — kick happy path

```
1. Admin clicks ⋮ on Bob's row → MemberRow dispatches event
2. CommunityMembersPanel opens ModerationReasonDialog action='kick'
3. Admin types "repeated spam" → clicks Kick
4. Dialog onConfirm → communityService.kickFromCommunity(id, bobAddr, "repeated spam")
5. Service invokes Tauri kick_from_community with reason
6. Backend: NodeState lock → mint_kick_event with reason → apply_membership_event
   → EpochRotation auto-fires excluding Bob → broadcast → lock drop
7. IPC returns Ok(()) → service resolves → dialog closes
8. community-state-updated event fires → CommunityMembersPanel refetches members + recent events
9. Member panel re-renders: Bob moves to Banned section, RecentActionsBadge prepends new event
10. Bob's device receives the kick event in pre-rotation epoch (he had keys up to that moment)
    → his last-visible community state shows the kick with reason
    → after EpochRotation broadcast, he cannot decrypt new community messages
```

## 9. Error handling

All IPC errors flow through the service layer with the existing extraction pattern:

```ts
try {
    await this.invoke<void>('kick_from_community', { ... });
} catch (e) {
    throw new Error(e instanceof Error ? e.message : String(e));
}
```

Specific user-visible cases handled by the dialog layer (toast on dialog or panel):

- `"insufficient power"` — viewer's power dropped between page render and submit. Toast: "You don't have permission for this action anymore."
- `"target is not currently banned"` — Unban raced against a re-Join via fresh invite. Toast: "Member is no longer banned."
- `"target not member"` — target left the community between page render and submit. Toast: "Member is no longer in this community."

Defense-in-depth: even though `LastAdminWarningDialog` handles the orphan case at the UI, the backend does not enforce a last-admin guard (per §3.2). The UI is the only gate. Future hardening could add a backend mirror, but per the design decision the UI gate is sufficient for the soft-warning policy.

## 10. Optimistic UI

Minimal. Dialogs disable buttons and show a spinner during the IPC round-trip. The member panel does **not** pre-update — it re-renders from authoritative state when `community-state-updated` fires after the broadcast. Reasoning:

- IPC round-trip is fast (local CRDT mutation + broadcast trigger).
- Pre-update + rollback on error is more confusing than a half-second spinner.
- The authoritative re-render path is the same one that handles remote-originated events; using it for local actions too keeps the data flow uniform.

## 11. Testing

### 11.1 Rust unit tests (`community_membership.rs::tests`)

5 new tests in the existing module:

1. `unban_event_succeeds_when_actor_is_admin_and_target_is_banned` — happy path.
2. `unban_event_rejected_when_actor_is_moderator` — `ActorPowerInsufficient`.
3. `unban_event_rejected_when_target_is_not_banned` — `UnbanTargetNotBanned`.
4. `unban_event_rejected_when_target_is_unknown` — `TargetNotMember`.
5. `unban_then_invite_then_join_round_trip_succeeds` — Banned → Unban → Left → Invite → Join → Joined.

### 11.2 Rust IPC tests (`lib.rs` new `unban_from_community_tests` module)

3 new tests mirroring existing `kick_from_community_tests` fixture pattern:

1. `unban_from_community_happy_path` — two-engine setup; A kicks B; A unbans B; verify both engines see Banned → Left.
2. `unban_from_community_returns_err_when_actor_lacks_power` — mod-tier viewer; `"insufficient power"`.
3. `unban_from_community_returns_err_when_target_not_banned` — clean state; `"target is not currently banned"`.

### 11.3 Rust IPC tests for kick-with-reason

1 new test: `kick_from_community_signs_reason_into_event` — pass `reason: Some("smoke")`; verify the materialized event carries the reason.

### 11.4 Rust IPC tests for `list_recent_moderation_events`

2 new tests:

1. `list_recent_moderation_events_returns_kick_unban_setpower_filtered` — verifies only moderation-event kinds appear; channel-config events excluded.
2. `list_recent_moderation_events_respects_limit_and_orders_by_hlc_desc` — verifies limit clamping + sort order.

### 11.5 Wire-format fixture pinning

Add canonical CBOR fixtures for:
- `Unban { target, reason: Some("test") }`
- `Unban { target, reason: None }`

Co-located with existing membership wire fixtures (whichever file ZEB-248 added them to).

### 11.6 Vitest UI tests

4 new test files; ~12 cases total:

**`CommunityMembersPanel.test.ts`**
- renders member list sorted by power desc
- banned section visible only when banned members exist
- viewer-is-admin sees kebab on all rows
- viewer-is-member sees no kebab

**`MemberRow.test.ts`**
- kebab action matrix: 6 viewer/target combinations verified

**`ModerationReasonDialog.test.ts`**
- kick happy path (with reason, blank reason)
- error path: IPC rejection surfaces as toast
- cancel doesn't fire IPC
- spinner shows during round-trip

**`LastAdminWarningDialog.test.ts`**
- typed-confirm enables Proceed only on exact match
- case-sensitive validation
- `DEMOTE` vs `LEAVE` token by action
- cancel doesn't fire IPC

### 11.7 Smoke test plan (manual, documented in PR test plan)

Two-engine local setup mirroring existing pattern:

1. Engine A creates community "Test"; invites Engine B; B joins. A is admin, B is member.
2. A promotes B to Moderator → both panels show "Moderator". Recent-actions badge prepends the event on A.
3. A demotes B back to Member → confirmed via panel.
4. A kicks B with reason "smoke test" → B's row moves to Banned section on A; B sees the kick in last-visible state with reason; B's community nav-tree entry disappears.
5. A unbans B → B's banned row transitions to Left. A re-invites B; B re-joins clean (status returns to Joined).
6. A attempts self-demote → typed-confirm dialog with `DEMOTE` token; cancel returns cleanly; typed `DEMOTE` proceeds.

## 12. Acceptance criteria

1. New `MembershipEventKind::Unban` variant verified + materialized + persisted.
2. New `unban_from_community` Tauri command registered in `tauri::generate_handler!`.
3. `kick_from_community` accepts optional `reason` parameter; backwards-compatible.
4. New `list_recent_moderation_events` Tauri command registered.
5. New `CommunityMembersPanel.svelte` accessible from `CommunitySettingsPanel.svelte`.
6. Kick + Unban + Promote + Demote all work via UI; reason captured on Kick/Unban.
7. Last-admin self-demote triggers `LastAdminWarningDialog` with `DEMOTE` token.
8. Last-admin self-leave triggers `LastAdminWarningDialog` with `LEAVE` token (handler wires into existing community-leave flow).
9. `RecentActionsBadge` surfaces ≥5 most-recent moderation events with human-readable rendering.
10. All 5 local gates green: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.
11. Smoke test passes.

## 13. Out of scope (deferred)

- **Community forking** — [ZEB-285](https://linear.app/zeblith/issue/ZEB-285), filed alongside this spec, capturing the "any member, any time" framing from the brainstorm. Sub-E-shaped.
- **Audit log tab** — full filterable history view in CommunitySettingsPanel. Inline badge ships now; tab is the eventual UX target.
- **Custom roles per community** — Discord-style per-community role names + colors. New `RoleDefine` CRDT event; big surface.
- **Cross-community ban lists / federation** — each community is sovereign per polycentric-governance model.
- **Public-vs-private reason channels** — single reason field for now; split into internal-mod-note + public-reason is a future bifurcation if needed.
- **Per-community threshold customization** — [ZEB-251](https://linear.app/zeblith/issue/ZEB-251) tracks `POWER_THRESHOLDS` becoming community-configurable. Independent of this PR.
- **Channel-level moderator role** — moderation is community-scope only in v2; channels inherit. v3 deferred.
- **Profile-level block** — distinct primitive from community membership; orthogonal.

## 14. References

### Backend (existing)

- `src-tauri/src/community_membership.rs:43-160` — `MembershipEventKind` enum + power-tier reasoning
- `src-tauri/src/community_membership.rs:1753-1767` — `POWER_THRESHOLDS`
- `src-tauri/src/community_membership.rs:381-475` — `VerifyError` enum + existing error variants
- `src-tauri/src/lib.rs:6353-6453` — `MemberInfoDto` + `member_info_for`
- `src-tauri/src/lib.rs:6412` — `list_community_members` Tauri command (canonical pattern to mirror)
- `src-tauri/src/lib.rs:11452-11616` — `mint_kick_event` + `kick_from_community` (canonical pattern to mirror)
- `src-tauri/src/lib.rs:11885-12000` — `mint_set_power_event` + `set_power_level`
- `src/lib/community-service.ts:185, 195` — existing `kickFromCommunity` + `setPowerLevel` wrappers
- `src/lib/components/CommunitySettingsPanel.svelte` — host for the new members tab/link
- `src/lib/components/ChannelMembersPanel.svelte` — sibling pattern; do not modify

### Memory rules applied

- `feedback_severe_action_confirmation` — three confirmation tiers (no-confirm / click-confirm / typed-confirm) by reversibility.
- `feedback_slider_pair_with_number_input` — not applicable (no slider in this PR; rejected raw-numeric option).
- `feedback_tauri_error_extraction` — `e instanceof Error ? e.message : String(e)` for all frontend IPC error surfacing.
- `feedback_metadata_before_irreversible_write` — not applicable (no irreversible writes from this PR; CRDT mutations are reversible via their respective inverse events or, in the orphan case, via forking).
- `feedback_two_ipc_toctou` — not applicable (no preview/commit IPC pairs; all moderation IPCs are single-call mutations).
- `feedback_engineer_for_real_scale` — applied: forking-as-recovery instead of last-admin-guard-as-prevention is the scale-resilient choice.

### Linear refs

- [ZEB-284](https://linear.app/zeblith/issue/ZEB-284) — this ticket
- [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) — Sub-C v1 (communities ship)
- [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) — Sub-C v2 (channels) — the predecessor that established the channel-config event pattern
- [ZEB-251](https://linear.app/zeblith/issue/ZEB-251) — per-community threshold customization (independent)
- [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) — community forking primitive (the recovery path forward-pointed to by the last-admin warning)
