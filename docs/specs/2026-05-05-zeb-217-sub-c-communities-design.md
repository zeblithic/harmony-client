# ZEB-217 (Sub-C of ZEB-206): Community Membership CRDT + Invite/Join + Admin UI — Design

**Linear:** [ZEB-217](https://linear.app/zeblith/issue/ZEB-217/zeb-206-sub-c-harmony-client-community-membership-crdt-invitejoin)
**Parent epic:** [ZEB-206](https://linear.app/zeblith/issue/ZEB-206/) (nav-tree real-data wiring)
**Date:** 2026-05-05 (refreshed 2026-05-06 against shipped Phase 2 — PR #84, merge commit `466e6c2`; previously refreshed 2026-05-05 against shipped Phase 1 — PR #82, merge commit `bd1d01b`)
**Status:** Phases 1 + 2 shipped; Phases 3–5 pending implementation
**Author:** brainstormed against shipped ZEB-215 (owner-state CRDT) + ZEB-216 (DM transport) patterns; refreshed against shipped Phase 1 + Phase 2 primitives

> **Refresh note (2026-05-05):** This spec was originally written before Phase 1 implementation. After PR #82 landed the membership-CRDT primitives, six rounds of bot review surfaced invariants that weren't pinned in the original draft (pubkey-to-OwnerAddr binding, bootstrap-admin self-Join exemption, defense-in-depth at both verify-event AND materialize layers, the full `event_sort_key` tiebreak chain, CounterSignature wire codes, same-SpaceId apply-time rejection of community-creation field changes, idempotent state transitions, the `EventPayload` named unsigned-portion type, and ed25519 `verify_strict`). The "Data model", "Materialization rules", "Verification", and "Phase 1" sections below now describe the shipped primitives so Phases 2–5 can reference them precisely.

> **Refresh note (2026-05-06):** PR #84 landed Phase 2 — encrypted-Zenoh state-root sync — through six rounds of bot review that surfaced a similar set of operational invariants the original Phase 2 plan didn't pin. The "Phase 2" section below now describes the shipped sync engine + the deltas caught during review; the "Bug-class coverage we know to watch for" section adds Phase 2 lessons that Phases 3–5 should inherit. One Critical-class gap remains explicitly **deferred to [ZEB-256](https://linear.app/zeblith/issue/ZEB-256/)**: cryptographic publisher authentication on the state-root publish envelope. Phase 2's `CommunityRootPublishPayload.publisher_device_id` is authenticated only by the per-community AEAD (`MembershipKey`), so any current member can spoof another member's `publisher_device_id` and silently advance their slot in the replay tracker. The gap has no exploitation path while Phase 2 ships open communities only with no IPC surface; **it MUST close before Phase 4 invite-only flows ship**, because a kicked member retains the `MembershipKey` until [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/) (TreeKEM rotation) lands and could otherwise censor admin publishes.

## Goal

Add Harmony's first-class moderation primitive — **communities** — as a multi-owner CRDT with signed events for join/leave/kick/power-level operations. v1 ships open + invite-only flavors with full admin UX (member list, kick, power-level, invite link manager) but **defers channels** to a follow-up ([ZEB-248](https://linear.app/zeblith/issue/ZEB-248/)).

Per the polycentric governance principle (project memory, locked in during ZEB-206 brainstorm): communities are Harmony's *only* first-class moderation primitive. Public channels and DMs have no moderation surface — open broadcast / closed link respectively. There is no global moderation, no platform-level admin, no algorithmic content promotion. Communities self-govern.

## Architecture

```text
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

> **Topic shorthand:** the diagram uses `{addr}` / `{id}` and the unversioned `state-root` suffix purely for visual alignment. The full wire form (matching shipped owner-state sync at `src-tauri/src/event_loop.rs`) is `harmony/owner/{addr_hex}/state-root-v1` and `harmony/community/{id_hex}/state-root-v1` — lowercase hex of the 16-byte OwnerAddr / SpaceId, plus the explicit `-v1` version suffix so a future wire-format break can ship a parallel `-v2` topic without breaking old clients. Every later prose reference uses the full form.

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

**LWW creation-pinning (shipped Phase 1, load-bearing for Phase 2):** `lww_merge_space` pins `admin_addr`, `membership_key`, `is_invite_only`, AND `created_at` to the side with the OLDER `created_at`. Without this, an attacker that backdates `created_at` could "win" the LWW pin and shift bootstrap-admin authority, rotate the membership key (locking prior encrypted state), or flip privacy mode. Cross-creator divergence is an invariant violation caught upstream by `validate_invariants`; same-creator merge yields identical values.

**Same-SpaceId apply-time rejection (shipped Phase 1, defense-in-depth for the LWW pin):** `apply_space` rejects any same-SpaceId update where `kind == Community` AND `membership_key` / `admin_addr` / `is_invite_only` / `created_at` differs from the existing entry. Mirrors the `content_key` rejection for DMs. Phase 1 has no community-creation IPC, so this branch is unreachable today; it becomes load-bearing in Phase 2 once encrypted Zenoh state-root sync can deliver remote Space writes.

**`validate_invariants` adds (shipped Phase 1):** `prior_content_keys` MUST be empty for `SpaceKind::Community` (no historical content-key chain — `membership_key` is fixed for the lifetime of the community in v1; rotation deferred to ZEB-249).

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

Per-community CRDT (encrypted-CBOR-blob via CAS DAG-sync; "Prolly Tree" is aspirational shorthand for the layered Merkle structure ZEB-215 originally targeted, but as-shipped owner-state sync uses a single encrypted blob per state-root publish — Phase 2 mirrors that). Replicated via `harmony/community/{id_hex}/state-root-v1` topic.

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
    pub countersig: Option<CounterSignature>,  // required for non-admin invite-only Join
}

pub enum MembershipEventKind {
    Join,
    Leave,
    Invite    { target: OwnerAddr },
    Kick      { target: OwnerAddr, reason: Option<String> },
    SetPower  { target: OwnerAddr, level: u8 },
}

pub struct CounterSignature {
    pub signer: OwnerAddr,   // existing member with power ≥ invite_threshold;  wire code "sn"
    pub sig: [u8; 64],       // signs the joiner's signed Join event payload;   wire code "sg"
}

/// Unsigned portion of `SignedMembershipEvent` — the bytes the actor's
/// `sig` AND (when present) the countersig cover. Named so signing /
/// verifying paths share a single source of truth for field order +
/// coverage (no "sign with sig=zeros" ambiguity). `From<&SignedMembershipEvent>`
/// extracts it.
pub struct EventPayload {
    pub id: EventId,
    pub community_id: SpaceId,
    pub kind: MembershipEventKind,
    pub actor: OwnerAddr,
    pub at: Hlc,
}
```

**Wire-key invariant (verified against PR #82):** every nesting level of these types satisfies the **same-length-keys CBOR invariant** — all field codes at any single map nesting are exactly 2 chars (text(2) = 3 bytes per key). For `CounterSignature`, the codes `sn` (signer) and `sg` (sig) match the convention "sg = signature at every nesting level"; an earlier draft used `sg`/`sx` (inverted) and was fixed before Phase 1 merge so cross-language deserializers don't have to special-case CounterSignature.

**Sig coverage:** The actor's `sig` covers the canonical-CBOR encoding of `EventPayload` — `sig` and `countersig` are excluded so an inviter can append a countersig without invalidating the actor's sig. Because countersig is therefore wire-malleable (a peer could append, strip, or replace it on any event without breaking the actor sig), the verifier MUST reject any event carrying a countersig outside its allowed slot — see "Verification" below.

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

**Replay order — `event_sort_key` (canonical total ordering):**

Events are replayed in `(wall_ms, logical, device_id, EventId, sig)` ascending. The HLC triple is causal-ish but not total — two events with the same `wall_ms` / `logical` / `device_id` collide. `EventId` is a strong but caller-supplied tiebreaker; a buggy or malicious peer could emit two distinct events with the same id. The 64-byte `sig` is the field that makes the order truly total across any malformed input — distinct payloads under the same key produce distinct sigs, and signature security guarantees no useful collisions.

Phase 2 sync MUST use the same `event_sort_key` comparator when computing the verifier's prior-state prefix; re-implementing "all events strictly before in HLC order" elsewhere would miss the EventId / sig tiebreakers and silently authorize against stale state when same-HLC predecessors exist. Use the `prior_state_at_event` helper.

**Per-kind transition tables.** Materialize is a pure function over a pre-verified event log; `verify_event` enforces the same invariants at the input layer. Materialize re-pins them as defense-in-depth for events that slip past verification (corrupted log, replay before a Ban arrived):

- **`Join { actor, at }`** —
    - prior status `None` / `Invited` / `Left` → set `Joined`, `joined_at = at`
    - prior status `Joined` → **no-op** (idempotent; preserves original `joined_at` so an actor can't push their own join date forward by replaying Join with no privilege gate)
    - prior status `Banned` → **no-op** (Banned-sticky)
- **`Leave { actor, at }`** —
    - prior status `Joined` / `Invited` / `Left` (via existing record) → set `Left`, `left_at = at`
    - prior status `Banned` → **no-op** (Banned-sticky; verify_event also rejects `BannedActorLeave`)
    - actor never joined → **no-op** (verify_event tolerates this; insert-with-Left would corrupt state from a malformed event)
- **`Invite { actor, target, at }`** —
    - prior target status `None` / `Left` → set `Invited`, `joined_at = at`, `left_at = None` (replace entry; legitimate re-invite of a former member)
    - prior target status `Invited` → **no-op** (idempotent; preserves original invite timestamp)
    - prior target status `Joined` → **no-op** (already past invited stage)
    - prior target status `Banned` → **no-op** (Banned-sticky; verify_event also rejects `InviteTargetBanned`)
- **`Kick { actor, target, at }`** —
    - target HAS an existing entry → set `Banned`, `left_at = at`
    - target never joined → **no-op** (verify_event rejects `KickTargetNotMember` at the input layer; falling back to `entry().or_insert(...)` would fabricate a phantom `Banned` entry with `joined_at = kick_time`)
- **`SetPower { actor, target, level, at }`** → `power_levels[target] = level`. (Power-rule checks live in `verify_event`; materialize is a pure write.)

### Verification (run at every event before insertion into Prolly Tree)

Phase 1 ships `verify_event` as the single source of truth for "is this event authorized?". Pure function — caller supplies prior materialized state + the verifier's expectations:

```rust
pub struct VerifyContext<'a> {
    /// Caller's expected community_id; must match event.community_id.
    pub expected_community_id: SpaceId,
    /// Community's bootstrap admin (Space.admin_addr). Admin self-Join
    /// in invite-only communities is exempt from the countersig
    /// requirement — without this exemption a fresh invite-only
    /// community is unbootstrappable from empty state.
    pub admin_addr: OwnerAddr,
    pub is_invite_only: bool,
    /// Canonical 64-byte combined identity public bytes
    /// (X25519_pub(32) || Ed25519_pub(32)) for `event.actor`.
    /// Source: Sub-A's owner-device cache.
    pub actor_identity_pub: &'a [u8; 64],
    /// Same shape, for the countersig signer. None for open communities,
    /// non-Join events, and admin self-Join in invite-only.
    pub countersigner_identity_pub: Option<&'a [u8; 64]>,
}

pub fn verify_event(
    event: &SignedMembershipEvent,
    prior_state: &MaterializedMembership,
    ctx: &VerifyContext,
) -> Result<(), VerifyError>;
```

**Verification order (every gate fires BEFORE the next):**

1. **Community binding** — reject `WrongCommunity` if `event.community_id != ctx.expected_community_id`. Defends against cross-community authorization (caller has community A's state, event signed for community B). Fires before any cryptographic work so a misrouted event surfaces with the specific discriminant rather than `SignatureInvalid` masking the cause.
2. **Countersig presence rule** — reject `UnexpectedCounterSig` if `event.countersig.is_some()` AND the slot is not "non-admin invite-only Join". Because `sig` excludes countersig (so an inviter can append it without invalidating the actor sig), countersig is wire-malleable; rejecting it outside its allowed slot keeps the invariant "countersig present iff non-admin invite-only Join" end-to-end.
3. **Pubkey-to-claimed-signer binding (actor)** — derive `address_hash = SHA256(X25519_pub || Ed25519_pub)[:16]` from `ctx.actor_identity_pub`. Reject `ActorPubkeyMismatch` if it does not equal `event.actor.0`. Defends against caller-side cache-lookup bugs that pair a pubkey with the wrong claimed identity (cache lookup bug, stale cache, key-substitution attack). Bad bytes (non-curve points) → `InvalidIdentityPub`.
4. **Actor signature** — verify the Ed25519 component of `ctx.actor_identity_pub` against canonical-CBOR-encoded `EventPayload::from(event)` using `verify_strict` (NOT `verify`). Strict mode rejects non-canonical S values and small-order R points, matching RFC 8032's strict subset and protecting against signature malleability. Mirrors how `dm_envelope` verifies its own signed payloads.
5. **Banned-status guard** — for `Join` and `Leave`, look up `prior_state.members[event.actor]`. Reject `BannedActorJoin` / `BannedActorLeave` if the prior status is `Banned`. Without this, a kicked actor could send Leave (no power gate) to flip status from Banned → Left, then Join (no longer Banned-blocked) to rejoin — defeating Kick-as-ban.
6. **Invite-only countersig logic** — for `Join` in an invite-only community where `event.actor != ctx.admin_addr`:
    - Reject `CounterSigRequired` if `event.countersig.is_none()`.
    - Reject `CounterSignerPubkeyMismatch` if `ctx.countersigner_identity_pub` does not hash to `event.countersig.signer`.
    - Reject `CounterSigInvalid` if the countersig doesn't verify (Ed25519 strict over the same bytes the actor sig covers — `EventPayload::from(event)`).
    - Reject `CounterSignerNotJoined` if `prior_state.members[signer].status != Joined`.
    - Reject `CounterSigPowerInsufficient` if `power(signer) < POWER_THRESHOLDS.invite`.
    - **Admin self-Join exemption:** when `event.actor == ctx.admin_addr`, no countersig is required (and an UnexpectedCounterSig at step 2 already rejects a stray countersig on this slot).
7. **Joined-membership gate (Invite / Kick / SetPower)** — reject `ActorNotJoined` if the actor's prior status is not `Joined`. Power levels alone aren't sufficient — a non-member with high assigned power (former member after Leave/Kick, or an address that received SetPower without ever Joining) cannot wield community moderation.
8. **Per-kind power rules:**
    - **Invite** — actor power ≥ `invite_threshold` (currently 0); reject `InviteTargetBanned` if `prior_state.members[target].status == Banned` (admin must unban first; materialize() also no-ops Banned-sticky).
    - **Kick** — actor power ≥ `kick_threshold` (50) AND `> target.power`; reject `KickTargetNotMember` if target has no member record (don't fabricate a phantom Banned entry).
    - **SetPower** — actor power ≥ `set_power_threshold` (100); reject `PowerLevelOutOfRange` if `level > POWER_THRESHOLDS.max` (an authorized actor cannot grant a power higher than the cap, since that would create a member admin can no longer kick).
    - **Join, Leave** — no further power check (anyone can Leave; Join is gated by invite-only countersig logic above).

**Power lookups treat unset entries as 0** (the default per the spec). Bootstrap (`admin_addr` → 100) is already baked into `prior_state` by `materialize`, so the lookup is uniform across all actors.

**Verification is idempotent and pure** — given the same prior event log + the same candidate event, returns the same accept/reject. This makes Prolly Tree DAG-sync convergent: two devices that receive the same set of events materialize the same state regardless of arrival order.

**Defense-in-depth (Phase 2 sync layer note):** every invariant above ALSO appears in `materialize`'s transition tables (Banned-stickiness, KickTargetNotMember, idempotent Join/Invite). Phase 2's sync layer rejects unverified events before they reach `materialize`, but if a corrupted log or unverified replay surfaces an out-of-policy event, materialize stays correct rather than silently fabricating phantom state.

**`VerifyError` discriminants (Phase 1, 19 variants):** `WrongCommunity`, `SignatureInvalid`, `CounterSigRequired`, `UnexpectedCounterSig`, `CounterSigInvalid`, `CounterSigPowerInsufficient`, `ActorPowerInsufficient`, `KickTargetPowerNotLower`, `KickTargetNotMember`, `InviteTargetBanned`, `PowerLevelOutOfRange`, `BannedActorJoin`, `BannedActorLeave`, `ActorNotJoined`, `CounterSignerNotJoined`, `ActorPubkeyMismatch`, `CounterSignerPubkeyMismatch`, `InvalidIdentityPub`, `EncodeError(String)`. Phase 2's sync layer surfaces each as the corresponding `community-state-sync-degraded` reason or `Err` to the IPC caller.

## Sync protocol

### Topic & encryption

- **Topic:** `harmony/community/{id_hex}/state-root-v1` (Zenoh). Mirrors `harmony/owner/{addr_hex}/state-root-v1` from shipped ZEB-215 owner-state sync. `id_hex` is the lowercase hex of the 16-byte `SpaceId`. The `-v1` suffix is intentional — a future wire-format break can ship a parallel `-v2` topic so old clients stay safely silent rather than mis-parsing.
- **Wire payload:** Encrypted Prolly Tree root CID, published whenever a member appends a verified event.
- **Encryption:** ChaCha20-Poly1305 (same primitive as DM content + owner-state). Key = the community's `MembershipKey` (32 bytes), distributed via the invite payload. Topic is observable to anyone with `community_id`, but the payload is opaque without the key.

### Subscription lifecycle

When `community_state_sync.rs` starts, it scans `owner_state.spaces` for any `Space { kind: Community }` and subscribes to the corresponding state-root topic for each. When a new community Space appears in owner-state (because the owner joined a new community on this device, or another bound device replicated the join via Flow A), the sync module subscribes lazily. When a Space's `left_at` is set, it unsubscribes.

### Append flow (member publishes a new event)

```text
1. Frontend → IPC (e.g., kick_from_community)
2. community_membership::sign_event_with_identity builds + signs
   the SignedMembershipEvent
3. community_state_crdt verifies locally:
     prior = prior_state_at_event(&log, &event, admin_addr)
     verify_event(&event, &prior, &VerifyContext { ... })?
4. Prolly Tree insert → new root CID
5. Encrypt root CID + new block(s) with MembershipKey
6. Publish encrypted root CID to harmony/community/{id_hex}/state-root-v1
7. Other members subscribe → receive root → decrypt → DAG-sync missing
   blocks via existing CAS/DAG-sync (ZEB-215 Phase 3b machinery)
8. Each subscriber re-runs verify_event with the SAME prior-state helper
   on every newly-fetched event before inserting into their local
   Prolly Tree (defense-in-depth — peers don't trust each other's
   verification; same comparator everywhere prevents drift between
   author-side and receiver-side authorization)
```

### New-joiner bootstrap

When you redeem an invite link and become a member:

```text
1. Decode invite link → community_id, MembershipKey, (optional) inviter
2. Subscribe to harmony/community/{id_hex}/state-root-v1
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

Phone and desktop both publish `Join { actor: my_addr, at: hlc_phone }` and `Join { actor: my_addr, at: hlc_desktop }` (different `EventId`s, slightly different HLCs). Both are valid signed events. Both land in the per-community CRDT on every member. Materialized state's `members[my_addr]` reflects whichever event sorts first under `event_sort_key` — `(wall_ms, logical, device_id, EventId, sig)` ascending — typically the earlier HLC, with `EventId` then `sig` providing the deterministic tie-break when HLC components collide. Idempotent at the user-visible layer.

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

```text
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

```text
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

> See "Data model → Verification" above for the full `verify_event` contract (input order, error variants, pubkey binding, bootstrap-admin exemption). This subsection states the layered policy.

Every `SignedMembershipEvent` is gated at THREE points:

1. **At local append** — `verify_event` runs before publishing to the topic, with prior_state computed via `prior_state_at_event` against the current event log.
2. **At receive time on every peer** — `verify_event` runs before insertion into the local Prolly Tree (peers don't trust each other's verification). Same comparator and prior-state helper as the local-append path.
3. **At materialize time** — `materialize`'s per-kind transition tables re-pin Banned-stickiness, KickTargetNotMember, and idempotent Join/Invite as a state-machine defense. This catches events that slip past `verify_event` (corrupted log, replay before a Ban arrived, peers running out-of-date verifier code).

If any verification fails at points 1 or 2, the event is rejected and **not** replicated. The Prolly Tree DAG-sync still considers the block "fetched" so we don't re-fetch it endlessly, but the materialized state ignores it.

### Failure modes

| Failure | Behavior |
|---|---|
| Forged signature on event | Verification fails at receive — rejected, not added to Prolly Tree, not materialized |
| Event from actor with insufficient power | Rejected at receive — same as above |
| Two-device race: same admin kicks the same target simultaneously | Both events valid + signed by same actor with sufficient power. Both land in Prolly Tree. Materialized state shows kick once (idempotent at status=Banned) |
| Race: A sets bob's power to 75 on phone, simultaneously sets to 25 on desktop | Both events valid. HLC ordering picks last-writer-wins. The "loser" event lands in Prolly Tree but has no materialized effect (overwritten) |
| Race: A kicks B who is simultaneously kicking A | Whichever event sorts first under `event_sort_key` wins (typically earlier HLC; ties resolved by `EventId` then `sig`). The later event verifies against `prior_state_at_event` at its position, which includes the earlier kick — so the kicker who's already been kicked has insufficient power → rejected |
| Invite-only Join with countersig from a member who LATER lost power | The countersig was valid at the time the Join event was created (verified against materialized state at HLC time). HLC-ordered replay preserves the original verification. Join stays valid |
| Reticulum CommunityInvite delivery fails (no member online) | v1 returns `Err` immediately; user retries. Persistent retry / "join pending" UX deferred to [ZEB-254](https://linear.app/zeblith/issue/ZEB-254/) |
| Two of my devices both redeem the same invite link | Both publish Join events signed by my owner key with slightly different HLCs. Both land in the per-community CRDT. Materialized `members[my_addr]` reflects whichever event sorts first under `event_sort_key` (typically earlier HLC; `EventId`/`sig` tie-break). Idempotent at user level |
| Local DB corruption on one device | Owner-state DAG-syncs Space from peer bound device → membership_key recovered → community state DAG-syncs from peer member |
| MembershipKey leak (someone exfiltrates from device) | Attacker can decrypt the membership topic's history. Cannot publish events without a valid signing key for an existing member. Mitigation: rotation deferred to [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/) |
| Invite link leak (attacker grabs a `harmony://invite/...` URL) | For open communities: attacker can join. Admin can kick. For invite-only with `invitee_hint = None`: same — attacker can redeem. For invite-only with `invitee_hint = Some(addr)`: attacker can't redeem (the Join event signature won't match the hint). Mitigation: prefer targeted invites; admin can revoke open invites |
| All bound devices lost (full identity loss) | Falls into ZEB-173 identity-recovery story. Recovered owner has no community membership history; rejoining requires a new invite. **Out of scope for v1** |
| Community admin loses private key (single point of failure) | **Out of scope for v1.** Mitigation path: M-of-N admin power model, deferred to [ZEB-250](https://linear.app/zeblith/issue/ZEB-250/) |

### Multi-device convergence properties

- **All membership events are content-addressed** by their CBOR canonical encoding → DAG-sync deduplicates naturally
- **Canonical event ordering uses `event_sort_key`** — `(wall_ms, logical, device_id, EventId, sig)` ascending. The HLC triple `(wall_ms, logical, device_id)` provides causal ordering with the logical counter dominating wall-clock skew; `EventId` (16-byte ULID) is a strong but caller-supplied tiebreaker; the 64-byte `sig` makes the order truly total across any malformed input. Implementations that fall back to "HLC alone" miss the `EventId`/`sig` tiebreakers and silently authorize against stale state when same-HLC predecessors exist — use the `event_sort_key` helper (and `prior_state_at_event` for the verifier prefix) so the comparator can't drift.
- **Membership-CRDT root is per-community**, not per-owner → my desktop and phone independently subscribe + DAG-sync
- **Materialized state is deterministic** given the same event log → all bound devices converge to the same view

### Privacy properties (v1)

- **Member list private to joined members.** `harmony/community/{id_hex}/state-root-v1` topic is observable but encrypted with `MembershipKey`. Non-members see only "this community is active."
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
- New `make_test_community` helper: returns a `CommunityState` containing the creator's signed `Join` event as the first (and only mandatory) entry; the creator's power-100 admin authority comes from the bootstrap rule (`admin_addr` initializes `power_levels[admin_addr] = 100` BEFORE replay — see "Materialization rules"), so NO separate creator-admin-100 `SetPower` event is needed or correct here. A `SetPower` issued before the actor's `Join` would fail `verify_event` with `ActorNotJoined`. Optional variants (`make_test_community_with_member`, etc.) append additional `Join` events for non-creator members and any `SetPower` events strictly AFTER each member's `Join`, preserving the verify/model invariants.
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

From PR #84 retrospective (lessons from Phase 2 sync engine — Phases 3–5 should inherit these by default):

- **No `.await` while holding the state mutex** — Phase 2's 3-phase receive pipeline (resolve → batch insert → emit reports) is the load-bearing pattern. Phase 3's IPC handlers MUST follow the same shape: snapshot the lock-guarded state, drop the guard, run any async resolver work, re-acquire briefly to mutate, drop, then emit. Holding a state mutex across `.await` deadlocks on contended re-entry.
- **Snapshot-then-spawn fence on every CRDT-mutating IPC** — pre-IPC snapshot of `(state_arc, registry_arc)` before any spawn / await; post-mutation re-attachment via the registry handle. Phase 1's `apply_space` rejection of community-creation field changes is one defense; the snapshot fence is the other. Without it, joining + leaving + re-joining a community in rapid succession can race the registry's engine lifecycle and lose updates. The pattern is force-multiplied by Phase 4's invite-redemption flow which spans `redeem_invite` → counter-signer await → state mutation.
- **Sync I/O on async runtime → `tokio::task::spawn_blocking`** — `std::fs::read` / `write` / `rename` calls are sync. Phase 3+ persistence helpers (any new `save_*` / `load_*` for community-related on-disk state) MUST wrap in `spawn_blocking` to avoid stalling worker threads. Pattern matches `owner_state_sync.rs:376` and `community_state_sync.rs::persist_both`.
- **Best-effort channels use `try_send`, not `send().await`** — degraded-report channels (`error_tx`, IPC-event emitters) are sources, not sinks. `try_send`'s `Full(_)` / `Closed(_)` should be dropped silently. A `send().await` on a full channel back-pressures the producer pipeline (e.g., the receive loop), which is wrong for "tell the user something went wrong" semantics.
- **Distinct error variants per failure class** — `CommunitySyncError` ships 7 variants because each maps to a stable reason-tag the frontend's eventual degraded-banner copy switches on. Phase 3+ should keep adding variants rather than collapsing into `Generic(String)`. The taxonomy is part of the contract.
- **`tokio::select! biased` is a contract** — arm order is load-bearing under `biased`. Comments claiming "Data-flow arm first" must reflect actual ordering. The publisher and subscriber loops in `event_loop.rs::spawn_community_state_zenoh_adapter` are the reference templates; new select loops should follow the same pattern (data-flow first, then sender-closed detection, then timeout fallback).
- **Saturation handling for HLC counters** — `next_hlc` must handle `prev.logical == u32::MAX` by advancing `wall_ms` instead of incrementing logical. Phases 3+ that produce events under sustained same-millisecond bursts (e.g., bulk-import flows) hit this without the guard.
- **Persist self-heal beats hard-fail** — corrupted on-disk per-community CRDT files quarantine via `.corrupt.<unix_ms>` rename + start-with-default. Per-community state is recoverable from peers via the next state-root publish, so hard-failing engine spawn would maroon the community when the data is available across the network. Same logic applies to any new on-disk artifacts Phases 3–5 add.
- **Two-mode AEAD nonce discipline** — random-nonce for envelope publishes (each publish independent), deterministic-nonce for content-addressed blobs (CID dedup requires plaintext-stable ciphertext under the same key). Phase 4's `CommunityInvite` Reticulum payload should follow the same discipline; the trap to avoid is sharing a random-nonce CAS-side or a deterministic-nonce envelope-side.
- **Cryptographic publisher authentication is required before Phase 4 ([ZEB-256](https://linear.app/zeblith/issue/ZEB-256/))** — Phase 2 ships open-only with a known spoof gap on `publisher_device_id`. Phase 3 ships open-only and inherits the same scope. Phase 4 invite-only changes the threat model (kicked members retain `MembershipKey` until [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/) rotates) and CANNOT ship until ZEB-256 closes the gap.

## Phasing

Five phases, each one PR. Same cadence as ZEB-216.

### Phase 1 — Membership CRDT primitives (Rust only) — SHIPPED 2026-05-05

**Goal (achieved):** Land the type definitions, signing, verification, and materialization logic. No IPC, no Zenoh, no UI. Pure-function tests.

**Shipped via PR #82, merge commit `bd1d01b`.**

**Files (as shipped):**
- New: `src-tauri/src/community_membership.rs` — types, `sign_event` / `sign_event_with_identity`, `attach_countersig` / `attach_countersig_with_identity`, `verify_signature` / `verify_countersig`, `event_sort_key`, `materialize`, `prior_state_at_event`, `verify_event` (19-variant `VerifyError`), `POWER_THRESHOLDS`
- New: `src-tauri/src/community_invite.rs` — `CommunityInvitePayload` + `InviteToken` types only (Reticulum send path lands in Phase 4)
- Modified: `src-tauri/src/owner_state_types.rs` — `MembershipKey` newtype, `Space` extended with `mk` / `ad` / `io` fields, `validate_invariants` extended for Community kind (incl. `prior_content_keys.is_empty()`)
- Modified: `src-tauri/src/owner_state_crdt.rs` — `lww_merge_space` creation-pinning for community fields; `apply_space` same-SpaceId rejection of community-creation field changes (defensive — unreachable until Phase 2 sync, but cheaper to gate from day 1 than retrofit)
- Modified: `src-tauri/src/dm_crypto.rs`, `src-tauri/src/dm_outbox.rs`, `src-tauri/src/lib.rs`, `src-tauri/src/owner_state_persist.rs`, `src-tauri/src/owner_state_sync.rs` — Space-extension call-site adjustments (new fields wired through)
- New: `src-tauri/tests/community_membership_unit.rs` — 58 unit tests covering signing, verification, materialization, the full transition tables, defense-in-depth at both layers
- New: `src-tauri/tests/community_invite_unit.rs` — CommunityInvitePayload + InviteToken round-trip tests (open + invite-only forms)
- New: `src-tauri/tests/wire_format_community_fixtures.rs` — 12 pinned-byte CBOR golden fixtures (5 event kinds × wire layout + CounterSignature + invite payloads)

**Delta from original Phase 1 plan (caught during 6 rounds of bot review on PR #82):**

- `VerifyContext` shape with `actor_identity_pub: &[u8; 64]` + `countersigner_identity_pub` for pubkey-to-OwnerAddr binding (defense against caller cache-lookup bugs and key-substitution attacks)
- Bootstrap-admin self-Join exemption from countersig requirement (otherwise invite-only is unreachable from empty state)
- Defense-in-depth at BOTH `verify_event` AND `materialize` layers for Banned-stickiness, KickTargetNotMember, idempotent transitions
- `event_sort_key` exposed publicly with `(wall_ms, logical, device_id, EventId, sig)` total ordering
- `prior_state_at_event` helper so Phase 2 callers can't drift from the comparator
- `EventPayload` named type with `From<&SignedMembershipEvent>` impl (centralised so signing/verifying paths can't drift in field order or coverage)
- CounterSignature wire codes finalized as `sn` / `sg` (matching "sg = signature at every nesting level" convention)
- `verify_strict` (RFC 8032) for both actor sig and countersig
- `apply_space` same-SpaceId rejection of community-creation field changes (load-bearing for Phase 2's Zenoh sync path)

All deltas are documented in the Materialization rules / Verification / Data model sections above.

### Phase 2 — Per-community state CRDT + encrypted Zenoh sync — SHIPPED 2026-05-06

**Goal (achieved):** Multi-owner CRDT replicates across members via the encrypted state-root topic. Mirrors ZEB-215 Phase 3a/3b architecture for owner-state, multi-instanced (one `CommunitySyncEngine` per joined community, lifecycled by `CommunitySyncRegistry`). Consumes the Phase 1 primitives — `verify_event`, `materialize`, `prior_state_at_event`, `event_sort_key`, `POWER_THRESHOLDS` — without modifying them.

**Shipped via PR #84, merge commit `466e6c2`.**

**Files (as shipped):**
- New: `src-tauri/src/community_state_crdt.rs` — `CommunityState` Prolly Tree, `insert_event` calling `verify_event` with prior_state via `prior_state_at_event`, `MaterializedCache` keyed on `(version, admin_addr)` so bootstrap-admin changes correctly invalidate
- New: `src-tauri/src/community_state_sync.rs` — `CommunitySyncEngine` (debounced publish loop, 3-phase receive pipeline), `CommunitySyncRegistry` (per-community engine lifecycle), `CommunityRootPublishPayload` wire type, `CommunityRootHlcTracker` per-publisher replay protection, AEAD helpers (random-nonce for root publish + deterministic-nonce for content-addressed blob), `CommunitySyncError` taxonomy with stable reason-tags, `IdentityResolver` async trait, `OwnerDeviceCacheResolver` adapter
- New: `src-tauri/src/community_state_persist.rs` — atomic-rename-via-tempfile persistence for `crdt.cbor` + `replay.cbor`; self-heal on corruption via `.corrupt.<unix_ms>` quarantine + start-with-default
- Modified: `src-tauri/src/event_loop.rs` — `spawn_community_state_zenoh_adapter` (per-community pub/sub task with biased select arms + `subscriber_tx.closed()` arm); start_node boot scan for `Space { kind: Community }` rows
- Modified: `src-tauri/src/lib.rs` — `NodeState.community_registry: Option<Arc<CommunitySyncRegistry>>`; snapshot-then-spawn pattern for boot scan
- Modified: `src-tauri/src/owner_state_types.rs` — minor accessor adjustments
- New: `src-tauri/tests/community_state_crdt_unit.rs`, `community_state_persist_unit.rs`, `community_state_sync_crypto_unit.rs`, `community_sync_engine_unit.rs`, `community_sync_registry_unit.rs`, `community_root_hlc_tracker_unit.rs` — module-level unit coverage
- New: `src-tauri/tests/community_sync_integration.rs` — two-engine round-trip with `wait_until` polling helper for deterministic-but-bounded waits
- New: `src-tauri/tests/wire_format_community_sync_fixtures.rs` — pinned-byte CBOR golden fixture for `CommunityRootPublishPayload`

**Delta from original Phase 2 plan (caught during 6+ rounds of bot review on PR #84):**

- **3-phase receive pipeline** (`handle_incoming_publish`): resolve identities under no state lock → batch-insert events under state lock → emit degraded reports under no lock. Avoids holding the state mutex across `.await` points (`IdentityResolver::resolve` is async because the production resolver is `OwnerDeviceCacheResolver`, which itself awaits cache lookups).
- **`MaterializedCache` keyed on `(version, admin_addr)`** rather than just `version`. Bootstrap-admin self-Join is exempt from countersig, so a mid-stream `admin_addr` change (rare but possible during initial setup) MUST invalidate the cache — keying on version alone would surface stale materialization.
- **`next_hlc` saturation guard**: when `prev.logical == u32::MAX`, manufacture `wall_ms + 1` advance and reset logical to 0. Prevents the `record()` debug_assert from firing under sustained same-millisecond bursts.
- **`CommunitySyncError::BlobNotFound { cid }` distinct from `ContentStore(Io)`**: `Ok(None)` from `content_store.get()` is "blob not yet available, recoverable from next state-root" — semantically different from a transport / disk fault. Distinct reason-tag (`"blob_not_found"`) so the eventual frontend banner can switch on it.
- **`CommunitySyncError::MisroutedBlob { expected, found }` distinct from `CborDecode`**: a blob whose CBOR parsed cleanly but whose `community_id` mismatches is a routing failure, not a format failure. Distinct reason-tag prevents misdirecting operators chasing format bugs.
- **`CommunitySyncError::MissingIdentityResolver`** as a configuration-class error: receive-side verify cannot run without an identity resolver; surfacing it as crypto / transport would mislead operators.
- **Snapshot-then-spawn fence on every IPC mutating community CRDT state**: same hardening pattern PR #81 round 6 forced across `send_dm` / `add_space` / `delete_outbox_entry`. Pre-IPC snapshot of `(state_arc, registry_arc)` before any spawn / await; post-mutation re-attachment via the registry handle. Prevents TOCTOU on community lifecycle (joined/left between snapshot and mutation).
- **`tokio::task::spawn_blocking` for sync I/O**: `CommunityState`/`tracker` CBOR codec + atomic-write-via-rename are sync `std::fs` calls. Wrapping `spawn_engine`, `persist_both`, and `persist_replay_only` in `spawn_blocking` keeps the async runtime's worker threads from stalling on disk I/O. Pattern matches `owner_state_sync.rs:376`.
- **`try_send` for fire-and-forget `error_tx`**: degraded reports are best-effort. A blocked `send().await` would back-pressure the receive pipeline; `try_send`'s `Full(_)` / `Closed(_)` are dropped silently.
- **`subscriber_tx.closed()` arm in adapter loop**: without it, the JoinHandle hangs on `sub.recv_async()` forever after the engine drops `subscriber_rx` while idle. Combined with `biased` arm ordering (data-flow first, closed-detection second) so a same-poll race delivers the inbound sample before exit.
- **Deterministic two-mode AEAD nonce**: random-nonce for the outer envelope (each publish independent), deterministic-nonce for the content-addressed blob (so the same plaintext under the same key produces a stable CID — required for CAS dedup to work). Two distinct helpers in `community_state_sync.rs::encrypt_*` so the nonce-reuse trap is impossible to step on.
- **`CommunityRootHlcTracker` monotonicity preserved on dedupe-merge** (the bug fixed in PR #81 round 3 — community Spaces use `Id` dedupe key, same trap as DM Spaces). Tracker advance only after blob fetch + decrypt + decode + misroute check + at least one event accepted; an early advance on a malformed publish would let a correctly-routed re-publish at the same HLC be silently dropped.
- **Persist self-heal**: `load_crdt` / `load_replay` quarantine corrupted on-disk files via `.corrupt.<unix_ms>` rename + start-with-default. Per-community CRDT is fully recoverable from peers via the next state-root publish, so a hard-fail-on-decode-error would needlessly maroon the community.
- **HLC publisher authentication gap explicitly deferred to [ZEB-256](https://linear.app/zeblith/issue/ZEB-256/)** — see Refresh note above. Phase 2's `publisher_device_id` field is authenticated only by the per-community AEAD; closing the spoof gap requires an envelope-shape change (per-publisher device signature + identity binding via `OwnerDeviceCache`). Required before Phase 4 invite-only ships.

All deltas above are in the shipped code; the spec's "Architecture" + "Bug-class coverage" sections below absorb the operational lessons so Phases 3–5 inherit them.

### Phase 3 — Open community flow (create + join + leave)

**Goal:** Open communities are usable end-to-end via IPC (no UI yet). Create a community, generate an invite link with no counter-sig hop, redeem it, see member list update across both peers.

**Files:**
- Modified: `src-tauri/src/lib.rs` — IPC commands: `create_community`, `redeem_invite` (open path only), `leave_community`, `list_community_members`, `generate_invite` (open: produces token-less payload)
- Modified: `src-tauri/src/owner_state_sync.rs` — community Space writes (create/leave) integrate with owner-state HLC tracker
- New: `src-tauri/tests/community_open_flow_integration.rs`

**Deliverables:** Open community fully working at the IPC layer. `community-members-changed` event fires correctly with delta payload. Frontend doesn't exist yet; tests exercise IPC directly via `tauri::test::mock_app`.

### Phase 4 — Invite-only flow (Reticulum CommunityInvite + counter-sig)

**Goal:** Invite-only flavor using the Reticulum unicast + DmInvite Path B pattern from ZEB-227. Uses the existing `dm_outbox` immediate-attempt unicast path (`UnicastSendRequest` mpsc into `event_loop`) for online counter-signers; if no member with `power ≥ invite_threshold` is reachable at redemption time, `redeem_invite` returns `Err` per the v1 contract documented above. v1 does NOT extend `dm_outbox` with a persistent queue or a "pending Join event" model — the persistent offline-counter-signer UX is deferred to [ZEB-254](https://linear.app/zeblith/issue/ZEB-254/) so Sub-C v1 stays focused on the bootstrap loop.

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
| [ZEB-256](https://linear.app/zeblith/issue/ZEB-256/) | Cryptographic publisher authentication for community state-root publishes (close HLC-spoof gap before Phase 4) |

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
