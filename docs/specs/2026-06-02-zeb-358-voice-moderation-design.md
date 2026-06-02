# ZEB-358 — Community voice moderation design

**Status:** Approved (brainstorm 2026-06-02)
**Epic:** [ZEB-348](https://linear.app/zeblith/issue/ZEB-348) (voice comms) — follow-on after V1–V5 (ZEB-349…353) merged.
**Linear:** [ZEB-358](https://linear.app/zeblith/issue/ZEB-358)
**Prior art reused:** ZEB-350 (presence beacons), ZEB-351 (voice session/roster), ZEB-339 (enrolled-device-key power verification), ZEB-352 (signed+sealed signal pattern), ZEB-248/217 (community power levels).

## Goal

Give community moderators **power-gated server-mute and remove-from-voice** controls for participants in a community voice channel. Both are **ephemeral, voice-only** actions — they never touch the durable membership CRDT (that's the heavyweight community-Ban escalation) and never re-key the channel. Moderation is a signed side-channel plus a receiver-side drop/hide filter.

## Settled decisions (from the brainstorm)

| # | Decision | Choice |
|---|----------|--------|
| D1 | **Enforcement model** | **Honest-client receiver-side.** A power-signed directive that every honest client obeys: drop the target's audio + hide/flag them in the roster. A malicious/modified target client can keep emitting, but no honest peer hears it. No `ChannelKey` rotation. Residual gap (a coalition of hacked clients) is documented, not solved. |
| D2 | **Kick stickiness** | **Sticky for a cooldown.** Honest clients keep a kicked owner suppressed for a cooldown window (issuer-driven), not just a momentary disconnect. Ephemeral (in-memory, never written to the CRDT); text access untouched. |
| D3 | **Server-mute lifetime** | **Time-boxed cooldown** (default 5 min, same as kick). A server-mute auto-expires after its window; a mod re-applies to extend. Self-limiting, zero dangling state. A server-mute always blocks the target's *self*-unmute (otherwise it isn't a mod mute). **On exit from server-mute (expiry *or* a mod Unmute) the target's client falls back to a local self-mute** — the user must explicitly opt back in to transmit. This protects the *unattended-mic* case: an absent user's mic never auto-resumes transmitting just because the server-mute lapsed. |
| D4 | **Architecture** | **Approach 1 — dedicated signed-directive control plane.** New `voice_moderation.rs` module + a new per-channel Zenoh control topic, reusing the presence/power/verify primitives. Mute and kick are the *same* primitive: a signed, time-boxed directive honest clients obey and that lapses on its own. |

Mute and kick unify into one directive type because D1–D3 make both "a signed, time-boxed instruction that honest clients enforce receiver-side."

## Why this is tractable (existing ground truth)

- **Presence layer** (`src-tauri/src/voice_presence.rs`): a signed (device #2) + `ChannelKey`-sealed beacon `{owner[16], device[32], muted, joined_hlc, seq, left}` on `harmony/voice-presence/{community}/{channel}`, 4 s heartbeat / 12 s TTL. The subscriber already verifies signature **and** membership (`beacon_signer_is_member`). `muted` already rides in the beacon. The roster (`RosterEntry { owner, device, muted }`) is emitted as `voice-presence-changed`.
- **Power layer** (`src-tauri/src/community_membership.rs`): `PowerThresholds { invite: 0, kick: 50, set_power: 100, max: 100 }`; `MaterializedMembership.power_levels: BTreeMap<OwnerAddr, u8>` (default 0); the `actor_power > target_power` rule; the ZEB-339 enrolled-device-key → Ed25519 `verify_membership_signer` path; `EnrolledDeviceKey { owner, device_ed25519[32] }`.
- **The cryptographic reality:** every member holds the shared `ChannelKey`, so a muted/kicked client *can* keep emitting sealed audio. We cannot prevent that without re-keying. Voice mute is therefore inherently **receiver-side** — we make honest clients drop the target. (Hard crypto-exclusion = the existing persistent community Ban, deliberately out of this plane.)
- **No existing per-channel control topic** beyond presence, and **no per-member UI controls** today (tiles are display-only) — both are added here.

## Architecture & units

**New module** `src-tauri/src/voice_moderation.rs` (mirrors `voice_presence.rs`): the directive wire type, sign/seal/open, signature + authority verification, and the in-memory `ActiveModeration` enforcement map with `apply()` + `sweep()`. Pure — no Zenoh, no Tauri — and fully unit-testable.

**Touched files:**
- `src-tauri/src/voice.rs` — `VoiceChannelRequest::Moderate {…}`, `ModerateVoicePayload` IPC struct, `ModAction` enum.
- `src-tauri/src/event_loop.rs` — on join, spawn a **control-topic subscriber** + an **issuer re-assertion task** beside the existing presence sub/pub; handle the `Moderate` request (build→sign→seal→publish→track); thread `ActiveModeration` into the **media subscriber** (drop), the **roster emission** (hide kicked, flag mod-muted, annotate power), and **self-gating**.
- `src-tauri/src/lib.rs` — `#[tauri::command] moderate_voice` + registration.
- `src/lib/voice-session.ts` — moderation fields on roster/session, a `moderate()` method, self-target handling, reading the enriched presence payload.
- `src/lib/components/VoiceChannelView.svelte` — power-gated per-member controls, a mod-muted indicator distinct from self-mute, self banners.

**Tests:** new `src-tauri/tests/voice_moderation_integration.rs`; a canonical-CBOR wire fixture; frontend additions to `voice-session.test.ts` + `VoiceChannelView.test.ts`.

**Data flow (happy path):**
```
Mod clicks "Mute X"
  → FE moderate(community, channel, targetOwner='X', 'mute')
  → IPC moderate_voice
  → backend: build VoiceModerationDirective → sign(device #2) → seal(ChannelKey)
            → publish harmony/voice-control/{c}/{ch}  + add to issuer re-assert set
  → ALL clients' control subscriber: open → verify sig → verify authority
            → ActiveModeration.apply()
            → re-emit roster (X flagged modMuted; kicked → hidden; +power annotations)
            → if self==X: surface self-moderation state
  → media subscriber: drop any frame whose sender device maps to a moderated owner
  → sweep task: expire by enforce_until → re-emit roster / clear self-state
```

## The directive — wire type, crypto & authority

**Wire type** (follows the presence beacon shape and the `ChannelKind` u8-tag precedent):

```rust
pub enum ModAction { Mute, Unmute, Kick, Unkick }   // (de)serializes as a u8 tag 0..=3

pub struct VoiceModerationDirective {
    actor_owner:  [u8; 16],   // "ao" moderator's OwnerAddr
    actor_device: [u8; 32],   // "ad" moderator's device #2 verifying key (verifies the sig)
    target_owner: [u8; 16],   // "to" the person being moderated — covers ALL their devices
    action:       ModAction,  // "ac"
    issued_hlc:   Hlc,        // "ih" last-writer-wins ordering primary
    seq:          u64,        // "sq" LWW tiebreak within one actor
}
pub struct SignedVoiceModerationDirective {
    directive: VoiceModerationDirective,   // "dr"
    sig:       [u8; 64],                   // "sg" Ed25519 over canonical-CBOR(directive)
}
```

- Moderation targets the **owner** (person), not a device — honest clients drop *all* of that owner's devices.
- **No `expires_at` on the wire.** Liveness is a rolling TTL refreshed by re-assertion (see lifecycle) — dodges cross-client clock skew exactly like the presence TTL.
- **Sign:** the moderator's device #2 Ed25519 key signs `canonical_cbor_encode(directive)`.
- **Seal:** ChaCha20-Poly1305 under the channel's `ChannelKey`, `AAD = b"harmony-voice-moderation-v1" ‖ community_id ‖ channel_id`, random 12-byte nonce prepended → published on the control topic. (Identical framing to presence.)

**Authority verification on receive (in order):**
1. **Open** under `ChannelKey` with the moderation AAD; drop on failure. *(Only members hold the key — confidentiality + a coarse first membership gate.)*
2. **Verify sig** via Ed25519 over the canonical CBOR using `actor_device`; drop on failure.
3. **Bind actor to membership:** confirm `actor_device ∈ actor_owner.enrolled_device_keys` **and** `actor_owner` is a joined member — the same resolution `beacon_signer_is_member` uses against materialized membership. *(Stops a member forging another member's identity.)*
4. **Power gate:** `power(actor_owner) ≥ kick (50)` **and** `power(actor_owner) > power(target_owner)`. *(Reuses `power_levels` + the existing `actor_power > target_power` rule — can't moderate a peer or a superior; self-moderation is impossible since power isn't `>` itself.)*
5. **Accept** → `ActiveModeration.apply()`.

The issuing client applies its own directive through the same receive path (loopback), so there is one enforcement code path. A canonical-CBOR fixture pins the directive wire format (byte-identity), matching every other voice wire type.

## Transport & lifecycle

**Control topic:** `harmony/voice-control/{community}/{channel}` — a subscriber + the issuer's re-assert publisher, spawned on join next to presence.

**Liveness = rolling TTL (mirrors presence):** a receiver enforces a directive for `ENFORCE_TTL = 12s` from last receipt; the issuing moderator re-publishes each active directive every `RE_ASSERT_INTERVAL = 4s` (same cadence/constants as presence), always carrying the *original* `issued_hlc/seq`. Nothing wall-clock-absolute crosses the wire → no clock-skew exposure.

**Moderation duration** is chosen by the moderator (default **5 min for both mute and kick**) and enforced **issuer-side** — the re-assert task runs for that long, then stops, and the directive lapses on every client within `ENFORCE_TTL`. A longer mute is just a longer re-assert loop; *permanent* removal remains the existing community-Ban escalation.

**Early lift (Unmute / Unkick):** the moderator stops re-asserting and broadcasts a revocation with a fresh, higher `(issued_hlc, seq)`; it is re-asserted a few times across one TTL window for reliable delivery, then dropped. Clients that miss the explicit revoke still lapse within `ENFORCE_TTL` once the positive re-assert stops — self-healing.

**Ordering / multi-moderator conflicts:** `ActiveModeration` tracks **two independent classes per target** — mute-class `{Mute, Unmute}` and kick-class `{Kick, Unkick}` — each resolved **LWW by `(issued_hlc, seq)`** (HLC is a total order across moderators). A client **yields**: it stops re-asserting its own directive for a `target+class` the instant it observes a strictly-higher-ordered directive there, so a senior mod's Unmute cleanly overrides a junior's re-asserted Mute.

**Late-join:** a joiner subscribes to the control topic on join and converges on all active directives within ≤ one re-assert interval (~4 s). Bounded and documented.

**Kick + rejoin-suppression (anti-flap):**
- While a Kick is active for target T, the **presence subscriber** suppresses T's beacons (T never enters anyone's visible roster) and the **media subscriber** drops T's frames.
- The **kicked client itself** tears down its mic sender + presence publisher (goes silent + disappears) **but keeps its channel subscribers alive**, so it keeps receiving re-asserts (its own kicked-state stays refreshed for the cooldown) and will hear an Unkick. UI: "You were removed by a moderator."
- When the kick lapses (issuer stopped re-asserting ⇒ cooldown elapsed) or an Unkick arrives, the client clears the state and re-enables a **manual Rejoin button** — it never auto-rejoins, so there is no kick→rejoin→kick flapping.

## Enforcement state, roster/power annotation & IPC (backend)

**`ActiveModeration`** (`voice_moderation.rs`, pure + unit-tested):
```
ActiveModeration: (SpaceId, ChannelId) → target_owner[16] → TargetState
TargetState { mute: ClassState, kick: ClassState }
ClassState { latest: (Hlc, seq), enforced: bool, enforce_until_ms: u64 }
```
- `apply(directive, now_ms)`: LWW by `(issued_hlc, seq)` within the directive's class. A strictly-newer directive flips `enforced` (Mute→on / Unmute→off) and, for re-asserts of the current latest, refreshes `enforce_until_ms = now + ENFORCE_TTL`. Strictly-older directives are ignored. The "off" tombstone retains `latest` so a delayed older Mute cannot resurrect (presence-gravestone pattern), GC'd after `enforce_until + grace`.
- `sweep(now_ms)`: lapses entries past `enforce_until_ms`; returns changed targets so the loop re-emits.
- Queries: `is_muted(c, ch, owner, now)`, `is_kicked(c, ch, owner, now)`.

**Media drop** (existing media subscriber `harmony/voice/{c}/{ch}/{deviceHex}`): before emitting `voice-frame-received`, resolve the sender → owner and drop the frame if that owner `is_muted` or `is_kicked`. Resolution uses the **full 32-byte device hex** carried in the media topic's last segment, looked up via `VoicePresenceMap::owner_for_device`. The presence map retains every verified `device → owner` mapping — including kicked owners, which are hidden only from the *visible* roster, never from this lookup — so a kicked owner's media is still resolvable and droppable. (The 16-byte `sender_hash = VK[0..16]` used for frontend speaking-correlation is a separate concern and never touches media-drop.)

**Roster + power annotation** (at emit time in `event_loop`, where roster + registry + `ActiveModeration` are all in scope), the `voice-presence-changed` payload is enriched to:
- omit `is_kicked` owners entirely (suppressed);
- add per surviving entry `modMuted: bool` (from `ActiveModeration`) and `power: u8` (from `materialized().power_levels`, default 0);
- add top-level `selfPower: u8`, `selfModMuted: bool`, `selfKicked: bool`.

The FE gates controls purely from this payload — authority logic stays server-side; the FE renders what it is told.

**Self-state:** when `ActiveModeration` changes for *my* owner, the enriched roster event carries the updated `selfModMuted`/`selfKicked`. On `selfKicked` going true the backend tears down my mic sender + presence publisher but **keeps my channel subscribers alive** (anti-flap, above).

**IPC** (`lib.rs`):
```rust
#[tauri::command]
async fn moderate_voice(payload: ModerateVoicePayload /* { communityId, channelId,
    targetOwnerHex, action: "mute"|"unmute"|"kick"|"unkick", durationMs? } */, …)
    -> Result<(), String>
```
→ `VoiceChannelRequest::Moderate {…}`. The loop **pre-checks authority locally** (self `power ≥ 50` and `> target power`; target is a current roster member; target ≠ self) and returns `Err` immediately for instant moderator feedback — then builds (`issued_hlc` via `reserve_next_hlc_for_device`, `seq` from a moderation counter), signs with the device-#2 `community_signing_key`, seals under `ChannelKey`, publishes once, and registers it in the issuer re-assert set (duration = `durationMs` or the default). All receivers independently re-verify per the authority section — the local pre-check is only UX.

## Frontend UX

**`voice-session.ts`:** `RosterMember` gains `modMuted: boolean` + `power: number`; `VoiceSessionState` gains `selfPower: number`, `selfModMuted: boolean`, `selfKicked: boolean`. The `subscribePresence` handler maps the enriched payload through. New `moderate(targetOwnerHex, action)` → `moderate_voice`. Two self-target behaviors:
- **`selfModMuted`** forces `muted` true and makes `setMuted(false)` a no-op — a server-mute can't be self-cleared. **When `selfModMuted` clears (expiry *or* a mod Unmute) the client stays locally self-muted** (it sets `muted = true` and publishes a self-mute presence beacon) and requires an explicit user unmute before transmitting — so an unattended mic never auto-resumes. The roster then shows the ordinary self-mute 🔇, not the mod-muted badge.
- **`selfKicked`** drops the session into a *kicked* UI state (subscribers stay alive); a manual Rejoin is gated until it clears.

**`VoiceChannelView.svelte`:**
- Per-member mod controls render only when `selfPower ≥ 50 && selfPower > m.power && m.ownerHex !== selfOwnerHex`:
  - **Mute / Unmute** (toggles on `m.modMuted`) — *no confirm* (low-risk, reversible).
  - **Remove** (kick) — *click-confirm* (a second confirming click at a different position), per the tier-confirmation-to-severity rule: severe but reversible.
- **Mod-muted badge** — a distinct shield+mute glyph, visually separable from the self-mute 🔇, so observers can tell a server-mute from a self-mute.
- **Self banners:** `selfModMuted` → `role="status"` "You've been muted by a moderator" + the unmute control disabled with a tooltip; `selfKicked` → `role="alert"` "You were removed by a moderator" with a Rejoin disabled until it clears.

## Error handling & edge cases

- **Insufficient power:** IPC returns `Err` → FE shows a transient inline notice; the control optimistically does nothing.
- **Target left before the directive lands:** harmlessly lapses (nothing in roster to enforce); the issuer's re-assert runs out its duration or yields.
- **Moderator leaves mid-mute:** re-assertion stops → the directive lapses within `ENFORCE_TTL` on all clients. This is the documented "needs a moderator present" property of ephemeral enforcement; another mod can re-issue.
- **Two moderators race (mute vs unmute):** LWW by `(issued_hlc, seq)` + the yield rule → no ping-pong.
- **Brief media leak from an unknown device during a roster race:** a frame may play for ≤ a few seconds until the device→owner index catches up. Documented minor.
- **Forged / non-member / cross-channel-replay directive:** dropped at verify steps 1–4 (AAD binds community+channel; the membership + power gates reject non-authorized signers).
- **Self-kicked client:** manual Rejoin only; no auto-rejoin → no flap.
- **Unattended mic on mute-expiry:** when a server-mute lapses (or a mod lifts it), the target stays locally self-muted and must opt back in to transmit — an absent user's mic never auto-resumes. The transition is driven purely by the target's own client; other clients simply stop dropping the (now silent) target.

## Testing

- **Rust unit** (`voice_moderation.rs`): `ModAction` u8 round-trip; sign/verify; seal/open + wrong-channel-AAD reject; authority matrix (non-member rejected, member-without-power rejected, `power`-not-`>`-target rejected, valid accepted); `ActiveModeration` LWW (newer wins, older ignored, re-assert refresh, unmute-tombstone-blocks-stale-mute) + sweep expiry.
- **Wire fixture:** canonical-CBOR byte-identity pin for a sample directive (under `tests/wire_format_*`, `test-fixtures` feature).
- **Multi-engine integration** (`voice_moderation_integration.rs`):
  1. Mod mutes target → a third engine drops the target's media + roster shows `modMuted`; target's `selfModMuted` set.
  2. Kick → target evicted from third-party roster + media dropped + target `selfKicked`; a rejoin attempt is suppressed during re-assert; after re-assert stops, it lapses.
  3. Early Unmute / Unkick propagates.
  4. Non-mod directive rejected (no effect).
  5. `actor_power` not `>` `target_power` rejected.
  6. Expiry lapse after re-assert stops.
- **Frontend** (vitest):
  - `voice-session.test.ts`: enriched payload maps `modMuted`/`power`/`selfPower`/`selfModMuted`/`selfKicked`; `setMuted(false)` suppressed while `selfModMuted`; **`selfModMuted` clearing leaves the client locally self-muted (`muted` stays true, not transmitting) and requires an explicit unmute**; `moderate()` invokes the IPC; `selfKicked` enters and clears the kicked state.
  - `VoiceChannelView.test.ts`: controls visible only when power-gated; Mute toggles via `moderate`; Remove requires confirm; mod-muted badge renders; `selfModMuted` disables the unmute control + shows the note; `selfKicked` shows the alert + a disabled Rejoin until cleared.

## Non-goals (v1)

- No CRDT / persistent moderation — the community **Ban** is the permanence escalation.
- No `ChannelKey` rotation / cryptographic exclusion (out of this plane by D1).
- No per-device targeting — moderation is owner-level (covers all a person's devices).
- No moderation audit log / history (possible follow-up).
- No duration-picker UI — backend defaults; the IPC accepts `durationMs` for a later UI.
- No "move to another channel" (the epic mentioned it as optional; deferred).

## Security notes

- Enforcement is **honest-majority**: a single hacked target client cannot make itself heard against honest peers, but a coalition of modified clients can ignore directives among themselves. True exclusion requires re-keying (community Ban), which is intentionally out of scope.
- **Media-drop sender identity is unauthenticated (known limitation).** The voice media path (ZEB-35 engine, V1–V5) seals frames under the shared `ChannelKey` with no per-sender signature; a receiver identifies the sender only by the `harmony/voice/{c}/{ch}/{deviceHex}` topic suffix, which the sender controls. So media-drop reliably silences an **honest** muted/kicked client (it publishes under its true device) and any accidental/standard-client case, but a **modified** client could publish under a random or impersonated suffix to evade the drop. This is consistent with the honest-majority model above (a modified client is already outside the guarantee). A complete fix — per-sender authenticated media so the receiver can cryptographically bind a frame to its owner — is a larger cross-cutting follow-on against the voice engine, tracked separately, not part of this PR.
- Confidentiality: directives are sealed under the `ChannelKey`, so non-members can't read who is being moderated.
- Authority is verified independently by **every** receiver (not just trusted by the issuer), reusing the ZEB-339 enrolled-device-key + power path — the local pre-check in the IPC is only for moderator UX.
