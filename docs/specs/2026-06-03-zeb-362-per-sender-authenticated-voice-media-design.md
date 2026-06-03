# ZEB-362: Per-sender authenticated voice media — design

- **Linear:** ZEB-362 (parent epic ZEB-348 — voice comms)
- **Origin:** ZEB-358 (voice moderation) PR #179 review, CodeRabbit rated *Critical*
- **Branch:** `zeb-362-per-sender-authenticated-voice-media` off `origin/main` `7a01a16`
- **Date:** 2026-06-03
- **Scope:** community voice media only (the `encrypt_voice_packet`/`decrypt_voice_packet` pair). DM 1:1 + group-DM call media is explicitly out of scope.

## Problem

Community voice frames are sealed with ChaCha20-Poly1305 under the **shared** channel key
(`derive_channel_key` → `ChannelKey`), with AAD = `domain ‖ community ‖ channel`. The sender's
identity is **not bound into the packet at all**. A receiver decides *who sent a frame* purely from
the **Zenoh topic suffix** — `key.rsplit('/').next()` → device-hex → `VoicePresenceMap::owner_for_device`
(`event_loop.rs:2980`). That suffix is sender-controlled.

Because every channel member holds the same `ChannelKey`, any member can seal a valid frame and publish
it under **any** topic suffix:

- publish under an **unknown** suffix → today the moderation media-drop *fails open* and the frame plays;
- publish under **another member's** device suffix → impersonate that member's audio / attribution;
- publish under their **own** truthful suffix but never having announced presence → resolves to `None`,
  *fails open*.

The ZEB-358 moderation enforcement (server-mute / kick → drop that owner's media) therefore reliably
silences only an **honest** client. A modified client can evade the drop by lying about its suffix. This
is the one place the moderation guarantee leans on an unauthenticated value. It is documented as a known
limitation in `docs/specs/2026-06-02-zeb-358-voice-moderation-design.md` §Security notes.

The 23-byte cleartext header's `senderHash` (`= device VK[0..16]`) is likewise just an unauthenticated
routing hint for the receiver's per-sender jitter buffer.

## Goal

Give community voice media frames a **cryptographically verifiable sender identity**, so a receiver can
bind every frame to its true owner device and enforce mute/kick (and speaking attribution) **without
trusting the topic suffix**.

## Threat model

- **In scope:** a *member* of the channel (holds `ChannelKey`) running a modified client that lies about
  its identity — to evade a mute/kick, or to impersonate another member's audio/attribution.
- **Out of scope (already covered):** non-members (cannot decrypt — no `ChannelKey`); the honest-majority
  assumptions that govern the rest of the voice epic; transport-layer confidentiality (AEAD already covers
  it). Forward secrecy beyond the existing EpochKey rotation is unchanged.

## Why only an asymmetric signature works

Inside a group sharing **one symmetric `ChannelKey`**, every member can forge any symmetric construction:
a per-session MAC, a value "keyed off the presence beacon", or AAD-binding the sender device are all
reproducible by every other key-holder. None of them authenticate *one* member to the others. Only a
signature against the sender's **device private key** does. The design question is therefore not "signature
vs. something cheaper" but "how do we make signatures cheap enough" — and **DTX answers that**: only active
speakers transmit (typically 1–3 at once, never all 64), so per-frame signing/verification is bounded to a
handful of streams in practice.

(Approaches considered and rejected: a per-talk-spurt signed hash chain — sound but fragile on a lossy,
reordering medium, not justified once DTX tames the per-frame cost; AAD-binding the device alone —
insufficient because the shared key makes it forgeable, so it is *folded into* the chosen design as
defense-in-depth rather than used standalone.)

## Settled decisions

1. **Scope:** community voice media only. The DM/group seal-open pair is untouched; the v2 envelope is
   designed so that path could adopt it later as a trivial follow-up.
2. **Verification posture:** **always sign + always verify.** Every received community-voice frame is
   authenticated, giving true speaking-attribution + impersonation resistance even when no moderation is
   active. DTX bounds the cost.
3. **Rollout:** clean, version-tagged break. No active users, so no dual-version interop / migration window.
   The AEAD domain tag bumps `harmony-voice-pkt-v1` → `harmony-voice-pkt-v2` so any stray v1 frame cleanly
   fails to decrypt rather than mis-parsing.
4. **Fail-closed:** a frame that cannot be authenticated (unknown device, bad signature, AAD/attribution
   mismatch) is dropped. This flips the old fail-*open* behavior; see §Fail-closed for why it costs no real
   audio.

## Architecture

### v2 wire envelope (community voice media)

```
[12B random nonce] [ChaCha20-Poly1305 ciphertext + 16B tag] [64B Ed25519 signature]
```

Today the envelope is `[nonce][ct+tag]`; we append a 64-byte detached signature. The ChaCha20-Poly1305
layer and the 23-byte plaintext header (`flags|seq|ts|senderHash` + codec payload) are **unchanged** — the
signature wraps the existing seal (encrypt-then-sign). The entire change is additive envelope bytes and
lives in Rust; **there are no frontend wire changes.**

### AAD (v1 → v2), sender bound in

```
AAD = b"harmony-voice-pkt-v2" ‖ community(16) ‖ channel(16) ‖ sender_device_vk(32)
```

Binding `sender_device_vk` into the AAD is the folded-in defense-in-depth (ties the ciphertext to the
claimed device). The domain bump guarantees v1↔v2 frames cleanly fail to open.

### Signature transcript

The device key signs a domain-separated transcript over the **ciphertext** (encrypt-then-sign, so a
forgery is rejectable before spending an AEAD-open, and the exact transmitted bytes are bound):

```
sig = Ed25519_sign( device_sk,
        b"harmony-voice-pkt-sig-v2" ‖ community(16) ‖ channel(16) ‖ nonce(12) ‖ ciphertext_with_tag )
```

`device_sk` is the **device-#2 signing key already used to sign presence beacons** (`VoiceJoinCaps`'s
signing key) — the same key whose public half a receiver already trusts from the verified
`VoicePresenceMap`. No new key material, no new trust root. Ed25519 is deterministic, so a fixed test key +
fixed nonce yields a byte-stable envelope we can pin in fixtures. The `…-sig-v2` domain prefix is distinct
from the AAD domain to prevent cross-context signature confusion.

### Sender path (Rust seal)

In the community media-publish arm of `event_loop.rs`, capture the device `SigningKey` (the same handle
presence-beacon signing already holds) and, at seal time: `encrypt → sign transcript → append 64B sig →
publish` under the existing `harmony/voice/{c}/{ch}/{deviceHex}` topic. DTX still gates transmission, so
signing only happens on frames actually sent.

### Receiver path (Rust subscribe) — fail-closed verify sequence, every frame

1. Parse the claimed device VK (32B) from the topic suffix. Not 32-byte hex → **drop**.
2. Look it up in the verified `VoicePresenceMap` → trusted owner. Unknown device → **drop** *(the one
   behavior flip: today fails open; now fails closed)*.
3. `verify_voice_frame_sig` — Ed25519-verify the detached signature against that device VK (public-key only,
   no symmetric work yet). Bad sig → **drop**.
4. We now hold a **cryptographically authenticated `(device, owner)`**. If `voice_moderation_active`,
   consult the moderation map on that *authenticated owner* → drop if muted/kicked. **← the fix:** the
   mute/kick decision no longer rests on a sender-controlled suffix, so it cannot be evaded.
5. `open_voice_packet` — AEAD-open with the v2 AAD. Failure → **drop**.
6. **Attribution integrity:** confirm the decrypted header's `senderHash` (`VK[0..16]`) equals the
   authenticated device's `VK[0..16]`; mismatch → **drop**. Then emit `frameBytes` exactly as today.

The presence-map lookup now runs on every received frame (was moderation-only). Under DTX this is a cheap
per-frame `BTreeMap` lookup over 1–3 active speakers. A cached read-copy is an easy optimization **if** a
profile ever shows it; not built now (YAGNI).

## Fail-closed

A frame that cannot be authenticated is dropped. The join-race window that justified the old fail-*open*
behavior is closed by the **start-muted-on-connect (D10)** invariant: a device joins muted, immediately
emits and heartbeats its presence beacon, and must *explicitly unmute* before it can transmit — by which
point every receiver already holds its verified presence. So fail-closed costs no real audio clipping.

## Attribution

Because verification is always-on, the receiver always knows the true sender. Step 6 makes the per-sender
jitter-buffer routing the frontend already does (by header `senderHash`) **guaranteed honest** — a member
cannot sign their own frame but mislabel the audio as someone else. Comparing the authenticated device's
16-byte VK prefix is sound: forging a false label would require a 128-bit prefix collision with the
victim's device key. This keeps the frontend contract intact (zero FE change).

## Components / files touched

Rust-only; community-voice path. The DM/group seal-open pair is untouched (no cross-contamination).

| File | Change |
|---|---|
| `src-tauri/src/voice_crypto.rs` | Replace the community `encrypt_voice_packet`/`decrypt_voice_packet` pair with three v2 helpers, deliberately keeping **signature-verify separable from decrypt** so the moderation drop can run on an authenticated owner *before* any symmetric work: `seal_and_sign_voice_packet(channel_key, device_sk, community, channel, plaintext)` (sender); `verify_voice_frame_sig(device_vk, community, channel, packet)` (public-key only — strips the 64B sig, Ed25519-verifies the transcript over `nonce ‖ ciphertext`, no channel key needed); `open_voice_packet(channel_key, device_vk, community, channel, packet)` (strips the sig, AEAD-opens `[nonce][ct]` with the v2 AAD). Add `VOICE_PACKET_AAD_V2`, `VOICE_PACKET_SIG_DOMAIN_V2`, `SIG_LEN = 64`, `MIN_PACKET_LEN_V2 = 92` (nonce+tag+sig), and a `…_with_nonce` deterministic seal variant for fixtures. DM constants/functions unchanged. |
| `src-tauri/src/event_loop.rs` | Community **publish** arm: capture the device `SigningKey` + seal-and-sign. Community **subscribe** arm: restructure to the always-verify sequence above. DM media arms unchanged. |
| `src-tauri/src/voice.rs` | Thread the device signing key into the community media-publish capability if not already reachable in that closure (minor wiring). |
| `src-tauri/tests/wire_format_voice_fixtures.rs` | Replace the community `voice_packet_wire_bytes_pinned` with a v2 byte-identity pin (fixed test device key + fixed nonce). DM/group/presence/moderation pins untouched. |

## Test plan

1. **Unit (`voice_crypto.rs`):** round-trip seal→open; tamper-ciphertext → open fails; tamper-signature →
   verify fails; verify under the *wrong* device VK → fails; cross-channel transcript (seal for channel A,
   verify for B) → fails; a v1-shaped (unsigned) frame → rejected under v2.
2. **Wire fixture:** v2 byte-identity pin using a fixed test device key + fixed nonce (Ed25519 determinism
   makes the full `[nonce][ct][sig]` envelope byte-stable). Regenerate-and-commit on intentional drift.
3. **Integration (multi-engine, mirrors the ZEB-358 moderation integration test):**
   - (a) an honest sender's frames are received;
   - (b) a **spoofer** publishing under another member's device suffix *without that member's key* → frames
     **dropped** (signature fails);
   - (c) a **muted owner cannot evade** the drop by switching suffixes → still dropped (decision runs on the
     authenticated owner);
   - (d) **attribution mismatch** (sign with own key, mislabel the header) → dropped.
4. **Gates:** `cargo fmt --all -- --check` + `cargo clippy -p harmony-app --lib` + nextest (`voice_crypto`,
   `voice_presence`, the new integration test) + `wire_format_voice_fixtures`. Final sweep adds
   `--all-targets`, MSRV, and the frontend gates (unaffected — no FE change). The 6 known iroh/zenoh
   loopback flakes remain non-blocking.

## Non-goals

- DM 1:1 / group-DM call media authentication (separate seal-open pair; possible trivial follow-up).
- Per-frame forward secrecy / key rotation changes (EpochKey rotation unchanged).
- A signed-hash-chain amortization (kept in reserve as a pure optimization if profiling ever demands it).
- Any frontend wire or UI change.

## Refs

- ZEB-348 (voice epic), ZEB-358 (moderation — origin), ZEB-35 (voice engine / media path).
- `docs/specs/2026-06-02-zeb-358-voice-moderation-design.md` §Security notes (the documented limitation).
- PR #179 review thread (CodeRabbit "Authenticate the sender before enforcing moderation").
- Prior art: SFrame (RFC 9605) — per-frame signatures over encrypted media.
