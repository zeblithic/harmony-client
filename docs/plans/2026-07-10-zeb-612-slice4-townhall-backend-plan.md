# ZEB-612 Slice 4 — Town Hall backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Town Hall backend per spec §5 (`docs/specs/2026-07-09-zeb-612-commons-i-town-hall-vines-files-design.md`): a `townhall` channel kind, a raise-hand field on the voice presence beacon (surfaced as `handRaisedAt` on the roster), an invite-to-speak moderation directive (surfaced as `invitedOwners`/`selfInvited` on the moderation overlay), the `set_voice_hand` IPC, wire fixtures both directions, and the frontend *service-layer* surface (S5 builds TownHallView on top of it).

**Architecture:** Everything rides existing rails. The channel kind is a third `serde_repr` discriminant on the membership-CRDT `ChannelCreate`. The hand is an optional beacon field following the exact `left` pattern (`skip_serializing_if` keeps a lowered-hand beacon byte-identical to today's wire). Invite-to-speak is a fifth `ModAction` on the signed voice-control directive, with its own enforcement class inside `ActiveModeration` so it can never clobber mute/kick state.

**Tech Stack:** Rust (tauri, serde/ciborium canonical CBOR, ed25519-dalek, zenoh), Svelte 5 + TypeScript, vitest, cargo-nextest.

## Global Constraints

- Copy verbatim from spec §5: hand = `Option<u64>` "wall-clock ms when raised; absent = lowered"; invite authority = "actor power ≥ 50" with the `actor>target` clause explicitly NOT applied; invite TTL ≈ 2 min; speaker queue is DERIVED (hand timestamp order, tiebreak owner hex) — no new store, no sync protocol; quorum needs NO new backend.
- Canonical-CBOR invariants: 2-char field keys (`hd` for hand); new optional fields use `#[serde(default, skip_serializing_if = ...)]`; serde field-declaration order IS wire order — new fields append LAST; `serde_repr` discriminants for enums.
- Wire-fixture discipline: generate-then-pin; every changed payload gets a pinned fixture + a byte-identity guard proving the old wire is unchanged when the new field is absent.
- Gates (CLAUDE.md): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --features test-fixtures -E '<filter>'` per task (`scripts/test-select --context task` for iterative rounds); `npx tsc --noEmit` + `npx vitest run` frontend; final full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures` before PR.
- IPC naming: Rust snake_case params, JS camelCase; errors extracted `e instanceof Error ? e.message : String(e)`.
- One PR for the whole slice; commit per task.

## Ground-truth premise pins (verified 2026-07-10, flagged for review in the PR body)

1. **Stale-client posture for `ChannelKind::Townhall = 2` (spec §5 asked us to verify + pin).** `ChannelKind` is `serde_repr` and rejects unknown discriminants; the whole `CommunityState` decodes as ONE blob (`community_state_sync.rs:3388`). A stale client receiving a state containing a townhall `ChannelCreate` therefore rejects the entire sync with `ErrPreMutation(CborDecode)` — **no crash, no partial mutation, loudly logged — but the community freezes for that client until it upgrades.** This is *exactly* the posture the ZEB-349 Voice introduction shipped (the code comment at `community_membership.rs:371` only guarantees byte-compat for Text). The spec's "degrade to a joinable voice room / inert row" alternatives are **not achievable** for signed canonical-CBOR CRDT payloads: tolerant decode (ignored unknown field or fallback variant) makes decode lossy, and both signature verification and state re-publish re-encode from the decoded struct — a lossy decode breaks signatures fleet-wide. Pinned behavior: same as Voice precedent; fleet discipline is upgrade-before-create (our 3-node fleet flag-days routinely).
2. **Mixed-fleet raised-hand visibility.** `verify_presence_beacon_sig` re-encodes the decoded beacon, so a stale client that decodes a hand-raised beacon (unknown field ignored by serde) re-encodes WITHOUT `hd` → sig mismatch → beacon dropped. Consequence: a hand-raiser vanishes from stale rosters (12 s TTL) until they lower the hand; hand-lowered beacons stay byte-identical to today's wire. Transient, contained, and the same posture the `left` field's own introduction had — the spec explicitly names the `left` pattern as the model.
3. **Invite TTL mechanics.** Receive-side enforcement keeps the existing `ENFORCE_TTL_MS` (12 s) refreshed by the issuer's 4 s re-asserts; the ~2 min invite window is the ISSUER-side re-assert window (`INVITE_TTL_MS = 120_000` as the invite's default `duration_ms`, analog of `DEFAULT_MODERATION_MS`). An unclaimed invite expires ≤12 s after the issuer stops re-asserting — and dies fast if the issuer leaves. No new sweep machinery.
4. **Channel topic does not exist** (`ChannelModify` carries name + write_power only) — S5's agenda line will be omitted per spec §6's "verify at plan time".

## File structure

| File | Responsibility |
|---|---|
| `src-tauri/src/community_membership.rs` | `ChannelKind::Townhall = 2` |
| `src-tauri/src/lib.rs` | `parse_channel_kind` + `channel_info_dto` townhall arms; `set_voice_hand` IPC + registration |
| `src-tauri/src/voice_presence.rs` | beacon `hand` field, `PresenceEntry`/`RosterEntry` plumbing, publisher hand cell, `update_hand_cell` helper |
| `src-tauri/src/voice_moderation.rs` | `ModAction::InviteToSpeak`, `ModClass`, invite slot in `ActiveModeration`, `INVITE_TTL_MS`, action-aware authority |
| `src-tauri/src/voice.rs` | `SetHand` request, `SetVoiceHandPayload`, `parse_action` "invite" |
| `src-tauri/src/event_loop.rs` | `voice_hand_flags`, `SetHand` arm, `ModClass` idkey, overlay `invitedOwners`/`selfInvited` |
| `src-tauri/tests/wire_format/voice_fixtures.rs` | hand-beacon pin + invite-directive pins |
| `src-tauri/tests/wire_format/community_fixtures.rs` | townhall `ChannelCreate` pin |
| `src/lib/community-service.ts` | `'townhall'` kind union |
| `src/lib/components/CreateChannelDialog.svelte` | Town Hall option |
| `src/lib/components/CommunityView.svelte` | interim: townhall renders VoiceChannelView (S5 swaps in TownHallView) |
| `src/lib/voice-session.ts` | `handRaisedAt`, `invited`, `selfInvited`, `setHand`, `inviteToSpeak`, `speakerQueue` |

---

### Task 1: `ChannelKind::Townhall` — enum, IPC parse, DTO, creation event

**Files:**
- Modify: `src-tauri/src/community_membership.rs:380-397` (enum + doc)
- Modify: `src-tauri/src/lib.rs` (`parse_channel_kind` ~21172, `channel_info_dto` ~21185)
- Test: inline `#[cfg(test)]` additions near existing kind tests

**Interfaces:**
- Produces: `ChannelKind::Townhall` (discriminant 2), `parse_channel_kind(Some("townhall"))`, DTO string `"townhall"` — consumed by Task 2's fixture and Task 7's frontend union.

- [ ] **Step 1: extend the enum** (append variant; update doc comment to note the ZEB-612 townhall kind and the pinned stale-client posture from the premise-pins section):

```rust
pub enum ChannelKind {
    #[default]
    Text = 0,
    Voice = 1,
    /// ZEB-612 Town Hall: voice fused with assembly affordances (raise-hand
    /// queue, invite-to-speak, motion card). Same media/presence topics as
    /// Voice; the kind only routes the frontend view. Stale-client posture on
    /// decode is the ZEB-349 Voice precedent: unknown discriminants reject the
    /// containing state blob loudly (no crash; upgrade-before-create fleet rule).
    Townhall = 2,
}
```

- [ ] **Step 2: extend `parse_channel_kind` + `channel_info_dto`** in `lib.rs`:

```rust
Some("townhall") => Ok(crate::community_membership::ChannelKind::Townhall),
```

```rust
crate::community_membership::ChannelKind::Townhall => "townhall".to_string(),
```

Grep for other exhaustive `match` sites on `ChannelKind` (`community_fork.rs`, `community_channel_log_engine.rs`, `community_state_sync.rs`, `open_join_dial.rs`) and give each a `Townhall` arm behaving exactly like `Voice` (channel-log/backfill/open-join treat a townhall channel as a voice channel — same infrastructure).

- [ ] **Step 3: extend the existing kind unit tests** (find via `cargo nextest list -E 'test(parse_channel_kind) or test(channel_info_dto)'`): townhall parses, townhall stringifies, bogus kind still rejects.

- [ ] **Step 4: run** `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(channel_kind) or test(parse_channel_kind) or test(channel_info_dto)'` → PASS; commit `ZEB-612 S4: ChannelKind::Townhall backend plumbing`.

### Task 2: townhall `ChannelCreate` wire pin

**Files:**
- Modify: `src-tauri/tests/wire_format/community_fixtures.rs` (mirror `signed_event_channel_create_voice_wire_bytes_pinned`, ~446)

- [ ] **Step 1: write the pin test** — identical fixture to the Voice pin but `kind: ChannelKind::Townhall`; expected hex = Voice pin's hex with the trailing `62636b01` map entry becoming `62636b02` (generate-then-pin: run once, paste printed hex, re-run green). Doc comment states the delta explicitly, mirroring the Voice pin's comment.
- [ ] **Step 2: run** `cargo nextest run --locked --features test-fixtures -E 'test(channel_create)'` → all pins PASS (Text pin unchanged = byte-identity proof); commit `ZEB-612 S4: pin townhall ChannelCreate wire bytes`.

### Task 3: beacon `hand` field + roster plumbing

**Files:**
- Modify: `src-tauri/src/voice_presence.rs`
- Test: inline tests in the same file

**Interfaces:**
- Produces: `VoicePresenceBeacon.hand: Option<u64>`; `RosterEntry.hand_raised_at` (JSON `handRaisedAt`); `build_heartbeat_beacon(..., hand: Option<u64>)`; `publish_presence_once(..., hand: Option<u64>)`; `spawn_voice_presence_publisher(..., hand_raised_at: Arc<AtomicU64>, ...)`; `update_hand_cell(&AtomicU64, Option<u64>) -> Option<u64>`.

- [ ] **Step 1: beacon field** — append LAST (declaration order is wire order), exactly the `left` pattern:

```rust
/// ZEB-612 Town Hall: wall-clock ms when this member raised their hand;
/// absent = lowered. `skip_serializing_if` keeps a lowered-hand beacon
/// byte-identical to the pre-ZEB-612 wire (the `left` pattern).
#[serde(rename = "hd", default, skip_serializing_if = "Option::is_none")]
pub hand: Option<u64>,
```

- [ ] **Step 2: thread through state** — `PresenceEntry.hand: Option<u64>`; in `apply()`: every arm that copies `muted` also copies `hand`; the same-session newer-seq arm's visible-change predicate becomes `let visible_changed = e.muted != beacon.muted || e.hand != beacon.hand;` (comment: hand flips are roster-visible via `handRaisedAt`); `RosterEntry` gains `pub hand_raised_at: Option<u64>` (camelCase rename_all already emits `handRaisedAt`; JSON null when lowered) populated from `e.hand` in `roster()`.

- [ ] **Step 3: constructors** — `build_heartbeat_beacon` gains trailing `hand: Option<u64>`; `publish_presence_once` gains trailing `hand: Option<u64>` and forwards; `build_presence_tombstone` sets `hand: None`; `spawn_voice_presence_publisher` gains `hand_raised_at: Arc<AtomicU64>` (param right after `muted`) read each tick:

```rust
let hr = hand_raised_at.load(Ordering::SeqCst);
let hand = (hr != 0).then_some(hr);
```

`spawn_groupdm_presence_publisher`'s call to `build_heartbeat_beacon` passes `None` (group-DM calls have no hand semantics). Add the pure helper the event loop's `SetHand` arm will use:

```rust
/// ZEB-612: apply a raise/lower to the shared hand cell and return the beacon
/// value. Raising keeps the ORIGINAL timestamp if already raised (queue
/// position must be stable across repeat raises); lowering always resets.
/// 0 is the "lowered" sentinel — wall-clock ms is never 0.
pub fn update_hand_cell(cell: &AtomicU64, raised_at: Option<u64>) -> Option<u64> {
    match raised_at {
        Some(ts) => {
            let prev = cell.load(Ordering::SeqCst);
            if prev == 0 {
                cell.store(ts, Ordering::SeqCst);
                Some(ts)
            } else {
                Some(prev)
            }
        }
        None => {
            cell.store(0, Ordering::SeqCst);
            None
        }
    }
}
```

- [ ] **Step 4: fix all compile sites** (test constructors of `VoicePresenceBeacon` get `hand: None`; `build_heartbeat_beacon` callers updated), then write tests: (a) `apply` reports a visible change when only `hand` flips and the roster carries the new `handRaisedAt`; (b) `apply` does NOT report a change on a bare heartbeat with unchanged hand; (c) `update_hand_cell` — first raise stamps, repeat raise keeps the original, lower resets, raise-after-lower restamps; (d) `RosterEntry` serializes `handRaisedAt` (number when raised, null when lowered).

- [ ] **Step 5: run** `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(hand) or test(presence)'` → PASS; commit `ZEB-612 S4: raise-hand field on the voice presence beacon`.

### Task 4: presence-beacon wire fixtures (both directions)

**Files:**
- Modify: `src-tauri/tests/wire_format/voice_fixtures.rs`

- [ ] **Step 1:** run `cargo nextest run --locked --features test-fixtures -E 'test(presence_beacon)'` FIRST — the existing pins MUST still pass untouched (fixture beacons get `hand: None`; `skip_serializing_if` keeps bytes identical). This green run is the "old beacon decodes" direction: the pinned bytes round-trip through `open_presence_beacon` into a struct whose `hand` defaults to `None`.
- [ ] **Step 2:** add `presence_beacon_with_hand_wire_bytes_pinned`: same fixture but `hand: Some(1_720_000_000_000)`, sealed with the zeroed nonce; generate-then-pin the hex; assert round-trip `opened == signed` (proves the new field survives sign→seal→open→verify).
- [ ] **Step 3:** run `cargo nextest run --locked --features test-fixtures -E 'test(presence_beacon)'` → PASS; commit `ZEB-612 S4: pin hand-raised presence beacon wire bytes`.

### Task 5: `ModAction::InviteToSpeak` + invite class in `ActiveModeration`

**Files:**
- Modify: `src-tauri/src/voice_moderation.rs`
- Test: inline tests in the same file

**Interfaces:**
- Produces: `ModAction::InviteToSpeak = 4`; `ModClass { Mute, Kick, Invite }` + `ModAction::class()`; `INVITE_TTL_MS: u64 = 120_000`; `ActiveModeration::{is_invited, snapshot -> (Vec, Vec, Vec)}`; authority check that skips `actor>target` for invites. Consumed by Task 6 (event loop) and Task 7 (IPC parse).

- [ ] **Step 1: enum + class** — add variant and the 3-way class (deriving `Hash` so it can key the issuer-directives map):

```rust
pub enum ModAction {
    Mute = 0,
    Unmute = 1,
    Kick = 2,
    Unkick = 3,
    /// ZEB-612 Town Hall: a mod invites `target_owner` to speak. Benign (no
    /// enforcement against the target): surfaces on the overlay as
    /// `invitedOwners`; the TARGET's own client lowers its hand on accept or
    /// dismiss — the invite never mutates another owner's beacon.
    InviteToSpeak = 4,
}

/// Enforcement class: each class is an independent LWW slot per target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModClass {
    Mute,
    Kick,
    Invite,
}

impl ModAction {
    pub fn class(self) -> ModClass {
        match self {
            ModAction::Mute | ModAction::Unmute => ModClass::Mute,
            ModAction::Kick | ModAction::Unkick => ModClass::Kick,
            ModAction::InviteToSpeak => ModClass::Invite,
        }
    }
    /// True for the "positive" directives that turn enforcement ON.
    pub fn enforces(self) -> bool {
        matches!(
            self,
            ModAction::Mute | ModAction::Kick | ModAction::InviteToSpeak
        )
    }
}
```

Delete `is_mute_class` (its two callers migrate to `class()`: the `apply` routing here and the event-loop idkey in Task 6).

```rust
/// ZEB-612: issuer-side re-assert window for an unclaimed invite-to-speak
/// (~2 min; the invite's default `duration_ms`, analog of
/// `DEFAULT_MODERATION_MS`). Receive-side liveness stays `ENFORCE_TTL_MS`
/// refreshed by the 4 s re-asserts, so an invite dies ≤12 s after the issuer
/// stops re-asserting (expiry or issuer departure).
pub const INVITE_TTL_MS: u64 = 120_000;
```

- [ ] **Step 2: third slot** — `TargetState.invite: Option<ClassState>`; `apply()` routes via:

```rust
let slot = match d.action.class() {
    ModClass::Mute => &mut target.mute,
    ModClass::Kick => &mut target.kick,
    ModClass::Invite => &mut target.invite,
};
```

`sweep`/`any_enforced`/prune iterate `[&mut t.mute, &mut t.kick, &mut t.invite]`; `targets.retain` includes `t.invite.is_some()`; `is()` gains the invite arm via a `ModClass` param (refactor `is(..., mute: bool)` → `is(..., class: ModClass)`); add `is_invited`; `snapshot` returns `(Vec<[u8; 16]>, Vec<[u8; 16]>, Vec<[u8; 16]>)` (muted, kicked, invited).

- [ ] **Step 3: authority** — in `verify_directive_authority`, split the power gate with the spec's rationale verbatim in a comment:

```rust
if actor_power < MOD_POWER {
    return Err(ModError::NotAuthorized);
}
// The `actor > target` clause is punitive-action logic (blocks equal-power
// retaliation and self-moderation). The benign invite skips it: a mod may
// invite an equal- or higher-power member to speak. (spec §5)
if d.action != ModAction::InviteToSpeak && actor_power <= target_power {
    return Err(ModError::NotAuthorized);
}
```

- [ ] **Step 4: tests** — extend `mod_action_discriminant_and_roundtrip` with `InviteToSpeak` (+ assert discriminant 4); `invite_and_kick_are_independent_classes` (invite + kick on the same target coexist; kick lapses independently); `invite_expires_via_sweep`; `authority_allows_invite_to_equal_or_higher_power_target` (actor 50, target 60 → Ok; contrast: `Mute` with the same powers → NotAuthorized); `authority_rejects_invite_below_mod_power` (actor 49 → NotAuthorized); `snapshot_lists_invited_targets`.

- [ ] **Step 5: run** `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(invite) or test(mod_action) or test(authority)'` → PASS; commit `ZEB-612 S4: InviteToSpeak directive + invite enforcement class`.

### Task 6: invite-directive wire fixtures

**Files:**
- Modify: `src-tauri/tests/wire_format/voice_fixtures.rs`

- [ ] **Step 1:** existing directive pins still green (`ModAction` variants 0–3 unchanged on the wire). Add `voice_moderation_invite_directive_canonical_cbor_is_pinned` (fixture_directive with `action: ModAction::InviteToSpeak`; the hex delta vs the Mute pin is exactly the `ac` value byte `00` → `04`) and `voice_moderation_sealed_invite_directive_is_pinned` (signing key `[7u8; 32]`, nonce `[9u8; 12]`, valid `actor_device`; generate-then-pin).
- [ ] **Step 2:** run `cargo nextest run --locked --features test-fixtures -E 'test(voice_moderation)'` → PASS; commit `ZEB-612 S4: pin invite-to-speak directive wire bytes`.

### Task 7: event loop + IPC — hand flags, `SetHand`, invite flow-through, overlay

**Files:**
- Modify: `src-tauri/src/voice.rs` (request + payload + parse_action)
- Modify: `src-tauri/src/event_loop.rs` (flag map ~3164, Join arm ~4622, Leave arm ~4933, new SetHand arm after SetMuted ~5019, Moderate idkey ~5262, `emit_moderation_changed` ~483, re-assert/`Leave` retain patterns)
- Modify: `src-tauri/src/lib.rs` (`set_voice_hand` command near `set_voice_muted` ~17885; register ~53308; `moderate_voice` doc note)
- Test: `voice.rs` inline (parse_action), `voice_presence.rs` already covers `update_hand_cell`

**Interfaces:**
- Consumes: Task 3's `update_hand_cell` + publisher param; Task 5's `ModClass`/`INVITE_TTL_MS`/3-tuple snapshot.
- Produces: IPC `set_voice_hand({communityId, channelId, raised})`; `moderate_voice` accepts `action: "invite"`; overlay event gains `invitedOwners: string[]`, `selfInvited: boolean`.

- [ ] **Step 1: `voice.rs`** — add request variant + payload + parse arm (+ test):

```rust
/// ZEB-612 Town Hall: raise/lower our hand. `raised_at` is the wall-clock ms
/// minted at the IPC boundary for `Some` (raise); `None` lowers. The event
/// loop keeps the ORIGINAL stamp on repeat raises (stable queue position).
SetHand {
    community_id: SpaceId,
    channel_id: ChannelId,
    raised_at: Option<u64>,
},
```

```rust
/// ZEB-612: payload for the `set_voice_hand` Tauri command.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetVoiceHandPayload {
    pub community_id: String,
    pub channel_id: String,
    pub raised: bool,
}
```

`parse_action`: `"invite" => Ok(InviteToSpeak),` (doc comment string list updated).

- [ ] **Step 2: event loop hand plumbing** — declare `voice_hand_flags: std::collections::HashMap<(SpaceId, ChannelId), Arc<AtomicU64>>` beside `voice_mute_flags`; Join arm creates `Arc::new(AtomicU64::new(0))`, inserts, passes to `spawn_voice_presence_publisher` (after `mute_flag`); Leave arm removes it beside `voice_mute_flags.remove`. New arm after `SetMuted` (mirrors its shape — immediate beacon carries the CURRENT mute state from `voice_mute_flags`):

```rust
crate::voice::VoiceChannelRequest::SetHand { community_id, channel_id, raised_at } => {
    if let Some(flag) = voice_hand_flags.get(&(community_id, channel_id)) {
        let hand = crate::voice_presence::update_hand_cell(flag, raised_at);
        // Immediate beacon so the queue reflects the hand without waiting
        // out the next ≤4 s heartbeat (mirrors the SetMuted arm).
        if let (Some(key), Some((owner, device, joined_hlc, signing_key)), Some(seq_counter), Some(mute_flag)) = (
            voice_keys.get(&(community_id, channel_id)),
            voice_identity.get(&(community_id, channel_id)),
            voice_presence_seq.get(&(community_id, channel_id)),
            voice_mute_flags.get(&(community_id, channel_id)),
        ) {
            let pres_topic = format!(
                "harmony/voice-presence/{}/{}",
                hex::encode(community_id.0),
                hex::encode(channel_id.0),
            );
            let seq = seq_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Err(e) = crate::voice_presence::publish_presence_once(
                &session, &pres_topic, key, &community_id, &channel_id, signing_key,
                *owner, *device, joined_hlc, seq,
                mute_flag.load(std::sync::atomic::Ordering::SeqCst),
                hand,
            )
            .await
            {
                tracing::warn!(%pres_topic, err = ?e, "immediate hand beacon publish failed");
            }
        }
    }
}
```

- [ ] **Step 3: invite flow-through** — Moderate-arm idkey becomes `(community_id, channel_id, target_owner.0, action.class())` (map type + `Leave`-retain pattern + re-assert destructuring updated); the issuer default duration becomes action-aware:

```rust
let stop_after = now
    + duration_ms.unwrap_or(match action {
        crate::voice_moderation::ModAction::InviteToSpeak => {
            crate::voice_moderation::INVITE_TTL_MS
        }
        _ => crate::voice_moderation::DEFAULT_MODERATION_MS,
    });
```

`emit_moderation_changed`: destructure the 3-tuple snapshot, add `"invitedOwners"` + `"selfInvited": invited.contains(&self_owner.0)` to the JSON.

- [ ] **Step 4: IPC** — `set_voice_hand` mirrors `set_voice_muted` (id parsing, tx snapshot) and mints the stamp:

```rust
/// ZEB-612 Town Hall: raise or lower this node's hand in an active voice
/// channel. Mints the raise timestamp (wall-clock ms) at the IPC boundary;
/// the event loop keeps the FIRST stamp across repeat raises so queue
/// position is stable, and the presence heartbeat republishes it.
#[tauri::command]
async fn set_voice_hand(
    payload: voice::SetVoiceHandPayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let community =
        crate::owner_state_types::SpaceId(parse_voice_id_16("communityId", &payload.community_id)?);
    let channel = crate::community_membership::ChannelId(parse_voice_id_16(
        "channelId",
        &payload.channel_id,
    )?);
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard
            .voice_channel_tx
            .clone()
            .ok_or_else(|| "not connected".to_string())?
    };
    let raised_at = payload.raised.then(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    });
    tx.send(voice::VoiceChannelRequest::SetHand {
        community_id: community,
        channel_id: channel,
        raised_at,
    })
    .await
    .map_err(|_| "event loop not running".to_string())
}
```

Register `set_voice_hand` in the handler list after `set_voice_muted`; check `src-tauri/capabilities/` / `gen/schemas` for whether commands are enumerated (mirror however `set_voice_muted` is exposed).

- [ ] **Step 5: gates** — `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(parse_action) or test(hand) or test(invite)'` → PASS; `scripts/test-select --context task`; commit `ZEB-612 S4: SetHand + invite-to-speak through the event loop and IPC`.

### Task 8: frontend service layer — kind union, creation UI, session state

**Files:**
- Modify: `src/lib/community-service.ts` (kind unions ~94, ~388)
- Modify: `src/lib/components/CreateChannelDialog.svelte` (third kind option)
- Modify: `src/lib/components/CommunityView.svelte:438` (interim routing)
- Modify: `src/lib/voice-session.ts` (roster + overlay + methods + `speakerQueue`)
- Test: `src/lib/voice-session.test.ts`, `src/lib/components/__tests__/CreateChannelDialog.test.ts` (or wherever its tests live), `src/lib/community-service.test.ts`

**Interfaces:**
- Consumes: backend `handRaisedAt` (roster entries), `invitedOwners`/`selfInvited` (overlay), `set_voice_hand`, `moderate_voice` `"invite"`.
- Produces (for S5): `RosterMember.handRaisedAt: number | null`, `RosterMember.invited: boolean`, `VoiceSessionState.selfInvited: boolean`, `session.setHand(raised)`, `session.inviteToSpeak(ownerHex)`, `speakerQueue(roster)`.

- [ ] **Step 1: kind unions** — `ChannelInfo.kind: 'text' | 'voice' | 'townhall'`; `createChannel(..., kind: 'text' | 'voice' | 'townhall' = 'text')`.
- [ ] **Step 2: CreateChannelDialog** — widen the `kind` state type, add the third button after Voice (`⚖ Town Hall`, `aria-pressed`, same classes); reset paths already set `'text'`.
- [ ] **Step 3: CommunityView interim routing** — `{#if activeChannel.kind === 'voice' || activeChannel.kind === 'townhall'}` with a `<!-- ZEB-612 S5 swaps townhall to TownHallView -->` note (spec's own degradation: a joinable voice room).
- [ ] **Step 4: voice-session** — `RosterMember` gains `handRaisedAt: number | null` and `invited: boolean`; `rostersEqual` compares both; presence-listener mapping reads `handRaisedAt` from each roster entry (`?? null`); moderation listener stores `invitedOwners` Set + `selfInvited` (payload type widened), roster refresh maps `invited`; `INITIAL` gains `selfInvited: false`; join/leave reset paths clear the new state. New methods mirroring `setMuted`/the moderate path:

```ts
/** Raise/lower our hand (Town Hall queue). No-op unless connected. */
async setHand(raised: boolean): Promise<void> { /* invoke('set_voice_hand', { communityId, channelId, raised }) */ }

/** Invite `ownerHex` to speak (power-gated backend-side). */
async inviteToSpeak(ownerHex: string): Promise<void> { /* invoke('moderate_voice', { communityId, channelId, targetOwnerHex: ownerHex, action: 'invite' }) */ }
```

Pure helper (exported for S5):

```ts
/** Derived speaker queue: raised hands ordered by raise time, owner-hex tiebreak. */
export function speakerQueue(roster: RosterMember[]): RosterMember[] {
  return roster
    .filter((m) => m.handRaisedAt !== null)
    .sort((a, b) => a.handRaisedAt! - b.handRaisedAt! || (a.ownerHex < b.ownerHex ? -1 : 1));
}
```

- [ ] **Step 5: tests** — voice-session: presence payload with `handRaisedAt` lands on the roster; overlay payload with `invitedOwners`/`selfInvited` patches state + marks members; `setHand`/`inviteToSpeak` invoke with the exact camelCase args; `speakerQueue` ordering incl. tiebreak + empty. CreateChannelDialog: Town Hall option selectable and passed to `createChannel`. community-service: createChannel forwards `'townhall'`.
- [ ] **Step 6: run** `npx tsc --noEmit` + `npx vitest run` → PASS; commit `ZEB-612 S4: frontend service surface — townhall kind, hand + invite state`.

### Task 9: final gates + PR

- [ ] `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (full sweep)
- [ ] `npx tsc --noEmit && npx vitest run`
- [ ] Style-token guard budget unchanged (this slice adds no new styling surfaces beyond the dialog button reusing existing classes)
- [ ] Push, open PR (premise pins §1–§4 lead the body), fire CodeRabbit once, converge.

## Self-review notes

- Spec §5 coverage: channel kind → Tasks 1–2; raise-hand → Tasks 3–4, 7; derived queue → Task 8 (`speakerQueue`, no store); invite-to-speak → Tasks 5–7; wire fixtures both directions → Tasks 2, 4, 6; quorum → no-op (verified: roster + `adminQuorum` both already surfaced).
- Type consistency: `hand: Option<u64>` end-to-end; `handRaisedAt: number | null` in TS; `ModClass` keys the issuer map and routes slots; snapshot 3-tuple has exactly two consumers (both updated in Task 7).
- The `set_voice_hand` no-op-when-not-joined posture matches `set_voice_muted` (flag absent → silently ignored backend-side).
