# ZEB-263 — Phase 5 Community Frontend Design

> **Status:** Brainstormed; awaiting user review before plan-writing.
>
> **Linear:** [ZEB-263](https://linear.app/zeblith/issue/ZEB-263) · parent [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) Sub-C.
>
> **Branch:** `zeb-263-community-frontend` cut from `origin/main` at `26007ce` (merge of PR #90, ZEB-260).

---

## 1. Context

Phase 5 is the final phase of ZEB-217 Sub-C (community moderation primitive). It surfaces the Phase 3 + Phase 4 backend IPC surface to the user. After this lands, ZEB-217 closes.

Backend state going into Phase 5:

- Phase 1 (PR #82): `community_membership.rs` — signed-event CRDT primitives.
- Phase 2 (PR #84): per-community state CRDT + encrypted Zenoh sync.
- Phase 3 (PR #87): open-community flow IPCs (`create_community`, `redeem_invite` open branch, `leave_community`, `list_community_members`, `generate_invite`).
- Phase 4 (PR #89): invite-only flow + kick + set-power IPCs (`redeem_invite` invite-only branch, `kick_from_community`, `set_power_level`).
- Phase 4 follow-up (PR #90, ZEB-260): cold-cache bootstrap fix — invite URL carries admin's signed bootstrap event, redemption inserts it before publish-back arrives.

Frontend state going in: **zero** call sites for any of these IPCs. `nav-service.ts:117` confirms — "Phase 4 only handles DM/GroupDm Spaces — channel/community/folder ... reserved for later phases." There are no `Community*` components in `src/lib/components/`.

## 2. Goals

Wire the seven Phase 3 + Phase 4 community IPCs to UI such that:

- A user can create open and invite-only communities.
- A user can redeem any valid invite URL (open or invite-only) and end up in the community.
- An admin can list members, kick, set power levels, and generate invite links.
- The community appears in the nav tree as a first-class node, alongside DMs and folders.
- Multi-device convergence is automatic via the existing `community-members-changed` event.
- All severe actions get appropriate confirmation calibrated to reversibility.

### Non-goals (deferred)

- Channel-level UI inside a community (no channel IPCs ship in Phase 4 — voice channels live in a separate ticket family; text channels TBD).
- ZEB-260 Cases B+C (open-community first-Join cold-cache, self-Re-Join after Leave) — backend gate redesign work, deferred per ZEB-260 spec.
- M-of-N admin recovery — separate followup.
- Per-community customizable power thresholds (ZEB-251) — backend not started.
- Community-inside-community placement — semantically unclear (linux-fs-style permission inheritance), deferred. Phase 5 allows community placement in user folders but not inside other communities.
- `invitee_hint` and `expires_at` invite-link parameters — accepted by the backend IPC but ignored (`lib.rs:5539`); no UI surface for them yet.
- Cancellation of in-progress `redeem_invite` — backend has 15s timeout; UI shows spinner-only loading state and waits for completion or timeout.

## 3. Architecture overview

### New files

| File | Responsibility |
|---|---|
| `src/lib/components/CreateCommunityDialog.svelte` | Modal: name + open/invite-only toggle → calls `create_community` |
| `src/lib/components/RedeemInviteDialog.svelte` | Modal: paste URL → calls `redeem_invite`; loading spinner during round-trip; friendly + diagnostic-disclosure error UX |
| `src/lib/components/CommunitySettingsPanel.svelte` | Modal: Info / Members / Invites / Danger sections; power-gated actions |
| `src/lib/components/SetPowerDialog.svelte` | Sub-modal: slider + bidirectionally-synced number input + threshold annotations + role-badge preview |
| `src/lib/components/InviteLinkManager.svelte` | Section component used inside CommunitySettingsPanel: generate-on-demand + copy-to-clipboard + regenerate |
| `src/lib/components/ConfirmationModal.svelte` | New tier-2 confirmation primitive: confirm button positioned LEFT (offset from row-end-right triggers) |
| `src/lib/components/TypedConfirmationModal.svelte` | New tier-3 confirmation primitive: typed-string match enables destructive button |
| `src/lib/community-service.ts` | Service mirroring `MessageService`/`NavService` shape: thin wrapper over the 7 IPCs + 2 event listeners; emits change events for UI re-render |

### Modified files

| File | Change |
|---|---|
| `src/lib/types.ts` | Add `'community'` to `NavNodeType`; add `Community`, `CommunityMember`, `PowerRole` types; add `POWER_THRESHOLDS` + `powerToRole()` helper |
| `src/lib/nav-service.ts` | Rename `addOrUpdateDmSpace` → `addOrUpdateNavSpace`; handle `kind: 'community'` (creates community NavNode); continue ignoring `kind: 'channel'`; `community-members-changed` listener updates `myPower` + `memberCount` on affected community |
| `src/lib/components/NavPanel.svelte` | Add global "+" FAB right of the existing mode-toggle row; fan-out menu (4 items: New DM / New Group DM / New community / Redeem invite); render community-kind nodes as collapsible-folder-like; right-click context menu |
| `src/App.svelte` | Mount `CommunityService` on Tauri-connect; wire all 4 dialogs + CommunitySettingsPanel + sub-modals; route community-node-clicks to overview placeholder right-pane |

**No backend changes.** Phase 4 IPCs (PR #89) + ZEB-260 verify chain (PR #90) are the entire backend surface.

## 4. NavService changes

### Type additions (`types.ts`)

```typescript
export type NavNodeType = 'folder' | 'channel' | 'dm' | 'group-chat' | 'community';

export interface Community {
  id: string;          // hex-encoded community_id (32 chars)
  name: string;
  kind: 'open' | 'invite-only';
  myPower: number;     // 0-100, derived from materialized state
  memberCount: number;
}

export interface CommunityMember {
  address: string;
  displayName?: string;  // resolved via NavService.profiles
  power: number;         // 0-100
  status: 'joined' | 'invited' | 'banned';
  joinedAt?: number;
}

// Power-level thresholds — mirrors backend `POWER_THRESHOLDS` in community_membership.rs:1108
export const POWER_THRESHOLDS = {
  invite: 0,
  kick: 50,
  setPower: 100,
  max: 100,
} as const;

export type PowerRole = 'member' | 'mod' | 'admin';

export function powerToRole(power: number): PowerRole {
  if (power >= POWER_THRESHOLDS.setPower) return 'admin';
  if (power >= POWER_THRESHOLDS.kick) return 'mod';
  return 'member';
}
```

### `addOrUpdateNavSpace` extension

Existing `addOrUpdateDmSpace` becomes `addOrUpdateNavSpace`. New branch for `kind: 'community'`:

```typescript
if (kind === 'community') {
  const newNode: NavNode = {
    id: spaceId,
    type: 'community',
    name,
    parentId: parentId ?? null,
    expanded: true,         // default expanded; user can collapse
    unreadCount: 0,
    unreadLevel: 'none',
    peer: undefined,         // communities have no single peer
  };
  // Same add/modified/removed semantics as DM branch, with the
  // community-specific Fix-G concerns (preserve user-applied state on
  // duplicate `added`).
  // ...
}
```

`kind: 'channel'` continues to be silently ignored with a comment ("Phase 5 doesn't ship channel IPCs; reserved for the channel-introduction phase").

### `community-members-changed` listener

Lives in `community-service.ts` (not directly in `nav-service.ts`). When fired, the service:

1. Refetches `list_community_members(community_id)` for the affected community.
2. Updates `Community.myPower` and `Community.memberCount` in its in-memory cache.
3. Emits `onChange` so any open `CommunitySettingsPanel` re-renders.
4. Tells `NavService` to re-emit its own `onChange` so member-count badges in the nav update.

### `community-state-sync-degraded` listener

`CommunityService` tracks `degraded: Map<communityId, boolean>`. Updates on event. Settings panel reads from this for the "Sync status" line.

## 5. Entry-point UX

### Global "+" FAB in NavPanel

- Placement: right end of the existing mode-toggle row, separated by a vertical divider.
- Click opens a popover anchored to the button.
- Menu items (split with a divider between DM section and community section):
  1. "💬 New direct message" → opens existing `DmCreateDialog` (1 recipient).
  2. "👥 New group DM" → opens existing `DmCreateDialog` (multi-recipient).
  3. (divider)
  4. "🏛️ New community" → opens new `CreateCommunityDialog`.
  5. "🔗 Redeem invite link" → opens new `RedeemInviteDialog`.
- Popover dismisses on: click-outside, Escape, menu-item click, another "+" click.
- Keyboard accessible: Tab + Enter; arrow keys navigate menu items; Escape closes.

### Community node rendering

- Renders like a folder: chevron + name + kind indicator (lock for invite-only, globe for open) + member-count badge.
- Click chevron → expand/collapse.
- Click row body → emit `selectCommunity` to App.svelte → right pane shows overview placeholder.
- Right-click row → context menu: Manage / Leave / Copy invite URL (last only if user has invite power).

### Right-pane overview placeholder

When user clicks a community node, the right pane shows:

- Community icon (placeholder for future avatar).
- Community name.
- One-line summary: kind icon + member count + your role badge.
- Honest empty-state copy: "No channels yet — channels arrive in a later phase. Until then, manage members and invites here."
- "Manage community" button → opens `CommunitySettingsPanel`.

## 6. CreateCommunityDialog

- Modal centered, ~420px wide.
- Fields: Community name (text input, required, trim whitespace, max length matches backend constraint — verify in plan); Kind (toggle: Open / Invite-only).
- Default kind: **invite-only** (matches polycentric-governance ethos; more secure default).
- Submit button disabled until name is non-empty.
- On submit: spinner; calls `create_community(name, kind)`; on success closes modal, NavService picks up new community via `nav-updated`, App auto-selects it (right pane shows overview placeholder).
- On error: in-modal error banner with retry; modal stays open, fields preserved.

## 7. RedeemInviteDialog

- Modal centered, ~480px wide.
- Single field: invite URL (textarea or wrapping input — URLs can be long).
- Submit button disabled until URL contains `harmony://invite/` prefix.
- On submit: spinner shows in-modal (no descriptive text required — implementer may add "Verifying..." or similar at their discretion); calls `redeem_invite(url)`. Backend has 15s timeout; UI does not need to support cancellation.
- On success: modal closes, NavService picks up new community via `nav-updated`, App auto-selects it.
- On error: error banner at top of modal; URL preserved for retry; banner shows friendly summary + expandable diagnostic disclosure.

### Error → user-facing-summary mapping

| Backend rejection | Friendly summary | Recovery hint |
|---|---|---|
| `BootstrapMissing` | Invite link is incomplete. | Ask the inviter to regenerate the link from a recent client build. |
| `BootstrapInvalidPubkey` | Invite link is malformed. | The embedded admin key isn't valid. Ask the inviter to regenerate. |
| `BootstrapAddressMismatch` | Invite link is malformed. | Embedded admin keys don't agree with each other. Ask the inviter to regenerate. |
| `BootstrapActorMismatch` | Invite link is malformed. | Bootstrap event was signed by someone other than the admin. Ask the inviter to regenerate. |
| `BootstrapCommunityMismatch` | Invite link points to a different community than the one it advertises. | The inviter may have a corrupted client. Ask them to reinstall and regenerate. |
| `BootstrapSignatureInvalid` | Invite link signature is invalid. | Either the link was tampered with in transit, or the inviter's client is buggy. Ask the inviter to regenerate via a different channel. |
| `BootstrapKindInvalid` | Invite link contains the wrong event type. | Likely a malformed client. Ask the inviter to regenerate. |
| `BootstrapInsertFailed(...)` | Couldn't bootstrap the community on this device. | Disclosure shows the inner `LocalInsertError`. Most likely transient — retry. |
| Inviter offline (timeout, ~15s) | Inviter is offline — try again later. | The community admin needs to be reachable when you redeem. Retry once they're back online. |
| Already a member | You're already in this community. | Modal closes; nav scrolls to existing community node. |
| Malformed URL (parse fail) | That URL doesn't look like a Harmony invite. | Make sure you copied the full URL starting with `harmony://invite/`. |
| Network failure | Couldn't reach the network. | Check your connection and retry. |

### Diagnostic disclosure content

When user expands the disclosure (`<details>` element), show:

```text
Variant: <RedeemBootstrapVerifyError variant name>
Telemetry tag: <reason_tag() output, e.g. bootstrap_signature_invalid>
Step: <step number of 6, when applicable>
Raw error: <Display string from the IPC>
```

The `reason_tag()` helper exists on the backend per ZEB-260 spec. UI parses the raw error string for the variant name when present.

## 8. CommunitySettingsPanel

Modal centered, ~640px wide, scrolling content. Four stacked sections:

### Info section

- Name, Type (lock/globe icon + label), Members count, Your role (badge + numeric power).
- Sync status: "● Healthy" (green) or "⚠ Degraded" (yellow). Pre-1.0 prototype signal; revisit at 1.0.

### Members section

- Search-by-name input (filters client-side over `list_community_members` results).
- Roster: avatar + display name + truncated address + role badge + action buttons.
- Action buttons (always visible, smaller styling — 10px font, muted backgrounds):
  - **Set role** — opens `SetPowerDialog` for that member. Visible only if caller's power ≥ `POWER_THRESHOLDS.setPower` AND caller's power > target's power.
  - **Kick** — opens tier-2 confirmation modal. Visible only if caller's power ≥ `POWER_THRESHOLDS.kick` AND caller's power > target's power.
- Caller's own row: never shows Kick or Set role on themselves.

### Invites section

Hosts the `InviteLinkManager` component (see §10). Render only if caller has invite power (`POWER_THRESHOLDS.invite` = 0 in v1, so effectively always rendered for joined members).

### Danger zone section

- Single button: "Leave community".
- Click triggers `leave_community(community_id)` flow:
  - If caller is **not** the only admin: opens tier-2 confirmation modal.
  - If caller **is** the only admin: opens tier-3 typed-confirmation modal.
- Determination of "only admin": `list_community_members` filtered to `power >= 100 AND status == 'joined'` — if length is 1 and includes self, route to tier 3.

## 9. SetPowerDialog

Sub-modal centered atop the settings panel, ~460px wide.

- Header: "Set <name>'s role" + truncated address + current power.
- Live role badge above the slider (Member / Mod / Admin).
- Slider: `<input type="range" min="0" max="100" step="1">`.
- **Number input alongside the slider** (~64px wide, monospace font, primary-color border): `<input type="number" min="0" max="100" step="1">`. Bidirectionally synced via Svelte two-way binding to the same state. Out-of-range values clamp on blur (not on every keystroke — let users type "1" before "100").
- Threshold annotations below: "0 — Member", "50 — Mod", "100 — Admin" with pipe markers.
- Action row: "Set role" (primary) and "Cancel".
- If new power = 100 (promote to admin) or current power = 100 and new power < 100 (demote from admin): on Submit, opens **tier-2 confirmation modal** before calling the IPC. Otherwise calls `set_power_level(community_id, target, power)` directly.

## 10. InviteLinkManager

Section inside the settings panel's Invites section. Two states.

### Initial state

- Brief explanation: "Generate a one-time invite link to share via DM, email, or any side channel."
- Single button: "+ Generate invite link".

### After "Generate" click

- Calls `generate_invite(community_id, null, null)` — `invitee_hint` and `expires_at` are `null` since backend ignores them.
- Renders the returned URL in a code-styled box with a "📋 Copy" button (uses `navigator.clipboard.writeText`).
- Below: "↻ Regenerate" (replaces visible URL with a new one) and "+ Generate another" (keeps current visible, generates an additional URL — useful for inviting multiple people in succession).
- Warning copy:
  - For invite-only: "Don't post publicly — it embeds your admin bootstrap signature. Each link can only be redeemed once."
  - For open: "Anyone with this URL can join. The same link works indefinitely."

Backend has no admin-side bookkeeping of generated invites (it doesn't track issued tokens), so the UI is fire-and-forget — once the URL is generated and copied, it's gone from the UI (regenerate replaces it).

## 11. Confirmation tiering

Three tiers, mapped to action severity / reversibility.

### Tier 1 — No confirmation

- Generate invite link
- Copy URL
- Set role within Member↔Mod range (power 0-99 transitions)

### Tier 2 — Click-confirm at offset position (`ConfirmationModal.svelte`)

Modal opens centered with destructive button on the **LEFT** of the action row, Cancel on the **RIGHT**. Trigger buttons (Kick at row-end-right, Leave in panel footer) are positioned such that a fast double-tap at the trigger location lands on Cancel, not Confirm.

- Kick member
- Promote to admin (set power 100)
- Demote from admin (set power 100 → <100)
- Leave community when other admins exist

### Tier 3 — Typed confirmation (`TypedConfirmationModal.svelte`)

User must type the community name into a monospace input. Confirm button stays disabled until typed value matches exactly (case-sensitive, trim-trailing-whitespace).

- Leave community when caller is the only admin

The typed confirmation message explains: "If you leave, no one can promote new admins, kick disruptive members, or generate new invite links. The community CRDT will persist on the network but become permanently ungoverned. Promote another member to admin first if you want to hand off control."

## 12. CommunityService

New service mirroring the shape of `MessageService` / `NavService`.

### Responsibilities

- Wrap the 7 community IPCs as typed methods (`createCommunity`, `redeemInvite`, `leaveCommunity`, `kickMember`, `setPowerLevel`, `generateInvite`, `listCommunityMembers`).
- Listen for `community-members-changed` and `community-state-sync-degraded` events.
- Maintain a per-community in-memory cache: `Map<communityId, { members, degraded, lastFetched }>`.
- Expose `onChange` callback so panels can re-render.
- Lazy-load: only fetch member roster when a settings panel opens for that community; subsequent `community-members-changed` events trigger a refetch only if the panel is still open.

### Tauri error extraction

All IPC error paths must use the canonical pattern from user memory:

```typescript
catch (e) {
  const message = e instanceof Error ? e.message : String(e);
  // ...
}
```

(Production rejections are strings; tests use Error objects with "Error: " prefix. Per the `feedback_tauri_error_extraction` memory.)

## 13. Testing strategy

### Vitest UI tests (per component)

| Component | Critical tests |
|---|---|
| `CreateCommunityDialog` | renders name + kind toggle; default kind = invite-only; empty name disables Create; submit calls `create_community(name, kind)` |
| `RedeemInviteDialog` | renders URL field + spinner during IPC; loading state visible while pending; all 12 error mappings render correct user-facing summary; disclosure expands to show variant + reason_tag; modal preserves URL on error for retry |
| `CommunitySettingsPanel` | renders Info / Members / Invites / Danger sections; member rows show role badges; Set role / Kick render only if caller has the threshold power; sync status reflects degraded event; community-members-changed triggers re-render |
| `SetPowerDialog` | slider ↔ number input bidirectional sync; number input clamps on blur; role badge derives from current value; submit calls `set_power_level` with numeric value |
| `InviteLinkManager` | Generate calls `generate_invite`; URL renders with copy button; copy uses clipboard API; regenerate replaces visible URL |
| `NavService` | `addOrUpdateNavSpace` handles `kind: 'community'`; continues ignoring `kind: 'channel'`; parentId placement for communities |
| `NavPanel` | "+" button renders; fan-out menu opens on click; menu items dispatch correct dialog-open events; community node click emits `selectCommunity`; right-click context menu |
| `CommunityService` | listens for `community-members-changed` + `community-state-sync-degraded`; tracks per-community degradation state; `list_community_members` caches per-community |
| `ConfirmationModal` | Confirm button positioned LEFT; cancel RIGHT; Escape cancels; Enter on Cancel cancels (not Confirm) |
| `TypedConfirmationModal` | "Leave anyway" disabled until typed string matches; case-sensitive; partial match keeps disabled; matching enables |

### App-level golden-path tests

- Create open community → nav shows new community node → click renders overview → Manage opens settings.
- Create invite-only community → admin generates URL → URL contains `ab` + `ap` fields (per ZEB-260 wire format).
- Redeem open URL → community appears in nav.
- Redeem invite-only URL → spinner → community appears in nav → admin's member list shows joiner via `community-members-changed`.

### Manual smoke test plan

Documented in PR body:

1. Two-device round-trip: Alice creates invite-only community on Device A, mints URL, sends to Bob. Bob redeems on Device B. Alice's settings panel auto-refreshes (via `community-members-changed`) showing Bob with MEMBER badge. Alice promotes Bob to mod (no confirmation). Alice kicks Bob (tier-2 confirmation). Bob's community node disappears.
2. Tier-3 typed confirmation: Alice (only admin) tries to Leave, sees typed-confirm modal, types wrong text (button stays disabled), types exact community name (button enables), confirms.
3. Sync degraded: disconnect from network, observe "⚠ Degraded" in Info section, reconnect, observe return to "● Healthy".

## 14. Acceptance criteria

- All 7 Phase 4 IPCs callable from UI and round-trip end-to-end.
- ZEB-217 parent acceptance criteria all satisfied:
  - Open community: any peer can join via URL.
  - Invite-only community: redeem requires valid admin bootstrap (verified per ZEB-260's chain of 6 checks).
  - Power-level enforcement: kick / set-power buttons gated by caller's power level.
  - Multi-device convergence: joining a community on phone surfaces on desktop within bounded latency.
  - Admin UI lets a community admin invite / kick / set-power and view the member list.
  - Invite links work cross-device; redeeming creates a Space + joins the community.
- All 10 vitest test files green; 100% of golden paths covered.
- All gates green on CI: cargo fmt + clippy + test (no backend changes but Phase 5 may touch shared types via the IPC contract layer); vitest; tsc.
- Manual two-device smoke documented in PR body.
- One PR delivered against `origin/main` of harmony-client.

## 15. Out-of-scope and follow-up tickets

The following are **not** addressed in this PR and remain for follow-up tickets (existing or to-be-filed by user):

- ZEB-260 Cases B+C (open-community first-Join cold-cache, self-Re-Join after Leave).
- Channel UI (text channels: not yet specified; voice channels: separate ticket family).
- ZEB-251 per-community customizable power thresholds.
- M-of-N admin recovery.
- Community-inside-community placement (semantically deferred).
- `invitee_hint` / `expires_at` UI (backend ignores; needs backend wiring first).
- `redeem_invite` cancellation UI (backend has no cancellation primitive; 15s timeout is the bound).
- Custom named roles (Discord-style).

## Appendix A — Open implementation questions for the planner

Items the design knowingly leaves open for the implementer to resolve during plan-writing:

1. **NavService API rename versioning.** Renaming `addOrUpdateDmSpace` → `addOrUpdateNavSpace` is part of the public NavService API. Verify no external callers exist (Phase 4's wiring should be the only caller).
2. **Default community placement on create.** New community created via `create_community` lands at root of the nav tree (parentId = null). User can drag-and-drop into a folder afterward. Verify the existing folder-placement IPC supports moving community nodes.
3. **Member count source.** `Community.memberCount` should derive from `list_community_members` filtered to `status == 'joined'`. Confirm with the backend whether `list_community_members` includes Banned/Invited members or only Joined.
4. **Avatar resolution for community members.** Existing `NavService.profiles` map is keyed by address — community members should reuse the same resolution path.
5. **Empty state when no communities exist.** First-run user has no DMs and no communities. The nav tree should render a friendly empty state with a "Create your first community" CTA pointing at the FAB. This is implementation polish, not load-bearing for the design.
