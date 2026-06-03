# ZEB-360 — Group-DM voice calls (3+ participants) — design

**Status:** approved 2026-06-02
**Epic:** ZEB-348 (Voice comms). Builds on ZEB-352 (V4 1:1 DM calls), ZEB-351 (V3 N-party room/mix), ZEB-356 (incoming-call notifications), ZEB-228 (GroupDm spaces).
**Branch base:** `origin/main` @ `6d20594` (ZEB-356 merged).

## Goal

Add real-time voice calls to **group DMs** (3–16 members), extending the V4 1:1 DM call to N parties. A member places a call into a group DM; all other members are rung; anyone can join (including later, while it's ongoing). Calls are peer-to-peer, end-to-end encrypted, and reuse the existing voice engine and DM key material.

## Why this is mostly reuse

The V4/V3 backend is **already N-party at the media layer**. Frames flow on a per-call mesh topic `harmony/voice/dm/{callId}/{device}` (subscribed via wildcard `…/{callId}/*`); any number of participants publish to their own device segment and receive all others. The per-call media key `K_voice = derive_dm_voice_key(content_key, callId)` (`HKDF(content_key, salt=callId, info="voice-dm:")`) is **group-agnostic**, and a `GroupDm` space already carries a single shared `content_key` for all members. So group calls need **no new cryptographic machinery and no media-layer changes**.

The pieces that are hardwired 2-party — and that this work generalizes — are **signaling** (who to ring) and **presence** (who's in the call):

1. Invite routing seals to a single callee owner → must fan to all members.
2. The callee's invite handler infers the space by scanning for a 2-member DM → group invites must carry the `spaceId` explicitly.
3. 1:1 calls have no roster (`peerOwnerHex` only) → group needs a participant roster, driven by presence beacons.

## Key decisions

- **D1 — Lifecycle: drop-in + ring (Discord-style).** Placing a group call drops the caller straight into the media room (active, possibly alone) and rings all other members. The call stays alive while ≥1 participant is present and ends when the **last** participant leaves (emergent; there is no "end for everyone").
- **D2 — Join-in-progress via a persistent banner.** Whenever a call is active in a group DM, that DM shows an active-call banner (who's in it + a Join button) for all members — robust to a declined ring, a missed ring, or an app that was closed at ring time. Backed by **group-space-scoped presence** so a member can discover the active call (and its `callId`) without holding the `callId` in advance.
- **D3 — New `GroupCallSession` controller.** A purpose-built frontend controller fuses ring-all signaling with a VoiceSession-style roster + N-stream mix, addressed by `callId`. The proven 1:1 `CallSession` and community `VoiceSession` are left untouched; the media-engine wiring is **ported** (as `CallSession` itself was ported from `VoiceSession`), not shared. A shared-media-core extraction is a deliberate later follow-up (cf. ZEB-355 on the Rust side).
- **D4 — Separate `*_group_call` IPCs.** New commands rather than `groupDm`-flagged params on the 1:1 `*_dm_call` handlers, keeping the proven 1:1 path undisturbed.
- **D5 — Cap = group membership (≤16).** `GroupDm` is hard-capped at 3–16 members at creation, so the call can never exceed it. No separate soft-cap (the 64-cap is community-voice-only). Join is gated on `caller ∈ space.members`.
- **D6 — One active voice session at a time (D12 reuse).** Placing/joining a group call while in a community channel or a 1:1 call is busy-blocked, and vice versa.
- **D7 — Start muted on connect (D10 reuse).** Every entry path starts muted; the talk-gate transmits nothing until the user unmutes.
- **D8 — No moderation.** Group DMs are a flat peer group with no power hierarchy; a participant can mute themselves and leave, but cannot server-mute or kick others. (Moderation is community-voice-only, ZEB-358.)

## Signaling state machine (frontend `GroupCallSession`)

Phases: `idle → incoming → connecting → active → leaving → idle`. (No `ringingOut` — the caller drops straight in.)

Three entry paths:

1. **Caller (place):** `placeGroupCall(spaceId)` → backend mints `callId`, fans a sealed GroupInvite to every *other* member, returns `callId`. The frontend **immediately joins media** (`idle → connecting → active`, alone). Others ring.
2. **Callee (rung):** GroupInvite arrives → `idle → incoming` (ring toast + the ZEB-356 `IncomingCallAlerter` OS notification, reused verbatim). `accept()` → `connecting → active`; `decline()` or the 30 s ring timeout → `idle`.
3. **Join-in-progress (banner):** member opens the group DM, sees the active-call banner (from group presence), clicks Join → `joinActive(callId, spaceId)` → `idle → connecting → active` (no ring; proactive).

**Roster "ringing" feedback.** The in-call UI renders *all* group members: those with a live presence beacon are `in-call`; the rest are `ringing` for the first 30 s, then `not-in-call`. An optional `Decline`-to-caller signal flips `ringing → declined` early (included for parity with 1:1; low cost).

**End semantics.** Each participant `leave()`s, publishing a presence tombstone (`left: true`). When the last participant leaves, no beacons remain → the call is inactive → the banner clears for the whole group. A caller left alone stays in the (joinable) call until they leave; rings to others stop after 30 s, but the call remains joinable via the banner as long as anyone is present.

## Presence & roster

- **Topic — group-space-scoped:** `harmony/voice-presence/group-dm/{spaceId}` (not `callId`). This is the key to join-in-progress: a member subscribes to their group's presence topic and discovers the active call (and its `callId`) without holding the `callId` in advance. Each beacon carries the active `callId`.
- **Sealing key — group-stable presence key:** presence must be decryptable *before* joining, so it cannot use the call-specific `K_voice`. Derive a call-independent presence key from the shared `content_key`: `derive_groupdm_presence_key(content_key) = HKDF(content_key, info="voice-presence-groupdm:")`, domain-separated from the media key. Every member can derive it; it survives across successive calls in the group.
- **Beacon contents** (mirrors community-voice presence + `callId`): `{ callId, owner, device, muted, joinedHlc, seq, left }`, Ed25519-signed by device-#2, sealed under the presence key. Roster materializes as in community voice; the backend emits `group-call-presence-changed { spaceId, callId, roster }`.
- **Subscription lifecycle (v1):** a client opens a **read-only** presence subscription (`watch_group_call`) **while viewing that group DM** (drives the banner) and, on joining, additionally starts the presence **publisher** (`join_group_call`) for its own beacon. The read subscription drives the in-call roster too, so it is reused, not duplicated. The initial ring already reaches all online members proactively (invite + OS notification); the banner covers declined/late/returning members on their next open of the DM.
- **Speaking & mute:** identical to VoiceSession — `muted` rides in the beacon; `speaking` derives from the receiver's active-sender set (keyed by the 16-byte senderHash = device prefix).
- **Concurrent-place edge:** if two members place simultaneously before either sees the other's beacon, two `callId`s briefly exist. v1 reconciles by **lowest `callId` wins** — clients seeing two active calls in the same group converge on the lower `callId`.

## Crypto & media

- **Media room:** unchanged — `harmony/voice/dm/{callId}/{device}` mesh, wildcard subscribe, self-segment filtered. `VoiceSender`/`VoiceReceiver`/`VoiceMixer` reused verbatim.
- **Media key:** `K_voice = derive_dm_voice_key(content_key, callId)`, unchanged. A `GroupDm`'s single shared `content_key` means every member derives the identical key from the caller's `callId` — no per-member keys, no key exchange. Packet AEAD (`ChaCha20-Poly1305`, AAD = `domain ‖ callId`) is byte-identical to the 1:1 path.
- **Signaling message:** add an **optional `space_id`** to the signed `VoiceSignal`. 1:1 invites omit it (`None` → not serialized in canonical CBOR → **existing 1:1 fixture bytes unchanged**). Group invites set it; the callee's handler, when `space_id` is present, resolves *that* space (verifying `caller ∈ members` and `content_key.is_some()`) instead of scanning for a 2-member DM — lifting the `members.len() == 2` gate for the group path only.

## Backend changes (Rust)

New IPC commands (`#[tauri::command]`, registered in `generate_handler!`):

- `place_group_call(spaceId) -> callId` — verify space is `GroupDm` & caller is a member; mint `callId`; fan the sealed GroupInvite to every *other* member's `harmony/voice-signal/{member}` topic.
- `watch_group_call(spaceId)` / `unwatch_group_call(spaceId)` — **read-only** presence subscription for the banner. Starts/stops a subscriber on `harmony/voice-presence/group-dm/{spaceId}` that decrypts beacons (with the derived presence key) and emits `group-call-presence-changed`, *without* publishing a beacon. The frontend calls `watch` when a group DM is opened (so a not-in-call member sees the active-call banner) and `unwatch` when it's closed.
- `join_group_call(callId, spaceId)` — derive `K_voice`, subscribe the media wildcard, start the presence **publisher** + media subscriber (mirrors `join_dm_call` + presence publisher). The read subscription started by `watch_group_call` is reused for the in-call roster.
- `leave_group_call(callId, spaceId)` — publish a `left` presence tombstone, tear down media + the presence publisher (the read subscription persists if the DM is still open).
- `send_group_voice_frame` / `set_group_call_muted` — thin variants of the DM equivalents (the muted bit also updates the presence beacon).
- `decline_group_call(callId, spaceId)` — seal a `Decline` back to the caller only (parity; lets the caller's roster show "declined" before the 30 s timeout).

Generalized invite handler (`event_loop.rs` ~1637): when the decoded `VoiceSignal` carries `space_id`, resolve that space directly and emit `incoming-group-call { callId, callerOwner, spaceId }`; the `space_id`-absent 1:1 branch is unchanged.

Presence publisher/subscriber (new, modeled on the community-voice presence tasks): a periodic task publishes the signed+sealed beacon to `harmony/voice-presence/group-dm/{spaceId}`; a subscriber materializes the roster and emits `group-call-presence-changed`. The beacon signing/sealing/eviction logic is reused; the topic + presence-key derivation are the deltas.

New events: `incoming-group-call`, `group-call-presence-changed`, `group-call-declined`, `group-voice-frame-received` (parallel to `dm-voice-frame-received`, keeps media paths cleanly separated). Reused as-is: `voice-transport-lost`/`voice-transport-restored` (already `callId`-keyed).

## Frontend (`GroupCallSession` + UI)

New controller `src/lib/group-call-session.ts` — per-identity singleton (mirrors `getCallSession`; rebuilds on identity change, destroying live media):

- **State:** `{ phase, callId, spaceId, participants, muted, pttMode, pttHeld, deafened, startedAt, reconnecting }`, where `Participant = { ownerHex, deviceHex, muted, speaking, displayName?, avatarUrl?, state: 'in-call' | 'ringing' | 'declined' }`. The roster merges the presence beacons (`in-call`) with the group's full membership (the rest as `ringing`/`declined`).
- **Verbs:** `placeGroupCall(spaceId)` (mint+join, drop-in), `onIncomingGroup(callId, caller, spaceId)`, `accept()`, `decline()`, `joinActive(callId, spaceId)`, `leave()`. Remote handlers: `onPresenceChanged`, `onDeclined`, transport lost/restored.
- **Media core:** ported from `CallSession`/`VoiceSession` (sender/receiver/mixer/talk-gate/drain, `group-voice-frame-received` filtered by `callId`), plus the VoiceSession-style roster refresh. Mute/PTT/deafen keep the local-rollback-on-reject behavior.

App.svelte wiring (alongside the existing `incoming-call` listeners): `incoming-group-call` → `onIncomingGroup` + ring toast + `incomingCallAlerter.notify(...)`; `group-call-presence-changed` → `onPresenceChanged`; `group-call-declined` → `onDeclined`.

UI (mostly reuse):

- **Ring toast** — reuse the incoming-call toast; body "Alice is calling *{group name}*", accept/decline.
- **In-call bar + participant tiles** — reuse the existing N-party tiles; ringing/declined members render greyed with their state.
- **Group-DM header button** — "Call" when idle; **"Join call"** when the banner shows an active call.
- **Active-call banner** — a small new component in the group-DM view, driven by `group-call-presence-changed`. The group-DM view calls `watch_group_call(spaceId)` on mount and `unwatch_group_call(spaceId)` on unmount so the banner reflects an active call even for a member who was never rung.
- **OS notification** — the ZEB-356 `IncomingCallAlerter` is call-shape-agnostic, so incoming group-call escalation is a straight reuse.

## Non-goals (v1)

- No moderation (mute/kick others) — flat peer group (D8).
- No cross-DM "call active" sidebar badge across *unopened* DMs (needs always-on presence subscription to every group DM); the banner shows inside the opened DM only.
- No "end for everyone" — last-one-out ends it (D1).
- No video/screen-share.
- No call history / missed-call surface — that is the separate ZEB-357.

## Testing

- **Rust wire-format fixtures:** new pinned fixture for the `space_id`-present GroupInvite signal + the group-DM presence beacon (sealed under the derived presence key). The existing 1:1 invite fixture stays **byte-identical** (back-compat regression guard).
- **Rust multi-engine integration:** a 3-engine test — `place_group_call` fans invites; two callees join; the presence roster materializes to 3; media frames seal/relay/open across all three under the shared `K_voice`; a leave tombstones correctly; last-leave clears the roster. Negatives: wrong-`callId` key binding fails; a non-member can't join.
- **Frontend vitest:** `group-call-session.test.ts` — drop-in place, incoming accept/decline, join-in-progress, roster merge (in-call/ringing/declined), mute/PTT/deafen rollback-on-reject, leave/last-leave, identity-switch rebuild, transport-reconnect flag.
- **Gates:** `cargo fmt` / `clippy -D warnings` / `nextest` (with `--features test-fixtures`), `tsc` / `vitest`, MSRV.

## Manual smoke checklist (documented, not unit-tested)

1. 3-device place → ring → accept on two → all three hear each other.
2. Decline on one device → caller's roster shows "declined".
3. Join-in-progress: a 4th member opens the group DM mid-call → sees the banner → Join → joins the room.
4. Last-leave: everyone leaves → banner clears in the group DM for all.
5. Mute/PTT/deafen each work; deafen implies self-mute.
6. Transport blip → "Reconnecting…" shows then clears; audio resumes.
7. Busy: placing a group call while in a community voice channel is blocked (and vice versa).
