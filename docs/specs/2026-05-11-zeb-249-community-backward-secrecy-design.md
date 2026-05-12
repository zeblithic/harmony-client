# ZEB-249: Community backward secrecy via Epoch Key rotation

**Ticket:** [ZEB-249](https://linear.app/zeblith/issue/ZEB-249/)
**Status:** Design
**Author:** zeblith
**Created:** 2026-05-11
**Parent:** ZEB-217 (Sub-C v1 communities) — Done
**Replaces:** v1's long-lived `MembershipKey` distribution

## 1. Context

Sub-C v1 (ZEB-217) shipped community CRDT replication with a single long-lived `MembershipKey` per community, distributed via `CommunityInvitePayload.membership_key` and stored in `Space.membership_key`. The key is used for both wire encryption (random nonce) and at-rest blob encryption (deterministic nonce) on the community's membership topic.

The v1 design has an explicit privacy non-goal: **kicked or departed members keep the `MembershipKey`** and can decrypt every event published on the membership topic after their removal, indefinitely. This includes future Join/Kick/SetPower events, future channel-config changes, and (when channel content also rides this topic in future phases) future channel messages.

ZEB-249 closes this gap by introducing **per-epoch keys** that rotate on each Kick or Leave event. After rotation, the removed member no longer possesses the current key and cannot decrypt new events.

Harmony is pre-launch — no v1 communities exist in production. v1's long-lived key is replaced entirely; there is no parallel v1/v2 codepath.

## 2. Architecture

**Approach:** Epoch Key rotation. A single 32-byte ChaCha20-Poly1305 key (`EpochKey`) is current at any moment in a community's life. The key rotates on each `Kick` or `Leave` event. Each rotation is a CRDT event (`MembershipEventKind::EpochRotation`) whose payload includes a fresh `EpochKey` X25519-sealed to every remaining member's identity pubkey.

**Why not Sender Keys or TreeKEM:** Concrete scale analysis at the target community size (≤20k members) showed Sender Keys would burst ~4.8 GB of network traffic per kick (each active sender re-encrypts to all remaining members) for the same backward-secrecy guarantee that a single Epoch Key rotation delivers at ~800 KB. TreeKEM-style approaches close the post-kick window faster (autonomous per-leaf ratchets) but require maintaining a binary tree synchronized via CRDT, with subtle blanking and concurrent-update semantics. Epoch Key is the simplest scheme that satisfies the ticket's backward-secrecy goal within Harmony's scale and design values (correctness simplicity, no heavy dependencies, CRDT-native).

**Why not forward secrecy:** Out of scope per design decision. Forward secrecy (compromise of current keys cannot decrypt past messages) requires per-message ratcheting + irreversible key deletion, which conflicts with Harmony's offline + multi-device sync model. The encrypted-envelope wire format reserves a `ratchet_generation: Option<u64>` field that a future version could populate without a wire break.

**Threat model:** Backward secrecy only — a member kicked at HLC `T_k` cannot decrypt events with HLC `> T_k + window`, where `window` is the propagation latency of the rotation event in CRDT (typically sub-second; bounded by the self-healing path in pathological cases). See §10 for the post-kick vulnerability window discussion.

## 3. Data model & event shape

### 3.1 EpochKey rename

The v1 `MembershipKey` type is **renamed to `EpochKey`** in `owner_state_types.rs`. The underlying primitive is unchanged: 32-byte ChaCha20-Poly1305 key, ZeroizeOnDrop, redacted Debug, bstr CBOR serde. Only the name changes; this is a mechanical rename across the codebase.

```rust
pub struct EpochKey(
    #[serde(serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr")]
    [u8; 32],
);
```

### 3.2 Space struct changes

In `owner_state_types.rs`:

```rust
pub struct Space {
    // existing fields unchanged...

    // REMOVED: pub membership_key: Option<MembershipKey>,

    // NEW (Community kind only):
    pub current_epoch: Option<u64>,                // 0 at creation; increments on rotation
    pub current_epoch_key: Option<EpochKey>,       // active key for new outbound events
    pub old_epoch_keys: BTreeMap<u64, EpochKey>,   // historical keys for decrypting old events
}
```

Invariants enforced by `Space::validate`:

* Community: `current_epoch.is_some() && current_epoch_key.is_some()`. `old_epoch_keys` may be empty (epoch 0 → no history).
* Dm / GroupDm: all three fields are `None`.

### 3.3 New MembershipEventKind variants

In `community_membership.rs`. Two new variants, with related but distinct semantics:

```rust
/// Advances `current_epoch`. Triggered by Kick/Leave (subtractive —
/// excludes the kicked/leaving member from recipient_ciphertexts).
#[serde(rename = "r")]
EpochRotation {
    #[serde(rename = "pe")]
    prior_epoch: u64,                              // staleness gate (see §4.2)

    #[serde(rename = "ts")]
    triggered_by: EventId,                          // the Kick/Leave event that motivated this rotation

    #[serde(rename = "rc")]
    recipient_ciphertexts: Vec<(OwnerAddr, Vec<u8>)>, // per-recipient X25519-sealed new EpochKey
},

/// Delivers `current_epoch_key` to specified members without advancing
/// the epoch. Triggered by a Join whose snapshot was stale at
/// redemption time (see §4.6). Remedies the gap where new members
/// would otherwise be unable to decrypt events at the current epoch.
#[serde(rename = "f")]
EpochCatchup {
    #[serde(rename = "ep")]
    epoch: u64,                                     // the epoch whose key is being delivered
                                                    // (typically the rotator's current_epoch)

    #[serde(rename = "ts")]
    triggered_by: EventId,                          // the Join event that motivated this catchup

    #[serde(rename = "rc")]
    recipient_ciphertexts: Vec<(OwnerAddr, Vec<u8>)>, // per-recipient X25519-sealed EpochKey(epoch)
},
```

The 2-char field-key constraint (CBOR same-length-keys invariant at this nesting level) is honored: `pe`/`ep`, `ts`, `rc`.

`recipient_ciphertexts` is a `Vec<(OwnerAddr, Vec<u8>)>` not a `BTreeMap` because the wire CBOR is more compact as an array of pairs (avoids the BTreeMap → array-of-arrays serde round-trip overhead). The materialization code looks up `self.owner_addr` via linear scan; at ≤20k recipients per rotation/catchup this is microseconds.

**Why two variants instead of one with an `advances: bool` flag:** Different validation rules apply (rotation requires subtractive recipient list; catchup requires additive). Separate variants surface the distinction in the type system + wire format, making mis-use detectable at compile/parse time rather than at materialization.

### 3.4 Wire format for content events

The bytes that today are produced by `chacha20poly1305_encrypt(MembershipKey, random_nonce, plaintext)` become:

```rust
struct EncryptedEnvelope {
    epoch: u64,
    nonce: [u8; 12],          // ChaCha20-Poly1305 random nonce
    ciphertext: Vec<u8>,      // AEAD output (includes 16-byte tag)
    ratchet_generation: Option<u64>, // RESERVED, always None in v2 (see §9)
}
```

Wire CBOR field-keys: `ep`, `nc`, `ct`, `rg` (2-char, same-length).

Receivers select the decryption key:

```rust
let k = if envelope.epoch == space.current_epoch.unwrap() {
    space.current_epoch_key.as_ref()
} else {
    space.old_epoch_keys.get(&envelope.epoch)
};
```

Missing key → `KeyNotAvailable(epoch)` (see §6.2). Successful decryption is the normal path.

## 4. Rotation protocol

### 4.1 Atomic kick+rotation bundle

A `Kick` event MUST be paired with an `EpochRotation` in the same signed CRDT submission. The IPC handler `admin_kick_member`:

1. Open `community_sync_tx` (ZEB-274's primitive — gives us atomicity for membership-side changes)
2. Read current state (members, current_epoch, current_epoch_key)
3. Generate fresh `K_next` via `OsRng`
4. For each member `M` except the kick target:
   `encrypted[M] = X25519-seal(K_next, M.identity_pubkey)`
5. Build event bundle:
   * `Kick { target, reason }`
   * `EpochRotation { prior_epoch: current_epoch, triggered_by: kick.event_id, recipient_ciphertexts: encrypted }`
6. Sign both events with admin's signing key, same HLC tick (`tick.now()` once, both events use it)
7. Commit transaction → both events land in CRDT or neither does

The same pattern applies to `Leave` issued voluntarily by a member: the leaver builds the bundle, but the rotation MUST exclude the leaver's own address (validated by recipients per §4.4).

### 4.2 Staleness gate via `prior_epoch`

When materializing an `EpochRotation`, the receiver checks `prior_epoch == state.current_epoch`. If false, the rotation is **silently dropped** (no-op, no error). This handles concurrent kicks: if two admins kick concurrently and only one rotation can advance the epoch counter, the loser's rotation has a stale `prior_epoch` and is dropped.

The rotation's `triggered_by` field identifies which `Kick` or `Leave` event the rotation was generated for. This is used by the self-healing path (§4.3) to attribute pending rotations.

### 4.3 Self-healing: detecting kick-without-rotation

The materializer tracks `pending_rotation_for: BTreeSet<OwnerAddr>` (members whose exit hasn't been followed by a successful rotation yet).

Materialization rules:

```rust
match event {
    Kick { target } | Leave { } => {
        state.members.remove(target);
        state.pending_rotation_for.insert(target);
    }
    EpochRotation { prior_epoch, triggered_by, recipient_ciphertexts } => {
        if prior_epoch != state.current_epoch { return; }     // stale, drop
        let target = state.event_index[triggered_by].target;
        if recipient_ciphertexts contains target { return; }  // malformed, drop
        // validity: issuer must have admin power OR be the target themselves
        if !is_valid_issuer(event.signer, target, state) { return; }

        state.old_epoch_keys.insert(state.current_epoch, state.current_epoch_key);
        state.current_epoch_key = decrypt_my_ciphertext(recipient_ciphertexts, my_identity_privkey);
        state.current_epoch += 1;
        state.pending_rotation_for.remove(target);
    }
    ...
}
```

After every CRDT-apply cycle, the IPC layer checks `state.pending_rotation_for`. If non-empty AND the local user has admin power, the client synthesizes and posts a fresh `EpochRotation` for each pending target. First-admin-wins via CRDT HLC linearization; losers' rotations are dropped by the staleness gate.

### 4.4 Leaver-issued rotation validity

Two-rule validation for `EpochRotation.signer`:

1. **Admin path**: `signer` has admin-tier power (per current `state.power_levels`) → trust the rotation
2. **Leaver path**: `signer` is the target of the `triggered_by` Leave event AND `recipient_ciphertexts` does NOT include `signer.owner_addr` → trust the rotation

Otherwise the rotation is dropped (treated as if it didn't exist). The self-healing path then issues a correct rotation when an admin next materializes.

A cooperative leaver who bundles a correct rotation closes the vulnerability window in ~CRDT-propagation time. A malicious leaver who self-includes (trying to retain access) is detected and ignored; the window grows until an admin acts. See §10 for the bounded window discussion.

### 4.5 Concurrent kicks

A1 kicks Alice with bundle `(Kick{Alice}, EpochRotation{prior_epoch=5, target=Alice})`. A2 concurrently kicks Bob with bundle `(Kick{Bob}, EpochRotation{prior_epoch=5, target=Bob})`.

CRDT linearizes via HLC. Assume A1's HLC < A2's HLC:

1. A1's `Kick(Alice)` materializes → Alice removed, pending_rotation_for += Alice
2. A1's `EpochRotation` materializes → prior_epoch=5 matches; epoch advances to 6; pending_rotation_for -= Alice
3. A2's `Kick(Bob)` materializes → Bob removed, pending_rotation_for += Bob
4. A2's `EpochRotation` materializes → prior_epoch=5 != current=6 → DROPPED (stale)
5. Self-healing path: at next CRDT-apply, any admin sees pending_rotation_for = {Bob}, synthesizes a fresh rotation with prior_epoch=6

Final state: epoch 7, members exclude both Alice and Bob. Window: between (3) and the self-healing rotation landing, Bob still has K(6) and can decrypt new events.

### 4.6 Stale-invite catchup (Join with snapshot_epoch < current_epoch)

A kick can land between invite issuance and invite redemption. When the new member's Bootstrap-Join materializes, their local state is at `snapshot_epoch`, but CRDT's `current_epoch` may be ahead.

Without remediation, the new member can decode events at `snapshot_epoch` (they have `K(snapshot_epoch)` from the sealed invite) but events at `current_epoch` are encrypted under `K(current_epoch)` which they lack. Result: new member can't decrypt any new messages until the next Kick/Leave triggers a rotation that includes them.

Remediation via `EpochCatchup`:

**Materializer state**: `pending_catchup_for: BTreeSet<OwnerAddr>` tracks new members at stale epochs.

```rust
match event {
    BootstrapJoin { new_member, snapshot_epoch } => {
        state.members.insert(new_member, ...);
        if snapshot_epoch < state.current_epoch {
            state.pending_catchup_for.insert(new_member);
        }
    }
    EpochCatchup { epoch, triggered_by, recipient_ciphertexts } => {
        // Validate: epoch must equal current_epoch (no historical catchups)
        if epoch != state.current_epoch { return; }
        // Validate: triggered_by must reference a Join event whose target
        // is in recipient_ciphertexts
        let join_event = state.event_index[triggered_by];
        let target = match join_event.kind {
            BootstrapJoin { new_member, .. } => new_member,
            _ => return,  // catchup must reference a Join
        };
        if !recipient_ciphertexts.contains_key(&target) { return; }
        // Validate: issuer must have admin power (catchups are admin-only —
        // no cooperative-joiner path because new members cannot generate
        // a trustworthy key for themselves; see §4.4 weak-K concern)
        if !is_admin(event.signer, state) { return; }

        // For each recipient in this catchup, deliver the key.
        if let Some(my_ct) = recipient_ciphertexts.get(my_addr) {
            state.current_epoch_key = decrypt(my_ct, my_identity_privkey);
            // current_epoch unchanged (catchup is non-advancing)
        }
        for target_addr in recipient_ciphertexts.keys() {
            state.pending_catchup_for.remove(target_addr);
        }
    }
    ...
}
```

**Self-healing**: after CRDT-apply, IPC layer checks `pending_catchup_for`. If non-empty AND local user has admin power, synthesize and post an `EpochCatchup` for each pending new member, sealing `current_epoch_key` to their identity pubkey.

`EpochCatchup` does NOT advance `current_epoch`. Multiple catchups can land at the same epoch without conflict — they just deliver the existing key to additional members. Stale catchups (where the named `epoch` is no longer current) are dropped.

Window of vulnerability: same shape as the post-kick window — new member can't decrypt current-epoch content until catchup lands. Bounded by admin availability. New member's UI surfaces "establishing access" state during this window.

## 5. Invite bootstrap & lazy catchup

### 5.1 New CommunityInvitePayload shape

In `community_invite.rs`:

```rust
pub struct CommunityInvitePayload {
    // existing: community_id, admin_addr, community_name, is_invite_only,
    //          expires_at, invite_token, admin_bootstrap, created_at

    // REMOVED: pub membership_key: MembershipKey,

    // NEW: snapshot bound to invitee
    pub epoch_snapshot: InviteEpochSnapshot,
}

pub struct InviteEpochSnapshot {
    /// The epoch the invitee will join at. Materialized state below is
    /// frozen at this epoch.
    pub epoch: u64,

    /// Current EpochKey at issuance, X25519-sealed to invitee's
    /// identity pubkey. On redemption, invitee decrypts → can decrypt
    /// all events with HLC > invite issuance.
    pub sealed_epoch_key: Vec<u8>,

    /// Frozen materialized state at invite issuance (members, channels,
    /// power levels). Used as a UI bootstrap hint; CRDT replay is the
    /// authoritative source post-redemption.
    pub state_snapshot: MaterializedCommunityState,
}

pub struct MaterializedCommunityState {
    pub members: BTreeMap<OwnerAddr, MemberState>,
    pub channels: BTreeMap<ChannelId, ChannelInfo>,
    pub power_levels: BTreeMap<OwnerAddr, u8>,
}
```

CBOR field-keys: `ep`, `sk`, `ss` for `InviteEpochSnapshot`; existing keys for nested types.

### 5.2 Bootstrap on join (redeem_invite_inner)

The existing `redeem_invite_inner` IPC handler already wires CommunityInvitePayload → Space + materialized state. The diff is:

* `CommunityInvitePayload.membership_key` → `epoch_snapshot.sealed_epoch_key` (decrypted via invitee's identity privkey)
* New Space fields populated: `current_epoch = epoch_snapshot.epoch`, `current_epoch_key = decrypted_key`, `old_epoch_keys = BTreeMap::new()`
* Materialized state populated from `epoch_snapshot.state_snapshot` (UI bootstrap hint)
* Bootstrap-Join CRDT event posted under the new `current_epoch_key`

The snapshot is **not** signed by every member individually. It is signed by the inviter (via the existing `CommunityInviteSigned` envelope). If a malicious inviter ships a tampered snapshot, the invitee's local state diverges from CRDT reality until CRDT replay corrects it (~ one round trip). The snapshot is a fast-path hint, not a source of truth.

### 5.3 Lazy catchup

Member M offline through N rotations returns online:

1. M pulls CRDT events since `M.last_seen_hlc`
2. For each `EpochRotation` where M is in `recipient_ciphertexts`:
   * Decrypt M's sealed ciphertext via M's identity privkey
   * Recover `K_new` for that epoch
   * Insert into `old_epoch_keys` or update `current_epoch_key` (whichever is later)
3. Apply all content events from catchup window in HLC order using the recovered keys

Catchup cost: O(rotations) X25519 decryptions + O(events) ChaCha20-Poly1305 decryptions. At ~30 µs and ~1 µs per operation respectively, 1000 rotations + 10k events ≈ 40 ms total.

M kicked while offline: M has all keys up to kick HLC. Events past kick HLC fail to decrypt (`KeyNotAvailable`). UI surfaces "you no longer have access" and stops materializing past the kick HLC.

### 5.4 Multi-device sync of epoch state

Space (`current_epoch`, `current_epoch_key`, `old_epoch_keys`) lives on the owner-state CRDT (ZEB-209 Flow A). M's bound devices auto-sync the full key state. When M unpairs+repairs a device, the new device receives the full Space state from M's other bound devices — no fresh community invite needed.

## 6. CRDT / lock integration

### 6.1 Transactional submission

The kick+rotation bundle and the leave+rotation bundle both use ZEB-274's `community_sync_tx`. No new locking primitive is introduced. The transaction:

* Acquires `CommunitySyncRegistry.engines[community_id].lock()`
* Stages the event bundle
* On commit: writes to CRDT + advances local materialized state atomically
* On abort/drop: rolls back via the RAII guard (ZEB-274)

### 6.2 Error model

A new `EpochError` enum surfaces decryption-path failures:

```rust
#[derive(Debug, thiserror::Error)]
pub enum EpochError {
    #[error("key for epoch {0} not available locally")]
    KeyNotAvailable(u64),

    #[error("AEAD tag mismatch on event at epoch {0}")]
    DecryptionFailed(u64),

    #[error("rotation references stale prior_epoch {provided}, current is {current}")]
    StaleRotation { provided: u64, current: u64 },

    #[error("malformed rotation: target {target:?} included in recipient_ciphertexts")]
    MalformedRotation { target: OwnerAddr },

    #[error("rotation issuer {issuer:?} lacks authority (not admin and not target)")]
    InvalidIssuer { issuer: OwnerAddr },
}
```

`KeyNotAvailable` is classified by the caller into one of four legitimate cases:

1. **New member, `epoch < join_epoch`** — expected; UI shows "joined at epoch N, history before that is unavailable"
2. **Kicked member, `epoch > kick_epoch`** — expected on kicked-member's local replica; UI shows "you no longer have access"
3. **New member at stale snapshot awaiting catchup, `epoch == current_epoch` and `my_addr ∈ pending_catchup_for`** — expected transient state; UI shows "establishing access" until `EpochCatchup` lands
4. **Anything else** — a bug; log loudly + surface as application error

Cases 1 and 2 are tracked via `state.members[my_addr].join_epoch` and `state.members[my_addr].kick_epoch` (the latter set when self-kick lands in CRDT). The classification is local to each member's view.

`DecryptionFailed`, `StaleRotation`, `MalformedRotation`, and `InvalidIssuer` are silently dropped during materialization (logged at `debug` level). The materializer is conservative — events that fail any check are treated as if they didn't exist.

### 6.3 IPC surface changes

* `admin_kick_member(community_id, target_addr, reason)` — existing IPC; internal change to bundle a rotation. No frontend-visible API change.
* `leave_community(community_id)` — existing IPC; internal change to bundle a leaver-rotation excluding self.
* `decrypt_event(epoch, envelope)` — new internal helper, not exposed as IPC.

### 6.4 Files touched

* `src-tauri/src/owner_state_types.rs`: rename `MembershipKey` → `EpochKey`; add `current_epoch`, `current_epoch_key`, `old_epoch_keys` fields on `Space`; remove `membership_key`; update validation
* `src-tauri/src/community_membership.rs`: add `EpochRotation` + `EpochCatchup` variants + `pending_rotation_for` + `pending_catchup_for` tracking + materialization rules
* `src-tauri/src/community_state_sync.rs`: `encrypt_for_topic` / `decrypt_for_topic` become epoch-aware; reuse `community_sync_tx` for atomic bundles
* `src-tauri/src/community_invite.rs`: replace `membership_key` with `epoch_snapshot` in `CommunityInvitePayload`
* `src-tauri/src/lib.rs`: `admin_kick_member`, `leave_community`, `redeem_invite_inner`, `create_community_inner` IPC handlers
* `src-tauri/src/event_loop.rs`: self-healing rotation + catchup synthesis on CRDT apply
* `src-tauri/tests/community_backward_secrecy_integration.rs`: NEW — end-to-end tests
* `src-tauri/tests/wire_format_community_sync_fixtures.rs`: extended with new fixture tests

### 6.5 Testing strategy

**Unit tests** (in `community_membership.rs`'s `tests` module):

1. `epoch_rotation_advances_current_epoch` — apply a rotation, verify `current_epoch` and `current_epoch_key` advance, prior key moves to `old_epoch_keys`
2. `stale_rotation_dropped` — rotation with `prior_epoch != current_epoch` is no-op (state unchanged)
3. `malformed_rotation_dropped` — rotation including the kicked member is no-op
4. `concurrent_kicks_self_heal` — two concurrent kicks; only one rotation lands; admin self-heals the second; final state has both kicks applied and one final rotation
5. `leaver_issued_rotation_accepted_when_well_formed` — Leave event paired with leaver-signed rotation that excludes leaver → rotation applied
6. `leaver_issued_rotation_rejected_when_self_included` — Leave + leaver-signed rotation that INCLUDES leaver → rotation dropped; pending_rotation_for retains the leaver
7. `pending_rotation_tracking_clears_after_matching_rotation_lands` — kick + rotation → pending_rotation_for is empty
8. `kick_then_rotation_in_same_hlc_tick_materializes_atomically` — both events land in one CRDT submission; intermediate state never observable
9. `stale_invite_join_marks_pending_catchup_for` — Bootstrap-Join with `snapshot_epoch < current_epoch` enters `pending_catchup_for`
10. `epoch_catchup_delivers_current_key_without_advancing_epoch` — catchup applied; `current_epoch` unchanged; new member's `current_epoch_key` populated
11. `epoch_catchup_with_stale_epoch_dropped` — catchup referencing `epoch != current_epoch` is no-op
12. `epoch_catchup_referencing_non_join_event_dropped` — catchup whose `triggered_by` is a Kick is rejected (catchups must reference Joins)
13. `non_admin_issued_catchup_dropped` — only admins can issue catchups (no cooperative-joiner path; new members can't generate trustworthy keys for themselves per §4.4)
14. `epoch_catchup_for_already_caught_up_member_dropped` — idempotent (no harm if a stale catchup arrives after another already healed the gap)

**Integration tests** (new `tests/community_backward_secrecy_integration.rs`):

1. **Two-node kick-then-cannot-decrypt** — A creates community, invites B, kicks B; verify B's local replica returns `KeyNotAvailable` for events with HLC > kick HLC
2. **Three-node selective access** — A invites B and C; A kicks B; verify C still decrypts new events under the new epoch key
3. **Offline catchup** — B offline through 3 rotations; B comes back online, replays CRDT, successfully decrypts current-epoch content
4. **Invite bootstrap at current epoch** — D joins via invite with `snapshot_epoch == current_epoch`; D immediately decrypts new events (no catchup needed)
5. **Stale-invite catchup** — D joins via invite with `snapshot_epoch == 0` while CRDT current is at epoch=3; verify D's UI surfaces "establishing access" until an admin issues `EpochCatchup`; then D decrypts new events
6. **Concurrent kicks end-to-end** — A1 and A2 simultaneously kick X and Y; verify final CRDT state has both kicked, exactly one rotation per kick (self-heal landed), neither X nor Y can decrypt post-kick events
7. **Leaver cooperatively rotates** — B issues Leave bundled with a well-formed leaver-signed rotation; verify rotation applied without admin intervention; B can no longer decrypt
8. **Leaver malicious rotation rejected** — B issues Leave with rotation including B's own address; verify rotation dropped; admin self-heals; B can no longer decrypt

**Wire-format pinning fixtures** (extend `tests/wire_format_community_sync_fixtures.rs`):

1. `epoch_rotation_event_wire_bytes_pinned` — canonical CBOR for `EpochRotation` with 3 recipients
2. `epoch_catchup_event_wire_bytes_pinned` — canonical CBOR for `EpochCatchup` with 1 recipient
3. `encrypted_envelope_wire_bytes_pinned` — envelope with `rg = null`
4. `encrypted_envelope_with_ratchet_generation_pinned` — envelope with `rg = Some(0)` (v3 forward-compat smoke test)
5. `invite_payload_with_epoch_snapshot_wire_bytes_pinned` — full CommunityInvitePayload with snapshot

`★ Insight ─────────────────────────────────────`
The two load-bearing correctness tests are `concurrent_kicks_self_heal` (integration test #6) and `stale_invite_catchup` (integration test #5). These two exercise every state-machine transition in the rotation + catchup protocol; passing both is strong evidence that the protocol's subtlest invariants are locked in. Worth writing first in the implementation plan.
`─────────────────────────────────────────────────`

## 7. Wire format details

### 7.1 EpochRotation event (CBOR)

```text
{
  "tg": "r",
  "vl": {
    "pe": 5,                              // prior_epoch
    "ts": h'01020304...',                  // 16-byte EventId
    "rc": [                               // recipient_ciphertexts
      [h'aabbcc...', h'sealed...'],       // (OwnerAddr, X25519-sealed bytes)
      ...
    ]
  }
}
```

Sealed-key payload format (per recipient): X25519 ephemeral keypair generation + HKDF + ChaCha20-Poly1305 encryption. Reuses the `dm_signing` X25519+ChaCha20-Poly1305 hybrid envelope already in the codebase. Output is 32 bytes ephemeral pubkey + 12 bytes nonce + 32 bytes ciphertext + 16 bytes tag = 92 bytes per recipient.

At N=10k members: 92 × 9999 ≈ 920 KB per rotation event. At N=20k: 92 × 19999 ≈ 1.84 MB. Bandwidth budget confirmed acceptable per §2 analysis.

### 7.2 EncryptedEnvelope (CBOR)

```text
{
  "ep": 5,                                // epoch
  "nc": h'010203040506070809abcdef',      // 12-byte nonce
  "ct": h'...',                           // ciphertext + tag
  "rg": null                              // ratchet_generation (always null in v2)
}
```

`rg` is reserved for a future forward-secrecy extension. v2 readers MUST tolerate `rg` being present-but-null. v3 readers can populate it.

### 7.3 InviteEpochSnapshot in CommunityInvitePayload (CBOR)

```text
{
  // existing CommunityInvitePayload fields...
  "es": {                                 // epoch_snapshot
    "ep": 7,                              // epoch
    "sk": h'...',                         // sealed_epoch_key (X25519+ChaChaPoly, 92 bytes)
    "ss": {                               // state_snapshot
      "mb": { ... },                      // members
      "ch": { ... },                      // channels
      "pl": { ... }                       // power_levels
    }
  }
}
```

### 7.4 EpochCatchup event (CBOR)

```text
{
  "tg": "f",
  "vl": {
    "ep": 7,                              // epoch being delivered
    "ts": h'01020304...',                 // 16-byte EventId (Join)
    "rc": [                               // recipient_ciphertexts
      [h'aabbcc...', h'sealed...'],       // (OwnerAddr, X25519-sealed bytes)
      ...
    ]
  }
}
```

### 7.5 Wire-format pinning fixtures

New canonical-CBOR test fixtures in `tests/wire_format_community_sync_fixtures.rs`:

1. `epoch_rotation_event_wire_bytes_pinned` — single rotation with 3 recipients
2. `epoch_catchup_event_wire_bytes_pinned` — single catchup with 1 recipient
3. `encrypted_envelope_wire_bytes_pinned` — envelope with `rg = null`
4. `encrypted_envelope_with_ratchet_generation_pinned` — envelope with `rg = Some(0)` (v3 forward-compat smoke test)
5. `invite_payload_with_epoch_snapshot_wire_bytes_pinned` — full CommunityInvitePayload with snapshot

## 8. Plan-time decisions

1. **Epoch Key vs Sender Keys vs TreeKEM** — Epoch Key chosen for simplicity, code surface (~150-300 LOC for the rotation logic vs ~500-1000 for Sender Keys), and acceptable per-kick bandwidth at the target scale. See §2.
2. **Replace v1 entirely** — Pre-launch, no v1 communities exist. No parallel codepath; v1 fixtures updated in this PR.
3. **Future-only history visibility** — Default. Pinned-messages future-extension noted in §9 as a follow-up.
4. **Backward only (no forward secrecy)** — Confirmed; `ratchet_generation` reserved in wire format for future extension.
5. **Atomic kick+rotation bundle** — Single CRDT submission, both events same HLC tick. Vs separate-events-then-rotate which would always leave a vulnerability window.
6. **Staleness gate via `prior_epoch`** — Vs other concurrent-resolution strategies (e.g., "second-kicker retries explicitly"). Self-healing via pending_rotation_for tracking is simpler and CRDT-native.
7. **Leaver-issued rotation cooperatively trusted** — With recipient-list validation. Vs admin-only rotation which would create vulnerability windows on every voluntary leave.
8. **`Vec<(OwnerAddr, Vec<u8>)>` for recipient_ciphertexts** — Vs BTreeMap; more compact CBOR encoding (avoids array-of-arrays nesting).
9. **Per-OwnerAddr granularity** — One key per owner regardless of device count. Vs per-DeviceIdentityHash which would multiply key count by ~2-3× without meaningful security gain in the backward-secrecy threat model.
10. **Separate `EpochCatchup` variant (non-advancing)** — Caught during self-review: a kick between invite issuance and redemption leaves the new member with a stale snapshot key, unable to decrypt current-epoch events. Three alternatives considered: (a) reuse `EpochRotation` triggered by Join (advances epoch on every join — high bandwidth at scale); (b) admin-issued catchup as separate event variant; (c) accept the gap and require new member to wait for next kick. Chose (b): catchup is admin-only (no cooperative-joiner path because new members can't generate trustworthy keys for themselves), doesn't advance epoch (avoids gratuitous rotation cost on every join), and surfaces the additive-vs-subtractive distinction in the type system.
11. **Admin-only `EpochCatchup` (no cooperative-joiner)** — Unlike `EpochRotation` (where a leaver MAY cooperatively issue a well-formed rotation per §4.4), `EpochCatchup` is strictly admin-issued. A new member generating their own first key would let them choose weak/predictable entropy, weakening security for everyone receiving that key. Admin issuance closes this hole.

## 9. Out of scope (deferred to follow-up tickets)

1. **Pinned messages** — Second Epoch Key with different rotation policy for curated content visible to newcomers. Wire-format space reserved on `Space` for a future `pinned_epoch_key: Option<EpochKey>` field; no implementation in this spec. To be filed as a sub-ticket after this PR merges.
2. **Forward secrecy** — Per-message ratcheting + irreversible key deletion. `EncryptedEnvelope.ratchet_generation` reserved as forward-compat field.
3. **Per-device sender granularity** — v2 uses per-OwnerAddr keys. Future v3 could move to per-device for finer compromise-isolation.
4. **TreeKEM-style sub-linear rotation** — Only relevant at 100k+ scale. Out of scope for this spec; if Harmony ever scales communities that large, a separate protocol design (likely with separate community classes) is appropriate.
5. **Channel content encryption** — Currently channel-log messages live on a separate topic (ZEB-271). Whether channel content adopts the same Epoch Key scheme or has its own per-channel mechanism is a follow-up question, not addressed here.

## 10. Known limitations

### 10.1 Post-kick vulnerability window

Between a `Kick` or `Leave` event landing in CRDT and the matching `EpochRotation` materializing, the kicked/leaving member still holds the current `EpochKey` and can decrypt new events published in that window.

**Atomic bundle case (kicker is online):** Window = CRDT propagation latency, typically hundreds of ms in healthy network conditions.

**Self-healing case (concurrent kick loser, or leaver who didn't bundle a rotation, or admin offline at leave time):** Window = time until next admin observes pending_rotation_for and synthesizes a fresh rotation. Can extend to hours in low-admin communities.

This is strictly better than v1 (which had infinite window — never rotated). It is strictly worse than TreeKEM (which closes the window immediately via autonomous per-leaf ratchets). The trade-off is accepted given Harmony's chosen design values (simplicity, no heavy dependencies, CRDT-native).

### 10.2 Compromise of current member's device exposes current content

Any current member's device, if compromised, leaks `current_epoch_key` to the attacker. The attacker can decrypt all current and historical content the member had access to. This is identical to v1's threat model — backward secrecy protects against ex-members, not against current members' device compromise. Forward secrecy (out of scope per §9.2) is the standard mitigation; not adopted in this spec.

### 10.3 Inviter trust for state_snapshot

A malicious inviter can ship a tampered `state_snapshot` in the invite payload (wrong members list, fabricated channels). The invitee's local materialized state diverges from CRDT reality until the first CRDT replay corrects it (~ one round trip post-redemption). The snapshot is documented as a UI bootstrap hint, not a source of truth.

Mitigations not adopted: cryptographic attestation of snapshot contents by N-of-M current members. Considered overkill for a transient hint that's corrected by CRDT replay within seconds.

### 10.4 Stale-invite catchup window

When a kick lands between invite issuance and invite redemption, the new member's local state starts at `snapshot_epoch < current_epoch`. They can't decrypt new events until an admin issues an `EpochCatchup` (see §4.6). UI surfaces "establishing access" during this window.

This is a **UX degradation, not a privacy leak** — it's the dual of §10.1 (post-kick window). The new member is *missing* access rather than *retaining* access they shouldn't have. The window is bounded by admin availability, same shape as §10.1.

Mitigations not adopted: rotating on every Join (would double rotation frequency for an additive operation that doesn't require it). The catchup-on-detect approach was chosen instead.

### 10.5 Storage growth for `old_epoch_keys`

Each rotation adds one entry to `old_epoch_keys` (~32 bytes). At 1 kick/month, growth is ~400 bytes/year per community. Communities with very high churn (100 kicks/month) would see ~40 KB/year. After 10 years, still ~400 KB worst case. Bounded and acceptable. No pruning policy needed for v2.

### 10.6 Remote-rotation key extraction (implemented in PR #106 R4)

Previously, `Space.current_epoch_key` was updated only when the LOCAL
node's kick/leave handler issued a rotation. When a REMOTE admin's
rotation arrived via CRDT sync, the local node was stuck at the old key.

**Phase A — live-key wiring for CommunitySyncEngine:**
`CommunitySyncEngine`'s `publish_root_now` and `handle_incoming_publish`
now read the epoch key from `crdt_state` (the live `OwnerState`) via a
new `live_epoch_key()` helper rather than using the spawn-time
`membership_key`. Old keys are tried in reverse for decryption
(multi-key retry loop) to handle cross-epoch receive ordering.

**Phase B — `apply_remote_epoch_event`:**
A new `pub async fn apply_remote_epoch_event(...)` in `lib.rs` is
called from the CRDT delta consumer immediately before
`self_heal_community_observer`. It:
1. Identifies the local user's `recipient_ciphertexts[local_addr]`
   entry in the incoming `EpochRotation` or `EpochCatchup`.
2. Decrypts it via `dm_signing::open_from_owner` +
   `ed25519_priv_to_x25519(local_signing_key)`.
3. Updates `crdt_state.spaces[community_id]` directly (bypassing
   `apply_space`, which rejects epoch-key mutations as CRDT-replicated
   ops — remote key extraction is a local-only side effect).
4. For `EpochRotation`: archives the old key into `old_epoch_keys`,
   advances `current_epoch` by 1.
5. For `EpochCatchup`: sets `current_epoch` + `current_epoch_key`
   directly (no archiving — catchup delivers the current key to a
   latecomer).
6. Idempotent: no-op if `current_epoch` already ≥ target epoch.

**Phase C — hydration watermark (moot):**
A separate "replay-complete" flag is not needed. Phase A reads live
state on every encrypt/decrypt; Phase B updates live state immediately
on delta arrival. The key is always current by the time any operation
needs it. See doc-comment on `current_epoch_key_for` in
`owner_state_crdt.rs`.

**Phase D — 4 cross-node integration tests** added to
`community_backward_secrecy_integration.rs`:
- `two_node_remote_rotation_propagates_new_key`
- `offline_catchup_via_remote_rotation_observation`
- `remote_rotation_apply_is_idempotent`
- `remote_rotation_noop_when_not_in_recipient_list`

**Phase E — R3 bot review fixes:**
- E1: catchup dedupe uses `(SpaceId, OwnerAddr, EventId)` key.
- E2: invite-only `sealed_epoch_key` minimum-size check before decryption.
- E3: `InviteUrlError::TooLarge` message + doc updated to 85 333 chars.
- E4: `leave_community` solo-leave sentinel is a named constant
  (`LEAVE_SOLO_SENTINEL`); comparison uses `==` not `contains`.
- E5: `apply_rotation_to_space` test helper validates `prior_epoch`.
- E6: `stale_invite_catchup_unlocks_decryption_end_to_end` asserts
  that `EpochCatchup` does NOT populate `old_epoch_keys`.

## 11. Acceptance criteria

1. New communities created post-merge use `EpochKey` rotation. v1's `MembershipKey` field is fully removed from `Space`; no parallel codepath.
2. Removed members (Kick or Leave) cannot decrypt events published after the matching `EpochRotation` lands. Verified by the two-node integration test in §6.4.
3. Concurrent kicks resolve correctly via the staleness gate + self-healing path. Verified by the four-node integration test in §6.4.
4. Offline members successfully catch up across multiple missed rotations. Verified by the offline-catchup integration test in §6.4.
5. New members invited at epoch N can decrypt all events with HLC > invite issuance; events with HLC < invite issuance return `KeyNotAvailable` (expected, UI surfaces "joined at epoch N, history unavailable").
6. New members whose invite snapshot is stale at redemption (`snapshot_epoch < current_epoch`) successfully receive an `EpochCatchup` from an admin and proceed to decrypt new events. Verified by the stale-invite-catchup integration test in §6.5.
7. All 5 CI gates green: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `cargo check --locked --all-targets --features test-fixtures` (MSRV), `npx tsc --noEmit && npx vitest run` (frontend).
8. Wire-format pinning fixtures (§7.5) committed with canonical bytes for both `EpochRotation` and `EpochCatchup`.
9. One PR delivered against `origin/main` of harmony-client.
