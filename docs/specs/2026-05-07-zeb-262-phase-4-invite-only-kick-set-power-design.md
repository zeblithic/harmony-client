# ZEB-262 — Sub-C Phase 4: invite-only flow + kick + set-power (backend)

> **Linear:** [ZEB-262](https://linear.app/zeblith/issue/ZEB-262), parent [ZEB-217](https://linear.app/zeblith/issue/ZEB-217). Folds in [ZEB-258](https://linear.app/zeblith/issue/ZEB-258) (atomic rollback on failed `create_community` / `redeem_invite`).
> **Reference spec:** `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md` (Sub-C Phase 1-5 design). This document covers Phase 4 only.

## Goal

Ship the IPC surface that distinguishes communities from a chat group: invite-only redemption (Reticulum counter-sig hop), `kick_from_community`, `set_power_level`. Plus the auto-counter-sign receive path in `event_loop.rs`. Plus the [ZEB-258](https://linear.app/zeblith/issue/ZEB-258) atomic-rollback fix folded in (Phase 4 already restructures `redeem_invite`, marginal cost is small).

Frontend (Phase 5: `CommunitySettingsPanel`, `MemberRow`, `InviteRedeemDialog`) ships in a separate PR. Phase 4 backend can be exercised via direct IPC calls or the existing test infrastructure.

## Architecture

```
                ┌────────────────────────┐                      ┌────────────────────────┐
                │  Alice (joiner) node   │                      │  Bob (inviter) node    │
                └────────────────────────┘                      └────────────────────────┘

  redeem_invite(invite-only URL)             ─Reticulum unicast─►       event_loop receives
    1. decode URL                                                          CommunityInvite packet (0x10)
    2. reserve HLC under tracker lock                                      ▼
    3. mint+sign Join event (countersig=None)                              verify Path B envelope sig
    4. spawn engine (NEW order: BEFORE owner-state commit)                 verify Join event sig (Phase 1 verify_signature)
    5. dispatch adapter request                                            verify InviteToken sig (signer=self)
    6. register oneshot keyed on Join event id                             verify community_id agreement
    7. send CommunityInviteSigned packet                                   verify expires_at hasn't passed
    8. await oneshot ≤ 15s                                                  verify invitee_hint (if Some) matches
                                                                            verify self is currently Joined
                       ◄──── state-root publish (Phase 2 + ZEB-256) ─       ▼
                            counter-signed Join                            attach_countersig_with_identity
                                                                            engine.insert_local_event
    9. on landing: commit owner-state Space (NEW position — LAST)           (debounce → state-root publish)
   10. return Ok(space_id)
```

For `kick_from_community` and `set_power_level`, no Reticulum hop is involved — they mint + insert directly through the engine, and Phase 2's debounce + ZEB-256's publisher-sig machinery carries the publish to peers.

## Files touched

### New files

- `src-tauri/src/community_invite.rs` — currently only carries URL encode/decode. Phase 4 extends it with:
  - `CommunityInviteSigned` struct (the Reticulum unicast packet body)
  - `CommunityInvitePacket` enum (Path B app-sig wrapper, mirrors `dm_envelope::DmPacket`)
  - `encode_packet` / `decode_packet` (canonical CBOR + sig append/split)
  - `build_signed_invite_packet` (sign with sender's device key)
  - `handle_unicast` (receive-side verify + counter-sign + publish)
  - `CommunityInviteVerifyError` enum
- `src-tauri/tests/community_invite_unit.rs` — unit tests for the new wire format + verify rules
- `src-tauri/tests/community_invite_only_integration.rs` — two-node Alice-redeems-invite-only happy path + timeout + rollback

### Modified files

- `src-tauri/src/lib.rs` — `redeem_invite` invite-only branch + `kick_from_community` + `set_power_level` IPCs + ZEB-258 reorder in `create_community` + `redeem_invite`
- `src-tauri/src/event_loop.rs` — discriminant-based pre-dispatch for `RuntimeAction::UnicastReceived`; route `0x10` to `community_invite::handle_unicast`
- `src-tauri/src/community_state_sync.rs` — add `pending_redemptions` map to `CommunitySyncRegistry` (event-id → oneshot for the IPC's wait-for-counter-signed-Join detector); add `shutdown_engine_and_cleanup_persistence` for ZEB-258 rollback
- `src-tauri/src/dm_outbox.rs` — add `private_identity: Arc<PrivateIdentity>` field parallel to `signing_key` so the receive-side counter-sign has access to a `&PrivateIdentity` for `attach_countersig_with_identity`
- `src-tauri/tests/community_membership_unit.rs` — add kick/set-power edge-case tests
- `src-tauri/tests/wire_format_community_fixtures.rs` — pin `CommunityInviteSigned` canonical bytes
- `src-tauri/tests/community_sync_registry_unit.rs` — pin the new `shutdown_engine_and_cleanup_persistence` surface

## Data model

### `CommunityInviteSigned` (new wire type)

Reticulum-unicast packet sent from joiner → counter-signer. Mirrors `DmInviteSigned` from `dm_envelope.rs`. Path B app-sig binding: the signing device hash is INSIDE the signed body so an attacker can't swap which device claims authorship without invalidating the signature.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityInviteSigned {
    /// The community being joined.
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    /// The joiner's signed Join event WITHOUT countersig.
    #[serde(rename = "je")]
    pub join_event: SignedMembershipEvent,

    /// The InviteToken from the URL payload — proves the inviter
    /// authorized this redemption.
    #[serde(rename = "it")]
    pub invite_token: InviteToken,

    /// Joiner's full 64-byte identity public bytes (X25519_pub || Ed25519_pub).
    /// Bootstrap-only — receiver doesn't yet have an OwnerDeviceCache entry
    /// for the joiner. Mirrors DmInviteSigned.inviter_identity_pub.
    #[serde(
        rename = "ip",
        serialize_with = "serialize_identity_pub_as_bstr",
        deserialize_with = "deserialize_identity_pub_from_bstr"
    )]
    pub joiner_identity_pub: [u8; 64],

    /// Joiner's DeviceIdentityHash. Receiver verifies hash binds to
    /// `joiner_identity_pub` (defense-in-depth against a buggy sender that
    /// pairs pubs with the wrong device claim).
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,

    /// Wall-clock at packet creation. Used for staleness checks against
    /// `invite_token.expires_at`. The receiver also rejects packets whose
    /// `created_at.wall_ms > now + 60s` (clock-skew guard).
    #[serde(rename = "ca")]
    pub created_at: Hlc,
}
```

### `CommunityInvitePacket` (Path B app-sig wrapper)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommunityInvitePacket {
    Invite {
        signed: CommunityInviteSigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,  // captured at decode for re-verify
    },
}
```

Wire layout: `[u8 discriminant=0x10][CBOR(signed_body)][64 raw signature bytes]`. Reserved namespace: `0x10-0x1F` for community packets, `0x01-0x03` already taken by DM packets, `0x20+` reserved for Sub-D directory packets.

The signature is 64 raw bytes appended after the CBOR body — same pattern as `DmPacket` (NOT a CBOR bstr; encode appends via `extend_from_slice`, decode splits via `split_at(len - 64)`). This lets the receive path capture `signed_bytes` exactly as transmitted, so signature verification operates on bit-exact bytes regardless of encoder drift.

## Send path: `redeem_invite`

Both branches (open + invite-only) share the new ordering. The reorder fixes [ZEB-258](https://linear.app/zeblith/issue/ZEB-258) by deferring the owner-state commit to the last persistent step.

```text
redeem_invite(url):

  1. decode URL → CommunityInvitePayload (rejects malformed URLs)

  2. snapshot NodeState handles in a single guard scope:
       crdt_state, hlc_tracker, device_id, self_owner, self_private_identity,
       community_registry, community_adapter_request_tx, dm_outbox, generation

  3. wall_now_ms = SystemTime::now() since UNIX_EPOCH

  4. RESERVE HLC under tracker lock:
       prev_hlc ← hlc_tracker.lock().get(device_id)
       new_hlc  ← next_hlc(prev_hlc, wall_now_ms, device_id)
       hlc_tracker.lock().insert(device_id, new_hlc.clone())
     Drop guard. The reservation may be wasted if the operation aborts
     later — that's fine because HLC monotonicity only requires
     strictly-increasing, not gap-free.

  5. mint_redemption(payload, self_owner, signing_key, device_id, wall_now_ms,
                     prev_hlc.as_ref()) → MintedCommunity {
         community_id, membership_key, space, bootstrap_join
     }
     (mint_redemption already uses payload.is_invite_only — Phase 3
      groundwork; no edits needed.)

  6. spawn_engine + dispatch adapter:
       community_registry.spawn_engine(community_id, mk, admin_addr,
                                       is_invite_only, pub_tx, sub_rx)
       community_adapter_request_tx.send(AdapterRequest { ... })
     Either failure: tear down (no engine spawned yet → nothing to tear down
     in the spawn-fail case; adapter-fail case calls
     shutdown_engine_and_cleanup_persistence). Owner-state UNCHANGED.

  7. branch on payload.is_invite_only:

  ───── 7a. OPEN (Phase 3 path, just reordered) ─────
       engine.insert_local_event(bootstrap_join).await
         → InsertOutcome::Inserted asserted; debounce kicks the publish.
       (No wait needed; Alice publishes from her own engine.)

  ───── 7b. INVITE-ONLY (NEW for Phase 4) ─────
       a. register a oneshot in community_registry.pending_redemptions
          keyed by bootstrap_join.id (16-byte EventId). The receive-side
          merge loop in handle_incoming_publish fires the matching oneshot
          when an event with that id inserts.

       b. build CommunityInviteSigned {
              community_id,
              join_event: bootstrap_join,
              invite_token: payload.invite_token,
              joiner_identity_pub: <self pubs from outbox>,
              signing_device_hash: <derive from joiner_identity_pub>,
              created_at: new_hlc,
          }
          Sign with self.signing_key over canonical CBOR; encode with
          discriminant 0x10.

       c. resolve inviter's Reticulum dest via existing dm_outbox
          machinery. Inviter == invite_token.signer. Send via
          UnicastSendRequest mpsc.

       d. await the oneshot with timeout T (=15000ms by default,
          override via env var HARMONY_REDEEM_INVITE_TIMEOUT_MS for
          tests):
            on Ok(()):  counter-signed Join landed → continue to step 8.
            on timeout: drop oneshot from map, tear down engine, return
                        Err("invite-only redemption timed out after 15s").

  8. SNAPSHOT-THEN-COMMIT FENCE (ZEB-258 protective):
       reacquire NodeState briefly; if generation != snapshot_generation,
       node was stopped+restarted under us; tear down engine, return Err.

  9. COMMIT owner-state Space (NEW position — atomic-by-construction):
       crdt_state.lock().apply_space_with_canonicalization(space)
       Assert ApplyOutcome::Inserted | Updated; on Rejected, tear down
       engine and return Err.

 10. emit `nav-updated`; return Ok(community_id_hex).
```

**Invariants the new order preserves:**

- **HLC monotonicity** under concurrent IPCs from the same user — tracker advance happens at step 4 (BEFORE any potentially-failing async work). Reserved-but-unused HLCs are harmless.
- **Owner-state atomicity** — owner-state mutation is the LAST persistent step. All async failure paths (engine spawn, packet send, counter-sign timeout) abort BEFORE step 9, leaving owner-state untouched.
- **Engine-without-Space window is ephemeral** (≤ 15s for invite-only, microseconds for open). On abort, `shutdown_engine_and_cleanup_persistence` removes both the engine task and its persistence directory — same end-state as if the IPC had never been called.

## Send path: `create_community`

`create_community` gets the same reorder — engine spawn + adapter dispatch + insert_local_event(creator-Join) FIRST, owner-state Space commit LAST. Step list mirrors `redeem_invite` open path (no Reticulum hop, no oneshot).

The marginal cost is small (Phase 4 is restructuring `redeem_invite` anyway), but the symmetry is load-bearing for the [ZEB-258](https://linear.app/zeblith/issue/ZEB-258) regression test.

## Receive path: `community_invite::handle_unicast`

`event_loop` gets a discriminant-based pre-dispatch at the top of `handle_runtime_action_or_dispatch`:

```text
On RuntimeAction::UnicastReceived { packet, .. }:

  1. peek packet[0]:
       0x01-0x03  → existing dm_outbox.handle_unicast (unchanged)
       0x10       → community_invite::handle_unicast
       else       → drop + tracing::warn!("unknown packet discriminant")

  2. handle_unicast (community_invite) — try_lock pattern + retry_buffer
     (mirrors line 1418-1421 of event_loop's existing DM dispatch):

     a. decode_packet(bytes) → CommunityInvitePacket::Invite { signed,
                                                                signature,
                                                                signed_bytes }
        On DecodeError: drop + log; no degraded-event emit (caller can't
        identify community_id without a successful decode).

     b. verify Path B envelope sig:
          ed25519_dalek::VerifyingKey::from_bytes(signed.joiner_identity_pub[32..64])
              .and_then(|vk| vk.verify_strict(&signed_bytes, &Signature::from_bytes(&signature)))
        On Err → emit degraded(EnvelopeSigInvalid).

     c. verify signing_device_hash binding:
          let derived = SHA256(signed.joiner_identity_pub)[..16];
          if derived != signed.signing_device_hash.0:
              emit degraded(DeviceHashMismatch); return.
        Defense-in-depth — without this, a buggy sender could pair pubs
        with a wrong device claim. Mirrors community_membership::
        verify_signature line 446's identity-binding check.

     d. verify community_id agreement:
          if signed.community_id != signed.join_event.community_id:
              emit degraded(CommunityIdMismatch); return.
          if signed.community_id != signed.invite_token.community_id:
              emit degraded(CommunityIdMismatch); return.

     e. verify Join event sig:
          community_membership::verify_signature(&signed.join_event,
                                                 &signed.joiner_identity_pub)
        Auto-rebinds join_event.actor.0 == derived address_hash.
        On Err → emit degraded(JoinSigInvalid).

     f. verify InviteToken signer is self (v1 single-shot):
          if signed.invite_token.signer != self_owner:
              emit degraded(InviteSignerMismatch{...}); return.
        (When ZEB-251 customizable thresholds + fallback counter-signers
        ship, this becomes an OwnerDeviceCacheResolver lookup.)

     g. verify InviteToken sig with self's identity_pub:
          let token_canon = canonical_cbor_encode(&signed.invite_token.payload());
          self_identity.verifying_key.verify_strict(&token_canon, &Signature::from_bytes(&signed.invite_token.sig))
        On Err → emit degraded(InviteTokenSigInvalid).

     h. verify expiry:
          let now = SystemTime::now() ...;
          if signed.created_at.wall_ms > now + 60_000:
              emit degraded(Expired); return.  // clock-skew guard
          if let Some(exp) = signed.invite_token.expires_at:
              if signed.created_at.wall_ms >= exp:
                  emit degraded(Expired); return.

     i. verify invitee_hint match:
          if let Some(hint) = signed.invite_token.invitee_hint:
              if signed.join_event.actor != hint:
                  emit degraded(InviteeHintMismatch); return.

     j. resolve engine + state for community_id:
          let state_arc = community_registry.state_for(&signed.community_id).await
              .ok_or_else(|| degraded(CommunityUnknown{...}))?;
          let engine = community_registry.engine_arc(&signed.community_id).await
              .ok_or_else(|| degraded(CommunityUnknown{...}))?;

     k. eligibility check — self is currently Joined member with power
        ≥ invite_threshold. Materialize over the engine's events under
        a brief state lock; drop the guard before any subsequent await.
          let (status, power) = {
              let s = state_arc.lock().await;
              let events: Vec<SignedMembershipEvent> = s.events.values().cloned().collect();
              let admin_addr = engine.admin_addr();  // exposed via Phase 3's engine
              drop(s);
              let mat = community_membership::materialize(&events, admin_addr);
              let st = mat.members.get(&self_owner).map(|m| m.status);
              let pw = mat.power_levels.get(&self_owner).copied().unwrap_or(0);
              (st, pw)
          };
          if status != Some(MemberStatus::Joined):
              emit degraded(SelfNotJoined); return.
          if power < POWER_THRESHOLDS.invite:  // = 0 in v1, structural no-op
              emit degraded(SelfPowerInsufficient{...}); return.

  3. ALL CHECKS PASSED → counter-sign:
        let counter_signed = community_membership::attach_countersig_with_identity(
            &signed.join_event,
            &self_private_identity,  // Arc<PrivateIdentity> from dm_outbox
        )?;
        engine.insert_local_event(counter_signed).await
            → InsertOutcome::Inserted asserted; debounce kicks the
              state-root publish. Phase 2's machinery handles the rest.

  4. ANY CHECK FAILED → emit community-state-sync-degraded:
        app.emit("community-state-sync-degraded", json!({
            "communityId": hex::encode(community_id.0),
            "reason": <verify_err.reason_tag()>,
        }));
        Drop the packet. No retry — Reticulum will retransmit from the
        sender if the sender's client decides to.
```

**Lock-ordering note.** Same `try_lock` + `retry_buffer` pattern as the DM dispatch at line 1418-1421. Hold `crdt_state` only for engine resolution, drop before the verify chain. The eligibility check (step 2k) re-locks the per-community engine state, not the global owner-state — no lock-order conflict with concurrent IPCs.

**Idempotence.** Two arrivals of the same packet (Reticulum retransmit) → two counter-sign attempts → second `engine.insert_local_event` returns `InsertOutcome::AlreadyKnown` (CRDT keys events by id; counter-signed Join has the same id). No duplicate publish. Free idempotence from Phase 1's CRDT shape.

## IPC surface (Phase 4 additions)

```rust
/// Already exists; Phase 4 swaps the "Phase 3 supports OPEN only" stub
/// branch for the Reticulum counter-sig hop.
redeem_invite(url: String) -> Result<String>


/// Power-gated. Verified locally before publishing: actor's power must
/// be ≥ kick_threshold (50) AND strictly greater than target's power.
/// Returns Err with the relevant VerifyError discriminant on rejection.
kick_from_community(
    community_id: String,
    target_addr: String,
    reason: Option<String>,
) -> Result<()>


/// Power-gated. Actor's power must be ≥ set_power_threshold (100).
/// Returns Err on PowerLevelOutOfRange (level > 100) or actor power
/// insufficient.
set_power_level(
    community_id: String,
    target_addr: String,
    level: u8,  // 0..=100
) -> Result<()>
```

`kick_from_community` and `set_power_level` are structurally identical and ~40 lines each. Both follow the same pattern:

```text
1. decode hex params
2. snapshot NodeState handles
3. resolve engine via community_registry.engine_arc
4. reserve HLC under tracker lock
5. mint event (Kick or SetPower) signed by self
6. engine.insert_local_event(event) — verify_event runs power-gate locally
7. translate InsertOutcome::Rejected(VerifyError) → user-readable Err
   string; on InsertOutcome::Inserted → Ok
```

**Power gates are enforced by Phase 1's `verify_event`, not by the IPC.** Per the parent spec at line 297-304: actor must be Joined; Kick requires actor power ≥ 50 AND > target.power AND target is a member; SetPower requires actor power ≥ 100 AND level ≤ 100. The IPC trusts these gates and translates `VerifyError` discriminants — pre-validating in the IPC would duplicate the rules and risk drift. Engine's verify is the source of truth.

## ZEB-258 atomic rollback: the reorder

```text
Old order (Phase 3, leaves orphan window):
  1. apply_space_with_canonicalization     ← owner-state mutation lands HERE
  2. hlc_tracker.insert                     ← second persistent mutation
  3. spawn_engine                           ← can fail (FS error)
  4. adapter_tx.send(AdapterRequest)        ← can fail (channel closed)
  5. engine.insert_local_event(Join)        ← can fail (resolver missing)
  Failure between (3-5) leaves a Space + tracker entry with no engine.

New order (Phase 4):
  1. hlc_tracker advance under lock         ← HLC reserved (harmless if aborted)
  2. mint event(s)
  3. spawn_engine                           ← FAILURE → return Err (no rollback needed)
  4. adapter_tx.send(AdapterRequest)        ← FAILURE → shutdown_engine_and_cleanup_persistence
  5. open OR invite-only path produces Join
  6. snapshot-then-spawn fence (generation check)
  7. apply_space_with_canonicalization      ← owner-state mutation lands HERE — LAST
```

**Why HLC advance stays at step 1 (not deferred to step 7):** under concurrent IPCs from the same user, deferred reads would race — IPC A reads `prev_hlc=N`, IPC B reads `prev_hlc=N`, both mint at HLC `N+1`, collision. Advancing under the tracker lock at step 1 reserves the slot. If the IPC aborts later, the reserved HLC is unused — fine because HLC monotonicity only requires strictly-increasing.

### New surface: `shutdown_engine_and_cleanup_persistence`

```rust
impl CommunitySyncRegistry {
    /// Stop the engine task for `community_id` (drops adapter + Zenoh
    /// subscriber), wait for it to drain, and remove its persistence
    /// directory. Used by IPCs that spawn an engine speculatively and
    /// need to roll back on a downstream failure.
    ///
    /// Idempotent on unknown community_id (returns Ok). Caller is
    /// responsible for ensuring no other thread holds an Arc<Engine>
    /// from this registry — typical use is "I just spawned this; no one
    /// else has a handle yet." If a handle has leaked elsewhere, those
    /// holders see TransportClosed once teardown completes.
    pub async fn shutdown_engine_and_cleanup_persistence(
        &self,
        community_id: &SpaceId,
    ) -> Result<(), CommunitySyncError>;
}
```

Implementation: existing per-engine shutdown to stop the task, await join handle, then `tokio::fs::remove_dir_all` on the per-community persistence path. ~30 lines.

## Pending-redemption oneshot wiring

`CommunitySyncRegistry` gains a new field:

```rust
pending_redemptions: Arc<Mutex<HashMap<EventId, oneshot::Sender<()>>>>
```

The receive-side merge loop in `handle_incoming_publish` (Phase B, after a successful insert) checks each inserted event id against this map. If a match exists, the oneshot fires and the entry is removed.

The IPC registers a oneshot at step 7b.a, awaits it with `tokio::time::timeout`, removes the entry on timeout (drop semantics — the receiver side polls the map under lock).

**Why not the existing `community-members-changed` Tauri delta channel?** That channel is for FRONTEND consumption — push notifications to the UI. Adding an internal IPC consumer to the same channel mixes concerns and forces the IPC to subscribe to a Tauri event from inside another Tauri command. The registry-internal map is a single Arc<Mutex<HashMap>> + an event-id check in the receive loop — much simpler.

## Error taxonomy

### `CommunityInviteVerifyError` (new, in `community_invite.rs`)

Receive-side rejection variants. Each maps to a `community-state-sync-degraded` reason tag for the frontend banner.

| Variant | Reason tag | Trigger |
|---|---|---|
| `EnvelopeSigInvalid` | `community_invite_envelope_sig_invalid` | Path B envelope sig didn't validate |
| `DeviceHashMismatch` | `community_invite_device_hash_mismatch` | `signing_device_hash` ≠ SHA256(joiner_identity_pub)[:16] |
| `JoinSigInvalid` | `community_invite_join_sig_invalid` | Inner Join event sig failed |
| `InviteTokenSigInvalid` | `community_invite_token_sig_invalid` | InviteToken sig failed |
| `InviteSignerMismatch{signer, self_owner}` | `community_invite_signer_mismatch` | InviteToken signer ≠ self (v1 only counter-signs invites we issued) |
| `CommunityIdMismatch` | `community_invite_id_mismatch` | community_id disagreement across envelope, Join, token |
| `Expired` | `community_invite_expired` | created_at ≥ expires_at, OR created_at > now + 60s |
| `InviteeHintMismatch` | `community_invitee_hint_mismatch` | invite_token.invitee_hint ≠ join_event.actor |
| `CommunityUnknown{community_id}` | `community_invite_unknown` | No engine for this community — packet was misrouted |
| `SelfNotJoined` | `community_invite_self_not_joined` | Self isn't currently a Joined member |
| `SelfPowerInsufficient{self_power, threshold}` | `community_invite_self_power_insufficient` | Self power < invite_threshold (= 0 in v1, structural no-op today) |

### IPC-layer errors

Stay as `Result<_, String>` — Phase 3's pattern. IPC wraps engine/verify errors into user-readable strings. For redeem timeout specifically: `"invite-only redemption timed out after 15s"` so the frontend can substring-match on it if needed.

## Test surface (~13 new tests)

### `tests/community_invite_unit.rs` (new file)

- `community_invite_signed_canonical_roundtrip` — encode → decode → struct equality
- `community_invite_signed_envelope_sig_verifies_on_valid_input`
- `community_invite_envelope_sig_rejected_on_tampered_body` — flip a byte, expect `EnvelopeSigInvalid`
- `community_invite_device_hash_mismatch_rejected`
- `community_invite_token_signer_mismatch_rejected`
- `community_invite_expired_rejected`
- `community_invitee_hint_mismatch_rejected`
- `community_invite_id_mismatch_rejected`

### `tests/community_membership_unit.rs` (extend)

- `kick_self_rejected_with_kick_target_power_not_lower` — admin kicking self
- `set_power_out_of_range_rejected` — level=200
- `set_power_admin_self_demote_inserts` — admin → 50 (foot-gun, but allowed)

### `tests/wire_format_community_fixtures.rs` (extend)

- `community_invite_signed_wire_bytes_pinned` — canonical CBOR pinned to known bytes; regenerate the day this changes

### `tests/community_invite_only_integration.rs` (new file, mirrors `community_open_flow_integration.rs`)

- `alice_redeems_invite_only_against_bob_admin` — full happy path: Bob creates invite-only community, generates invite, Alice redeems; counter-signed Join lands in both engines within timeout
- `redeem_invite_only_times_out_when_inviter_offline` — Bob's node is up but Reticulum delivery suppressed; Alice's `redeem_invite` returns Err within timeout; engine torn down; owner-state unchanged
- `redeem_invite_only_rolls_back_on_engine_spawn_failure` — ZEB-258 acceptance test; uses a test-only registry double whose `spawn_engine` returns Err; assert owner-state CRDT byte-identical pre/post

### `tests/community_sync_registry_unit.rs` (extend)

- `shutdown_engine_and_cleanup_persistence_idempotent_on_unknown_id`
- `shutdown_engine_and_cleanup_persistence_removes_dir_after_engine_stops`

### Test infrastructure

- **Env-var override for the timeout** — `HARMONY_REDEEM_INVITE_TIMEOUT_MS`, default 15000. Integration tests set this to ~500ms via `std::env::set_var` so the timeout test runs fast.
- **Reticulum send/receive double** — already exists for DM tests; reuse the same pattern (paired `mpsc::channel`s + a forwarder task).
- **Two-node fixture** — pattern lifted from `community_open_flow_integration.rs`. Two `NodeState` instances, separate `tempfile::tempdir()` persistence dirs, shared CAS via `spawn_shared_cas()`, manual Reticulum forwarder between the two.

### Test gates

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. CI workflow already enforces all three.

## Acceptance criteria

- `redeem_invite(invite_only_url)` no longer returns `Err("Phase 3 supports OPEN ...")` — it dispatches to the counter-sig hop, waits ≤ 15s, returns `Ok(SpaceId)` on counter-sign success or `Err` on timeout / inviter offline / verification failure.
- `kick_from_community` mints a verified Kick event, publishes via the state-root topic, returns Ok; rejects with the right `VerifyError` discriminant on power-gate violations.
- `set_power_level` analogous for SetPower events.
- Inbound `CommunityInvite` Reticulum packet (discriminant `0x10`) → `event_loop` routes to `community_invite::handle_unicast` → auto-verifies + counter-signs + publishes; verification failures surface a `community-state-sync-degraded` event with the reason tag.
- ZEB-258 regression: simulate engine-spawn failure during `create_community` → owner-state CRDT byte-identical to pre-call snapshot.
- Wire-format fixture: `CommunityInviteSigned` pinned in `wire_format_community_fixtures.rs`.
- Two-engine integration test: Alice (open node) redeems an invite-only invite for community admined by Bob (separate node) → counter-signed Join lands in Alice's CRDT within timeout.
- All existing tests still green; new gates: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`.

## Out of scope (deferred to follow-up tickets)

- **User-prompt UX for inbound CommunityInvite** — auto-counter-sign in v1 (matches ZEB-228 DmInvite Phase 3b; user-prompt UX tracked separately, mirrors ZEB-236 pattern).
- **Persistent offline-counter-signer queue** — [ZEB-254](https://linear.app/zeblith/issue/ZEB-254). Currently inviter-offline → Err → user retries.
- **Fallback counter-signers** — v1 ships single-shot inviter-only. Multi-counter-signer fan-out + fallback ordering tracked separately when ZEB-251 customizable thresholds creates real demand.
- **TreeKEM-style MembershipKey rotation on kick** — [ZEB-249](https://linear.app/zeblith/issue/ZEB-249). Phase 4 ships kick under the assumption that kicked members retain MK temporarily; the publisher-auth gate (ZEB-256) defends against the resulting censorship attack.
- **Per-community power-threshold customization** — [ZEB-251](https://linear.app/zeblith/issue/ZEB-251). v1 hardcodes `invite=0`, `kick=50`, `set_power=100`.
- **Phase 5 admin UI** — `CommunitySettingsPanel`, `MemberRow`, `InviteRedeemDialog`, etc. Backend-first cadence; UI ships in a separate PR.
- **First-Join + self-Re-Join admission via blob-event inspection** — [ZEB-260](https://linear.app/zeblith/issue/ZEB-260). Phase 4 invite-only flow naturally avoids this gap because the inviter (already Joined) publishes the counter-signed Join, not the joiner.
- **Membership-gate cache optimization** — [ZEB-261](https://linear.app/zeblith/issue/ZEB-261). v1 re-materializes per receive; cache is premature.

## References

- Parent spec: `docs/specs/2026-05-05-zeb-217-sub-c-communities-design.md`
- Phase 3 reference: PR #87 / commit `bc0facd` — `create_community`, `redeem_invite` open path, `leave_community`, `generate_invite`, `list_community_members`
- DmInvite reference (Path B app-sig binding): PR #80 — `dm_envelope`, `dm_outbox` `UnicastSendRequest` machinery
- [ZEB-256](https://linear.app/zeblith/issue/ZEB-256) (PR #88): publisher authentication — Phase 4's published events all flow through the same auth gate
- [ZEB-258](https://linear.app/zeblith/issue/ZEB-258): atomic rollback (closes when this PR merges)
- Linear ticket: [ZEB-262](https://linear.app/zeblith/issue/ZEB-262)
