# ZEB-285 Phase 1: Community forking primitive — design

> **Scope:** Phase 1 of [ZEB-285](https://linear.app/zeblith/issue/ZEB-285). CRDT primitive + IPC + minimal UI sufficient for any joined member to fork a community they belong to. Defers polish (disclosure UI in original community, library-directory inheritance affordance, fork-of-fork chain visualization, verify-on-redeem of snapshot signatures, snapshots >5000 messages via content-addressed delivery) to follow-up tickets filed after merge.
>
> **Surface area:** ~3000–4000 lines of Rust + TypeScript + Svelte. End-to-end functional: a user can fork a community via the settings panel and see the fork in their nav, invite others to it, and view a unified timeline merging pre-fork history with live post-fork activity.

---

## 1. Context

### 1.1 Origin

This ticket was surfaced during [ZEB-284](https://linear.app/zeblith/issue/ZEB-284) (community moderation UX) brainstorming, when the user reframed the "last-admin orphans community" problem from a *prevention* axis ("block self-demote / block last-admin leave") to a *recovery* axis ("forking is always available"). The full reframing quote is captured in the ZEB-285 ticket body and shapes every design decision below.

### 1.2 Foundational alignment

Three project memories are load-bearing for this design:

- **`project_harmony_polycentric_governance`** — communities are the only first-class moderation primitive; no global moderation, no platform admin. Forking is the secession primitive that makes "communities are sovereign" operational.
- **`feedback_design_for_eventual_state`** — forking is the eventual UX for community lifecycle; ZEB-284's typed-confirm warning was explicitly forward-pointing here.
- **`feedback_engineer_for_real_scale`** — forking-as-recovery scales better than last-admin-guard-as-prevention; the prevention mechanism doesn't address admin-loses-identity at scale.

### 1.3 What this lands

A user who is currently a `Joined` member of community C can, from C's settings panel:

1. Click "Fork this community."
2. Provide a name (defaulted to `"{C's name} (fork)"`).
3. Optionally check "Fork silently" (no event in C's log) and/or "Also leave the original community."
4. Confirm (with tier-escalation to typed-confirm when `also_leave` is checked).

The result: a new community appears in their nav with a fork-glyph badge, lineage block in settings ("Forked from C, 2026-05-14 13:42 UTC, 1247 messages bundled"), and a unified channel timeline merging pre-fork history with live post-fork activity. Other members of C (if not silent) see a Fork event in C's log surfaced via C's settings panel. The forker can mint fork-invites via the existing community-invite flow; redemption auto-bundles the pre-fork snapshot for joiners.

## 2. Decisions

Seven design decisions locked in during brainstorming (2026-05-14):

| # | Axis | Decision |
|---|------|----------|
| 1 | Fork visibility | **Hybrid:** CRDT event in original's log by default, opt-out silent fork |
| 2 | History scope | **Snapshot-on-fork:** frozen pre-fork history bundled into fork's data dir, dual-keyset verifier |
| 3 | Library directory inheritance | **None:** forker adds manually post-fork |
| 4 | Forker's status in original | **User choice at fork time:** default stay, checkbox to also-leave |
| 5 | Permission to fork | **Any joined member, no power gate** (`POWER_THRESHOLDS.invite = 0`) |
| 6 | Provenance shape | **Single-hop:** `forked_from: Option<SpaceId>` (chain depth >1 resolved at display time in Phase 2) |
| 7 | Re-invite mechanism | **Existing `mint_invite` flow extended** to carry snapshot — no separate fork-invite IPC |

## 3. Wire surface

### 3.1 New `MembershipEventKind::Fork` variant

Adds one variant to the existing 11-variant enum in `src-tauri/src/community_membership.rs:43-167`:

```rust
/// ZEB-285: a joined member declares they have forked this community
/// into a new community with `fork_space_id` as its SpaceId. Non-mutating
/// — does NOT change materialized membership/power/channels, does NOT
/// trigger EpochRotation. Other members materialize it as visible
/// fork-lineage history. Verify rule: signer must be Joined at the
/// event's HLC (power threshold = 0, "any joined member, any time").
///
/// Variant tag "x" (1-char value, lowercase, unused before this).
/// Inner field key "fs" (2-char) per same-length-keys invariant at this
/// nesting level.
#[serde(rename = "x")]
Fork {
    #[serde(rename = "fs")]
    fork_space_id: SpaceId,
},
```

**Variant tag table after this change** (1-char value, lowercase):

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
| **`x`** | **Fork (new)** |

Silent-fork path emits **no** event in the original's log. `Fork` is wire-relevant only on the visible path.

### 3.2 `CommunityState.forked_from` field

Adds one optional field to `CommunityState` in `src-tauri/src/community_state_crdt.rs:28-59`:

```rust
/// ZEB-285: SpaceId of the community this one was forked from, or
/// None for a top-level (non-fork) community. Persisted in wire form
/// so a fork's lineage survives round-trips and is visible to anyone
/// who decodes the state. Set once at fork creation, never mutated.
/// Byte-compatible with pre-ZEB-285 blobs (omitted when None).
#[serde(rename = "ff", skip_serializing_if = "Option::is_none", default)]
pub forked_from: Option<SpaceId>,
```

CBOR key `"ff"`. Backwards-compatible: non-forked CommunityStates encode byte-identically to pre-ZEB-285 form (no `ff` key emitted via `skip_serializing_if`).

### 3.3 `CommunityInvitePayload` extensions

Two new optional fields on `CommunityInvitePayload` in `src-tauri/src/community_invite.rs:90-148`:

```rust
/// ZEB-285: SpaceId of the community this one was forked from.
/// Mirrors CommunityState.forked_from; carried in the invite so
/// joiners can mirror it into their local CommunityState during
/// redeem_invite_inner. None for non-fork invites.
#[serde(rename = "ff", skip_serializing_if = "Option::is_none", default)]
pub forked_from: Option<SpaceId>,

/// ZEB-285: frozen snapshot of the forker's pre-fork view of the
/// ORIGINAL community. Present only on fork-invites (None for normal
/// community invites). Bounded by snapshot policy (§4.2): all
/// membership events plus most-recent N=500 channel-log messages per
/// channel, capped at total M=5000 messages. Joiner stores the
/// snapshot in the fork's data dir keyed by the original SpaceId for
/// dual-keyset verification of pre-fork events.
#[serde(rename = "fs", skip_serializing_if = "Option::is_none", default)]
pub pre_fork_snapshot: Option<PreForkSnapshot>,
```

Both fields use `skip_serializing_if = "Option::is_none"` so non-fork invites encode byte-identically to pre-ZEB-285 form.

### 3.4 New `PreForkSnapshot` type

Defined in `community_invite.rs` (alongside the other invite types):

```rust
/// ZEB-285: frozen snapshot of an original community's history,
/// bundled into fork-invites so fork-invitees can see pre-fork
/// context. Self-contained for verification: `identity_pubs` carries
/// the owner-pubkeys needed to verify every signer in
/// `membership_events` and `channel_log`, so joiners do NOT need to
/// query profile-broadcast to verify the snapshot.
///
/// Wire format: 6-key CBOR map. Field codes 2-char per same-length-
/// keys at this nesting level. Variant codes inside membership_events
/// and channel_log follow their own encoding rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreForkSnapshot {
    /// The original community's SpaceId. Signed pre-fork events
    /// reference this SpaceId in their bodies; the dual-keyset
    /// verifier dispatches by this value.
    #[serde(rename = "oi")]
    pub original_community_id: SpaceId,

    /// Display name of the original community at fork time. Used for
    /// the fork's Lineage UI ("Forked from {name}").
    #[serde(rename = "on")]
    pub original_community_name: String,

    /// Membership-CRDT events from the original, signed against the
    /// original's keyset. Replayed at display time against
    /// `identity_pubs` for verification; not inserted into the fork's
    /// own CommunityState event log.
    #[serde(rename = "ev")]
    pub membership_events: Vec<SignedMembershipEvent>,

    /// Bounded channel-log snapshot per §4.2 policy.
    #[serde(rename = "cl")]
    pub channel_log: BoundedChannelLogSnapshot,

    /// Map from every OwnerAddr that signs any event in this snapshot
    /// to their 64-byte identity public bytes (X25519_pub(32) ||
    /// Ed25519_pub(32) matching Identity::to_public_bytes()).
    /// Required because fork members are NOT necessarily members of
    /// the original community, so OwnerDeviceCache won't have signers
    /// cached. Bundled inline so verification needs no external lookup.
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pubs_map",
        deserialize_with = "deserialize_identity_pubs_map"
    )]
    pub identity_pubs: BTreeMap<OwnerAddr, [u8; 64]>,

    /// Forker's local HLC at fork time. Informational — used to
    /// render the "Fork point" divider in the fork's unified timeline.
    /// NOT used for any verification or ordering decision.
    #[serde(rename = "ts")]
    pub forked_at: Hlc,
}
```

`BoundedChannelLogSnapshot` is a new sibling type wrapping per-channel `Vec<SignedChannelLogEvent>` (using the existing channel-log event type from `community_channel_log.rs`), bounded by snapshot policy.

### 3.5 Verify rule for `Fork`

In `verify_event` in `community_membership.rs` (existing function around line 1620+), the match arm for `MembershipEventKind::Fork` requires:

```rust
MembershipEventKind::Fork { .. } => {
    // ZEB-285: any joined member can fork at any time. Power
    // threshold 0 — same as Leave. Fork is non-mutating, doesn't
    // affect membership/power/channels, doesn't trigger EpochRotation.
    if actor_power < POWER_THRESHOLDS.invite {
        return Err(VerifyError::InsufficientPower {
            required: POWER_THRESHOLDS.invite,
            actual: actor_power,
        });
    }
    // No additional checks — fork_space_id is a self-reported value
    // from the forker; receivers don't (and can't) verify the fork's
    // existence on the forker's device.
}
```

`POWER_THRESHOLDS.invite` is `0` in v1, so this gates on "actor is a Joined non-Banned member" (the membership-at-HLC check is performed upstream by the existing verifier scaffolding).

### 3.6 Materialize rule for `Fork`

In `materialize` in `community_membership.rs`, the match arm for `MembershipEventKind::Fork` is a **no-op**:

```rust
MembershipEventKind::Fork { .. } => {
    // ZEB-285: non-mutating. Fork events are recorded in the event
    // log for historical/audit visibility but do not change the
    // materialized membership/power/channels view. They are
    // surfaced separately in the settings panel "Recent forks"
    // listing (Phase 2 — Phase 1 only stores them in events).
}
```

The Fork event lives in `CommunityState.events` (and is byte-identical across replicas), but doesn't alter the materialized view returned by `materialized()`.

## 4. Storage + dual-keyset verifier

### 4.1 Fork's data dir layout

A fork has the same on-disk layout as a normal community (it IS a community) PLUS one extra read-only artifact:

```text
app_data_dir/communities/{fork_space_id}/
├── community_state.bin       # Fork's CommunityState (live, post-fork events)
├── channel_log_{ch_id}.bin   # Per-channel logs (live, post-fork posts)
├── pre_fork_snapshot.bin     # NEW: frozen PreForkSnapshot artifact (Phase 1)
└── ... (other per-community files: keys, settings, etc.)
```

`pre_fork_snapshot.bin` is written ONCE at fork creation (locally via `fork_community` IPC) or ONCE at fork-invite redemption (via `redeem_invite_inner`), then never mutated. Atomic write via tempfile + rename, matching the existing `follows.rs:36-86` / `content_index.rs` idiom.

### 4.2 Snapshot policy (Phase 1 caps)

- **Membership events:** ALL events in the forker's local view of the original's `CommunityState.events`. Typically small (kilobytes; bounded by the original's own CRDT bounds — no membership-event-count cap applies in v1).
- **Channel-log messages:** most-recent `N = 500` messages per channel by HLC descending, **capped at total `M = 5000` messages across all channels**. Trim algorithm:
  1. For each channel, take its most-recent N=500 by HLC descending. Call this `slice_i` for channel `i`.
  2. If `Σ |slice_i| ≤ M`: bundle every `slice_i` as-is.
  3. Otherwise: scale each `slice_i` down to `floor(|slice_i| × M / Σ |slice_i|)`, retaining the most-recent items in each. Any remainder from rounding goes to the channel with the largest `|slice_i|` first.
  4. Total bundled is in `[M − K, M]` where K is the channel count (rounding loss).
- **Identity pubs:** owner-pubkey for every unique signer referenced by `membership_events` or `channel_log`. Inline.

Snapshots larger than M are a Phase 2 enhancement (content-addressed delivery via Zenoh BLOB transfer; invite carries pointer, recipient fetches separately).

Hard byte limit not enforced in Phase 1; M=5000 plus typical message sizes keeps invite payloads well under 1MB.

### 4.3 Dual-keyset verifier

Today's `verify_event` is already SpaceId-aware (the event's signed body carries `community_id`). For ZEB-285, we add a sibling entry point:

```rust
/// ZEB-285: verify a single signed event against a frozen pre-fork
/// snapshot's identity_pubs map. Used by the fork's UI when loading
/// pre-fork history for display — fork members are not necessarily
/// members of the original, so the live OwnerDeviceCache won't have
/// the original's signers cached.
///
/// Replays the snapshot's `membership_events` in HLC order to
/// reconstruct the materialized-at-HLC context required for power-rule
/// checks. (Phase 1 invokes this lazily at display time; Phase 2 will
/// invoke it eagerly at redeem time to reject malicious snapshots
/// with forged signatures.)
pub fn verify_snapshot_event(
    event: &SignedMembershipEvent,
    snapshot: &PreForkSnapshot,
) -> Result<(), VerifyError> { ... }
```

Mechanically: look up `event.body.actor` in `snapshot.identity_pubs`, verify the Ed25519 signature against the canonical-CBOR encoding of the signed body, then check the event's power preconditions against materialized state replayed from `snapshot.membership_events` up to `event.signed_at_hlc`.

The fork's **live** event stream uses the existing `verify_event` path unchanged.

### 4.4 Lazy vs eager verification

**Phase 1 verifies snapshot events lazily** (at UI display time, only for events actually rendered). Rationale: a malicious forker could ship forged signatures regardless — they had the plaintext, so the signatures' authenticity is partly an honesty signal rather than a cryptographic guarantee for downstream-only readers. Lazy verification keeps redemption cheap.

**Phase 1 verifies membership events only; channel events in `BoundedChannelLogSnapshot` are rendered without per-message signature verification.** Phase 2 adds eager + channel-event verification.

**Phase 2 will eagerly verify snapshot events at redemption** time (replay all `membership_events` + sample-verify channel-log events) so a known-malicious forker can be detected before snapshot bytes hit disk. Filed as a follow-up after Phase 1 merges.

### 4.5 Original community on the forker's device

The fork's data dir is separate; the forker's original community data continues to live under `app_data_dir/communities/{original_space_id}/` and is untouched by the fork operation itself.

If the forker checks `also_leave`, the original's CRDT receives a `Leave` event normally (visible-fork path: bundled atomically with the Fork event; silent-fork path: standalone Leave). EpochRotation auto-fires per ZEB-249 in either case. The forker's data dir for the original is NOT deleted on Leave — that's a separate user action ("delete my local data for {original}") and is out of scope for ZEB-285.

## 5. IPC + fork-invite payload

### 5.1 New IPC: `fork_community`

```rust
#[tauri::command]
pub async fn fork_community(
    state: tauri::State<'_, NodeState>,
    community_id: SpaceId,
    opts: ForkCommunityOpts,
) -> Result<ForkCommunityResult, String> { ... }
```

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ForkCommunityOpts {
    /// Display name for the new fork community. Required — forker
    /// must distinguish the fork from the original in their own nav.
    /// Default suggestion in UI: "{original_name} (fork)".
    pub name: String,

    /// When true, skip emitting `MembershipEventKind::Fork` into the
    /// original community's log. Pure local fork creation. Default
    /// false.
    #[serde(default)]
    pub silent: bool,

    /// When true, also emit a Leave event in the original (triggers
    /// the normal EpochRotation-on-Leave per ZEB-249). Default false.
    #[serde(default)]
    pub also_leave: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForkCommunityResult {
    /// SpaceId of the newly-created fork community. Frontend uses
    /// this to navigate the user into the new community.
    pub fork_space_id: SpaceId,

    /// Whether the original community received a Fork event (visible
    /// path) or not (silent path). For UI confirmation feedback.
    pub visible: bool,

    /// Number of pre-fork messages bundled into the snapshot
    /// (post-capping by §4.2 policy). For UI feedback.
    pub snapshot_message_count: usize,
}
```

### 5.2 Operation steps

Inside `fork_community`, in order:

1. **Validate** — forker is `Joined` (not Left/Banned) in `community_id`. Otherwise `Err("not a member of community {id}")`.
2. **Generate** fork `SpaceId` via existing `SpaceId::new()` (16 random bytes).
3. **Build `PreForkSnapshot`** — read forker's local `CommunityState` and per-channel logs, apply §4.2 caps, gather `identity_pubs`, set `forked_at = Hlc::now()`.
4. **Construct fork bootstrap state** — forker's signed `Join` (admin power 100), signed `ChannelCreate { #general }`. Persist as new community at `app_data_dir/communities/{fork_space_id}/community_state.bin`.
5. **Set `CommunityState.forked_from = Some(community_id)`** on the fork; persist.
6. **Write `pre_fork_snapshot.bin`** to fork's data dir via atomic tempfile + rename.
7. **If `!silent`:** mint and sign `MembershipEventKind::Fork { fork_space_id }`, insert into original's `CommunityState`, publish via existing Zenoh state-root broadcast (using existing `community_state_sync.rs` plumbing).
8. **If `also_leave`:** mint and sign `MembershipEventKind::Leave`, insert into original, publish. EpochRotation auto-fires per ZEB-249.
9. **Emit `community-forked` frontend event** with `{ fork_space_id, original_id }` so NavService surfaces the new community.

**Rollback strategy:**
- Failures in steps 4-6 (fork-side creation) → no events emitted in original; clean up fork's partial data dir; return `Err`.
- Failures in steps 7-8 (post-creation events in original) → log the failure (`tracing::warn!`) but keep the fork already on disk; return `Ok` with `visible = false`. Forker can retry the announce / leave from settings later (Phase 2 retry surface; Phase 1 logs the failure for diagnostic recovery).

### 5.3 Extending `mint_invite` for fork-invites

Existing `mint_invite(community_id, ...)` produces a `CommunityInvitePayload`. For fork-invites, the same code path:

- Reads the fork's `CommunityState.forked_from`. If `Some(original)`:
  - Bundles `forked_from: Some(original)` into the payload.
  - Reads `pre_fork_snapshot.bin` from the fork's data dir and bundles `pre_fork_snapshot: Some(snapshot)`.

No new IPC. The same `mint_invite(fork_space_id, ...)` call diverges only because the community has `forked_from` set.

### 5.4 Extending `redeem_invite_inner` for fork-invite redemption

After the existing CommunityState-bootstrap steps in `redeem_invite_inner`:

- If `payload.forked_from.is_some()`: set `CommunityState.forked_from = payload.forked_from` on the joiner's local view.
- If `payload.pre_fork_snapshot.is_some()`: write the snapshot bytes to `app_data_dir/communities/{this_community_id}/pre_fork_snapshot.bin` via atomic tempfile + rename.

**Verify-on-redeem of snapshot signatures is NOT performed in Phase 1** (see §4.4). Phase 2 hardening lands as a follow-up.

### 5.5 Frontend wrapper

In `src/lib/community-service.ts` (alongside the existing `kickFromCommunity` / `setPowerLevel` wrappers):

```typescript
async forkCommunity(
  communityId: string,
  opts: { name: string; silent?: boolean; alsoLeave?: boolean }
): Promise<{
  forkSpaceId: string;
  visible: boolean;
  snapshotMessageCount: number;
}> {
  try {
    return await this.adapter.invoke('fork_community', {
      communityId,
      opts,
    });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    throw new Error(`Fork failed: ${msg}`);
  }
}
```

Per `feedback_tauri_error_extraction` memory: catch always uses `e instanceof Error ? e.message : String(e)`.

Tauri IPC param naming: Rust `community_id` / `opts` ↔ JS `communityId` / `opts`. Inner `opts` fields: Rust `also_leave` ↔ JS `alsoLeave`. The Tauri boundary auto-converts.

## 6. UI surface (Phase 1 minimal)

### 6.1 `ForkConfirmDialog.svelte`

New component, triggered from `CommunitySettingsPanel.svelte` via a new "Fork this community" button.

Layout (text mockup):

```text
┌─ Fork this community ──────────────────────────┐
│                                                 │
│  This creates a new community with a frozen    │
│  copy of the history you can see in this one. │
│  Anyone you invite to the fork will see that   │
│  history.                                       │
│                                                 │
│  Name:  [{original_name} (fork)        ]       │
│                                                 │
│  [ ] Fork silently (don't tell other members)  │
│  [ ] Also leave the original community         │
│                                                 │
│  Snapshot will include ~{N} messages.          │
│                                                 │
│            [ Cancel ]   [ Create fork ]        │
└─────────────────────────────────────────────────┘
```

**Inputs:**
- `vine`-style props (matching `ReshareConfirmDialog`): `originalName: string`, `messageCount: number`, `onConfirm: (opts) => void`, `onCancel: () => void`.
- Local state: `name: string` (prefilled), `silent: boolean`, `alsoLeave: boolean`.

**Confirmation tier behavior** (per `feedback_severe_action_confirmation` memory):

- `also_leave === false`: severe-but-reversible → "Create fork" button is a secondary-position click-confirm. Pressing it invokes `onConfirm({ name, silent, alsoLeave: false })` immediately.
- `also_leave === true`: leave-half is effectively irreversible for invite-only communities (no auto-rejoin) → escalate. Pressing "Create fork" with `also_leave` checked opens a second-stage typed-confirm modal (`TypedConfirmationModal` shape) reading: "Type **leave** to confirm leaving the original community." Only after the typed match does `onConfirm({ name, silent, alsoLeave: true })` fire.

The `silent` checkbox does NOT change the tier — silent vs visible is symmetric in reversibility (just whether a `Fork` event is emitted to the original log).

**Validation:**
- "Create fork" button is disabled when `name` is empty or whitespace-only.

### 6.2 NavService fork-glyph

`NavService` renders communities with `forked_from: Some(_)` with a small fork-glyph prefix in the nav-tree node label:

```text
↳ Cool Community (fork)
```

The glyph is `↳` (U+21B3 "DOWNWARDS ARROW WITH TIP RIGHTWARDS"). Tooltip on hover:

- If forker is still a member of the original AND has it in their local nav: "Forked from {original_name}"
- Otherwise: "Forked from another community"

Resolution happens via a new `NavService.resolveForkParentName(forkedFromId)` helper that looks up the original community's display name from the local nav-store, returning `null` if not found.

### 6.3 `CommunitySettingsPanel` Lineage block

For forked communities (`forked_from: Some(_)`), `CommunitySettingsPanel.svelte` renders a new "Lineage" section:

```text
Lineage
─────────────────────────────────────
Forked from: Cool Community
Forked at:   2026-05-14 13:42 UTC
Snapshot:    1247 messages bundled
```

For non-forked communities (`forked_from: None`), the entire Lineage section is omitted from the rendered output (NOT shown as "no lineage" empty state — just absent).

### 6.4 Unified timeline rendering

When displaying channel messages in a forked community, the channel-view component loads BOTH:

- `pre_fork_snapshot.channel_log` filtered by current channel
- Live channel log from post-fork events

Merge by HLC ascending; render a non-interactive divider row at the boundary:

```text
─── Forked from Cool Community on 2026-05-14 13:42 UTC ───
```

**Visual treatment:**
- **Above the divider** (pre-fork messages, original-signed): rendered with a subtle muted treatment OR a tiny "from {original_name}" per-message badge. Either is acceptable; final visual TBD by frontend implementation.
- **Below the divider** (live fork messages): normal rendering.

**Phase 1 lazy verification:** each pre-fork message is verified on demand via `verify_snapshot_event` when it scrolls into view. Verification failures render with a "couldn't verify signature" badge but don't hide the message.

### 6.5 Settings panel "Fork this community" entry

`CommunitySettingsPanel.svelte` gains a new button in the existing actions area (likely below the Member panel link):

```text
[ Fork this community ]
```

Available to any user who is `Joined` in the current community (i.e., any user viewing this panel — the panel itself isn't shown to non-members). Clicking opens `ForkConfirmDialog.svelte` (§6.1).

## 7. Testing strategy

### 7.1 Rust unit tests — `community_membership.rs`

Added to the existing in-module `#[cfg(test)] mod tests`:

| Test | Verifies |
|------|----------|
| `fork_event_cbor_roundtrip` | `MembershipEventKind::Fork { fork_space_id }` encodes/decodes byte-identical; variant tag is `"x"`, inner key is `"fs"` |
| `fork_event_all_variants_roundtrip_extended` | Existing `all_variants_roundtrip` test fixture extended to include Fork |
| `verify_event_fork_allows_any_joined_member` | Power-0 Joined member's Fork passes; Banned signer rejected |
| `verify_event_fork_rejects_non_member` | Fork signed by non-member rejected with `VerifyError::NotMember` |
| `materialize_fork_is_non_mutating` | Inserting a Fork event does NOT change materialized members/power/channels |
| `fork_does_not_trigger_epoch_rotation` | Insert Fork → no EpochRotation auto-fires |

### 7.2 Wire-format pinning

Either extends `src-tauri/tests/wire_format_channel_log_fixtures.rs` or lands as a new sibling `wire_format_membership_event_fixtures.rs`. Whichever location, the tests:

| Test | Verifies |
|------|----------|
| `fork_event_canonical_cbor_pinned` | Deterministic-nonce signed Fork event encodes to a pinned byte fixture |
| `pre_fork_snapshot_canonical_cbor_pinned` | A small synthetic PreForkSnapshot encodes byte-identically across runs |
| `community_invite_with_fork_fields_pinned` | A CommunityInvitePayload with both fork fields set round-trips byte-identically |

### 7.3 Rust unit tests — `community_state_crdt.rs`

| Test | Verifies |
|------|----------|
| `community_state_forked_from_cbor_skip` | `forked_from = None` encodes byte-identical to pre-ZEB-285 wire form (no `ff` key emitted) |
| `community_state_forked_from_some_roundtrip` | With `forked_from = Some(_)`, the `ff` key appears and round-trips |

### 7.4 Rust unit tests — `community_invite.rs`

| Test | Verifies |
|------|----------|
| `invite_payload_with_pre_fork_snapshot_roundtrip` | Snapshot round-trips through encode/decode |
| `invite_payload_without_pre_fork_snapshot_byte_compat` | Non-fork invite encodes byte-identical to pre-ZEB-285 form |
| `redeem_invite_writes_snapshot_to_data_dir` | After `redeem_invite_inner` on a fork-invite, `pre_fork_snapshot.bin` exists on disk with expected bytes |

### 7.5 Rust integration tests

New file `src-tauri/tests/community_fork_integration.rs`:

| Test | Verifies |
|------|----------|
| `visible_fork_announces_in_original_log` | Engine A forks community C (visible); engine B (member of C) materializes the Fork event and can read `fork_space_id` from C's events log |
| `silent_fork_leaves_original_untouched` | Engine A forks C silently; engine B's local view of C's events shows no Fork event |
| `fork_creates_independent_community` | Post-fork: engine A's fork has its own SpaceId, fresh epoch keys, forker as power-100 admin, `forked_from = Some(C)` |
| `fork_invite_carries_snapshot_to_invitee` | Engine A forks C, mints fork-invite, engine D (non-member of C) redeems; D's fork data dir has `pre_fork_snapshot.bin` with all forker's pre-fork events |
| `also_leave_emits_leave_and_rotates_epoch` | Fork with `also_leave: true` results in original having Fork + Leave events; EpochRotation auto-fires per ZEB-249 |
| `dual_keyset_verify_snapshot_events` | Snapshot events verify against `pre_fork_snapshot.identity_pubs`, NOT against the fork's live OwnerDeviceCache |

Uses the same engine/tempdir harness as `community_state_sync` integration tests. No real Zenoh — local engine pairs only.

### 7.6 Frontend vitest tests

New file `src/lib/components/__tests__/ForkConfirmDialog.test.ts`, mirroring `ReshareConfirmDialog.test.ts` shape (170 lines, ~8 tests):

| Test | Verifies |
|------|----------|
| `renders_heading_inputs_checkboxes_snapshot_count` | Heading, name input, both checkboxes, and snapshot message count all present in DOM |
| `default_name_prefilled` | Name input is pre-populated with `"{originalName} (fork)"` |
| `fork_silently_checkbox_toggles_silent_flag` | Toggling "Fork silently" updates the `silent` flag in the outgoing payload |
| `also_leave_checkbox_toggles_alsoLeave_flag` | Toggling "Also leave" updates the `alsoLeave` flag in the outgoing payload |
| `onConfirm_called_with_payload_silent_false_path` | `onConfirm` fires with `{ name, silent, alsoLeave }` on Create click when `also_leave === false` |
| `also_leave_true_opens_typed_confirm_modal` | `also_leave === true` path opens second-stage typed-confirm; `onConfirm` only fires after typing `"leave"` exactly |
| `onCancel_called_on_cancel_escape_backdrop` | `onCancel` called on Cancel button / Escape key / backdrop click |
| `create_button_disabled_when_name_empty_or_whitespace` | Create button is `disabled` when name is empty or whitespace-only |

### 7.7 Smoke test (manual, documented in PR body)

Two-engine local run:
1. Engine A creates community C and posts ~10 messages across 2 channels.
2. Engine B joins C via invite, posts a few replies.
3. Engine A forks C with `name = "C (fork)"`, `silent = false`, `also_leave = false`.
4. Engine B sees the Fork event surface in C's settings panel.
5. Engine A mints a fork-invite, sends to engine D (not a member of C).
6. Engine D redeems the fork-invite, sees the fork in their nav with the `↳` glyph + Lineage block in settings.
7. Engine D scrolls the fork's main channel, sees pre-fork messages above the "Fork point" divider and post-fork (empty in this scenario) below.

### 7.8 CI gate verification

All five gates must be green at every commit (per HARD RULE):

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
# From repo root:
npx tsc --noEmit
npx vitest run
```

## 8. Out of scope

### 8.1 Phase 2/3 follow-ups (file as sub-tickets after Phase 1 merges)

- **Disclosure UI in original community** — modal/banner informing members "your messages can be re-broadcast by anyone via fork." UX work.
- **Original-community Fork-event timeline rendering** — render visible-path Fork events as inline system messages in the original's UI ("Alice forked this community"). Phase 1 surfaces them only via settings-panel listing or the `events` log.
- **Library-directory inheritance affordance** — one-click "add this fork to libraries the original was in." Phase 2.
- **Fork-of-fork chain visualization** — breadcrumb UI showing full ancestor chain. Phase 1 stores only `forked_from: Option<SpaceId>` (single hop); recursive resolution at display time is a Phase 2 UX layer.
- **Snapshot verification-on-redeem** — Phase 1 lazy-verifies snapshot signatures at display time only. Phase 2 hardens by replaying + verifying every signature at redemption (rejects malicious forker with forged signatures before disk write).
- **Snapshots >5000 messages via content-addressed delivery** — Phase 1 caps at 5000 to keep invite payloads <1MB. Phase 2 introduces Zenoh BLOB transfer with invite carrying a pointer.
- **"Recently forked" surface in original-community settings** — a curated list of visible forks (sourced from Fork events in the original's log) with one-click "Visit fork."
- **Pre-fork message author display via profile-broadcast resolution** — if a snapshot includes messages from someone the joiner has a `ProfileMembershipBroadcast` cache entry for, resolve display name from that. Falls back to `Unknown ({short_addr})` in Phase 1.
- **Forker-side retry surface for failed announce/leave** — if step 7 or 8 of `fork_community` fails after the fork is on disk, Phase 2 surfaces a retry button in settings; Phase 1 logs and forgets.

### 8.2 Permanent omissions (NOT planned, by design)

- **Bidirectional sync between forks** — forks are independent; no shared event log.
- **Merging forks** — once forked, no merge primitive. Users who want to bridge run accounts in both.
- **Forking DMs** — DMs are 1-1, not community-scoped. Not applicable.
- **Cross-fork ban lists / federation** — each fork is sovereign per polycentric-governance memory.
- **Auto-rejoin original after `also_leave`** — leaving is treated as the user's authoritative intent. They can re-request an invite manually.
- **Forker-side preview of the snapshot before commit** — Phase 1 commits the snapshot at fork-creation time. The dialog displays the message count up-front ("Snapshot will include ~{N} messages"); users who want fine-grained preview can scroll their original-community view first.

## 9. Acceptance criteria

1. **CRDT variant**: `MembershipEventKind::Fork { fork_space_id: SpaceId }` defined with CBOR variant tag `"x"`, inner key `"fs"`. Verify rule: power ≥ 0 (any joined member); non-mutating materialize; no EpochRotation triggered.
2. **CommunityState lineage**: `forked_from: Option<SpaceId>` field with CBOR key `"ff"`, `skip_serializing_if = "Option::is_none"`. Byte-compatible with pre-ZEB-285 blobs (omitted when `None`).
3. **CommunityInvitePayload extensions**: `forked_from: Option<SpaceId>` (key `"ff"`) AND `pre_fork_snapshot: Option<PreForkSnapshot>` (key `"fs"`), both `skip_serializing_if`. Byte-compatible with pre-ZEB-285 invites when both `None`.
4. **`PreForkSnapshot` type** with fields `original_community_id`, `original_community_name`, `membership_events`, `channel_log`, `identity_pubs`, `forked_at`. Round-trips byte-identically through canonical CBOR.
5. **Snapshot caps**: most-recent N=500 messages per channel, capped at total M=5000 messages across all channels (proportional trim).
6. **`fork_community` IPC** with `ForkCommunityOpts { name, silent, also_leave }` and `ForkCommunityResult { fork_space_id, visible, snapshot_message_count }`. Atomic-ish per §5.2 step list. Failures in steps 7-8 (post-creation) are logged but don't tear down the fork.
7. **Invite extensions**: existing `mint_invite` reads `CommunityState.forked_from` and auto-bundles `pre_fork_snapshot` when set. `redeem_invite_inner` writes the snapshot to the joiner's data dir at `app_data_dir/communities/{this_id}/pre_fork_snapshot.bin`.
8. **Dual-keyset verifier**: `verify_snapshot_event(event, &snapshot)` validates against `snapshot.identity_pubs` rather than the live OwnerDeviceCache.
9. **`ForkConfirmDialog.svelte`** with name input (default `"{original} (fork)"`), `silent` checkbox, `also_leave` checkbox. Tier escalation: typed-confirm `leave` second stage gates submit ONLY when `also_leave` is checked.
10. **NavService fork-glyph**: communities with `forked_from = Some(_)` render with `↳` prefix in nav; tooltip resolves original name when available.
11. **`CommunitySettingsPanel` Lineage block** with original name, fork timestamp, snapshot message count. Omitted entirely for non-forked communities.
12. **Unified timeline rendering**: forked communities load `pre_fork_snapshot.channel_log + live channel_log`, merge by HLC ascending, render a non-interactive "Fork point" divider at the boundary.
13. **`CommunityService.forkCommunity()`** wrapper in `src/lib/community-service.ts` with `e instanceof Error ? e.message : String(e)` error extraction.
14. **Five CI gates green** at every commit: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.
15. **Tests per §7**: all Rust unit + wire-format pinning + integration tests pass; all 8 frontend vitest tests pass.
16. **PR body** includes smoke-test reference (§7.7), markdown-linked Linear refs ([ZEB-285](https://linear.app/zeblith/issue/ZEB-285) in auto-close paragraph; [ZEB-217](https://linear.app/zeblith/issue/ZEB-217), [ZEB-248](https://linear.app/zeblith/issue/ZEB-248), [ZEB-249](https://linear.app/zeblith/issue/ZEB-249), [ZEB-281](https://linear.app/zeblith/issue/ZEB-281), [ZEB-284](https://linear.app/zeblith/issue/ZEB-284) as linked context refs per `feedback_linear_pr_auto_close` memory).

## 10. References

### 10.1 Code

- `src-tauri/src/community_membership.rs` — `MembershipEventKind` enum (line 43), `verify_event` (line ~1620), `materialize` (line ~1180), `POWER_THRESHOLDS` (line 1855)
- `src-tauri/src/community_state_crdt.rs:28-59` — `CommunityState` struct
- `src-tauri/src/community_invite.rs:90-148` — `CommunityInvitePayload` struct; `mint_invite` and `redeem_invite_inner` (line ~1000+)
- `src-tauri/src/community_state_sync.rs` — Zenoh state-root broadcast plumbing
- `src-tauri/src/community_channel_log.rs` — `SignedChannelLogEvent` type for snapshot bundling
- `src-tauri/src/lib.rs` — Tauri IPC dispatch; `start_node` (production wiring site for fork_community)
- `src/lib/community-service.ts:185, 195` — existing `kickFromCommunity` / `setPowerLevel` wrappers (template for `forkCommunity`)
- `src/lib/components/CommunitySettingsPanel.svelte` — host for "Fork this community" button + Lineage block
- `src/lib/nav-service.ts` — fork-glyph integration site (new `resolveForkParentName(forkedFromId)` helper for tooltip resolution)
- `src/lib/components/ReshareConfirmDialog.svelte` + `src/lib/components/__tests__/ReshareConfirmDialog.test.ts` — template for `ForkConfirmDialog`
- `src/lib/components/TypedConfirmationModal.svelte` — typed-confirm primitive for the `also_leave` tier escalation

### 10.2 Linear tickets

- [ZEB-285](https://linear.app/zeblith/issue/ZEB-285) (this ticket — community forking primitive)
- [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) (Sub-C v1 community CRDT — foundational)
- [ZEB-248](https://linear.app/zeblith/issue/ZEB-248) (Sub-C v2 channels — channel-config CRDT precedent)
- [ZEB-249](https://linear.app/zeblith/issue/ZEB-249) (epoch rotation — auto-fires on `also_leave`)
- [ZEB-260](https://linear.app/zeblith/issue/ZEB-260) (invite-only redemption — admin_bootstrap precedent)
- [ZEB-281](https://linear.app/zeblith/issue/ZEB-281) (Sub-D Phase 4 profile-membership broadcast — Phase 2 author resolution)
- [ZEB-284](https://linear.app/zeblith/issue/ZEB-284) (moderation UX — this ticket's progenitor)

### 10.3 Memories

- `project_harmony_polycentric_governance` — communities-only governance, no platform admin
- `feedback_design_for_eventual_state` — forking is the eventual UX for community lifecycle
- `feedback_engineer_for_real_scale` — fork-as-recovery scales better than last-admin-prevention
- `feedback_severe_action_confirmation` — three-tier confirmation ladder for the `also_leave` escalation
- `feedback_tauri_error_extraction` — `e instanceof Error ? e.message : String(e)` in catch blocks
- `feedback_linear_pr_auto_close` — markdown-linked Linear refs in PR body; only ZEB-285 in auto-close paragraph
- `feedback_metadata_before_irreversible_write` — atomic tempfile+rename for `pre_fork_snapshot.bin` writes
