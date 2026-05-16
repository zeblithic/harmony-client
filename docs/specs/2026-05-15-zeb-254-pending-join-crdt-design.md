# ZEB-254: Persistent offline counter-signer queue for invite-only community redemption

**Branch:** `zeb-254-pending-join-crdt`
**Linear:** [ZEB-254](https://linear.app/zeblith/issue/ZEB-254)
**Parent:** [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) (Sub-C v1, shipped 2026-05-08 via PR #113)
**Related:** [ZEB-249](https://linear.app/zeblith/issue/ZEB-249) (epoch rotation — informs CRDT extension idiom), [ZEB-251](https://linear.app/zeblith/issue/ZEB-251) (per-community thresholds — keeps current `invite_threshold=0` hardcoded for v1)

## §1 Problem

[ZEB-217](https://linear.app/zeblith/issue/ZEB-217) Sub-C v1 invite-only redemption (`redeem_invite_inner` at `src-tauri/src/lib.rs:9370`) requires **≥1 community member with `power ≥ invite_threshold` to be online at the moment the joiner redeems**. The joiner sends a `CommunityInviteSigned` Reticulum unicast packet to the inviter's known device(s) and awaits a counter-sign oneshot for ≤15s. If no admin device is online, redemption returns `Err` and the user retries.

This works for the bootstrap MVP but is a UX gap. Real-world Discord/Slack/Matrix invites work even when the admin is offline because their join model doesn't require third-party counter-signature. Our model DOES require counter-signature (providing strong "an existing member vouched for this person" semantics) — but introduces this synchronization problem.

End-user observation: clicking an invite link while no admin is reachable results in `Err` from the redeem IPC and a "please try again later" message. The user has no signal about *when* later might work, and no automated recovery — they must remember to re-click the link.

## §2 Architecture

**Pair pattern.** Two new `MembershipEventKind` variants in the community CRDT:

- `PendingJoin { invite_token, joiner_identity_pub }` — joiner-signed event proving "I have a valid invite and want to join." Distributed via the community CRDT (Zenoh state-root publish).
- `JoinCountersign { target_event_id }` — admin-signed event paired with a specific PendingJoin. Verifies the counter-signing admin has `power ≥ invite_threshold` at the JoinCountersign's HLC.

Joined membership is materialized by **pairing** a PendingJoin with a matching JoinCountersign — not by replacing one event with another. The community CRDT's existing "duplicate event_id → skip" rule (`community_state_sync.rs:2535`) stays untouched; no merge-upgrade rule is introduced.

### State-flow

```text
   ┌───────────────────────────────────────────────────────────────────┐
   │                                                                   │
   │  JOINER                                                           │
   │  ──────                                                           │
   │  redeem_invite_inner:                                             │
   │    1-4. decode URL, snapshot, wall-now, RESERVE HLC               │
   │    5.   mint_pending_join (signed by joiner)                      │
   │    6.   spawn engine + adapter                                    │
   │    7a.  register oneshot keyed on PendingJoin.id                  │
   │    7b.  engine.insert_local_event(pending_join)                   │
   │         └──► state-root publish loop picks it up (Zenoh)          │
   │    7c.  send CommunityInviteSigned via Reticulum unicast          │
   │         (fast-path early-notify to inviter's known devices)       │
   │    7d.  await oneshot ≤ 5s                                        │
   │         (FAST PATH wins → joined; TIMEOUT → pending)              │
   │    8.   fence_check                                               │
   │    9.   COMMIT Space.pending_join_at = Some(hlc) iff timeout      │
   │    10.  return Ok(RedeemInviteResultDto { pending: bool })        │
   │                                                                   │
   └───────────────────────────────────────────────────────────────────┘
                                  │
                            unicast / state-root publish
                                  │
                                  v
   ┌───────────────────────────────────────────────────────────────────┐
   │                                                                   │
   │  ADMIN (auto-counter-signs whenever it receives a PendingJoin)    │
   │  ─────                                                            │
   │  • Reticulum receipt (handle_unicast):                            │
   │      verify packet → insert_local_event(pending_join)             │
   │      → emit_counter_sign_for(pending_join.id)                     │
   │                                                                   │
   │  • CRDT state-root receipt (post-Inserted hook on PendingJoin):   │
   │      check self_power ≥ invite_threshold + Joined                 │
   │      check no self-authored JoinCountersign already (idempotent)  │
   │      → spawn_emit_counter_sign(pending_join.id)                   │
   │                                                                   │
   │  emit_counter_sign:                                               │
   │      build JoinCountersign event, sign with self identity         │
   │      insert_local_event(join_countersign)                         │
   │      └──► state-root publish loop carries to all peers (Zenoh)    │
   │                                                                   │
   └───────────────────────────────────────────────────────────────────┘
                                  │
                          state-root sync (Zenoh)
                                  │
                                  v
   ┌───────────────────────────────────────────────────────────────────┐
   │                                                                   │
   │  JOINER (now or later, via state-root sync)                       │
   │  ──────                                                           │
   │  • engine inserts JoinCountersign                                 │
   │  • post-Inserted hook: target = a PendingJoin I authored?         │
   │      → resolve oneshot if still pending (5s window not yet up)    │
   │      → emit nav-updated { modified, pending: false }              │
   │      → enqueue Space.pending_join_at = None update                │
   │                                                                   │
   │  Frontend NavService: greyed → full color; toast "You're in!"     │
   │                                                                   │
   └───────────────────────────────────────────────────────────────────┘
```

### Why this design

**No CRDT merge changes.** The pair pattern keeps the existing community CRDT semantics intact: each event is independent, indexed by its own event_id. Today's `if state.events.contains_key(&event.id) { continue; }` short-circuit at `community_state_sync.rs:2535` continues to work without modification.

**Wire-compat with legacy invite-only Joins.** Pre-ZEB-254 `j` (Join) events with `countersig=Some` continue to verify and materialize as Joined — they remain the canonical shape when the Reticulum fast path succeeds in clients that have not yet upgraded. New clients emit the PendingJoin + JoinCountersign pair. Both paths converge on `MemberStatus::Joined`.

**Idempotent auto-counter-sign.** A PendingJoin can arrive via both Reticulum unicast AND Zenoh state-root. The auto-counter-sign hook checks for self-authored JoinCountersign(target=pending.id) in the local event log before emitting; a second delivery from the same admin is a no-op. Multiple admin devices may each emit their own JoinCountersign for the same PendingJoin — both are accepted; materialize pairs against the first by HLC order. No deduplication at CRDT layer (acceptable: each event is ~200 bytes; rare-event traffic).

**Pure-function expiry.** Stale PendingJoins (>30 days) are hidden from materialize without emitting tombstone events. Deterministic across peers based on community's current HLC vs. pending_at HLC. Matches `OutboxEntry::compute_status` precedent for time-based state transitions.

**Trust model unchanged from v1.** The InviteToken issued by the admin IS the admin's consent — they signed it at invite-issue time. Auto-counter-sign on receipt matches today's Reticulum path semantics: any admin device with sufficient power signs without further human gate. Suspicious or unwanted joiners are revoked via the existing Kick primitive after the fact, surfaced via the CommunitySettingsPanel "Recent joins" feed.

### Failure modes

**InviteToken expired between issue and pending-Join publish.** Verify rejects (`PendingJoinTokenExpired`). Joiner sees `Err` from redeem IPC — same UX as today. Mitigation: ZEB-217 v1 sets `not_after` to 7 days from issue; ZEB-254 doesn't change this.

**Joiner publishes PendingJoin, immediately closes app, never returns.** PendingJoin sits in community CRDT. Admin auto-counter-signs whenever they come online. JoinCountersign propagates. Materialize gives joiner `MemberStatus::Joined` — but the joiner's local app never sees it (they never restarted). Net effect: joiner is a community member but doesn't know it. On next launch their owner-state still has Space.pending_join_at = Some, but their community engine respawns + state-root sync converges + post-Inserted hook updates Space.pending_join_at = None. Self-healing on restart.

**Admin auto-counter-signs malicious PendingJoin.** The InviteToken validates that this joiner was explicitly invited by an admin with the InviteToken-signing private key. If the admin's key is compromised, the attacker can issue arbitrary InviteTokens — but that's a key-compromise scenario, not a ZEB-254-specific weakness. Existing Kick primitive lets any sufficiently-powered admin remove the joiner post-counter-sign. CommunitySettingsPanel "Recent joins" feed surfaces the audit trail.

**Two admins counter-sign the same PendingJoin near-simultaneously.** Both JoinCountersign events are inserted; both reference the same `target_event_id`. Materialize: PendingJoin paired with the FIRST JoinCountersign by HLC ordering. The second is a duplicate but does no harm — both signatures are valid; the joiner is Joined regardless. No deduplication at CRDT layer.

**Joiner cancels pending join via Leave event.** Materialize: Leave supersedes earlier PendingJoin (existing `Leave` rule transitions any prior state to `Left`). Space.left_at = Some via existing flow. CommunitySettingsPanel pending-Joins feed no longer shows the entry. No new code path.

**Joiner is Banned via Kick before counter-sign lands.** Kick(target=joiner) lands first by HLC; joiner status → Banned. Subsequent PendingJoin (by Banned actor) is rejected at verify (`PendingJoinAlreadyMember` — extended to cover Banned). Or, if PendingJoin lands first and then Kick: verify accepts both individually; materialize: Kick supersedes PendingJoin → Banned. JoinCountersign for the Banned PendingJoin is still verifiable but materialize ignores it (Banned-sticky).

### Cross-peer dedupe with legacy clients

A pre-ZEB-254 client receiving a PendingJoin via state-root sync would emit `community-state-sync-degraded` (unknown variant) and skip the event. The pending join would be invisible to legacy clients until they upgrade. Once upgraded, they re-replay the event log and materialize PendingJoin correctly. **Acceptable behavior**: Sub-C v1 pre-Alpha; small client population; no live data loss; legacy clients can still join via the Reticulum fast path (which keeps using the legacy `j` event shape). ZEB-254 ships during the same release window as the wire-format change.

## §3 Wire format

### MembershipEventKind additions (in `src-tauri/src/community_membership.rs`)

```rust
/// ZEB-254: joiner-signed pending join for invite-only communities.
/// Distributed via the community CRDT (Zenoh) so admins who were
/// offline at redemption time can counter-sign asynchronously.
///
/// Verify rules:
///   - actor sig over (id, community_id, kind, actor, at) valid
///   - joiner_identity_pub hashes to actor: SHA256(joiner_identity_pub)[..16]
///     matches event.actor.0 (per harmony_identity::Identity::address_hash;
///     same derivation as DeviceIdentityHash via dm_signing::derive_device_hash_from_identity_pub)
///   - invite_token.signer == ctx.admin_addr
///   - invite_token.invitee_hint matches actor (existing InviteToken rule)
///   - invite_token.not_after >= event.at.wall_ms
///   - invite_token signature verifies against admin's identity_pub
///   - prior state has actor as None | Some(Left) (block double-pending,
///     block pending-from-Banned)
///
/// Variant code "g" (gate / guest, unused before this).
/// Inner field keys are 2-char (it, jp) per same-length-keys invariant.
#[serde(rename = "g")]
PendingJoin {
    #[serde(rename = "it")]
    invite_token: InviteToken,
    /// 64-byte concatenation of X25519_pub || Ed25519_pub matching
    /// `harmony_identity::Identity::to_public_bytes()`. Same shape as
    /// `CommunityInviteSigned.joiner_identity_pub` in
    /// `community_invite.rs:258`.
    #[serde(
        rename = "jp",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    joiner_identity_pub: [u8; 64],
},

/// ZEB-254: admin-signed counter-sign approving a PendingJoin.
/// Pairs by target_event_id; materialize joins them at replay time.
///
/// Verify rules:
///   - actor sig valid
///   - is_joined_member(prior_state, event.actor) == true
///   - prior_state.power_levels[event.actor] >= invite_threshold (= 0 in v1)
///   - target_event_id existence is materialize-time concern (NOT verify),
///     so JoinCountersign may land before its target PendingJoin without
///     rejection
///
/// Variant code "y" (yes / approve).
/// Inner field key 2-char (tg) — matches existing target-field convention
/// from Invite/Kick/SetPower/Unban variants.
#[serde(rename = "y")]
JoinCountersign {
    #[serde(
        rename = "tg",
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    target_event_id: EventId,
},
```

### MemberStatus addition (in `community_membership.rs:860`)

```rust
pub enum MemberStatus {
    #[serde(rename = "j")] Joined,
    #[serde(rename = "i")] Invited,
    #[serde(rename = "l")] Left,
    #[serde(rename = "b")] Banned,
    #[serde(rename = "p")] PendingJoin, // ZEB-254
}
```

Existing variants already carry explicit `#[serde(rename = "...")]` (see `community_membership.rs:860-869`). ZEB-254 adds `PendingJoin` with `rename = "p"` (free single-char tag). Fixture test 35 pins canonical CBOR for the round-trip.

### Space addition (in `src-tauri/src/owner_state_types.rs`)

```rust
pub struct Space {
    // ...existing fields unchanged...
    
    /// ZEB-254: set when the joiner has minted a PendingJoin for this
    /// community but no JoinCountersign has yet landed locally. None
    /// means the joiner is fully Joined (or this Space is non-Community,
    /// or pre-ZEB-254 Space). Transitions:
    ///   - None → Some(hlc): set at redeem-invite commit when the 5s
    ///     fast-path timeout fires without a counter-sign
    ///   - Some(hlc) → None: cleared by the community engine's post-Inserted
    ///     hook when self's PendingJoin receives a JoinCountersign
    ///
    /// CRDT merge: existing LWW-by-updated_at handles None ↔ Some
    /// transitions (Space.updated_at advances on each transition).
    #[serde(rename = "pj", skip_serializing_if = "Option::is_none", default)]
    pub pending_join_at: Option<Hlc>,
}
```

`Hlc` not `bool` so the pending-since timestamp is preserved for the staleness rule (joiner-side display: "Joining since ...").

### Wire fixtures (`src-tauri/tests/wire_format_zeb254_fixtures.rs`, new file)

Byte-pinned canonical CBOR fixtures for:

1. `PendingJoin` SignedMembershipEvent with synthetic actor / token / pubkey.
2. `JoinCountersign` SignedMembershipEvent with synthetic target_event_id.
3. `MemberStatus::PendingJoin` round-trip.
4. `Space` with `pending_join_at = Some(hlc)` and round-trip.

Uses `test-fixtures` feature for deterministic crypto helpers, following the `wire_format_zeb254_fixtures.rs` and `wire_format_channel_log_fixtures.rs` shape.

## §4 redeem_invite_inner changes

### Signature changes

```rust
/// IPC result for `redeem_invite`. Adds `pending` field.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RedeemInviteResultDto {
    pub community_id: String,
    pub community_name: String,
    pub is_invite_only: bool,
    /// ZEB-254: true if redemption returned before a JoinCountersign
    /// landed locally. Caller renders community as "joining…" / greyed.
    /// false if either (a) fast-path counter-sign came back within 5s,
    /// or (b) community is open (no countersign required).
    pub pending: bool,
}
```

### `mint_redemption` → `mint_pending_join` (signature change)

The existing `mint_redemption` at `lib.rs:9227` produces a `MintedCommunity { bootstrap_join: SignedMembershipEvent }` where `bootstrap_join.kind == MembershipEventKind::Join` with `countersig = None`.

ZEB-254: invite-only path returns `bootstrap_join.kind == MembershipEventKind::PendingJoin { invite_token, joiner_identity_pub }`. Open-community path is unchanged.

```rust
let event_kind = if payload.is_invite_only {
    MembershipEventKind::PendingJoin {
        invite_token: payload.invite_token.clone(),
        joiner_identity_pub: signing_key.verifying_key().to_bytes(),
    }
} else {
    MembershipEventKind::Join
};
```

`InviteToken` is a struct in `community_invite.rs` (existing — admin-signed, carries `invitee_hint` + `not_after`). It's already pulled from `payload.invite_token` for the admin-bootstrap verify path; reusing it here is field rewiring, no new crypto.

### `redeem_invite_inner` invite-only branch rewrite (`lib.rs:9370` onward)

| Step | Today | ZEB-254 |
|------|-------|---------|
| 5 | mint Join (countersig=None) | mint PendingJoin |
| 7a | register oneshot keyed on Join.id | register oneshot keyed on PendingJoin.id (resolves when matching JoinCountersign lands locally) |
| 7b (NEW) | (none) | `engine.insert_local_event(pending_join)` — engine state-root publishes via Zenoh |
| 7c | build CommunityInviteSigned, send Reticulum unicast | same, body now wraps PendingJoin |
| 7d | await ≤ 15s; timeout → `take_pending_redemption` + rollback + Err | await ≤ 5s; timeout → take_pending_redemption + PROCEED with `pending=true` (no rollback) |
| 8 | fence_check; on Err: rollback | fence_check; on Err: full rollback as today (engine teardown via community_sync_guard Drop). Note: the PendingJoin may have already been state-root-published to peers; an orphaned PendingJoin sits in peers' CRDTs and auto-expires at 30d via §2 pure-function expiry. Joiner restart → no engine respawn (Space rolled back) → no re-publish. Harmless. |
| 9 | commit Space (joined) | commit Space with `pending_join_at: Some(hlc)` if timeout fired, else `None` |
| 10 | return `Ok(community_id)` | return `Ok(RedeemInviteResultDto { ..., pending })` |

**5s timeout rationale.** Reticulum unicast counter-sign typically completes in 1-3s. 5s gives headroom for slower hops. If exceeded, the joiner falls through to the persistent CRDT path — they are no worse off than today (today: Err after 15s).

**HARMONY_REDEEM_INVITE_TIMEOUT_MS env var** is preserved and now applies to the 5s default. Tests can shorten it.

## §5 Admin-side flow

### `handle_unicast` change (`community_invite.rs:1471`)

Today's flow:
1. decode 0x10 packet
2. verify envelope sig
3. pure verify chain
4. resolve engine + state
5. self-eligibility check
6. attach_countersig via `attach_countersig_with_identity`
7. `engine.insert_local_event_with_pubs(counter_signed_join)`

ZEB-254:
1-5. unchanged (verify chain accepts the inner-event body whether it's a legacy Join or a PendingJoin; specifically, the InviteToken-binding-to-joiner check remains the same — `verify_packet_pure` still validates the bundle).
6. **`engine.insert_local_event_with_pubs(pending_join)`** — insert the PendingJoin into the admin's community CRDT. This triggers the engine's post-Inserted hook (step 7 below) which auto-emits the JoinCountersign.
7. **Auto-counter-sign hook fires.** Hook detects the PendingJoin, signs a `JoinCountersign(target=pending.id)`, inserts via `insert_local_event`. State-root publish loop carries both events to joiner.

**Wire-compat detail.** Today's `CommunityInviteSigned.signed_join` carries `SignedMembershipEvent { kind: Join, countersig: None, ... }`. ZEB-254 changes the inner-event kind to `PendingJoin { invite_token, joiner_identity_pub }`. The outer `CommunityInviteSigned` envelope ALSO carries its own `joiner_identity_pub: [u8; 64]` + `signing_device_hash: DeviceIdentityHash` (`community_invite.rs:258, 265`). The duplication is intentional: the outer envelope is Reticulum-unicast-only and is destroyed once decoded, but the inner PendingJoin event needs to self-carry the pubkey because it ALSO flows via Zenoh state-root sync where no outer envelope exists. The outer envelope's pubkey-binding check at `community_invite.rs:1211` (`SHA256(joiner_identity_pub)[..16] == signing_device_hash`) is unchanged; the inner PendingJoin's pubkey-binding check (same derivation, comparing to `event.actor.0`) is a new verify-time gate in ZEB-254.

### Auto-counter-sign post-Inserted hook (new, in `community_state_sync.rs`)

```rust
// Called from insert_local_event / insert_local_event_with_pubs after
// InsertOutcome::Inserted. Spawned (not awaited) so the insertion path
// doesn't block on counter-sign emission.
fn on_pending_join_inserted(
    &self,
    pending_event: &SignedMembershipEvent,
    state_snapshot: &CommunityState,
) {
    let MembershipEventKind::PendingJoin { .. } = &pending_event.kind else {
        return;
    };
    
    let self_addr = self.self_owner;
    let mat = materialize(&state_snapshot.events, self.admin_addr);
    let self_status = mat.members.get(&self_addr).map(|m| m.status);
    let self_power = mat.power_levels.get(&self_addr).copied().unwrap_or(0);
    
    if self_status != Some(MemberStatus::Joined) {
        return;
    }
    if self_power < POWER_THRESHOLDS.invite {
        return;
    }
    
    // Idempotency: skip if I've already counter-signed this PendingJoin.
    let already_signed = state_snapshot.events.values().any(|e| {
        e.actor == self_addr
            && matches!(
                &e.kind,
                MembershipEventKind::JoinCountersign { target_event_id }
                if *target_event_id == pending_event.id
            )
    });
    if already_signed {
        return;
    }
    
    // Spawn the counter-sign emit. Sync emit would re-enter the lock
    // we're holding for the insert.
    let pending_id = pending_event.id;
    let engine_arc = std::sync::Arc::clone(&self.engine_arc);
    let community_id = self.community_id;
    let self_addr = self.self_owner;
    let signing_key = std::sync::Arc::clone(&self.signing_key);
    let hlc_tracker = std::sync::Arc::clone(&self.hlc_tracker);
    let device_id = self.device_id;
    tokio::spawn(async move {
        // Reserve HLC via the established dm_outbox pattern (ZEB-267).
        let cs_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
            &hlc_tracker,
            &device_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        ).await;
        let counter_sign_payload = crate::community_membership::EventPayload {
            id: random_event_id(),
            community_id,
            kind: MembershipEventKind::JoinCountersign { target_event_id: pending_id },
            actor: self_addr,
            at: cs_hlc,
        };
        let signed = match crate::community_membership::sign_event(&counter_sign_payload, &signing_key) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "ZEB-254 auto-counter-sign: sign_event failed");
                return;
            }
        };
        if let Err(e) = engine_arc.insert_local_event(signed).await {
            tracing::warn!(error = ?e, "ZEB-254 auto-counter-sign: insert_local_event failed");
        }
    });
}
```

### Joiner-side post-Inserted hook (for received JoinCountersign)

Today's hook (`notify_pending_redemption_in_map`, fires on inserted Join with countersig from Reticulum-receive path) fires the joiner's `pending_redemptions[event_id]` oneshot. ZEB-254 generalizes to also fire on JoinCountersign whose target_event_id matches a registered key.

Additionally, when self's PendingJoin gets countersigned (whether via fast-path Reticulum or async CRDT delivery), the hook:
1. Resolves the oneshot if still registered (means redeem_invite_inner's await is still pending — fast path).
2. Enqueues a Space update: `Space.pending_join_at = None`, `updated_at = current_hlc`. Goes through `apply_space_with_canonicalization` like any other Space update; CRDT merge handles propagation to other joiner devices.
3. Emits `nav-updated { action: "modified", space_id, kind: "community", name, pending: false }` Tauri event.

## §6 New IPCs

```rust
/// ZEB-254: list pending joins for a community (admin audit feed).
/// Returns PendingJoin events that do NOT yet have a matching
/// JoinCountersign AND are within the 30-day expiry window. Sorted by
/// pending_at HLC ascending (oldest first).
#[tauri::command]
async fn list_pending_joins(
    community_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<PendingJoinDto>, String> { ... }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingJoinDto {
    /// Hex-encoded EventId of the PendingJoin event.
    pub event_id: String,
    /// Hex-encoded OwnerAddr of the joiner.
    pub joiner_addr: String,
    /// HLC at which the joiner published the pending Join.
    pub pending_at_hlc: HlcDto,
    /// Optional invitee_hint from the InviteToken (display name / handle).
    pub invitee_hint: Option<String>,
}

/// ZEB-254: list recent counter-signs by self (admin audit feed for
/// "Recent joins"). Returns most-recent first. limit caps result size.
#[tauri::command]
async fn list_recent_counter_signs(
    community_id: String,
    limit: u32,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<CounterSignDto>, String> { ... }

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CounterSignDto {
    pub join_event_id: String,
    pub joiner_addr: String,
    pub countersigned_at_hlc: HlcDto,
}
```

Snake-case Rust IPC names; the Tauri boundary auto-converts to camelCase for JS callers per project convention.

## §7 Frontend

### `RedeemInviteWizard.svelte` change

The wizard already calls `redeem_invite` IPC and handles the result. Change:

```typescript
const result = await invoke<RedeemInviteResultDto>('redeem_invite', {
    inviteUrl: url,
    comment: optionalComment,
});

if (result.pending) {
    toast.show("Join request sent. The community will unlock once an admin approves.");
} else {
    toast.show("You're in!");
}
// In both cases, dismiss the wizard and navigate to nav.
dismiss();
navService.refresh();
```

### `NavService` change

Community spaces with `pending_join_at !== null` render greyed with a "Joining…" tooltip. Clicking a pending community still opens the space view; ChannelList shows a banner "Your join is pending admin approval. You can leave at any time." No message-input enabled.

`nav-updated` listener: when `{ action: "modified", pending: false }` arrives for a community in pending state, transition to full color + toast "You're in!".

### `CommunitySettingsPanel.svelte` additions

Two new collapsible sections (only rendered if self has power ≥ invite_threshold):

1. **Awaiting counter-sign** — calls `list_pending_joins`, shows each with: joiner name (from invitee_hint or hex prefix), pending-since timestamp, "Kick this joiner" button (calls existing `kick` IPC).
2. **Recent joins** — calls `list_recent_counter_signs(limit=20)`, shows audit log.

Polling: both lists refresh on `community-state-sync-converged` event (existing event emitted by Phase 2's debounced state-root publish). No new polling loops.

### Cancel-pending UX

Right-click / long-press on a pending community shows a "Cancel join request" context menu item. Wired to the existing `leave_community` IPC. The Leave event supersedes the PendingJoin at materialize time; the Space disappears from nav via existing left_at flow.

## §8 Backward compatibility

| Direction | Behavior |
|-----------|----------|
| New client receiving legacy `j`+countersig event from old peer | Verifies + materializes as Joined unchanged. |
| Old client receiving new `g` PendingJoin from new peer | Verify rejects (unknown variant) → emits `community-state-sync-degraded`. Event is dropped from old client's view. Joiner is invisible to old clients until they upgrade. Acceptable: pre-Alpha, small population, no live data, fixed by upgrading. |
| Old client receiving new `y` JoinCountersign | Same as PendingJoin — rejected, dropped from view. |
| Old client receiving new `Space.pending_join_at = Some(hlc)` field | `skip_serializing_if = Option::is_none` + `serde default` means CBOR decodes ignore the unknown field (assuming `#[serde(deny_unknown_fields)]` is not set on Space, which it is not in current code — verify in implementation). Space appears as a normal community Space; old client doesn't render greyed state. Acceptable. |
| Self being a pre-ZEB-254 redeem-invite caller (e.g. headless CLI not yet upgraded) | Falls through legacy `j` event path. Reticulum unicast still works; admin counter-signs as today. Backwards-compat preserved end-to-end. |

## §9 Testing

### Unit tests (in `community_membership.rs` `mod tests`)

1. `pending_join_event_signs_and_verifies` — round-trip.
2. `pending_join_rejected_when_token_not_for_actor` — `invite_token.invitee_hint` ≠ event.actor → `PendingJoinTokenInvalid`.
3. `pending_join_rejected_when_token_expired` — `invite_token.not_after < event.at.wall_ms` → `PendingJoinTokenExpired`.
4. `pending_join_rejected_when_token_signer_not_admin` — `invite_token.signer` ≠ `ctx.admin_addr` → `PendingJoinTokenInvalid`.
5. `pending_join_rejected_when_actor_already_joined` — prior state has actor Joined → `PendingJoinAlreadyMember`.
6. `pending_join_rejected_when_actor_banned` — prior state Banned → `PendingJoinAlreadyMember`.
7. `pending_join_accepted_when_actor_was_left` — prior state Left → Ok.
8. `pending_join_rejected_when_identity_pub_does_not_hash_to_actor` — joiner_identity_pub binding violation.
9. `join_countersign_event_signs_and_verifies` — round-trip.
10. `join_countersign_rejected_when_actor_lacks_power` — actor.power < invite_threshold → `JoinCountersignActorNotAdmin`.
11. `join_countersign_rejected_when_actor_not_joined` — actor.status != Joined → `JoinCountersignActorNotJoined`.
12. `join_countersign_accepted_when_target_missing` — target_event_id not yet in state → Ok (out-of-order delivery).
13. `materialize_pending_join_only_yields_pending_status` — single PendingJoin → `MemberStatus::PendingJoin`.
14. `materialize_pending_join_with_countersign_yields_joined` — PendingJoin + matching JoinCountersign → `MemberStatus::Joined`.
15. `materialize_pending_join_older_than_30d_hidden` — community current HLC > pending_at + 30d → joiner absent from materialized members map.
16. `materialize_pending_join_countersign_resurrects_expired_pending` — even past 30d, JoinCountersign upgrades to Joined.
17. `materialize_legacy_join_with_countersig_still_yields_joined` — pre-ZEB-254 `j` events with countersig unchanged.
18. `materialize_pending_join_then_leave_yields_left` — Leave supersedes PendingJoin.
19. `materialize_pending_join_with_two_countersigns_yields_joined` — duplicate counter-sign accepted, no error.

### `redeem_invite_inner_tests` (`lib.rs`)

1. `redeem_invite_pending_returns_ok_pending_when_no_admin_online` — mock unicast destinations empty; 5s timeout fires; `Ok { pending: true }`; Space has `pending_join_at = Some`.
2. `redeem_invite_fast_path_returns_ok_joined_when_admin_online` — synthetic JoinCountersign delivered within 5s window; oneshot resolves; `Ok { pending: false }`; Space.pending_join_at = None.
3. `redeem_invite_pending_to_joined_transition_clears_pending_at` — async path: pending committed; JoinCountersign arrives later via state-root sync; Space.pending_join_at flips to None.

### Engine post-Inserted hook tests (`community_state_sync.rs`)

1. `admin_engine_auto_counter_signs_on_pending_join_insert` — admin's engine receives PendingJoin via state-root, auto-emits JoinCountersign.
2. `admin_engine_idempotent_no_duplicate_counter_sign` — same PendingJoin inserted twice; only one JoinCountersign emitted.
3. `non_admin_engine_does_not_auto_counter_sign` — self.power < invite_threshold → hook skips.
4. `kicked_admin_does_not_auto_counter_sign` — self.status == Banned → hook skips even with cached power_levels entry.

### Integration tests (new file `src-tauri/tests/community_pending_join_integration.rs`)

1. `pending_join_resolves_when_admin_comes_online` — two-engine harness: joiner mints PendingJoin and publishes (admin engine offline); admin engine starts; admin receives + auto-counter-signs; joiner observes JoinCountersign; Space.pending_join_at clears; status → Joined.
2. `pending_join_survives_joiner_restart` — joiner mints + publishes; admin offline; joiner shuts down + restarts; pending Join re-publishes via state-root on engine respawn; admin (started post-restart) sees + counter-signs; joiner observes.
3. `pending_join_resolves_under_two_admin_race` — two admin engines online; both auto-counter-sign; both JoinCountersign events accepted; materialize gives Joined.
4. `legacy_invite_only_join_with_countersig_still_accepted` — pre-ZEB-254 wire shape `j`+countersig still verifies + materializes Joined.
5. `pending_join_cancellation_via_leave` — joiner mints PendingJoin, then emits Leave; materialize: Left supersedes; admin's post-Inserted hook still emits JoinCountersign (idempotent — JoinCountersign just stays as audit trail; Left wins at materialize).
6. `pending_join_30d_expiry_hides_joiner` — joiner mints PendingJoin at HLC0; community HLC advances 30+ days; materialize hides joiner; later JoinCountersign upgrades to Joined regardless.

### Wire fixtures (new file `src-tauri/tests/wire_format_zeb254_fixtures.rs`)

1. `pending_join_canonical_cbor_pinned` — byte-exact CBOR for a synthetic PendingJoin.
2. `join_countersign_canonical_cbor_pinned` — byte-exact CBOR for a synthetic JoinCountersign.
3. `member_status_pending_join_round_trip` — `MemberStatus::PendingJoin` serde round-trip.
4. `space_with_pending_join_at_round_trip` — Space CBOR with `pending_join_at = Some(hlc)`.

### Frontend tests (vitest)

1. `RedeemInviteWizard.test.ts` — pending=true result → toast "Join request sent…" + dismiss + nav refresh.
2. `RedeemInviteWizard.test.ts` — pending=false result → toast "You're in!".
3. `NavService.test.ts` — community with pending_join_at renders greyed; `nav-updated { pending: false }` event removes greyed.
4. `CommunitySettingsPanel.test.ts` — `list_pending_joins` returns 2 entries → 2 rows; Kick button calls `kick` IPC with correct args.
5. `CommunitySettingsPanel.test.ts` — `list_recent_counter_signs` returns 3 entries → 3 audit-log rows.

### Acceptance criteria mapping

| Criterion (ticket) | Covered by tests |
|---|---|
| Joiner offline-redemption returns Ok with pending status | `redeem_invite_pending_returns_ok_pending_when_no_admin_online` |
| Admin comes online → sees pending → counter-signs → full member | `admin_engine_auto_counter_signs_on_pending_join_insert`, `pending_join_resolves_when_admin_comes_online` |
| Restarts preserve pending state | `pending_join_survives_joiner_restart` |
| Stale Joins >30 days auto-expire | materialize tests 15 + `pending_join_30d_expiry_hides_joiner` |
| Pending badge in NavService for joiner | `NavService.test.ts` + render check |
| "Pending join requests" in CommunitySettingsPanel for admin | `CommunitySettingsPanel.test.ts` |

## §10 Scope

**Single bundled PR.** Backend (verify gate, materialize, engine hooks, IPCs, fixtures) and frontend (wizard, nav, settings panel) ship together. The verify-gate change is meaningless without the frontend greyed-state rendering; the admin audit feed is meaningless without the backend `list_pending_joins` IPC. Splitting forces a transitional state where backend ships but frontend can't surface the new state, which `feedback_design_for_eventual_state` argues against.

Estimated size: ~1500-2000 lines Rust (mostly in `community_membership.rs`, `community_state_sync.rs`, `lib.rs`, new integration + fixture test files) + ~400-600 lines TS/Svelte.

## §11 Out of scope

- **Per-community `invite_threshold` customization** — deferred to [ZEB-251](https://linear.app/zeblith/issue/ZEB-251). ZEB-254 keeps the hardcoded 0 default.
- **Joiner-side withdrawal event type** — the existing Leave primitive covers cancellation. No new `WithdrawPendingJoin` variant.
- **TreeKEM-style backward secrecy on counter-sign emit** — covered by [ZEB-249](https://linear.app/zeblith/issue/ZEB-249) (merged).
- **Multi-admin M-of-N consent for invite-only** — deferred to [ZEB-250](https://linear.app/zeblith/issue/ZEB-250). v1 trust model: single InviteToken from any admin = consent.
- **Rate-limiting spam pending Joins from malicious URL-sharers** — separate cost-and-spam ticket; ZEB-254 accepts that anyone with a valid InviteToken can publish a PendingJoin and an admin counter-signs.
- **Auto-poke notifications to admin devices when a pending Join lands** — ProfileMembershipBroadcast cascade already wakes admin devices on relevant community-state changes. No new wiring.
- **Migrating the Reticulum fast path off the legacy `j`+countersig wire shape** — that's a wire-format-cleanup ticket; ZEB-254 keeps both paths interoperable.
- **Wire-bump version field on SignedMembershipEvent** — current shape preserves wire compat for unknown variants via serde's tag-based decode; no global version bump needed.

## §12 Acceptance

- Joiner can redeem an invite-only invite while no admin device is online → returns `Ok { pending: true }` within 5s; community appears in nav greyed.
- Admin comes online → community engine spawns → state-root sync delivers PendingJoin → admin auto-counter-signs → state-root publishes JoinCountersign → joiner observes within seconds; Space.pending_join_at clears; nav ungreys.
- Restart on joiner side preserves pending state across launches; PendingJoin re-publishes from persisted event log.
- Restart on admin side: PendingJoin still in community CRDT (came from joiner's state-root publish); admin's auto-counter-sign hook fires on engine spawn.
- Stale PendingJoins (>30d) hidden from materialized members map without admin action.
- All 5 CI gates green: `cargo fmt --all -- --check` + `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest run --locked --workspace --all-targets --features test-fixtures` + `npx tsc --noEmit` + `npx vitest run`.
