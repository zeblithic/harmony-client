# ZEB-358 Community Voice Moderation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add power-gated server-mute and remove-from-voice to community voice channels — ephemeral, voice-only, enforced receiver-side by honest clients.

**Architecture:** A dedicated signed-directive control plane. A new `voice_moderation.rs` module defines a `VoiceModerationDirective` (device-#2 signed, `ChannelKey`-sealed) that rides a new `harmony/voice-control/{community}/{channel}` Zenoh topic, plus an in-memory `ActiveModeration` map. Mute and kick are the same time-boxed directive; honest clients drop a moderated owner's audio + hide/flag them; nothing is written to the durable CRDT and the channel key is never rotated. Mirrors the V2 presence module (`voice_presence.rs`) and reuses the ZEB-339 power/enrolled-key verification.

**Tech Stack:** Rust (tokio, zenoh, ed25519-dalek, ChaCha20-Poly1305, serde/ciborium, serde_repr), Tauri IPC, Svelte 5 + TypeScript, vitest, cargo-nextest.

**Spec:** `docs/specs/2026-06-02-zeb-358-voice-moderation-design.md` (commit `c471c32`).

**Branch:** `zeb-358-voice-moderation` (already created off `main` @ `b48c209`).

**Constants (defined once in Task 1, reused everywhere):**
- `ENFORCE_TTL_MS: u64 = 12_000` — a directive stays effective this long after last receipt.
- `RE_ASSERT_INTERVAL_MS: u64 = 4_000` — issuer re-publishes each active directive this often.
- `DEFAULT_MODERATION_MS: u64 = 300_000` — default mute/kick duration (5 min), issuer-side.
- `MOD_POWER: u8 = 50` — minimum power to moderate (reuses the existing `kick` threshold).

---

## Reuse map (read these before starting — exact ground truth)

- **`src-tauri/src/voice_crypto.rs`** — AAD consts at lines 15-19 (`VOICE_PACKET_AAD`, `VOICE_PRESENCE_AAD`, `VOICE_DM_PACKET_AAD`); `scope_aad(domain, community, channel)` at 43; `encrypt_voice_packet(key, community, channel, domain, plain) -> Result<Vec<u8>>` at 62; `decrypt_voice_packet(...)` at 104; `encrypt_voice_packet_with_nonce(...)` at 205 (test-fixtures only).
- **`src-tauri/src/voice_presence.rs`** — the **template module**. Beacon struct (12-34), `CanonicalPayload` registration (56-59), `BeaconError` (62-73), `sign_presence_beacon` (80), `verify_presence_beacon_sig` (93), `seal_presence_beacon` (104), `open_presence_beacon` (116), `seal_presence_beacon_with_nonce` (130, test-fixtures), `device_is_enrolled(materialized, owner, device)` (385), `beacon_signer_is_member(registry, community, owner, device).await` (401), `VoicePresenceMap::{apply (209), sweep (327), roster (347), remove_channel (367)}`, `spawn_voice_presence_subscriber` (431), `spawn_voice_presence_publisher` (562), `publish_presence_once` (525), `build_presence_tombstone` (611).
- **`src-tauri/src/community_membership.rs`** — `ChannelKind` serde_repr pattern (334-345); `MaterializedMembership { members: BTreeMap<OwnerAddr, MemberState> (1304), power_levels: BTreeMap<OwnerAddr, u8> (1309) }`; `MemberState { status: MemberStatus (1391), enrolled_device_keys: BTreeSet<[u8;32]> (1401) }`; `MemberStatus::Joined` (1405); power lookup idiom `power_levels.get(&addr).copied().unwrap_or(0)`.
- **`src-tauri/src/voice.rs`** — `VoiceChannelRequest` enum (52), `VoiceJoinCaps` (37), payload structs (85-118), `SelfVoiceIdentity` (128).
- **`src-tauri/src/event_loop.rs`** — Join arm (2693) spawns presence subscriber (2887) + publisher (2920) guarded by `community_registry.clone()` (2870); Leave arm (2952) builds tombstone (2966) + emits empty roster (3007); SetMuted arm (3016) calls `publish_presence_once` (3045); roster snapshot via `g.roster(&c, &ch)` (3280); `voice-presence-changed` emit payload shape (3007-3012).
- **`src-tauri/src/lib.rs`** — `set_voice_muted` command (11760), id parsing via `parse_voice_id_16`, `voice_channel_tx` access; `generate_handler!` voice block (32801-32803).
- **`src/lib/voice-session.ts`** — `RosterMember` (24-31), `VoiceSessionState` (33-64), `setMuted` (345-363), `subscribePresence` (437-449), `refreshRoster` (478-502).
- **`src/lib/components/VoiceChannelView.svelte`** — grid/list roster (130-162), self controls (60-63, 165-219).
- **`src/lib/components/__tests__/VoiceChannelView.test.ts`** + **`src/lib/voice-session.test.ts`** — existing fakes/mocks to extend.

**Testing gates (per task vs final):**
- **Per backend task:** `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice_moderation)'` and `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`. **Use `--lib` per task** — a lib change relinks ~97 integration binaries under `--all-targets` (~25 min); reserve `--all-targets` for the final sweep (Task 12). Integration-test tasks scope to `--test voice_moderation_integration`.
- **Per frontend task:** `npx vitest run <file>` + `npx tsc --noEmit`.
- **`timeout`** is the Bash tool's own parameter on macOS, not a shell command.
- Always `cargo fmt --all` before committing Rust.

---

## Task 1: `voice_moderation.rs` — directive types, AAD, sign/verify/seal/open

**Files:**
- Modify: `src-tauri/src/voice_crypto.rs` (add the moderation AAD const)
- Create: `src-tauri/src/voice_moderation.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod voice_moderation;` near the other `mod voice*;` declarations)

- [ ] **Step 1: Add the AAD const to `voice_crypto.rs`** (after line 19, beside the other AADs):

```rust
/// Domain tag for sealed voice-moderation directives (ZEB-358). Distinct from
/// presence/media so a moderation packet can never be opened under another
/// domain's AAD.
pub const VOICE_MODERATION_AAD: &[u8] = b"harmony-voice-moderation-v1";
```

- [ ] **Step 2: Create `voice_moderation.rs` with the wire types + crypto.** Mirror `voice_presence.rs` lines 1-160 exactly, substituting the directive shape and `VOICE_MODERATION_AAD`:

```rust
//! ZEB-358 community voice moderation: power-gated server-mute + remove-from-
//! voice. A device-#2-signed, ChannelKey-sealed directive rides a dedicated
//! Zenoh control topic (never the CRDT); honest clients enforce it receiver-
//! side (drop the target's audio + hide/flag them). Mute and kick are the same
//! time-boxed directive. Mirrors `voice_presence.rs`.

use crate::community_membership::ChannelId;
use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::collections::BTreeMap;

/// Liveness TTL: a directive stays effective this long after last receipt.
pub const ENFORCE_TTL_MS: u64 = 12_000;
/// Issuer re-publishes each active directive this often (< ENFORCE_TTL_MS).
pub const RE_ASSERT_INTERVAL_MS: u64 = 4_000;
/// Default moderator-chosen duration (5 min), enforced issuer-side.
pub const DEFAULT_MODERATION_MS: u64 = 300_000;
/// Minimum power to moderate (reuses the existing `kick` threshold).
pub const MOD_POWER: u8 = 50;

/// What a directive asserts about the target owner. `serde_repr` encodes each
/// variant as its bare u8 discriminant (mirrors `ChannelKind`) and rejects
/// unknown discriminants on decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum ModAction {
    Mute = 0,
    Unmute = 1,
    Kick = 2,
    Unkick = 3,
}

impl ModAction {
    /// True for the mute class {Mute, Unmute}; false for the kick class.
    pub fn is_mute_class(self) -> bool {
        matches!(self, ModAction::Mute | ModAction::Unmute)
    }
    /// True for the "positive" directives that turn enforcement ON.
    pub fn enforces(self) -> bool {
        matches!(self, ModAction::Mute | ModAction::Kick)
    }
}

/// Unsigned directive. Canonical CBOR, 2-char keys (same-length invariant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceModerationDirective {
    #[serde(
        rename = "ao",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub actor_owner: [u8; 16],
    #[serde(
        rename = "ad",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub actor_device: [u8; 32],
    #[serde(
        rename = "to",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub target_owner: [u8; 16],
    #[serde(rename = "ac")]
    pub action: ModAction,
    #[serde(rename = "ih")]
    pub issued_hlc: Hlc,
    #[serde(rename = "sq")]
    pub seq: u64,
}

/// Directive + detached device-#2 signature over `canonical_cbor_encode(directive)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoiceModerationDirective {
    #[serde(rename = "dr")]
    pub directive: VoiceModerationDirective,
    #[serde(
        rename = "sg",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub sig: [u8; 64],
}

impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for VoiceModerationDirective {}
impl crate::owner_state_crypto::CanonicalPayload for VoiceModerationDirective {}
impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for SignedVoiceModerationDirective {}
impl crate::owner_state_crypto::CanonicalPayload for SignedVoiceModerationDirective {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModError {
    #[error("directive CBOR encode failed")]
    Encode,
    #[error("directive signature invalid")]
    BadSig,
    #[error("signer is not an enrolled, joined member")]
    NotMember,
    #[error("signer lacks moderation power over target")]
    NotAuthorized,
    #[error("directive transport publish failed")]
    Publish,
}

use crate::community_channel_log::ChannelKey;
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::voice_crypto::{decrypt_voice_packet, encrypt_voice_packet, VOICE_MODERATION_AAD};

pub fn sign_directive(
    directive: VoiceModerationDirective,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedVoiceModerationDirective, ModError> {
    use ed25519_dalek::Signer;
    let bytes = canonical_cbor_encode(&directive).map_err(|_| ModError::Encode)?;
    let sig = signing_key.sign(&bytes).to_bytes();
    Ok(SignedVoiceModerationDirective { directive, sig })
}

pub fn verify_directive_sig(signed: &SignedVoiceModerationDirective) -> Result<(), ModError> {
    let bytes = canonical_cbor_encode(&signed.directive).map_err(|_| ModError::Encode)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&signed.directive.actor_device)
        .map_err(|_| ModError::BadSig)?;
    let sig = ed25519_dalek::Signature::from_bytes(&signed.sig);
    vk.verify_strict(&bytes, &sig).map_err(|_| ModError::BadSig)
}

pub fn seal_directive(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoiceModerationDirective,
) -> Result<Vec<u8>, ModError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| ModError::Encode)?;
    encrypt_voice_packet(key, community, channel, VOICE_MODERATION_AAD, &plain)
        .map_err(|_| ModError::Encode)
}

pub fn open_directive(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    packet: &[u8],
) -> Option<SignedVoiceModerationDirective> {
    let plain = decrypt_voice_packet(key, community, channel, VOICE_MODERATION_AAD, packet).ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn seal_directive_with_nonce(
    key: &ChannelKey,
    community: &SpaceId,
    channel: &ChannelId,
    signed: &SignedVoiceModerationDirective,
    nonce: [u8; 12],
) -> Result<Vec<u8>, ModError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| ModError::Encode)?;
    crate::voice_crypto::encrypt_voice_packet_with_nonce(
        key, community, channel, VOICE_MODERATION_AAD, &plain, nonce,
    )
    .map_err(|_| ModError::Encode)
}
```

- [ ] **Step 3: Add `mod voice_moderation;` in `lib.rs`** beside the other `mod voice*;` lines (grep `mod voice_presence;` to find the spot).

- [ ] **Step 4: Write unit tests** (append a `#[cfg(test)] mod tests` to `voice_moderation.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::Hlc;

    fn key() -> ChannelKey { ChannelKey([9u8; 32]) }
    fn sk() -> ed25519_dalek::SigningKey { ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]) }
    const C: SpaceId = SpaceId([1u8; 16]);
    const CH: ChannelId = ChannelId([2u8; 16]);

    fn directive(action: ModAction, vk: [u8; 32]) -> VoiceModerationDirective {
        VoiceModerationDirective {
            actor_owner: [0xAA; 16],
            actor_device: vk,
            target_owner: [0xBB; 16],
            action,
            issued_hlc: Hlc { wall_ms: 100, logical: 0, device_id: [7u8; 16] },
            seq: 1,
        }
    }

    #[test]
    fn mod_action_u8_roundtrip() {
        for a in [ModAction::Mute, ModAction::Unmute, ModAction::Kick, ModAction::Unkick] {
            let b = canonical_cbor_encode(&a).unwrap();
            let back: ModAction = ciborium::from_reader(b.as_slice()).unwrap();
            assert_eq!(a, back);
        }
        // bare discriminant encoding
        assert_eq!(canonical_cbor_encode(&ModAction::Kick).unwrap(),
                   canonical_cbor_encode(&2u8).unwrap());
    }

    #[test]
    fn sign_then_verify_ok_and_tamper_fails() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
        assert!(verify_directive_sig(&signed).is_ok());
        let mut bad = signed.clone();
        bad.directive.action = ModAction::Unmute; // changes signed bytes
        assert_eq!(verify_directive_sig(&bad), Err(ModError::BadSig));
    }

    #[test]
    fn seal_open_roundtrip_and_wrong_channel_drops() {
        let signing = sk();
        let vk = signing.verifying_key().to_bytes();
        let signed = sign_directive(directive(ModAction::Kick, vk), &signing).unwrap();
        let sealed = seal_directive(&key(), &C, &CH, &signed).unwrap();
        let opened = open_directive(&key(), &C, &CH, &sealed).unwrap();
        assert_eq!(opened, signed);
        // wrong channel scope → AAD mismatch → None
        let other_ch = ChannelId([0xEE; 16]);
        assert!(open_directive(&key(), &C, &other_ch, &sealed).is_none());
    }
}
```

> Confirm `Hlc`'s real field names with `grep -n "pub struct Hlc" -A6 src-tauri/src/owner_state_types.rs` and adjust the `Hlc { … }` literal if they differ (e.g. `wallMs`/`wall_ms`). `ChannelKey`'s constructor shape: `grep -n "pub struct ChannelKey" src-tauri/src/community_channel_log.rs`.

- [ ] **Step 5: Run tests + clippy**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice_moderation)'`
Expected: 3 tests pass.
Run: `cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**
```bash
git add src-tauri/src/voice_moderation.rs src-tauri/src/voice_crypto.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-358): voice moderation directive type + sign/seal/open"
```

---

## Task 2: `ActiveModeration` state — apply (LWW + tombstone) + sweep + queries

**Files:**
- Modify: `src-tauri/src/voice_moderation.rs`

- [ ] **Step 1: Write the failing tests** (add to the `tests` mod):

```rust
fn applied(am: &mut ActiveModeration, action: ModAction, hlc_wall: u64, seq: u64, now: u64) -> bool {
    let mut d = directive(action, [0u8; 32]);
    d.issued_hlc = Hlc { wall_ms: hlc_wall, logical: 0, device_id: [7u8; 16] };
    d.seq = seq;
    am.apply(&C, &CH, &d, now, ENFORCE_TTL_MS)
}

#[test]
fn mute_then_expire() {
    let mut am = ActiveModeration::default();
    assert!(applied(&mut am, ModAction::Mute, 100, 1, 1_000));
    assert!(am.is_muted(&C, &CH, &[0xBB; 16], 1_000));
    assert!(am.is_muted(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS - 1));
    assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS));
}

#[test]
fn reassert_refreshes_ttl() {
    let mut am = ActiveModeration::default();
    applied(&mut am, ModAction::Mute, 100, 1, 1_000);
    // same (hlc, seq) re-assert at a later now refreshes enforce_until
    applied(&mut am, ModAction::Mute, 100, 1, 10_000);
    assert!(am.is_muted(&C, &CH, &[0xBB; 16], 10_000 + ENFORCE_TTL_MS - 1));
}

#[test]
fn newer_unmute_clears_and_blocks_stale_mute() {
    let mut am = ActiveModeration::default();
    applied(&mut am, ModAction::Mute, 100, 1, 1_000);
    // higher (hlc, seq) Unmute clears it
    assert!(applied(&mut am, ModAction::Unmute, 200, 2, 1_100));
    assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_100));
    // a delayed *older* Mute must NOT resurrect (tombstone retains latest order)
    assert!(!applied(&mut am, ModAction::Mute, 100, 1, 1_200));
    assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_200));
}

#[test]
fn mute_and_kick_are_independent_classes() {
    let mut am = ActiveModeration::default();
    applied(&mut am, ModAction::Mute, 100, 1, 1_000);
    applied(&mut am, ModAction::Kick, 100, 1, 1_000);
    assert!(am.is_muted(&C, &CH, &[0xBB; 16], 1_000));
    assert!(am.is_kicked(&C, &CH, &[0xBB; 16], 1_000));
    applied(&mut am, ModAction::Unmute, 200, 2, 1_100);
    assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_100));
    assert!(am.is_kicked(&C, &CH, &[0xBB; 16], 1_100)); // kick survives
}

#[test]
fn sweep_reports_lapsed_targets() {
    let mut am = ActiveModeration::default();
    applied(&mut am, ModAction::Mute, 100, 1, 1_000);
    let lapsed = am.sweep(1_000 + ENFORCE_TTL_MS);
    assert_eq!(lapsed, vec![(C, CH)]);
    assert!(!am.is_muted(&C, &CH, &[0xBB; 16], 1_000 + ENFORCE_TTL_MS));
}
```

- [ ] **Step 2: Run to confirm failure**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice_moderation)'`
Expected: compile error — `ActiveModeration` not defined.

- [ ] **Step 3: Implement `ActiveModeration`** (add to `voice_moderation.rs`, above the tests). `Hlc` is `Ord` (presence orders its roster by it); order by the `(issued_hlc, seq)` tuple.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClassState {
    latest_hlc: Hlc,
    latest_seq: u64,
    enforced: bool,
    enforce_until_ms: u64,
}

#[derive(Debug, Clone, Default)]
struct TargetState {
    mute: Option<ClassState>,
    kick: Option<ClassState>,
}

/// In-memory enforcement state: which owners are currently muted/kicked per
/// (community, channel). Never persisted. Two independent classes per target.
#[derive(Debug, Default)]
pub struct ActiveModeration {
    inner: BTreeMap<(SpaceId, ChannelId), BTreeMap<[u8; 16], TargetState>>,
}

impl ActiveModeration {
    /// Apply a (verified) directive. Returns true if effective state changed
    /// (so the caller re-emits the roster). LWW by (issued_hlc, seq) within the
    /// directive's class; equal order = a re-assert (refresh TTL only).
    pub fn apply(
        &mut self,
        c: &SpaceId,
        ch: &ChannelId,
        d: &VoiceModerationDirective,
        now_ms: u64,
        ttl_ms: u64,
    ) -> bool {
        let target = self.inner.entry((*c, *ch)).or_default().entry(d.target_owner).or_default();
        let slot = if d.action.is_mute_class() { &mut target.mute } else { &mut target.kick };
        let incoming = (d.issued_hlc, d.seq);
        match slot {
            Some(s) if incoming < (s.latest_hlc, s.latest_seq) => false, // stale → ignore
            Some(s) if incoming == (s.latest_hlc, s.latest_seq) => {
                // re-assert of the current directive: refresh TTL, no state flip
                if s.enforced {
                    s.enforce_until_ms = now_ms.saturating_add(ttl_ms);
                }
                false
            }
            _ => {
                let was = slot.map(|s| s.enforced).unwrap_or(false);
                let enforced = d.action.enforces();
                *slot = Some(ClassState {
                    latest_hlc: d.issued_hlc,
                    latest_seq: d.seq,
                    enforced,
                    enforce_until_ms: now_ms.saturating_add(ttl_ms),
                });
                was != enforced
            }
        }
    }

    fn is(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64, mute: bool) -> bool {
        self.inner
            .get(&(*c, *ch))
            .and_then(|m| m.get(owner))
            .and_then(|t| if mute { t.mute } else { t.kick })
            .is_some_and(|s| s.enforced && now_ms < s.enforce_until_ms)
    }
    pub fn is_muted(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64) -> bool {
        self.is(c, ch, owner, now_ms, true)
    }
    pub fn is_kicked(&self, c: &SpaceId, ch: &ChannelId, owner: &[u8; 16], now_ms: u64) -> bool {
        self.is(c, ch, owner, now_ms, false)
    }

    /// Lapse any enforced class whose TTL passed; return the (community,channel)
    /// pairs whose effective state changed so the caller re-emits the roster.
    pub fn sweep(&mut self, now_ms: u64) -> Vec<(SpaceId, ChannelId)> {
        let mut changed = Vec::new();
        for (scope, targets) in self.inner.iter_mut() {
            let mut any = false;
            for t in targets.values_mut() {
                for slot in [&mut t.mute, &mut t.kick] {
                    if let Some(s) = slot {
                        if s.enforced && now_ms >= s.enforce_until_ms {
                            s.enforced = false;
                            any = true;
                        }
                    }
                }
            }
            if any {
                changed.push(*scope);
            }
        }
        changed
    }

    /// Drop all state for a channel (called on Leave).
    pub fn remove_channel(&mut self, c: &SpaceId, ch: &ChannelId) {
        self.inner.remove(&(*c, *ch));
    }
}
```

- [ ] **Step 4: Run tests** — Expected: all `voice_moderation` tests pass (now 8).
- [ ] **Step 5: fmt + clippy** (same commands as Task 1 Step 5). Expected: clean.
- [ ] **Step 6: Commit**
```bash
git add src-tauri/src/voice_moderation.rs
git commit -m "feat(zeb-358): ActiveModeration LWW state + sweep + queries"
```

---

## Task 3: Authority verification (5-step) — `directive_signer_is_authorized`

**Files:**
- Modify: `src-tauri/src/voice_moderation.rs`

- [ ] **Step 1: Write the failing tests.** Build a `MaterializedMembership` by hand (mirror how `voice_presence.rs` tests / `community_membership.rs` tests construct one — `grep -n "MaterializedMembership {" src-tauri/src/community_membership.rs` for a literal, plus `MemberState`).

```rust
use crate::community_membership::{MaterializedMembership, MemberState, MemberStatus};
use std::collections::{BTreeMap, BTreeSet};

fn member(power: u8, device: [u8; 32]) -> MemberState {
    let mut keys = BTreeSet::new();
    keys.insert(device);
    MemberState {
        status: MemberStatus::Joined,
        enrolled_device_keys: keys,
        ..MemberState::default()  // confirm Default exists; else fill the real fields
    }
}

fn membership(actor_power: u8, target_power: u8, actor_dev: [u8; 32]) -> MaterializedMembership {
    let mut m = MaterializedMembership::default();
    m.members.insert(OwnerAddr([0xAA; 16]), member(actor_power, actor_dev));
    m.members.insert(OwnerAddr([0xBB; 16]), member(target_power, [0xCC; 32]));
    m.power_levels.insert(OwnerAddr([0xAA; 16]), actor_power);
    m.power_levels.insert(OwnerAddr([0xBB; 16]), target_power);
    m
}

#[test]
fn authority_accepts_powerful_member() {
    let signing = sk();
    let vk = signing.verifying_key().to_bytes();
    let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
    let mm = membership(60, 0, vk);
    assert!(verify_directive_authority(&mm, &signed).is_ok());
}

#[test]
fn authority_rejects_non_member() {
    let signing = sk();
    let vk = signing.verifying_key().to_bytes();
    let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
    let mm = MaterializedMembership::default(); // actor unknown
    assert_eq!(verify_directive_authority(&mm, &signed), Err(ModError::NotMember));
}

#[test]
fn authority_rejects_insufficient_power() {
    let signing = sk();
    let vk = signing.verifying_key().to_bytes();
    let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
    let mm = membership(49, 0, vk); // below MOD_POWER
    assert_eq!(verify_directive_authority(&mm, &signed), Err(ModError::NotAuthorized));
}

#[test]
fn authority_rejects_power_not_greater_than_target() {
    let signing = sk();
    let vk = signing.verifying_key().to_bytes();
    let signed = sign_directive(directive(ModAction::Kick, vk), &signing).unwrap();
    let mm = membership(60, 60, vk); // equal power → cannot moderate a peer
    assert_eq!(verify_directive_authority(&mm, &signed), Err(ModError::NotAuthorized));
}

#[test]
fn authority_rejects_device_not_enrolled_for_actor() {
    let signing = sk();
    let vk = signing.verifying_key().to_bytes();
    let signed = sign_directive(directive(ModAction::Mute, vk), &signing).unwrap();
    let mm = membership(60, 0, [0x11; 32]); // actor enrolled with a DIFFERENT device
    assert_eq!(verify_directive_authority(&mm, &signed), Err(ModError::NotMember));
}
```

- [ ] **Step 2: Run to confirm failure** — `verify_directive_authority` undefined.

- [ ] **Step 3: Implement the 5-step verify.** (Sig is step 2; open is the caller's step 1. This fn does sig→membership→power.)

```rust
/// Steps 2-4 of authority verification against materialized membership:
/// (2) device-#2 signature, (3) actor_device ∈ actor_owner.enrolled_device_keys
/// AND actor_owner is Joined, (4) power(actor) ≥ MOD_POWER AND power(actor) >
/// power(target). Step 1 (open under ChannelKey) is the caller's; step 5 is apply().
pub fn verify_directive_authority(
    materialized: &MaterializedMembership,
    signed: &SignedVoiceModerationDirective,
) -> Result<(), ModError> {
    verify_directive_sig(signed)?; // (2)
    let d = &signed.directive;
    // (3) bind device to the claimed actor + Joined status — reuses the
    // presence membership check's logic (voice_presence::device_is_enrolled).
    if !crate::voice_presence::device_is_enrolled(
        materialized,
        &OwnerAddr(d.actor_owner),
        &d.actor_device,
    ) {
        return Err(ModError::NotMember);
    }
    // (4) power gate
    let actor_power = materialized.power_levels.get(&OwnerAddr(d.actor_owner)).copied().unwrap_or(0);
    let target_power = materialized.power_levels.get(&OwnerAddr(d.target_owner)).copied().unwrap_or(0);
    if actor_power < MOD_POWER || actor_power <= target_power {
        return Err(ModError::NotAuthorized);
    }
    Ok(())
}

/// Async wrapper resolving materialized membership off the registry, mirroring
/// `voice_presence::beacon_signer_is_member`. Returns Ok(()) only if authorized.
pub async fn directive_signer_is_authorized(
    registry: &crate::community_state_sync::CommunitySyncRegistry,
    community: &SpaceId,
    signed: &SignedVoiceModerationDirective,
) -> Result<(), ModError> {
    let materialized = {
        let admin = registry.engine().admin_addr(); // confirm accessor name via voice_presence:401-419
        let guard = registry.engine().membership_guard().await; // mirror exact calls used at voice_presence.rs:416-419
        guard.materialized(admin)
    };
    verify_directive_authority(&materialized, signed)
}
```

> **Implementer note:** the exact registry→materialized resolution (the `{ … guard.materialized(admin) }` block) must be copied verbatim from `voice_presence::beacon_signer_is_member` (voice_presence.rs:401-421) — accessor names like `admin_addr()` / `membership_guard()` are illustrative here; use the real ones from that function. Also confirm `MemberState` has a `Default` (or construct it with its real fields — check `grep -n "pub struct MemberState" -A12 src-tauri/src/community_membership.rs`).

- [ ] **Step 4: Run tests** — Expected: 5 new authority tests pass (13 total).
- [ ] **Step 5: fmt + clippy.** Expected: clean.
- [ ] **Step 6: Commit**
```bash
git add src-tauri/src/voice_moderation.rs
git commit -m "feat(zeb-358): 5-step directive authority verification"
```

---

## Task 4: Canonical-CBOR wire fixture (byte-identity pin)

**Files:**
- Modify: the voice wire-format fixtures test file (find it: `grep -rln "seal_presence_beacon_with_nonce\|VOICE_PRESENCE_AAD" src-tauri/tests/`). If presence has a fixture test there, add the moderation one beside it; otherwise create `src-tauri/tests/wire_format_voice_moderation_fixtures.rs` with `#![cfg(feature = "test-fixtures")]`.

- [ ] **Step 1: Write the fixture test** — pins the canonical CBOR of an unsigned directive AND a deterministic-nonce sealed packet, so any wire change is caught.

```rust
#![cfg(feature = "test-fixtures")]
use harmony_app::voice_moderation::{
    sign_directive, seal_directive_with_nonce, ModAction, VoiceModerationDirective,
};
use harmony_app::community_channel_log::ChannelKey;
use harmony_app::community_membership::ChannelId;
use harmony_app::owner_state_types::{Hlc, SpaceId};
use harmony_app::owner_state_crypto::canonical_cbor_encode;

fn directive() -> VoiceModerationDirective {
    VoiceModerationDirective {
        actor_owner: [0xAA; 16],
        actor_device: [0xDD; 32],
        target_owner: [0xBB; 16],
        action: ModAction::Mute,
        issued_hlc: Hlc { wall_ms: 1, logical: 2, device_id: [3u8; 16] },
        seq: 4,
    }
}

#[test]
fn directive_canonical_cbor_is_pinned() {
    let bytes = canonical_cbor_encode(&directive()).unwrap();
    let hex = hex::encode(&bytes);
    // First run: print, paste the literal, then assert. Pin it:
    // eprintln!("{hex}");
    assert_eq!(hex, "<PASTE_ON_FIRST_RUN>");
}

#[test]
fn sealed_directive_is_pinned() {
    let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let signed = sign_directive(directive(), &signing).unwrap();
    let sealed = seal_directive_with_nonce(
        &ChannelKey([5u8; 32]), &SpaceId([1u8; 16]), &ChannelId([2u8; 16]), &signed, [9u8; 12],
    ).unwrap();
    assert_eq!(hex::encode(&sealed), "<PASTE_ON_FIRST_RUN>");
}
```

- [ ] **Step 2: First run to capture the bytes** — temporarily change each `assert_eq!` to `eprintln!("{hex}")` (or run with the assert against `""` and read the panic's `left`), capture the real hex.
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(voice_moderation_fixture) + test(directive_canonical) + test(sealed_directive)' --no-capture`
- [ ] **Step 3: Paste the captured hex** into both `assert_eq!` literals.
- [ ] **Step 4: Re-run** — Expected: both pinned tests pass.
- [ ] **Step 5: fmt + clippy** (`--all-targets` is fine here since this is a single new test file: `cargo clippy --locked --test wire_format_voice_moderation_fixtures --features test-fixtures --no-deps -- -D warnings`, or just the lib clippy).
- [ ] **Step 6: Commit**
```bash
git add src-tauri/tests/
git commit -m "test(zeb-358): pin canonical-CBOR + sealed directive wire fixtures"
```

---

## Task 5: `voice.rs` — `Moderate` request variant + `ModerateVoicePayload`

**Files:**
- Modify: `src-tauri/src/voice.rs`

- [ ] **Step 1: Write the failing test** (append to `voice.rs` tests, or create a `#[cfg(test)] mod tests`):

```rust
#[test]
fn moderate_payload_parses_action() {
    let json = r#"{"communityId":"aa","channelId":"bb","targetOwnerHex":"cc","action":"mute","durationMs":60000}"#;
    let p: ModerateVoicePayload = serde_json::from_str(json).unwrap();
    assert_eq!(p.action, "mute");
    assert_eq!(p.duration_ms, Some(60000));
    assert_eq!(ModerateVoicePayload::parse_action("kick").unwrap(), crate::voice_moderation::ModAction::Kick);
    assert!(ModerateVoicePayload::parse_action("bogus").is_err());
}
```

- [ ] **Step 2: Run to confirm failure.**

- [ ] **Step 3: Add the variant + payload.** In `VoiceChannelRequest` (after `SetMuted`, ~line 71):

```rust
    /// ZEB-358: a moderator issues a voice-moderation directive against `target_owner`.
    Moderate {
        community_id: SpaceId,
        channel_id: ChannelId,
        target_owner: OwnerAddr,
        action: crate::voice_moderation::ModAction,
        /// Issuer-side duration; how long to keep re-asserting. None → default.
        duration_ms: Option<u64>,
    },
```

Add the IPC payload struct (beside `SetVoiceMutedPayload`, ~line 95) with camelCase serde:

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerateVoicePayload {
    pub community_id: String,
    pub channel_id: String,
    pub target_owner_hex: String,
    pub action: String,            // "mute" | "unmute" | "kick" | "unkick"
    pub duration_ms: Option<u64>,
}

impl ModerateVoicePayload {
    pub fn parse_action(s: &str) -> Result<crate::voice_moderation::ModAction, String> {
        use crate::voice_moderation::ModAction::*;
        match s {
            "mute" => Ok(Mute),
            "unmute" => Ok(Unmute),
            "kick" => Ok(Kick),
            "unkick" => Ok(Unkick),
            other => Err(format!("unknown moderation action: {other}")),
        }
    }
}
```

- [ ] **Step 4: Run test** — Expected: pass. (Add `serde_json` to dev-deps if the test needs it — `grep serde_json src-tauri/Cargo.toml`; it is typically already present.)
- [ ] **Step 5: fmt + clippy.** Expected: clean.
- [ ] **Step 6: Commit**
```bash
git add src-tauri/src/voice.rs
git commit -m "feat(zeb-358): Moderate request variant + ModerateVoicePayload"
```

---

## Task 6: Event-loop wiring — control sub + issuer re-assert + shared state (Join/Leave)

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

This task spawns the control plane on Join and tears it down on Leave. No enforcement effects yet (that's Task 7) — this task just gets directives flowing into a shared `ActiveModeration` and re-asserted by the issuer. Verified by Task 11's integration test; locally, confirm it compiles + existing voice tests stay green.

- [ ] **Step 1: Add shared state beside the voice presence state.** Near where the presence `Arc<Mutex<VoicePresenceMap>>` and `muted: Arc<AtomicBool>` live for the active channel session (grep `VoicePresenceMap` in event_loop.rs), add:

```rust
let active_moderation = std::sync::Arc::new(tokio::sync::Mutex::new(
    crate::voice_moderation::ActiveModeration::default(),
));
// Directives this node is currently re-asserting (it issued them). Keyed by
// (target_owner, is_mute_class) so a Mute and a Kick on the same target coexist.
// Value: (SignedVoiceModerationDirective, stop_after_ms wall clock).
let issuer_directives = std::sync::Arc::new(tokio::sync::Mutex::new(
    std::collections::HashMap::<([u8; 16], bool), (
        crate::voice_moderation::SignedVoiceModerationDirective, u64)>::new(),
));
```

- [ ] **Step 2: Spawn the control-topic subscriber in the Join arm**, mirroring `spawn_voice_presence_subscriber` (event_loop.rs:2887, def at voice_presence.rs:431). Topic: `format!("harmony/voice-control/{}/{}", hex::encode(community_id.0), hex::encode(channel_id.0))`. The subscriber loop: `open_directive` → `verify_directive_sig` → `directive_signer_is_authorized(&registry, &community, &signed).await` → on Ok, `active_moderation.lock().await.apply(&community, &channel, &signed.directive, now_ms(), ENFORCE_TTL_MS)`; if it returns `true`, re-emit the roster (Task 7 provides the emit helper — for this task, call the same empty/`g.roster` emit the presence path uses so the wiring compiles). Drop on any verify failure (mirror the `continue;` drops at voice_presence.rs:461-471). Store its `JoinHandle` next to `pres_sub`.

- [ ] **Step 3: Spawn the issuer re-assert task** in the Join arm. A `tokio::time::interval(Duration::from_millis(RE_ASSERT_INTERVAL_MS))` loop that, each tick: locks `issuer_directives`, drops entries whose `stop_after_ms <= now`, and `session.put(&control_topic, seal_directive(&channel_key, &community, &channel, &signed)?)` for each survivor. Honor `closing` like the presence publisher (voice_presence.rs:577-604). Store the handle.

- [ ] **Step 4: Tear down on Leave.** In the Leave arm (event_loop.rs:2952), abort the two new handles alongside the presence ones, and `active_moderation.lock().await.remove_channel(&community_id, &channel_id)` + clear `issuer_directives`.

- [ ] **Step 5: Compile + existing voice tests.**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice)'`
Expected: existing voice + new voice_moderation unit tests pass; no regressions.
Run: `cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**
```bash
git add src-tauri/src/event_loop.rs
git commit -m "feat(zeb-358): control-topic sub + issuer re-assert + shared ActiveModeration"
```

---

## Task 7: Event-loop enforcement — media-drop, roster/power enrichment, self-gating, Moderate handler

**Files:**
- Modify: `src-tauri/src/event_loop.rs`
- Modify: `src-tauri/src/voice_presence.rs` (extend the emitted roster payload / add a `sender_id → owner` index accessor)

- [ ] **Step 1: Add the `sender-id → owner` index.** The media topic is keyed by the full device hex but media correlation uses the 16-byte device prefix (per ZEB-351). In the presence subscriber, every time a beacon is verified (voice_presence.rs:468-475, *before* the membership-visible apply), record `prefix16(device) → owner` in a shared `Arc<Mutex<HashMap<[u8;16],[u8;16]>>>` (sender-prefix → owner). This index includes kicked owners (who are hidden from the roster) so media-drop can still resolve them. Expose it from the Join arm.

- [ ] **Step 2: Media-drop in the media subscriber.** In the channel media subscriber arm (event_loop.rs:2717-2851, the `harmony/voice/{c}/{ch}/*` handler that emits `voice-frame-received`), before emitting: extract the sender prefix from the topic/packet (same way speaking correlation derives it), resolve `owner` via the index, and if `active_moderation.lock().await.is_muted(&c, &ch, &owner, now_ms()) || is_kicked(...)`, `continue;` (drop, do not emit). If the owner can't be resolved yet (race), emit as today (documented minor leak ≤ a few seconds).

- [ ] **Step 3: Enrich the roster emit.** Replace the bare `g.roster(&c, &ch)` snapshot (event_loop.rs:3280 and the Join/SetMuted emits) with an enriched payload builder. Add to `voice_presence.rs` an enriched roster entry type and a builder that takes the base roster + the `ActiveModeration` queries + the `power_levels` map + `self_owner`:

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeratedRosterEntry {
    #[serde(serialize_with = "ser_hex_16")] pub owner: [u8; 16],
    #[serde(serialize_with = "ser_hex_32")] pub device: [u8; 32],
    pub muted: bool,       // self-mute (from beacon)
    pub mod_muted: bool,   // server-mute (from ActiveModeration)
    pub power: u8,         // for FE control gating
}
```

The event-loop emit closure (where it currently builds the `"roster"` value): for each `RosterEntry`, skip if `is_kicked(owner)`, else build a `ModeratedRosterEntry { muted, mod_muted: is_muted(owner), power: power_levels.get(owner).copied().unwrap_or(0) }`. Emit payload becomes:
```rust
serde_json::json!({
    "community": hex::encode(c.0),
    "channel": hex::encode(ch.0),
    "roster": moderated_entries,
    "selfPower": power_levels.get(&self_owner).copied().unwrap_or(0),
    "selfModMuted": am.is_muted(&c, &ch, &self_owner.0, now),
    "selfKicked": am.is_kicked(&c, &ch, &self_owner.0, now),
})
```
Power levels come from the same `registry.engine().materialized()` clone used by `beacon_signer_is_member`. Re-emit from: the control subscriber (on `apply` change), the presence subscriber (on roster change), and the sweep tick (on lapse).

- [ ] **Step 4: Self-gating on self-kick.** When an emit computes `selfKicked == true` and it was false before, tear down the **mic sender + presence publisher** (abort those handles / stop sending) while leaving the subscribers + control sub running. (The FE drives the "leave vs stay-subscribed" UX off `selfKicked`; the backend just stops *our* outbound media/presence.) Track a `self_kicked: bool` latch in the loop to detect the edge.

- [ ] **Step 5: Handle the `Moderate` request.** New arm after `SetMuted` (event_loop.rs:3016):

```rust
crate::voice::VoiceChannelRequest::Moderate { community_id, channel_id, target_owner, action, duration_ms } => {
    // Resolve caps for the active channel session (channel_key, signing_key, self_owner/device, hlc).
    // Pre-check authority locally for instant UX feedback (re-verified by all receivers):
    //   build directive → verify_directive_authority(&materialized, &signed) → on Err, log + skip publish.
    // issued_hlc via reserve_next_hlc_for_device (same source as Join's joined_hlc);
    // seq via a per-channel AtomicU64 moderation counter.
    // Build → sign_directive(.., signing_key) → seal_directive(..) → session.put(control_topic, sealed) once.
    // For enforces() actions: insert into issuer_directives keyed (target_owner.0, action.is_mute_class())
    //   with stop_after_ms = now + duration_ms.unwrap_or(DEFAULT_MODERATION_MS).
    // For Unmute/Unkick: insert a short-lived re-assert (stop_after_ms = now + ENFORCE_TTL_MS) so the
    //   revoke reliably propagates, and remove any positive entry for that (target, class).
    // Apply locally via active_moderation (loopback) and re-emit roster.
}
```

> The "yield on supersede" rule (stop re-asserting a directive once a higher-ordered one for the same target+class is observed): in the control subscriber, after `apply`, if the applied directive's `(issued_hlc, seq)` is higher than one we're re-asserting for that `(target, class)`, drop ours from `issuer_directives`.

- [ ] **Step 6: Compile + existing voice tests.** Same commands as Task 6 Step 5. Expected: clean, no regressions.
- [ ] **Step 7: Commit**
```bash
git add src-tauri/src/event_loop.rs src-tauri/src/voice_presence.rs
git commit -m "feat(zeb-358): enforcement — media-drop, roster/power enrich, self-gate, Moderate handler"
```

---

## Task 8: `moderate_voice` IPC command + registration

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add the command**, modeled exactly on `set_voice_muted` (lib.rs:11760):

```rust
/// ZEB-358: issue a voice-moderation directive (mute/unmute/kick/unkick).
#[tauri::command]
async fn moderate_voice(
    payload: voice::ModerateVoicePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let community = crate::owner_state_types::SpaceId(parse_voice_id_16("communityId", &payload.community_id)?);
    let channel = crate::community_membership::ChannelId(parse_voice_id_16("channelId", &payload.channel_id)?);
    let target_owner = crate::owner_state_types::OwnerAddr(parse_voice_id_16("targetOwnerHex", &payload.target_owner_hex)?);
    let action = voice::ModerateVoicePayload::parse_action(&payload.action)?;
    let tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.voice_channel_tx.clone().ok_or_else(|| "not connected".to_string())?
    };
    tx.send(voice::VoiceChannelRequest::Moderate {
        community_id: community,
        channel_id: channel,
        target_owner,
        action,
        duration_ms: payload.duration_ms,
    })
    .await
    .map_err(|_| "event loop not running".to_string())
}
```

> Confirm `parse_voice_id_16` accepts a 32-hex string → `[u8;16]` (it's used for community/channel ids already; `targetOwnerHex` is also a 16-byte owner addr = 32 hex chars).

- [ ] **Step 2: Register it** in `generate_handler!` (lib.rs:32803, after `set_voice_muted,`): add `moderate_voice,`.

- [ ] **Step 3: Compile.**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(voice)'`
Expected: compiles, voice tests green.
Run: `cargo fmt --all && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`. Expected: clean.

- [ ] **Step 4: Commit**
```bash
git add src-tauri/src/lib.rs
git commit -m "feat(zeb-358): moderate_voice IPC command + registration"
```

---

## Task 9: Frontend `voice-session.ts` — moderation state + `moderate()` + self-target handling

**Files:**
- Modify: `src/lib/voice-session.ts`
- Modify: `src/lib/voice-session.test.ts`

- [ ] **Step 1: Write failing tests** (extend `voice-session.test.ts`; mirror its existing harness):

```typescript
it('maps enriched presence payload (modMuted/power/selfPower/selfModMuted/selfKicked)', async () => {
  const s = makeSession();           // use the file's existing factory
  await s.join('c', 'ch');
  emitPresence({                     // helper that fires the listen('voice-presence-changed') cb
    community: 'c', channel: 'ch',
    roster: [{ owner: 'bb', device: 'bbbb', muted: false, modMuted: true, power: 0 }],
    selfPower: 60, selfModMuted: false, selfKicked: false,
  });
  const st = get(s.store);
  expect(st.roster[0].modMuted).toBe(true);
  expect(st.roster[0].power).toBe(0);
  expect(st.selfPower).toBe(60);
});

it('blocks self-unmute while selfModMuted, and stays self-muted after it clears', async () => {
  const s = makeSession();
  await s.join('c', 'ch');
  emitPresence({ community: 'c', channel: 'ch', roster: [], selfPower: 0, selfModMuted: true, selfKicked: false });
  await s.setMuted(false);                      // must be a no-op while mod-muted
  expect(get(s.store).muted).toBe(true);
  emitPresence({ community: 'c', channel: 'ch', roster: [], selfPower: 0, selfModMuted: false, selfKicked: false });
  expect(get(s.store).muted).toBe(true);        // falls back to local self-mute, NOT auto-unmuted
});

it('moderate() invokes the moderate_voice IPC', async () => {
  const s = makeSession();
  await s.join('c', 'ch');
  await s.moderate('bb', 'mute');
  expect(invokeSpy).toHaveBeenCalledWith('moderate_voice',
    { communityId: 'c', channelId: 'ch', targetOwnerHex: 'bb', action: 'mute' });
});

it('selfKicked enters and clears the kicked state', async () => {
  const s = makeSession();
  await s.join('c', 'ch');
  emitPresence({ community: 'c', channel: 'ch', roster: [], selfPower: 0, selfModMuted: false, selfKicked: true });
  expect(get(s.store).selfKicked).toBe(true);
  emitPresence({ community: 'c', channel: 'ch', roster: [], selfPower: 0, selfModMuted: false, selfKicked: false });
  expect(get(s.store).selfKicked).toBe(false);
});
```

- [ ] **Step 2: Run to confirm failure.** Run: `npx vitest run src/lib/voice-session.test.ts`

- [ ] **Step 3: Implement.**
  - Extend `RosterMember` (line 24): add `modMuted: boolean;` and `power: number;`.
  - Extend `VoiceSessionState` (line 33): add `selfPower: number; selfModMuted: boolean; selfKicked: boolean;` and init them in the initial state.
  - In `subscribePresence` (line 437): read `modMuted`/`power` per entry into `lastRoster`, and set `selfPower`/`selfModMuted`/`selfKicked` on the store. On `selfModMuted` going `true`, force `muted = true`. On `selfModMuted` going `false` (was true), **keep `muted = true`** and publish a self-mute via the existing `set_voice_muted` path (so others see the self-mute glyph) — do not auto-unmute.
  - In `setMuted` (line 345): if `get(this.store).selfModMuted` and the caller asks to unmute (`muted === false`), return early (no-op) — a server-mute can't be self-cleared.
  - Add `refreshRoster` mapping for `modMuted`/`power` (line 478).
  - Add the method:
    ```typescript
    async moderate(targetOwnerHex: string, action: 'mute' | 'unmute' | 'kick' | 'unkick'): Promise<void> {
      if (!this.community || !this.channel) return;
      await this.deps.invoke('moderate_voice', {
        communityId: this.community, channelId: this.channel, targetOwnerHex, action,
      });
    }
    ```

- [ ] **Step 4: Run tests + tsc.** Run: `npx vitest run src/lib/voice-session.test.ts && npx tsc --noEmit`. Expected: pass + no type errors.
- [ ] **Step 5: Commit**
```bash
git add src/lib/voice-session.ts src/lib/voice-session.test.ts
git commit -m "feat(zeb-358): voice-session moderation state + moderate() + self-target handling"
```

---

## Task 10: Frontend `VoiceChannelView.svelte` — controls, badge, banners

**Files:**
- Modify: `src/lib/components/VoiceChannelView.svelte`
- Modify: `src/lib/components/__tests__/VoiceChannelView.test.ts`

- [ ] **Step 1: Write failing tests** (extend the existing `fakeSession` — add `selfPower`, `selfModMuted`, `selfKicked` to its state and a `moderate: vi.fn()`; add `selfOwnerHex` to props if the component needs it for self-exclusion):

```typescript
it('shows mod controls only when power-gated', () => {
  const session = fakeSession({ phase: 'connected', selfPower: 60,
    roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 0 }] });
  render(VoiceChannelView, { props: { session: session as never, selfOwnerHex: 'a'.repeat(32), ...base } });
  expect(screen.getByTestId('mod-mute')).toBeInTheDocument();
  expect(screen.getByTestId('mod-remove')).toBeInTheDocument();
});

it('hides mod controls when self lacks power over target', () => {
  const session = fakeSession({ phase: 'connected', selfPower: 50,
    roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 50 }] });
  render(VoiceChannelView, { props: { session: session as never, selfOwnerHex: 'a'.repeat(32), ...base } });
  expect(screen.queryByTestId('mod-mute')).not.toBeInTheDocument();
});

it('mute control calls moderate("mute")', async () => {
  const session = fakeSession({ phase: 'connected', selfPower: 60,
    roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 0 }] });
  render(VoiceChannelView, { props: { session: session as never, selfOwnerHex: 'a'.repeat(32), ...base } });
  await fireEvent.click(screen.getByTestId('mod-mute'));
  expect(session.moderate).toHaveBeenCalledWith('b'.repeat(32), 'mute');
});

it('Remove requires a confirm click before kicking', async () => {
  const session = fakeSession({ phase: 'connected', selfPower: 60,
    roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: false, power: 0 }] });
  render(VoiceChannelView, { props: { session: session as never, selfOwnerHex: 'a'.repeat(32), ...base } });
  await fireEvent.click(screen.getByTestId('mod-remove'));
  expect(session.moderate).not.toHaveBeenCalled();           // first click arms confirm
  await fireEvent.click(screen.getByTestId('mod-remove-confirm'));
  expect(session.moderate).toHaveBeenCalledWith('b'.repeat(32), 'kick');
});

it('renders a mod-muted badge distinct from self-mute', () => {
  const session = fakeSession({ phase: 'connected', selfPower: 0,
    roster: [{ ownerHex: 'b'.repeat(32), deviceHex: 'b'.repeat(64), muted: false, speaking: false, modMuted: true, power: 0 }] });
  render(VoiceChannelView, { props: { session: session as never, selfOwnerHex: 'a'.repeat(32), ...base } });
  expect(screen.getByTestId('mod-muted-badge')).toBeInTheDocument();
});

it('shows the moderator banners', () => {
  const muted = fakeSession({ phase: 'connected', selfModMuted: true });
  const { unmount } = render(VoiceChannelView, { props: { session: muted as never, selfOwnerHex: 'a'.repeat(32), ...base } });
  expect(screen.getByRole('status')).toHaveTextContent(/muted by a moderator/i);
  unmount();
  const kicked = fakeSession({ phase: 'connected', selfKicked: true });
  render(VoiceChannelView, { props: { session: kicked as never, selfOwnerHex: 'a'.repeat(32), ...base } });
  expect(screen.getByRole('alert')).toHaveTextContent(/removed by a moderator/i);
});
```

- [ ] **Step 2: Run to confirm failure.** Run: `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts`

- [ ] **Step 3: Implement.**
  - Add a `selfOwnerHex: string` prop (the component must know self to exclude self + gate). Wire it from the parent (`CommunityView`) — pass the value already used for self-detection elsewhere.
  - Compute per-member `canModerate = $voiceState.selfPower >= 50 && $voiceState.selfPower > m.power && m.ownerHex !== selfOwnerHex`.
  - In both the grid tile (130-149) and list row (150-162): when `canModerate`, render a `mod-mute` button (label Mute/Unmute by `m.modMuted`, `onclick={() => session.moderate(m.ownerHex, m.modMuted ? 'unmute' : 'mute')}`, `data-testid="mod-mute"`, no confirm) and a `mod-remove` button with a two-step confirm: first click sets a local `confirmingKick = m.deviceHex`; render a second `mod-remove-confirm` button (`data-testid="mod-remove-confirm"`) that calls `session.moderate(m.ownerHex, 'kick')`. Reset `confirmingKick` on blur/timeout.
  - Mod-muted badge: when `m.modMuted`, render `<span data-testid="mod-muted-badge" title="Muted by a moderator">🛡️🔇</span>` (distinct from the existing `m.muted` 🔇 glyph).
  - Self banners (near the existing micBlocked note ~role="status"): `{#if $voiceState.selfModMuted}<div role="status">You've been muted by a moderator.</div>{/if}` and `{#if $voiceState.selfKicked}<div role="alert">You were removed by a moderator. <button disabled={$voiceState.selfKicked} onclick={onRejoin}>Rejoin</button></div>{/if}`.
  - Disable the self unmute control when `$voiceState.selfModMuted` (the existing mute toggle, line ~165) with a tooltip "Muted by a moderator."

- [ ] **Step 4: Run tests + tsc.** Run: `npx vitest run src/lib/components/__tests__/VoiceChannelView.test.ts && npx tsc --noEmit`. Expected: pass. Also pass `selfOwnerHex` wherever `CommunityView` renders `VoiceChannelView` (update `CommunityView` + its test mock if it asserts props).
- [ ] **Step 5: Run the FULL frontend suite** (a shared-component prop change can break sibling tests — lesson from V5):
Run: `npx vitest run && npx tsc --noEmit`. Expected: all green; fix any mock that now needs `moderate`/`selfPower`/`selfOwnerHex`.
- [ ] **Step 6: Commit**
```bash
git add src/lib/components/VoiceChannelView.svelte src/lib/components/__tests__/ src/lib/components/CommunityView.svelte
git commit -m "feat(zeb-358): VoiceChannelView mod controls, badge, banners"
```

---

## Task 11: Multi-engine integration test

**Files:**
- Create: `src-tauri/tests/voice_moderation_integration.rs`

- [ ] **Step 1: Write the integration test**, mirroring `tests/voice_presence_two_engine_integration.rs` (find it: `ls src-tauri/tests/ | grep voice`) for engine/registry/channel-key setup. Use logical-time eviction like the V5 scale test (`now_ms` closure you control). Cover, with a 3-party setup (mod M power 60, target T power 0, observer O):

```rust
// Pseudocode of assertions — implement against the real two/three-engine harness:
// 1. mute: M issues Mute(T) → O's ActiveModeration.is_muted(T) true; a frame from T is dropped (not emitted);
//          the enriched roster O sees marks T modMuted; T's selfModMuted true.
// 2. kick: M issues Kick(T) → O's roster omits T; T's frame dropped; T selfKicked true; a T re-join beacon
//          stays suppressed while the kick is re-asserted.
// 3. unmute/unkick: M issues Unmute(T)/Unkick(T) (higher hlc) → enforcement clears on O within apply.
// 4. non-mod rejected: O (power 0) issues Mute(T) → M/observer drop it (verify_directive_authority Err); no effect.
// 5. power-not-greater: M issues Mute against an equal-power peer → rejected.
// 6. expiry: stop re-asserting; advance logical now past ENFORCE_TTL_MS; sweep lapses; is_muted false.
```

Build directives directly via `voice_moderation::{sign_directive, seal_directive}` and feed them through the real subscriber path (or assert on `ActiveModeration` + the emitted events), whichever the presence integration test's structure supports.

- [ ] **Step 2: Run it.**
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test voice_moderation_integration`
Expected: all cases pass. (If a case hits the known iroh/zenoh loopback flakes, re-run; those 6 flakes pass on CI — never block on them.)

- [ ] **Step 3: fmt + clippy on the test.** Run: `cargo fmt --all && cargo clippy --locked --test voice_moderation_integration --features test-fixtures --no-deps -- -D warnings`. Expected: clean.
- [ ] **Step 4: Commit**
```bash
git add src-tauri/tests/voice_moderation_integration.rs
git commit -m "test(zeb-358): multi-engine voice moderation integration"
```

---

## Task 12: Final gate sweep + push + PR

**Files:** none (verification + PR).

- [ ] **Step 1: Full Rust gate** (the `--all-targets` relink is acceptable once, at the end):
Run: `cd src-tauri && cargo fmt --all -- --check`
Run: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Run: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: fmt clean; clippy clean; tests pass except possibly the 6 known iroh/zenoh loopback flakes (reachability_publisher::force_notify_triggers_publish, zeb_321_connectivity_ipc_tests::force_republish_wakes_publisher, zenoh_iroh_link::paired_stream_roundtrip_via_loopback, two zenoh_iroh_transport tests, community_reachability_two_engine_integration::two_engines_exchange_via_iroh_zenoh) — verify any failures are exactly those before proceeding. Capture `$?` without piping (pipe exit codes lie).

- [ ] **Step 2: MSRV gate.**
Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: clean.

- [ ] **Step 3: Frontend gate.**
Run: `npx tsc --noEmit && npx vitest run`
Expected: all green.

- [ ] **Step 4: Push + open the PR.**
```bash
git push -u origin zeb-358-voice-moderation
gh pr create --repo zeblithic/harmony-client --title "ZEB-358: community voice moderation (server-mute + remove-from-voice)" --body "<see below>"
```
PR body: summary of the signed-directive control plane; link the spec (`docs/specs/2026-06-02-zeb-358-voice-moderation-design.md`) + this plan; parent ZEB-348; the four decisions (D1–D4) + the unattended-mic self-mute fallback; test plan checklist (Rust unit + wire fixture + multi-engine integration + frontend); note the 6 known loopback flakes are non-blocking.

- [ ] **Step 5: Autonomous bot-review loop.** Per standing directive: watch CodeRabbit / Cursor / CodeAnt / Qodo across all three comment buckets (inline review threads + PR issue-comments + reviews); bundle fixes into one push per round; ScheduleWakeup (~1200s) to self-pace; NEVER trigger Greptile; converge → pushover "ZEB-358 ready to merge" and STOP. **Standard merge gate — do NOT self-merge** (Jake merges). Post-merge: verify the Linear cascade (ZEB-358 → Done; ZEB-348 stays open with its other children ZEB-354/355/356/357/359/360).

---

## Self-review notes (spec coverage)

- D1 honest-client receiver-side → Task 7 media-drop + roster hide/flag (no key rotation). ✓
- D2 cooldown-sticky kick → Task 6 issuer re-assert (duration) + Task 7 presence suppression of kicked owners + self-kick teardown-but-stay-subscribed. ✓
- D3 time-boxed mute + unattended-mic self-mute fallback → Task 2 TTL/sweep + Task 9 `selfModMuted`-clears-to-self-mute + block self-unmute. ✓
- D4 dedicated control plane → Task 1 module + Task 6 control topic. ✓
- 5-step authority → Task 1 (open/sig) + Task 3 (membership/power). ✓
- Wire fixture → Task 4. ✓
- IPC → Task 8. ✓ FE controls/badge/banners → Task 10. ✓ Integration matrix → Task 11. ✓
- Type consistency: `ModAction`, `VoiceModerationDirective`, `ActiveModeration::{apply,sweep,is_muted,is_kicked,remove_channel}`, `verify_directive_authority`, `ModerateVoicePayload`, `RosterMember.{modMuted,power}`, `VoiceSessionState.{selfPower,selfModMuted,selfKicked}`, `session.moderate(ownerHex, action)` used consistently across tasks. ✓
