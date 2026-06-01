# Voice Comms — Community Voice Channels + 1:1 DM Calls — Design Spec

**Date:** 2026-05-31
**Status:** Approved (brainstormed + approved with Jake 2026-05-31)
**Linear:** epic **ZEB-348**; sub-tickets **ZEB-349** (V1), **ZEB-350** (V2), **ZEB-351** (V3), **ZEB-352** (V4), **ZEB-353** (V5). Prior art: ZEB-35 = the engine, ZEB-152 = Spellbook voice, ZEB-153 = voice TS-error debt.

---

## Goal

Add real-time peer-to-peer **voice** to harmony-client in two forms:

1. **Community voice channels** — Discord-style 🔊 channels members join to talk, full-mesh, soft-capped at **64** participants.
2. **1:1 DM voice calls** — ring/answer calls between two people.

Talk model: **open-mic + VAD + mute** by default, with a per-user **push-to-talk (PTT)** option. **Start muted on connect** in all cases (defense-in-depth against accidental broadcast).

## Why this is tractable

The **voice engine already exists and is merged** (ZEB-35, PR #36/#38) but is currently wired only into Spellbook (`FlashcardView`/`PttButton`), **not** comms:

- **Audio pipeline** — `src/lib/voice/`: Opus (16 kHz/20 ms) + codec2 codecs, `audio-capture` (AudioWorklet), `adaptive-jitter-buffer`, `comfort-noise`, a 23-byte `voice-packet` format, `voice-sender`/`voice-receiver` — all tested.
- **Transport relay** — `send_voice_frame` / `join_voice_channel` / `leave_voice_channel` IPCs exist + are registered (`src-tauri/src/lib.rs:11595/11617/11635`); the event loop already relays frames over Zenoh per-sender topic `harmony/voice/{channel}/{sender}` → `voice-frame-received` (`src-tauri/src/event_loop.rs:2316+`); types in `src-tauri/src/voice.rs`.
- **NAT traversal solved** — iroh QUIC + STUN + relay (`iroh_endpoint.rs`); members already exchange reachability records (`reachability_record.rs` / `reachability_resolver.rs`).

The work is the **channel/call layer** on top of this engine, not the engine.

## What already exists that we reuse (verified)

- **Channel CRDT** (`src-tauri/src/community_membership.rs`): `ChannelCreate { channel_id, name, write_power }`, `ChannelModify`, `ChannelDelete`; materialized `ChannelInfo { name, write_power, created_at, deleted_at }`. Power-gated creation. IPCs `create_channel`/`modify_channel`/`delete_channel` (`lib.rs:13176/13405/13579`).
- **Channel crypto** (`src-tauri/src/community_channel_log.rs`): `ChannelKey([u8;32])`, `derive_channel_key(EpochKey, community_id, channel_id)` (HKDF-SHA256, zeroize-on-drop), `encrypt_channel_packet`/`decrypt_channel_packet` (`[12B nonce][ChaCha20-Poly1305(..., AAD)]`). Engine holds the `Arc<ChannelKey>` (`community_channel_log_engine.rs`). **Channel text is already E2E-encrypted under `ChannelKey`; voice reuses the same key + AEAD.**
- **DM crypto** (`src-tauri/src/dm_signing.rs`): X25519 (ephemeral + static) + ChaCha20-Poly1305 sealing to an owner; device-#2 signing. DM calls seal under the DM key.
- **Frontend channel UI**: `ChannelSubSidebar.svelte` (channel rail), `CreateChannelDialog.svelte`, `CommunityView.svelte` (selects active channel → renders `ChannelMessageFeed.svelte`), `community-service.ts` `ChannelInfo` type, `ChannelMembersPanel.svelte` (right panel).

## Decisions (brainstormed + approved)

| # | Decision | Choice |
|---|---|---|
| D1 | Scope | Community voice channels **and** 1:1 DM calls |
| D2 | Talk model | Open-mic + VAD + mute default, per-user PTT option |
| D3 | Audio privacy | **Reuse `ChannelKey`** (channel) / DM key (calls); FS follows existing EpochKey rotation. No per-call rekey. |
| D4 | DM ringing | **Live signaling** (ring/answer/decline/timeout) |
| D5 | Presence/roster | **Ephemeral Zenoh beacons** (signed + sealed; never written to the CRDT) |
| D6 | Channel-view layout | **Hybrid** — stage grid that collapses to a compact list past ~12 participants |
| D7 | Voice channel text chat | **None** in v1 (voice-only; communities can pair their own text+voice channels) |
| D8 | DM ring surface | **Non-blocking toast** (banner) for v1; blocking-modal style is a future config toggle |
| D9 | In-call indicator | Persistent bottom **in-call bar** whenever connected (channel or DM) |
| D10 | Mic on connect | **Start muted** everywhere (channel join, DM accept, DM caller) — one-tap unmute |
| D11 | Second incoming call while busy | **Auto-decline(busy)** in v1 (no call-waiting) |
| D12 | Concurrent sessions | **One active session at a time** — joining a second leaves the first |

## Non-goals (v1)

- SFU / relay-node mixing (rejected — full-mesh ≤64 is sufficient; no privileged mixer).
- iroh-direct-QUIC per-peer transport (Zenoh path is uniform for v1; iroh-direct is a possible later DM-latency optimization).
- Blocking phone-style ring modal (deferred as a future config toggle — D8).
- Voice-channel-paired text chat (D7), call recording, video, screen-share.
- Per-call ephemeral rekey / per-call forward secrecy beyond the existing EpochKey rotation (D3).

---

## Architecture

### One data path, two scopes

```text
capture (AudioWorklet)
  → VAD gate / PTT / mute  [browser]
  → encode Opus|codec2     [browser]
  → send_voice_frame(scope, frame)         IPC
  → SEAL under scope key                    [Rust]
  → Zenoh session.put(topic, sealed)        [Rust]
        … mesh …
  → Zenoh subscriber                        [Rust]
  → OPEN under scope key (drop on failure)  [Rust]
  → emit voice-frame-received{sender,frame} IPC
  → per-sender jitter buffer → decode       [browser]
  → N-stream mix → AudioWorklet output      [browser]
```

The **browser owns codecs; Rust owns crypto + transport.** `ChannelKey`/DM key never cross into JS.

### Scopes, topics, keys

| Scope | Voice topic | Presence topic | Signaling topic | Seal key |
|---|---|---|---|---|
| Community channel | `harmony/voice/{community}/{channel}/{senderDevice}` | `harmony/voice-presence/{community}/{channel}` | — | channel `ChannelKey` |
| DM call | `harmony/voice/dm/{callId}/{senderDevice}` | (implicit: 2 parties) | `harmony/voice-signal/{calleeOwner}` | DM key |

Routing moves into the **topic** (publisher names its own `senderDevice`) so the **whole packet can be sealed** — unlike today's relay, which reads the sender hash from cleartext frame bytes (`event_loop.rs:2319`).

### AEAD seam

- New `encrypt_voice_packet`/`decrypt_voice_packet` (thin wrappers over the channel AEAD, **distinct AAD** `VOICE_AAD‖community‖channel` so a text packet can't be replayed as voice; for DM, `VOICE_DM_AAD‖callId`). `[12B nonce][ChaCha20-Poly1305]`.
- Outbound: the event-loop voice arm looks up the scope key (channel → ChannelLogEngine registry; DM → DM key), seals, publishes.
- Inbound: subscriber opens under the scope key; **AEAD failure (non-member / stale epoch / wrong call) → drop silently.**

---

## V1 — Channel typing

**CRDT wire** (`community_membership.rs`), additive exactly like ZEB-345's `profile_page_root`:

- `ChannelCreate` gains `kind: ChannelKind` (`enum ChannelKind { Text, Voice }`), serde code `"ck"`, `u8` tag (0 = Text, 1 = Voice), `skip_serializing_if = is_text`. **Text `ChannelCreate` stays byte-identical** (field omitted → existing wire fixtures untouched); only Voice carries the extra map entry.
- Materialized `ChannelInfo` gains `kind`. **Kind is immutable** — `ChannelModify` cannot change it. Creation power-gated identically to text (no new gate).

**Frontend:**

- `community-service.ts` `ChannelInfo` gains `kind: 'text' | 'voice'`.
- `create_channel` IPC gains `kind` param (default `'text'`; camelCase boundary `kind`).
- `CreateChannelDialog.svelte` — Text/Voice segmented control before the name field.
- `ChannelSubSidebar.svelte` — render a 🔊 glyph for voice channels (vs `#`).
- `CommunityView.svelte` — selecting a voice channel routes to `VoiceChannelView` (V3); in V1 it's a "voice channel" scaffold (roster placeholder, "Join" disabled-with-note until V2/V3).

**Tests:** wire fixture pinning a Voice `ChannelCreate`'s canonical bytes + asserting a Text one is byte-identical to the pre-change fixture; `ChannelKind` round-trip; FE dialog/sidebar rendering.

**Ships:** create + see voice channels (joinable scaffold, no audio yet).

## V2 — Presence + AEAD seam

**AEAD seam:** implement `encrypt_voice_packet`/`decrypt_voice_packet`; rework the event-loop voice arm (`event_loop.rs:2316+`) to seal outbound under the channel `ChannelKey` and open inbound, publishing to `harmony/voice/{community}/{channel}/{ownDevice}`. The `send_voice_frame`/`join_voice_channel`/`leave_voice_channel` IPCs (`lib.rs:11595/11617/11635`) and `voice.rs` types are reworked to carry `(community, channel)`.

**Presence beacons:**

- Topic `harmony/voice-presence/{community}/{channel}`; payload canonical-CBOR `{ owner, device, muted, joinedHlc, seq }`, **device-#2-signed and sealed under `ChannelKey`** (non-members can't enumerate the roster).
- Cadence: publish on join → **heartbeat every 4 s**; evict an entry after **12 s** of silence (3 missed) or an explicit `left: true` tombstone for instant removal.
- Rust `VoicePresence` map `(community, channel) → { device → { owner, muted, lastSeen } }`; emits `voice-presence-changed { community, channel, roster }` on any change + on the timeout sweep.
- `join_voice_channel` starts the presence publisher + subscribes voice + presence topics; `leave_voice_channel` sends the tombstone + unsubscribes.

**Tests:** AEAD round-trip + wrong-key-drops; beacon sign/seal round-trip; heartbeat/timeout eviction (logical-time test); two-engine presence-exchange integration (mirrors `community_reachability_two_engine_integration.rs`).

**Ships:** live roster + sealed relay proven (no mic capture in comms yet).

## V3 — Talk

**Voice-session controller** (`src/lib/voice/voice-session.ts` + a Svelte store): owns capture lifecycle, VAD gate, mute/PTT state, the existing `voice-sender`, an **N-stream `voice-receiver`+mixer**, and the presence heartbeat. **One active session at a time** (D12).

- **Join flow:** "Join" → mic-permission prompt (once) → connected **muted** (D10). Prominent one-tap unmute.
- **VAD:** energy threshold + ~200 ms hangover (no word-tail clipping). Below threshold → stop sending (DTX); receiver fills gaps via `comfort-noise`. **Mute** = hard stop overriding VAD. **PTT** mode = send only while held, ignoring VAD.
- **Mixer:** each remote sender → own jitter buffer → decode → summed into the AudioWorklet output with soft-clip (extends the current single-stream receiver to N streams).
- **Speaking indicator:** derived from per-sender inbound frame energy (self from local VAD).

**`VoiceChannelView.svelte`** (replaces `ChannelMessageFeed` when `kind === 'voice'`) — **hybrid layout (D6):** stage grid of avatar tiles (avatar, name, speaking ring, mute glyph) that **auto-collapses to a compact roster list past ~12 participants**; bottom control bar (Mute / PTT toggle / Deafen / Leave); header "🔊 {name} · N here". Voice-only (D7).

**Tests:** VAD gating (logical), mute/PTT overrides, N-stream mix unit tests, session-controller state machine, `VoiceChannelView` grid↔list threshold rendering, join-muted assertion.

**Ships:** **talk + hear each other in a community voice channel.**

## V4 — DM calls

**Signaling state machine.** `callId` = 16 random bytes (caller-generated). States: `idle → ringing(out) / incoming(in) → connecting → active → ended`.

- Signals on `harmony/voice-signal/{calleeOwner}` (each client subscribes to its own signaling topic while online), all **device-#2-signed + sealed under the DM key**: `invite{callId, caller, calleeDevice}` · `accept{callId}` · `decline{callId, reason}` · `cancel{callId}` (caller aborts pre-answer) · `end{callId}` (either hangs up post-connect).
- **Ring surface:** non-blocking **toast** (D8) — avatar + "Incoming call" + ✓/✗; auto-`decline(timeout)` after **~30 s** → caller sees "No answer."
- **Busy** (callee already in a session per D12) → auto-`decline(busy)` → "User is on another call." **Unreachable** (no signaling responder) → "User unavailable."
- **On accept:** both join `harmony/voice/dm/{callId}/*` (sealed under DM key), **start muted** (D10); the **in-call bar** (D9) appears.
- **Caller UI:** a "Call" button in the DM header; ringing state; cancel.

**Tests:** signaling state-machine transitions (invite/accept/decline/cancel/end/timeout/busy), seal/sign of signals, toast accept/decline wiring, in-call-bar mount/unmount, reuse of the V3 controller for a 2-party room.

**Ships:** 1:1 voice calls on DMs.

## V5 — Scale + polish

- **64 soft-cap:** join blocked when roster ≥ 64 ("voice channel full"). Full-mesh holds: 1 upstream, ≤63 downstream; DTX means only active speakers transmit.
- **Scale validation:** simulated N-publisher relay+presence test; documented watch on Zenoh fan-in at 64 (same transport-flake class as ZEB-347).
- **Polish:** speaking rings, **Deafen** (mute all inbound, implies self-mute), reconnect on transport blips, persistent in-call-bar wiring across navigation, mic-device-error surfacing, leave-on-app-close.

**Ships:** hardening to the 64-participant target.

---

## UI surfaces (approved via visual companion)

- **`VoiceChannelView`** — hybrid stage-grid↔roster-list (D6), voice-only (D7).
- **`CreateChannelDialog`** — Text/Voice selector.
- **`ChannelSubSidebar`** — 🔊 glyph + live participant count badge for voice channels.
- **DM "Call" button** in the DM header; **non-blocking ring toast** (D8) with ✓/✗.
- **Persistent in-call bar** (D9) — bottom, shown whenever connected (channel or DM): peer/channel label + timer + Mute / PTT / Leave; lets the user navigate the app while connected.

## Error handling

- Mic permission denied → cannot go live; can still listen (join muted, "mic blocked" note).
- AEAD open failure → drop frame silently (non-member / stale epoch).
- Presence heartbeat miss → roster eviction after 12 s (no hard error).
- Call invite to offline/unreachable peer → "User unavailable" (no responder / timeout).
- Transport blip → jitter buffer masks short gaps; session controller attempts re-subscribe; in-call bar shows "reconnecting…".
- Joining when roster ≥ 64 → "voice channel full," join refused.

## Testing strategy

- **Rust:** wire fixtures (Voice `ChannelCreate` byte-pin + Text byte-identical), AEAD voice-packet round-trip + wrong-key-drop, presence beacon sign/seal + heartbeat/timeout (logical time), signaling state-machine, two-engine presence + relay integration (mirror `community_reachability_two_engine_integration.rs`).
- **Frontend:** VAD/mute/PTT logic, N-stream mixer, session-controller state machine, `VoiceChannelView` grid↔list threshold, ring-toast + in-call-bar wiring, join-muted invariant.
- **Scale:** simulated N-publisher test; manual multi-machine (append to `ZEB-224` manual checklist).
- **All gates:** fmt / clippy / nextest / large-tests / MSRV / frontend.

## Decomposition → Linear tickets (filed after approval)

- **Epic:** "Voice comms — community voice channels + 1:1 DM calls."
  - **V1** Channel typing (`kind: Text|Voice` CRDT + dialog/sidebar/routing). *One PR.*
  - **V2** Presence + AEAD seam (ephemeral signed+sealed beacons + `ChannelKey` seal/open + IPC rework). *One PR.*
  - **V3** Talk (session controller, VAD/mute/PTT, N-stream mix, `VoiceChannelView` hybrid). *One PR — the big one.*
  - **V4** DM calls (signaling state machine + ring toast + in-call bar). *One PR.*
  - **V5** Scale + polish (64 cap, scale test, deafen, reconnect). *One PR.*

Critical path V1 → V2 → V3 = "voice channels work." V4 reuses V3. V5 hardens.

## Open risks

- **Zenoh fan-in at 64** — the one genuinely unvalidated scaling assumption (64 publishers × 64 subscribers at 20 ms). Mitigated by DTX (only active speakers transmit) and the soft cap; validated in V5. If it doesn't hold, the fallback is a smaller cap or a later SFU (out of scope now).
- **Browser N-stream decode+mix CPU** at 64 — expected fine (Opus decode is cheap; DTX limits concurrent active streams), validated in V5.
