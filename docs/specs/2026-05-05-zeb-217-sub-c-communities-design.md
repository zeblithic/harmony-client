# ZEB-217 (Sub-C of ZEB-206): Community Membership CRDT + Invite/Join + Admin UI — Design

**Linear:** [ZEB-217](https://linear.app/zeblith/issue/ZEB-217/zeb-206-sub-c-harmony-client-community-membership-crdt-invitejoin)
**Parent epic:** [ZEB-206](https://linear.app/zeblith/issue/ZEB-206/) (nav-tree real-data wiring)
**Date:** 2026-05-05
**Status:** Design — pending implementation
**Author:** brainstormed against shipped ZEB-215 (owner-state CRDT) + ZEB-216 (DM transport) patterns

## Goal

Add Harmony's first-class moderation primitive — **communities** — as a multi-owner CRDT with signed events for join/leave/kick/power-level operations. v1 ships open + invite-only flavors with full admin UX (member list, kick, power-level, invite link manager) but **defers channels** to a follow-up ([ZEB-248](https://linear.app/zeblith/issue/ZEB-248/)).

Per the polycentric governance principle (project memory, locked in during ZEB-206 brainstorm): communities are Harmony's *only* first-class moderation primitive. Public channels and DMs have no moderation surface — open broadcast / closed link respectively. There is no global moderation, no platform-level admin, no algorithmic content promotion. Communities self-govern.

## Architecture

```
                 OWNER STATE                         COMMUNITY STATE
                 (per owner, single-writer)          (per community, multi-writer)
                 ─────────────────                   ─────────────────────

   harmony/owner/{addr}/state-root  ◄───►            harmony/community/{id}/state-root  ◄───►
       (encrypted, ZEB-211 key)                          (encrypted, MembershipKey)
       │                                                  │
       │ Spaces (incl. Community kind)                    │ SignedMembershipEvents
       │ Outbox / Inbox (DM)                              │ (join/leave/invite/kick/set_power)
       │ ReadMarker                                       │ Materialized:
       │                                                  │   members → MemberState
       │ ⚠ Community Space carries:                       │   power_levels → u8
       │   - id (canonical community id)                  │
       │   - membership_key                               │
       │   - admin_addr (creator)                         │
       │   - is_invite_only (policy flag)                 │
       │                                                  │
       └──── Reticulum unicast ────►                      │
            (CommunityInvite payload —                    │
             Path B app-sig binding;                      │
             mirrors DmInvite from ZEB-227)               │
```

**Key architectural decisions:**

1. **Extend the existing `Space` row** with community-only fields (`membership_key`, `admin_addr`, `is_invite_only`) rather than introducing a parallel `Community` table. Keeps the owner-state CRDT model simple; pattern matches how DM Spaces carry `content_key`.
2. **Per-community CRDT (`community_state_crdt.rs`) parallels owner-state's shape** — Prolly Tree + encrypted root CID + DAG-sync. Reuses ZEB-215 Phase 3a/3b machinery wholesale; if a generalization opportunity emerges later we can refactor, but premature abstraction would obscure both.
3. **All work in `harmony-client`** — no cross-repo PRs anticipated. Reuses CAS (harmony-content) and Reticulum unicast primitives unchanged.

### New Rust modules (`src-tauri/src/`)

| Module | Responsibility |
|---|---|
| `community_membership.rs` | Signed-event types, materialization, signature/power-level verification |
| `community_state_crdt.rs` | Prolly Tree per community (mirrors `owner_state_crdt.rs`) |
| `community_state_sync.rs` | Encrypted Zenoh topic + DAG-sync (mirrors `owner_state_sync.rs`) |
| `community_invite.rs` | `CommunityInvitePayload` + Reticulum send/receive for invite-only counter-sig flow |

### Modified Rust files

- `src-tauri/src/owner_state_types.rs` — extend `Space` with community-only fields, add `MembershipKey` newtype, extend `validate_invariants` for `Community` kind
- `src-tauri/src/lib.rs` — register new IPC commands, wire community state-sync at start_node, deep-link plugin handler (Phase 5)
- `src-tauri/src/event_loop.rs` — new select arms for community state ops + Reticulum CommunityInvite reception
- `src-tauri/src/dm_outbox.rs` — accept `CommunityInvite` as a packet variant in the unicast queue (small extension)

### Frontend changes (Phase 5)

New Svelte components: `CommunityCreateDialog`, `CommunitySettingsPanel`, `InviteLinkManager`, `InviteRedeemDialog`, `MemberRow`. Modified: `App.svelte` (deep-link subscription + redeem dialog mount), `NavSidebar.svelte` (+ New community button), `nav-service.ts` (community Space → click routing).

## Data model

### Space struct additions (in `owner_state_types.rs`)

```rust
pub struct Space {
    // ... existing 14 fields ...

    /// Per-community symmetric key for membership topic encryption.
    /// MUST be Some for kind == Community; MUST be None otherwise.
    /// Wire: bstr(32) under "mk".  Zeroized on drop.
    #[serde(rename = "mk", skip_serializing_if = "Option::is_none", default)]
    pub membership_key: Option<MembershipKey>,

    /// Initial admin (creator) — receives power 100 at community creation.
    /// MUST be Some for kind == Community; MUST be None otherwise.
    /// Wire: bstr(16) under "ad".
    #[serde(rename = "ad", skip_serializing_if = "Option::is_none", default)]
    pub admin_addr: Option<OwnerAddr>,

    /// Policy flag — false = open (peers can publish join events),
    /// true = invite-only (join requires counter-sig from member with
    /// power ≥ POWER_THRESHOLDS.invite).
    /// MUST be Some for kind == Community; MUST be None otherwise.
    /// Wire: bool under "io".
    #[serde(rename = "io", skip_serializing_if = "Option::is_none", default)]
    pub is_invite_only: Option<bool>,
}
```

All three new field codes are 2-char to preserve the **same-length-keys CBOR invariant** at the Space nesting level (every key here is exactly 2 chars → CBOR text(2) = 3 bytes per key, deterministic encoded length). `MembershipKey` is a new newtype with `ZeroizeOnDrop`, identical shape to `DmContentKey`.

### `validate_invariants` extension for Community kind

```rust
SpaceKind::Community => {
    if self.membership_key.is_none() {
        return Err(InvariantError("community must have membership_key".into()));
    }
    if self.admin_addr.is_none() {
        return Err(InvariantError("community must have admin_addr".into()));
    }
    if self.is_invite_only.is_none() {
        return Err(InvariantError("community must have is_invite_only".into()));
    }
    if !self.members.is_empty() {
        return Err(InvariantError("community must have members=[] in owner-state Space (real membership is in CommunityState CRDT)".into()));
    }
    if self.transport.is_some() {
        return Err(InvariantError("community must have transport=None".into()));
    }
    if self.community_id.is_some() {
        return Err(InvariantError("community must have community_id=None (community Space IS the community)".into()));
    }
}
```

### `CommunityState` CRDT (in `community_state_crdt.rs`)

Per-community Prolly Tree. Replicated via `harmony/community/{id}/state-root` topic.

```rust
pub struct CommunityState {
    pub community_id: SpaceId,
    /// Append-only signed event log, keyed by EventId (ULID).
    /// The Prolly Tree gives us O(log n) DAG-sync diffing and
    /// byte-stable canonical encoding.
    pub events: BTreeMap<EventId, SignedMembershipEvent>,
}

pub type EventId = [u8; 16];

pub struct SignedMembershipEvent {
    pub id: EventId,
    pub community_id: SpaceId,
    pub kind: MembershipEventKind,
    pub actor: OwnerAddr,
    pub at: Hlc,
    pub sig: [u8; 64],                     // ed25519 over canonical CBOR
    pub countersig: Option<CounterSignature>,  // required for invite-only Join
}

pub enum MembershipEventKind {
    Join,
    Leave,
    Invite    { target: OwnerAddr },
    Kick      { target: OwnerAddr, reason: Option<String> },
    SetPower  { target: OwnerAddr, level: u8 },
}

pub struct CounterSignature {
    pub signer: OwnerAddr,   // existing member with power ≥ invite_threshold
    pub sig: [u8; 64],       // signs the joiner's signed Join event payload
}
```

### Materialized views

Computed on read, cached with version counter — same pattern as DM `inbox_entries_for_space`:

```rust
pub struct MaterializedMembership {
    pub members: BTreeMap<OwnerAddr, MemberState>,
    pub power_levels: BTreeMap<OwnerAddr, u8>,  // unset = 0 = default
}

pub struct MemberState {
    pub status: MemberStatus,
    pub joined_at: Hlc,
    pub left_at: Option<Hlc>,
}

pub enum MemberStatus { Joined, Invited, Left, Banned }
```

### Power thresholds (v1 hardcoded)

```rust
pub const POWER_THRESHOLDS: PowerThresholds = PowerThresholds {
    invite:    0,    // any joined member can invite
    kick:      50,   // moderator-tier
    set_power: 100,  // owner/admin-tier
    max:       100,
};
```

Per-community customization is deferred to [ZEB-251](https://linear.app/zeblith/issue/ZEB-251/).

### Materialization rules

**Bootstrap (read before processing any events):** the community's `admin_addr` field on the owner-state Space IS the creator's power-100 designation. Materialized state initializes with `power_levels[admin_addr] = 100` BEFORE replaying any events. This sidesteps the chicken-and-egg problem where the first SetPower event would otherwise require the actor to already have power 100. The creator can later issue a SetPower to demote themselves or grant power 100 to others; SetPower events override the bootstrap value.

Replay events in HLC order:

- **`Join { actor, at }`** → `members[actor] = MemberState { status: Joined, joined_at: at }`. For invite-only communities, requires `countersig` to be present + valid. The creator's first event MUST be a Join (they implicitly hold power 100 from the bootstrap rule, but they're not a "member" until they Join — same way every other member must Join to be counted in `members`).
- **`Leave { actor, at }`** → updates `members[actor].status = Left, left_at = at`. Reversible by re-Join.
- **`Invite { actor, target, at }`** → `members[target] = MemberState { status: Invited, joined_at: at, left_at: None }` (target still needs a Join event to become a full member).
- **`Kick { actor, target, at }`** → updates `members[target].status = Banned, left_at = at`. Requires `actor.power > target.power` AND `actor.power ≥ kick_threshold`.
- **`SetPower { actor, target, level, at }`** → `power_levels[target] = level`. Requires `actor.power ≥ set_power_threshold`.

### Verification (run at every event before insertion into Prolly Tree)

1. **Signature** valid against `actor`'s owner pubkey
2. **For invite-only Join:** `countersig` valid AND `signer`'s current power ≥ `invite_threshold`
3. **Action's required power** ≤ `actor`'s current power (computed from materialized state at HLC time)
4. **For Kick:** actor's power strictly > target's power
5. Reject + don't replicate on any failure

Verification is **idempotent and pure** — given the same prior event log + the same candidate event, returns the same accept/reject. This makes Prolly Tree DAG-sync convergent: two devices that receive the same set of events will materialize the same state regardless of arrival order.

## Sync protocol

### Topic & encryption

- **Topic:** `harmony/community/{id}/state-root` (Zenoh). Mirrors `harmony/owner/{addr}/state-root`.
- **Wire payload:** Encrypted Prolly Tree root CID, published whenever a member appends a verified event.
- **Encryption:** ChaCha20-Poly1305 (same primitive as DM content + owner-state). Key = the community's `MembershipKey` (32 bytes), distributed via the invite payload. Topic is observable to anyone with `community_id`, but the payload is opaque without the key.

### Subscription lifecycle

When `community_state_sync.rs` starts, it scans `owner_state.spaces` for any `Space { kind: Community }` and subscribes to the corresponding state-root topic for each. When a new community Space appears in owner-state (because the owner joined a new community on this device, or another bound device replicated the join via Flow A), the sync module subscribes lazily. When a Space's `left_at` is set, it unsubscribes.

### Append flow (member publishes a new event)

```
1. Frontend → IPC (e.g., kick_from_community)
2. community_membership.rs builds + signs the SignedMembershipEvent
3. community_state_crdt.rs verifies it locally (signature + power)
4. Prolly Tree insert → new root CID
5. Encrypt root CID + new block(s) with MembershipKey
6. Publish encrypted root CID to harmony/community/{id}/state-root
7. Other members subscribe → receive root → decrypt → DAG-sync missing
   blocks via existing CAS/DAG-sync (ZEB-215 Phase 3b machinery)
8. Each subscriber re-runs verification on every newly-fetched event
   before inserting into their local Prolly Tree (defense-in-depth —
   peers don't trust each other's verification)
```

### New-joiner bootstrap

When you redeem an invite link and become a member:

```
1. Decode invite link → community_id, MembershipKey, (optional) inviter
2. Subscribe to harmony/community/{id}/state-root
3. First message received contains current root CID (encrypted)
4. Decrypt with MembershipKey → DAG-sync the entire Prolly Tree from
   any peer member (CAS fetch — same machinery as ZEB-215 Phase 3b)
5. Walk fetched events in HLC order, materialize state
6. Publish your own Join event (open) OR send via Reticulum to inviter
   for counter-sig + republish (invite-only — see "Invite system")
```

### Catch-up vs live

The pattern reuses ZEB-215 Phase 3a/3b mechanics: subscribers maintain a per-community `RootHlcTracker` to deduplicate roots they've already DAG-synced. New events arriving while DAG-sync is in-flight queue and re-process after the sync resolves.

### Multi-device convergence

- Owner-state replicates the community Space (with its `MembershipKey`) to all of the owner's bound devices via Flow A.
- Each bound device independently subscribes to the membership topic — they're peers in the community sync, not coordinating with each other.
- The materialized membership state converges because the underlying event log is canonical.

### Race: simultaneous join from two of my devices

Phone and desktop both publish `Join { actor: my_addr, at: hlc_phone }` and `Join { actor: my_addr, at: hlc_desktop }` (different ULIDs, slightly different HLCs). Both are valid signed events. Both land in the Prolly Tree on every member. Materialized state's `members[my_addr]` reflects whichever has the earlier HLC for `joined_at` — idempotent at the user-visible layer.

### Encryption-key rotation on membership change

**Deferred** ([ZEB-249](https://linear.app/zeblith/issue/ZEB-249/)). The `MembershipKey` does NOT rotate when members join/leave in v1. Threat model accepts that an ex-member who kept the key can still decrypt the membership topic (they can observe membership changes after they left). Closing this gap requires either backward-secrecy via per-epoch keys (TreeKEM-style) or accepting that re-encrypting the entire history on every membership change isn't viable.

## Invite system

Two layers — invite *link* (out-of-band sharing format) and `CommunityInvite` Reticulum *packet* (only for invite-only counter-sig flow).

### Invite link payload

```rust
pub struct CommunityInvitePayload {
    pub community_id: SpaceId,           // 16 bytes
    pub membership_key: MembershipKey,   // 32 bytes
    pub admin_addr: OwnerAddr,           // 16 bytes — for "inviting you to {admin's} community" preview
    pub community_name: String,          // for preview before redeeming
    pub is_invite_only: bool,            // tells redeemer whether to use Reticulum counter-sig flow
    pub expires_at: Option<Hlc>,         // None = never expires

    // Invite-only ONLY: the inviter's pre-signed invite token. Carried
    // in the link so the recipient can present it via Reticulum to any
    // member with power ≥ invite_threshold for counter-sig.
    pub invite_token: Option<InviteToken>,
}

pub struct InviteToken {
    pub inviter: OwnerAddr,                // who minted this invite
    pub invitee_hint: Option<OwnerAddr>,   // None = open redemption (anyone can redeem);
                                           // Some = bound to that owner addr (rejected otherwise)
    pub minted_at: Hlc,
    pub sig: [u8; 64],                     // inviter signs (community_id, invitee_hint, minted_at, expires_at)
}
```

**Why `invitee_hint`?** Two flavors of invite link in invite-only communities:
- **Open invite** (`invitee_hint = None`) — link can be redeemed by anyone. Lower friction; higher abuse risk.
- **Targeted invite** (`invitee_hint = Some(addr)`) — link only valid for the named owner. Admin generates one-per-friend.

### Encoding

```text
Wire bytes  = canonical CBOR of CommunityInvitePayload (~120-180 bytes)
URL form    = harmony://invite/{base64url(wire_bytes)}
              ~200-260 char URL — fits in chat messages, qr codes,
              email subject lines, signal messages.
```

Base64url (no `+`/`/`/`=` padding) makes it copy-paste-safe across messengers that munge `+`. Custom URL scheme `harmony://invite/...` is registered via Tauri 2 [`tauri-plugin-deep-link`](https://crates.io/crates/tauri-plugin-deep-link) — per-platform setup is documented config (Info.plist on macOS, .desktop file on Linux, registry on Windows).

### Open community redemption (no Reticulum needed)

```
1. User clicks harmony://invite/... in browser → harmony-client opens
2. Deep-link handler emits IPC event invite-link-received with payload
3. Frontend shows confirm dialog: "Join {community_name}, created by {admin_addr_short}?"
4. User confirms → IPC redeem_invite(payload)
5. Rust:
   a. Validate signature on InviteToken (if present, even though not
      required for open — being signed is still a useful authenticity
      hint for the preview)
   b. Add Space { kind: Community, id, membership_key, admin_addr,
      is_invite_only: false, members: [] } to owner-state (Flow A
      replicates to bound devices)
   c. Subscribe to community state-root topic, DAG-sync history
   d. Build + sign Join event { actor: my_addr, at: now_hlc }
   e. Publish to community state-root topic
6. Other members see Join event → materialized state updates
7. IPC event community-members-changed fires → frontend updates
   member list
```

### Invite-only redemption (Reticulum counter-sig hop)

```
1-3. Same as open — preview + confirm
4. User confirms → IPC redeem_invite(payload)
5. Rust:
   a. Validate InviteToken signature + invitee_hint match
   b. Build (but DON'T publish) Join event { actor: my_addr, at: now_hlc,
      sig: <my sig> }
   c. Subscribe to community state-root topic + DAG-sync history (so we
      have the current member list — needed to find any member with
      power ≥ invite_threshold)
   d. Pick an online member (try the inviter first; fall back to any
      member with sufficient power)
   e. Build CommunityInvite Reticulum packet (Path B app-sig binding,
      mirrors DmInvite from ZEB-227): { community_id, my Join event,
      InviteToken }
   f. Send via Reticulum unicast (existing dm_outbox / unicast_send_tx
      machinery — community_invite.rs is a thin wrapper)
6. Receiving member's event_loop:
   a. Receives CommunityInvite packet
   b. Verifies my Join event's signature + InviteToken signature +
      checks the inviter has sufficient power
   c. Builds CounterSignature signing my Join event payload
   d. Attaches countersig to my Join event → publishes to community
      state-root topic
7. Other members (incl. me) see the counter-signed Join →
   materialized state updates
8. Same IPC event community-members-changed → frontend updates
```

**If no member is online to counter-sign?** v1 returns `Err` from `redeem_invite` with a "no community members currently reachable" message. Frontend surfaces this as "no admin online; try again in a moment." User retries when an admin is back online.

The persistent retry / "join pending" state is a real UX gap (clicking an invite when the admin is offline forces manual retry), but closing it cleanly requires either extending `OutboxEntry` to carry multiple packet types or a "pending Join event" model where the joiner publishes their unsigned Join to the membership topic and any online admin counter-signs lazily. Both add real protocol surface; deferred to [ZEB-254](https://linear.app/zeblith/issue/ZEB-254/) so Sub-C v1 stays focused on the bootstrap loop.

`UnicastSendRequest` (the existing transient mpsc into `event_loop`) carries the immediate-attempt CommunityInvite packet. `community_invite.rs` is a thin wrapper that builds the packet bytes and pushes one `UnicastSendRequest` per online destination. No persistent queue extension needed in v1.

## IPC surface

### Commands

```rust
/// Create a new community. Writes a Space { kind: Community,
/// admin_addr: self, ... } to owner-state and publishes the creator's
/// initial Join event to the community state-root topic. The creator
/// holds power 100 implicitly via the Space's admin_addr field — see
/// "Materialization rules / Bootstrap" — no separate SetPower event
/// needed at creation time. Returns the new community's SpaceId.
create_community(
    name: String,
    is_invite_only: bool,
) -> Result<String>  // SpaceId hex


/// Decode + redeem an invite URL. Handles both open and invite-only
/// flavors automatically based on payload's is_invite_only flag.
/// For invite-only without an online counter-signer, returns Err
/// (no admin reachable). User retries once an admin is online.
/// Persistent "join pending" UX is deferred to ZEB-254.
redeem_invite(url: String) -> Result<String>  // SpaceId hex


/// Sets actor's MemberState to Left. Reversible via re-Join (a future
/// invite-redemption clears Left). Does NOT remove the local Space —
/// caller must follow with remove_space after the Leave event has been
/// broadcast and acked locally (mirrors the existing remove_space
/// pattern documented in ZEB-206 spec).
leave_community(community_id: String) -> Result<()>


/// Power-gated. Verified locally before publishing: actor's power must
/// be ≥ kick_threshold (50) AND strictly greater than target's power.
kick_from_community(
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<()>


/// Power-gated. Actor's power must be ≥ set_power_threshold (100).
set_power_level(
    community_id: String,
    target_addr: String,
    level: u8,  // 0..=100
) -> Result<()>


/// Power-gated (caller must have power ≥ invite_threshold = 0, i.e.
/// any joined member). Returns harmony://invite/{base64url} URL.
generate_invite(
    community_id: String,
    invitee_hint: Option<String>,  // None = anyone-redeem; Some = bound to addr
    expires_at: Option<u64>,       // unix-ms; None = never expires
) -> Result<String>


/// Returns the materialized member list for a community, sorted by
/// power level descending then by joined_at ascending. Includes
/// status (Joined / Invited / Left / Banned).
list_community_members(community_id: String) -> Result<Vec<MemberInfo>>

pub struct MemberInfo {
    pub addr: String,                  // OwnerAddr hex
    pub display_name: Option<String>,  // resolved via existing profile cache
    pub status: MemberStatus,
    pub power: u8,
    pub joined_at: Hlc,
}
```

### Events (Tauri emit → frontend listen)

| Event | Payload | When |
|---|---|---|
| `community-members-changed` | `{ communityId, changes: [{type, target, by?, detail?}] }` | Any local Prolly Tree insert (own append OR DAG-synced from peer) — payload describes the delta so frontend updates incrementally without a full re-fetch |
| `invite-link-received` | `CommunityInvitePayload` (decoded) | Deep-link plugin delivers a `harmony://invite/...` URL |
| `community-state-sync-degraded` | `{ communityId, reason }` | DAG-sync timeout or repeated decrypt failure — informational banner. Communities are new and degraded paths are still under-tested; keep this event in v1 (unlike owner-state's equivalent which we removed in ZEB-215 Phase 3b once it was confidence-tested) |
| `nav-updated` | (existing) | Fires automatically when a community Space is added/removed from owner-state |

### Param-naming convention

All Rust IPC params use `snake_case`; Tauri 2 auto-converts JS `camelCase` → Rust `snake_case` at the boundary. (Per the bug we caught + fixed in PR #81 round 4 — `space_id_hex` → `space_id` to match JS `spaceId`. ZEB-247 follow-up will land an end-to-end Tauri::invoke test that exercises this binding.)

### Single-IPC entry points

We deliberately **do not** expose a separate `join_community(community_id, membership_key)` IPC for v1. The only join path is `redeem_invite(url)` — Sub-D directory ([ZEB-252](https://linear.app/zeblith/issue/ZEB-252/)) can add a direct join path later when it ships, since that's where naked-id joining becomes meaningful.

## Frontend (admin UI)

### Component inventory

| Component | Purpose | Pattern reused from |
|---|---|---|
| `CommunityCreateDialog.svelte` | Modal: name field + open/invite-only toggle. Triggered by "+ New community" button at bottom of NavSidebar. | `DmCreateDialog.svelte` (ZEB-228) |
| `CommunitySettingsPanel.svelte` | Main admin surface — member list, invite-link tab, leave button. Opens when you click a community Space in NavService (v1 has no channels to navigate into). | New |
| `InviteLinkManager.svelte` | Sub-tab within CommunitySettingsPanel: generate link, list active invites, revoke (=mark expired). | New |
| `InviteRedeemDialog.svelte` | Modal triggered by `invite-link-received`. Shows community preview + Accept/Cancel. | `ConfirmDialog.svelte` (existing) |
| `MemberRow.svelte` | Single row in member list — display name, power badge, kick/power-level buttons (visibility gated by your own power). | New |

### Integration with NavService

- **+ New community** button at bottom of NavSidebar (next to "+ New DM" shipped in ZEB-228 Phase 4)
- Community Spaces render in nav with a 🏛 icon (folder icon shape), under their parent folder if any
- Click a community → main pane shows `CommunitySettingsPanel` (no channels yet means there's nothing else to route to)
- Once channels ship in [ZEB-248](https://linear.app/zeblith/issue/ZEB-248/), the settings panel moves behind a ⚙️ icon and channels become the primary content

### Deep-link integration

- Tauri 2 `tauri-plugin-deep-link` handles `harmony://invite/...` URL on every platform
- `App.svelte` subscribes to `invite-link-received` → mounts `InviteRedeemDialog` with decoded payload
- Dialog shows: community name (from payload), inviter's display name (resolved via profile cache), is-invite-only badge, expires-at (if set)
- Accept → IPC `redeem_invite(url)` → on Ok, NavService inserts the new community Space, dialog closes
- Cancel → discard payload, no state change

### Visibility rules (UI-side gating)

The IPC layer enforces power checks server-side, but the UI also gates buttons to give clear affordance signals:

| You can see | If you have |
|---|---|
| Member list | Joined this community |
| 🛡 admin/mod badges | Joined this community |
| **Invite member** button | `power ≥ invite_threshold` (0 — always) |
| **Kick** button on a row | `power ≥ kick_threshold (50)` AND `your_power > target_power` |
| **Set power** button on a row | `power ≥ set_power_threshold (100)` |
| **Leave community** | Joined |

Hidden (not greyed-out) buttons reduce visual clutter for non-admins.

### Layout (validated via wireframe during brainstorm)

The CommunitySettingsPanel uses a **tab strip** at the top (Members | Invite Links) with "Leave community" pushed to the right corner. Power level is rendered as a **colored chip** (orange = 100 admin, blue = 50 mod, plain = 0) for at-a-glance hierarchy. **Invited / Banned rows are visible inline** with reduced opacity + status icon (✉️ / 🚫) — not filtered out by default, since "what happened to dave?" debugging is a real need.

Member rows use a 5-column grid: status-icon | name | power-chip | joined-time | action-buttons. Action buttons appear only when actionable for the viewer's power level.

## Error handling, edge cases, security

### Verification rules (defense-in-depth)

Every `SignedMembershipEvent` is verified at THREE points:

1. **At local append** — before publishing to the topic
2. **At receive time on every peer** — before insertion into local Prolly Tree (peers don't trust each other's verification)
3. **At materialization time** — when computing power levels for any new event, replay-verify against the current materialized state (catches HLC reordering edge cases)

If any verification fails, the event is rejected and **not** replicated. The Prolly Tree DAG-sync still considers the block "fetched" so we don't re-fetch it endlessly, but the materialized state ignores it.

### Failure modes

| Failure | Behavior |
|---|---|
| Forged signature on event | Verification fails at receive — rejected, not added to Prolly Tree, not materialized |
| Event from actor with insufficient power | Rejected at receive — same as above |
| Two-device race: same admin kicks the same target simultaneously | Both events valid + signed by same actor with sufficient power. Both land in Prolly Tree. Materialized state shows kick once (idempotent at status=Banned) |
| Race: A sets bob's power to 75 on phone, simultaneously sets to 25 on desktop | Both events valid. HLC ordering picks last-writer-wins. The "loser" event lands in Prolly Tree but has no materialized effect (overwritten) |
| Race: A kicks B who is simultaneously kicking A | Whichever event has earlier HLC wins. The later event verifies against materialized state at its HLC, which includes the earlier kick — so the kicker who's already been kicked has insufficient power → rejected |
| Invite-only Join with countersig from a member who LATER lost power | The countersig was valid at the time the Join event was created (verified against materialized state at HLC time). HLC-ordered replay preserves the original verification. Join stays valid |
| Reticulum CommunityInvite delivery fails (no member online) | v1 returns `Err` immediately; user retries. Persistent retry / "join pending" UX deferred to [ZEB-254](https://linear.app/zeblith/issue/ZEB-254/) |
| Two of my devices both redeem the same invite link | Both publish Join events signed by my owner key with slightly different HLCs. Both land in Prolly Tree. Materialized members[my_addr] reflects whichever has earlier HLC. Idempotent at user level |
| Local DB corruption on one device | Owner-state DAG-syncs Space from peer bound device → membership_key recovered → community state DAG-syncs from peer member |
| MembershipKey leak (someone exfiltrates from device) | Attacker can decrypt the membership topic's history. Cannot publish events without a valid signing key for an existing member. Mitigation: rotation deferred to [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/) |
| Invite link leak (attacker grabs a `harmony://invite/...` URL) | For open communities: attacker can join. Admin can kick. For invite-only with `invitee_hint = None`: same — attacker can redeem. For invite-only with `invitee_hint = Some(addr)`: attacker can't redeem (the Join event signature won't match the hint). Mitigation: prefer targeted invites; admin can revoke open invites |
| All bound devices lost (full identity loss) | Falls into ZEB-173 identity-recovery story. Recovered owner has no community membership history; rejoining requires a new invite. **Out of scope for v1** |
| Community admin loses private key (single point of failure) | **Out of scope for v1.** Mitigation path: M-of-N admin power model, deferred to [ZEB-250](https://linear.app/zeblith/issue/ZEB-250/) |

### Multi-device convergence properties

- **All membership events are content-addressed** by their CBOR canonical encoding → DAG-sync deduplicates naturally
- **HLC ordering is total across the owner's bound devices** (logical counter dominates wall-clock skew)
- **Membership-CRDT root is per-community**, not per-owner → my desktop and phone independently subscribe + DAG-sync
- **Materialized state is deterministic** given the same event log → all bound devices converge to the same view

### Privacy properties (v1)

- **Member list private to joined members.** `harmony/community/{id}/state-root` topic is observable but encrypted with `MembershipKey`. Non-members see only "this community is active."
- **No backward secrecy on membership change.** Per "Encryption-key rotation on membership change", ex-members keep `MembershipKey` and can decrypt future events. Closing this is [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/).
- **No forward secrecy on key compromise.** ChaCha20-Poly1305 with a long-lived `MembershipKey` — same as DM ContentKey precedent. Both are bound by the device's encryption-at-rest story (ZEB-211).
- **Profile-membership broadcast is opt-in.** `ProfileMembershipBroadcast` (already in spec) is per-community; default is private.

### Documented non-goals (security threat-model boundaries)

- **Sybil attacks on open communities.** Anyone can spin up identities and join. Admin's only recourse is kick. Per polycentric governance: communities self-police; no platform-level Sybil defense.
- **Vote-stuffing or governance attacks.** No cryptoeconomic mechanism in v1. Power levels are admin-granted, not earned.
- **Compelled disclosure resistance.** A subpoenaed member can produce the membership event log + their `MembershipKey`. Out of scope; same threat model as DM content.

## Testing strategy

### Test taxonomy

| Layer | Test type | What's covered |
|---|---|---|
| **CRDT primitives** | Rust unit tests (`#[cfg(test)]`) | Event signing/verification, materialized-state replay, power-level rule edge cases (kick wars, race conditions, idempotent merges), CBOR round-trips, deserialize-with re-normalization for prior_events |
| **Sync protocol** | Rust integration tests (`tests/community_sync_integration.rs`) | Two-member community DAG-syncs the full event log; new joiner bootstraps via DAG-sync from an existing peer; events from forged signers don't replicate; encryption round-trips through the topic; degraded paths (decrypt failure, timeout) |
| **Invite system** | Rust integration tests (`tests/community_invite_integration.rs`) | Open redemption flow end-to-end; invite-only counter-sig flow with online inviter; invite-only with NO online inviter surfaces as `Err` (offline-pending UX is ZEB-254); invitee_hint mismatch rejected; expires_at enforced; signature tampering rejected |
| **IPC surface** | Rust IPC integration tests (`tests/community_ipc_integration.rs`) | All 7 commands from "IPC surface": validate args, return correct types, surface power-level rejection as `Err`, emit correct events |
| **End-to-end IPC binding** | New harness from **ZEB-247** (the deferred follow-up from PR #81) | At least one community IPC exercised through the real `Tauri::invoke` path — catches `space_id_hex` → `space_id` style param-naming bugs that JS-mocking missed in DM transport. **Sub-C is the right time to land ZEB-247** — same shape as the DM IPC bug, fresher pattern in our heads |
| **Frontend services** | Vitest (`src/lib/community-service.test.ts`, `nav-service.test.ts`) | `community-members-changed` delta application; deep-link payload decoding; redeem flow's optimistic UI; admin power-gating in component-render decisions |
| **Frontend components** | Vitest + `@testing-library/svelte` (`__tests__/CommunitySettingsPanel.test.ts`, etc.) | Member list filtering/sorting, kick-button visibility per power level, invite link generate→display→revoke loop, redeem dialog accept/cancel, ARIA roles + Esc-dismiss |
| **Manual LAN validation** | Final phase smoke test, two-laptop setup | Open: laptop A creates community, laptop B redeems URL → both see member list. Invite-only: same with counter-sig hop. Multi-device: laptop A + phone on same owner-key, both see same membership state. Kick: A kicks B → B's UI removes the community + shows "you were removed" |

### Test data + fixtures

- Reuse the existing `make_test_owner` helper from DM transport tests
- New `make_test_community` helper: builds + signs creator-admin-100 SetPower event + initial Join, returns ready-to-use `CommunityState` for fast fixture setup
- Wire-format fixtures (CBOR golden files) for: `SignedMembershipEvent` × 5 kinds, `CommunityInvitePayload`, `InviteToken`, `CounterSignature` — pinning these prevents accidental wire breakage across phases (mirrors `wire_format_fixture.rs` we already use)

### Coverage gates

Every phase's PR must keep gates green:
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets --all-features`
- `vitest run`
- `tsc --noEmit`

### Bug-class coverage we know to watch for

From PR #81 retrospective (lessons from DM transport):

- **Tauri IPC param naming** — every new IPC needs the e2e binding test (ZEB-247 enables this)
- **HLC tracker monotonicity on dedupe-merge** — community Spaces use `Id` dedupe key; same trap as DM Spaces. Test: two devices simultaneously join the same community via different invite links (both redeem the same payload from different inviters) → dedupe-merge MUST NOT regress the HLC tracker
- **First-visit-guard for community switching** — when frontend opens a different community, the lazy state-DAG-sync should fire ONCE per community, not on every switch. Mirrors `loadedDmSpaces` Set pattern from ZEB-228 Phase 4
- **Skip-on-error in `decrypt_inbox_entries` style helpers** — community materialization MUST tolerate a single corrupt event (skip + log) rather than failing the whole replay

## Phasing

Five phases, each one PR. Same cadence as ZEB-216.

### Phase 1 — Membership CRDT primitives (Rust only)

**Goal:** Land the type definitions, signing, verification, and materialization logic. No IPC, no Zenoh, no UI. Pure-function tests.

**Files:**
- New: `src-tauri/src/community_membership.rs`
- New: `src-tauri/src/community_invite.rs` (CommunityInvitePayload + InviteToken types only — Reticulum send path lands in Phase 4)
- Modified: `src-tauri/src/owner_state_types.rs` — add `MembershipKey` newtype, extend `Space` with `mk` / `ad` / `io` fields + invariant rules
- New: `src-tauri/tests/community_membership_unit.rs`
- New: `src-tauri/tests/wire_format_community_fixtures.rs` (CBOR golden files for the 5 event kinds + invite payloads)

**Deliverables:** Materialization rules + verification + power thresholds + dedupe key for Community Space + CBOR canonicalization. Standalone unit tests pass; integration tests do not yet exist.

### Phase 2 — Per-community state CRDT + encrypted Zenoh sync

**Goal:** Multi-owner CRDT replicates across members via the encrypted state-root topic. Mirrors ZEB-215 Phase 3a/3b architecture for owner-state.

**Files:**
- New: `src-tauri/src/community_state_crdt.rs` (Prolly Tree per community)
- New: `src-tauri/src/community_state_sync.rs` (encrypted topic publish/subscribe + DAG-sync)
- Modified: `src-tauri/src/event_loop.rs` — new select arms for community state ops (subscribe / publish / DAG-sync)
- Modified: `src-tauri/src/lib.rs` — community state-sync wiring on start_node
- New: `src-tauri/tests/community_sync_integration.rs`

**Deliverables:** Two-member community DAG-syncs the full event log; degraded paths (decrypt failure, timeout) emit `community-state-sync-degraded`; per-community RootHlcTracker dedupes redundant roots.

### Phase 3 — Open community flow (create + join + leave)

**Goal:** Open communities are usable end-to-end via IPC (no UI yet). Create a community, generate an invite link with no counter-sig hop, redeem it, see member list update across both peers.

**Files:**
- Modified: `src-tauri/src/lib.rs` — IPC commands: `create_community`, `redeem_invite` (open path only), `leave_community`, `list_community_members`, `generate_invite` (open: produces token-less payload)
- Modified: `src-tauri/src/owner_state_sync.rs` — community Space writes (create/leave) integrate with owner-state HLC tracker
- New: `src-tauri/tests/community_open_flow_integration.rs`

**Deliverables:** Open community fully working at the IPC layer. `community-members-changed` event fires correctly with delta payload. Frontend doesn't exist yet; tests exercise IPC directly via `tauri::test::mock_app`.

### Phase 4 — Invite-only flow (Reticulum CommunityInvite + counter-sig)

**Goal:** Invite-only flavor using the Reticulum unicast + DmInvite Path B pattern from ZEB-227. Reuses `dm_outbox` queue when no member is online to counter-sign.

**Files:**
- Modified: `src-tauri/src/community_invite.rs` — Reticulum send/receive paths (mirrors `dm_envelope.rs`); imports `UnicastSendRequest` from `dm_outbox.rs` without modifying it
- Modified: `src-tauri/src/event_loop.rs` — receive path for incoming CommunityInvite packets
- Modified: `src-tauri/src/lib.rs` — IPC commands: `redeem_invite` (invite-only path with `invitee_hint`), `generate_invite` (with InviteToken signing), `kick_from_community`, `set_power_level`
- New: `src-tauri/tests/community_invite_integration.rs`
- New: `src-tauri/tests/community_admin_integration.rs` (kick + set_power power-gating)

**Deliverables:** Invite-only flow end-to-end with **online counter-signer path only**. Offline counter-signer (joiner clicks invite when admin not online) returns `Err` and is deferred to [ZEB-254](https://linear.app/zeblith/issue/ZEB-254/). Kick + power-level fully gated. Both invite-link flavors (`invitee_hint = None` / `Some`) work.

### Phase 5 — Admin UI + deep-link integration (Svelte + Tauri 2 plugin)

**Goal:** Full user-visible feature shipped. Tauri 2 deep-link plugin registered per-platform. All Svelte components from "Frontend (admin UI)" land. NavService consumes community Spaces. Includes ZEB-247's e2e Tauri::invoke harness.

**Files:**
- Modified: `src-tauri/Cargo.toml` — `tauri-plugin-deep-link`
- Modified: `src-tauri/tauri.conf.json` — URL scheme registration
- Per-platform OS bundles: macOS Info.plist (`CFBundleURLTypes`), Linux .desktop file (`MimeType=x-scheme-handler/harmony`), Windows registry shim (Tauri plugin handles)
- Modified: `src-tauri/src/lib.rs` — deep-link event handler emits `invite-link-received` IPC event
- New Svelte: `CommunityCreateDialog.svelte`, `CommunitySettingsPanel.svelte`, `InviteLinkManager.svelte`, `InviteRedeemDialog.svelte`, `MemberRow.svelte`
- Modified Svelte: `App.svelte` (deep-link event subscription + InviteRedeemDialog mount), `NavSidebar.svelte` (+ New community button), `nav-service.ts` (community Space → click routing)
- New: `src-tauri/tests/tauri_invoke_e2e.rs` (ZEB-247 — exercises ≥1 community IPC through real `Tauri::invoke`)
- New vitest: `__tests__/CommunitySettingsPanel.test.ts`, `__tests__/InviteLinkManager.test.ts`, `__tests__/InviteRedeemDialog.test.ts`, etc.

**Deliverables:** Full feature shipped. Manual LAN smoke validation closes the phase. Final PR opens against `origin/main`; ZEB-217 closes on merge.

### Cross-repo work

**None expected.** All work is in `harmony-client`. We're reusing CAS (harmony-content) and Reticulum unicast primitives that already exist; no upstream changes anticipated. If a missing primitive is discovered during implementation, file a companion PR to the appropriate repo (same pattern we used for ZEB-215 Phase 3b's harmony-content companion).

## Acceptance criteria (Sub-C v1)

* Open community: any peer with the invite link can join; admin can kick.
* Invite-only community: join requires inviter counter-signature via Reticulum hop. v1 requires ≥ 1 admin online at redemption time; offline-counter-sig is ZEB-254. Targeted invites (`invitee_hint = Some`) reject mismatched joiners.
* Power-level enforcement: kick requires `power ≥ 50` AND strict-greater-than-target; set_power requires `power = 100`.
* Forged events (bad signature or insufficient power) rejected and not replicated.
* Multi-device convergence: joining a community on phone surfaces on desktop within bounded latency (sub-second LAN, seconds across NATs).
* Admin UI lets a community admin invite/kick/set-power and view the member list.
* Invite links work cross-device — `harmony://invite/...` deep-link opens harmony-client, shows preview dialog, redeems on confirm. Falls back to in-app paste-URL field for environments where the URL scheme doesn't fire.
* All gates green (cargo fmt + clippy + test, vitest, tsc).
* End-to-end Tauri::invoke test (ZEB-247) lands in Phase 5 covering at least one community IPC.

## Deferred follow-ups (filed at design-doc commit time)

| Linear | Title |
|---|---|
| [ZEB-248](https://linear.app/zeblith/issue/ZEB-248/) | Sub-C v2 — channels-within-communities (CRDT + Zenoh broadcast + UI) |
| [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/) | TreeKEM-style backward secrecy on community membership change |
| [ZEB-250](https://linear.app/zeblith/issue/ZEB-250/) | M-of-N admin recovery for communities |
| [ZEB-251](https://linear.app/zeblith/issue/ZEB-251/) | Per-community power-threshold customization |
| [ZEB-252](https://linear.app/zeblith/issue/ZEB-252/) | Sub-D library-directory integration with communities |
| [ZEB-253](https://linear.app/zeblith/issue/ZEB-253/) | harmony-mobile (future): QR code scanning for community invite links |
| [ZEB-254](https://linear.app/zeblith/issue/ZEB-254/) | Persistent offline-counter-signer queue for invite-only redemption ("join pending" UX) |

## Out of scope (Sub-C v1)

* Channels (deferred to ZEB-248)
* Voice/video in communities (separate transport design)
* Cross-community channel migration tooling
* Per-community power-threshold customization (ZEB-251)
* M-of-N admin recovery (ZEB-250)
* Backward-secrecy / TreeKEM key rotation (ZEB-249)
* Library directory discovery (ZEB-218; integration is ZEB-252)
* Mobile QR code scanning (ZEB-253)
* Offline-counter-signer "join pending" UX (ZEB-254)

## References

* `docs/specs/2026-04-30-zeb-206-nav-tree-design.md` — original ZEB-206 umbrella spec, including the initial Sub-C section this design refreshes
* `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md` — DM transport design, source of the DmInvite Path B pattern reused here
* `docs/specs/2026-05-01-zeb-215-sub-a-phase3a-sync-design.md` and `2026-05-01-zeb-215-sub-a-phase3b-content-cas-design.md` — owner-state CRDT sync, source of the Prolly Tree + encrypted root + DAG-sync pattern reused here
* `src-tauri/src/owner_state_types.rs` — current `Space` + `SpaceKind` definitions; this design extends them
* `src-tauri/src/dm_outbox.rs` — Reticulum unicast queue extended to carry CommunityInvite packets in Phase 4
* PR #81 retrospective — lessons that informed the testing strategy (especially the Tauri IPC param-naming bug and ZEB-247 follow-up)
