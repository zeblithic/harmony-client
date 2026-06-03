# ZEB-360 — Group-DM Voice Calls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add real-time voice calls to group DMs (3–16 members) by generalizing the 1:1 DM-call signaling to N-party and adding a group-space-scoped presence roster, reusing the DM media path and crypto verbatim.

**Architecture:** The DM voice **media** path (`harmony/voice/dm/{callId}/{device}` mesh, `derive_dm_voice_key`, `VOICE_DM_PACKET_AAD`) is already N-party and is reused **unchanged**. The work is (1) **signaling** — add an optional `space_id` to `VoiceSignal` so a group invite carries its space, and fan the sealed invite to *all* members instead of one peer; (2) **presence** — a new group-space-scoped beacon system (modeled on community-voice presence) sealed under a call-independent derived key, which drives the participant roster + the join-in-progress banner; (3) a new frontend `GroupCallSession` controller that fuses ring-all signaling with a VoiceSession-style roster + N-stream mix. The proven 1:1 `CallSession` and community `VoiceSession` are left untouched (media-engine wiring is *ported*, not shared).

**Tech Stack:** Rust (Tauri IPC, ciborium CBOR, ChaCha20-Poly1305, Ed25519/X25519, HKDF-SHA256, zenoh, tokio), TypeScript/Svelte (svelte stores, vitest), the existing V3 voice engine (VoiceSender/VoiceReceiver/VoiceMixer/talk-gate).

**Spec:** `docs/specs/2026-06-02-zeb-360-group-dm-voice-calls-design.md` (commit `2f51802`). Branch `zeb-360-group-dm-voice-calls` off `origin/main` `6d20594`.

---

## Design decisions locked by the spec (do not reopen)

- **D1** drop-in + ring: caller joins media immediately (no `ringingOut`); others ring; last-one-out ends it (emergent — no "end for everyone").
- **D2** join-in-progress via a persistent banner backed by group-space-scoped presence.
- **D3** new `GroupCallSession` controller (port, don't share).
- **D4** separate `*_group_call` IPCs (do not overload the 1:1 `*_dm_call`/`*_call` handlers).
- **D5** cap = group membership (3–16, enforced at space creation); join gated on `caller ∈ space.members`.
- **D6** one active voice session at a time (frontend busy-block — reused).
- **D7** start muted on connect (reused).
- **D8** no moderation (flat peer group).

**Signaling collapses to two kinds for the group path:** `Invite` (fanned to all other members) and `Decline` (sent back to the caller, parity only). The drop-in model means there is **no group Accept/Cancel/End signal** — the caller discovers joiners via presence beacons, and a leave is a presence tombstone. (`VoiceSignalKind` already has all five variants; the group path simply only ever sends `Invite`/`Decline`.)

---

## Reuse map (what is touched vs. reused verbatim)

| Concern | Disposition |
|---|---|
| Media topic `harmony/voice/dm/{callId}/*`, `VoiceOutbound::Dm`, `JoinDmCall`/`LeaveDmCall`/`SetDmCallMuted` | **Reused verbatim.** `join_group_call` sends `JoinDmCall` for the media half; `send_group_voice_frame` sends `VoiceOutbound::Dm`. |
| `derive_dm_voice_key`, `encrypt/decrypt_dm_voice_packet`, `VOICE_DM_PACKET_AAD` | **Reused verbatim.** |
| `VoiceSender`/`VoiceReceiver`/`VoiceMixer`/`makeTalkGate` | **Reused verbatim** (frontend frame event swapped to `group-voice-frame-received`). |
| `VoiceSignal` struct | **Extended** — add optional `space_id` (skip-when-None → 1:1 bytes unchanged). |
| Invite handler (`event_loop.rs`) | **Generalized** — `space_id`-present branch resolves the space directly + emits group events. |
| `send_voice_signal` / `resolve_dm_call_peer` (`lib.rs`) | **New group siblings** (`send_group_voice_signal` / `resolve_group_call_members`) fan to all members. |
| Community-voice presence (`voice_presence.rs`) | **New group siblings** — group topic + derived presence key + group membership check; `VoicePresenceMap`/`sign`/`verify`/roster reused. |

---

## File structure

**Rust (create/modify under `src-tauri/`):**
- Modify `src/voice_signal.rs` — add optional `space_id` to `VoiceSignal`.
- Modify `src/community_channel_log.rs` — add `derive_groupdm_presence_key`.
- Modify `src/voice_crypto.rs` — add `VOICE_GROUPDM_PRESENCE_AAD` + `encrypt/decrypt_groupdm_presence_packet` (+ `_with_nonce` fixture variant).
- Modify `src/voice_presence.rs` — add group beacon seal/open + group presence publisher/subscriber spawn fns + `groupdm_beacon_signer_is_member`.
- Modify `src/voice.rs` — add group `VoiceChannelRequest` variants + payload structs.
- Modify `src/event_loop.rs` — generalize the invite handler; handle the new request variants; emit the new events.
- Modify `src/lib.rs` — add the 8 group IPC commands + 2 helpers; register in both `generate_handler!` lists.
- Create `tests/group_dm_voice_three_engine_integration.rs` — 3-engine integration test.
- Modify `tests/wire_format_voice_fixtures.rs` — 1:1 byte-identity guard (already exists, assert unchanged) + new group-invite fixture + new group presence-beacon fixture.

**Frontend (create/modify under repo root):**
- Create `src/lib/group-call-session.ts` — `GroupCallSession` controller + `getGroupCallSession`.
- Create `src/lib/group-call-session.test.ts` — vitest cases.
- Create `src/lib/components/GroupCallBanner.svelte` — active-call banner.
- Modify `src/App.svelte` — wire the three group listeners + alerter + getGroupCallSession.
- Modify the group-DM view component (the GroupDm conversation view) — Call/Join button + `watch_group_call`/`unwatch_group_call` on mount/unmount + in-call bar reuse.

---

## Per-task Rust gating (relink-cost discipline)

For Rust tasks, gate **per-task** with the `--lib`-scoped commands (avoids relinking ~97 integration binaries each task):

```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

For tasks that add/modify an **integration test file** (`tests/*.rs`) or a **fixture** test, additionally run that one file:

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures
cd src-tauri && cargo nextest run --locked --features test-fixtures --test group_dm_voice_three_engine_integration
```

The authoritative full `--all-targets` sweep (fmt + clippy + nextest + msrv) runs **once** in the final task (T14). Each implementer subagent prompt MUST include: commit-before-gate, a 10-minute wall-clock kill switch on any single cargo command, and the `DONE_WITH_CONCERNS` escape hatch. macOS `timeout` is `gtimeout` — use the Bash tool's own timeout parameter, not a `timeout` prefix.

**Frontend gating** (T1 fixtures are Rust; frontend tasks T10–T13):

```bash
npx tsc --noEmit
npx vitest run src/lib/group-call-session.test.ts
```

---

## Task 1: Add optional `space_id` to `VoiceSignal` + fixture guards

Adds the one wire-format change. The 1:1 invite fixture must stay **byte-identical** (regression guard); a new group-invite fixture pins the `space_id`-present bytes.

**Files:**
- Modify: `src-tauri/src/voice_signal.rs` (the `VoiceSignal` struct, ~lines 38–65)
- Modify: `src-tauri/tests/wire_format_voice_fixtures.rs` (add two tests; the existing `voice_signal_invite_signed_inner_wire_bytes_pinned` stays unchanged)

- [ ] **Step 1: Add the `space_id` field to `VoiceSignal`**

In `src-tauri/src/voice_signal.rs`, add a `space_id` field to the `VoiceSignal` struct, placed **after `caller` and before `decline_reason`** (CBOR map order follows struct field order; placing it here keeps a stable, documented layout). Use a serde rename `"si"` and the same skip-when-None pattern `decline_reason` already uses so that a `None` value serializes to **zero bytes** (canonical CBOR omits the key), preserving 1:1 invite byte-identity. `SpaceId` is `pub struct SpaceId(pub [u8; 16])` from `crate::owner_state_types`; reuse the existing bstr (de)serializers used elsewhere in this file for 16-byte ids.

```rust
    /// OwnerAddr of this signal's **sender** … (existing `caller` field, unchanged)
    #[serde(rename = "cl")]
    pub caller: OwnerAddr,
    /// Group-DM space this call belongs to. `Some` only for group-DM calls
    /// (ZEB-360): the inbound handler resolves *this* space directly instead of
    /// scanning for a 2-member DM, lifting the `members.len() == 2` gate for the
    /// group path. `None` for 1:1 DM calls — and because it is skipped when
    /// `None`, the 1:1 canonical-CBOR bytes are unchanged (back-compat fixture
    /// guard in tests/wire_format_voice_fixtures.rs).
    #[serde(
        rename = "si",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_opt_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_opt_bytes_from_bstr"
    )]
    pub space_id: Option<crate::owner_state_types::SpaceId>,
    /// Optional decline reason; only present on `Decline` signals.
    #[serde(rename = "dr", default, skip_serializing_if = "Option::is_none")]
    pub decline_reason: Option<DeclineReason>,
```

> **If `serialize_opt_bytes_as_bstr` / `deserialize_opt_bytes_from_bstr` do not exist** in `owner_state_types.rs`: check first (`grep -n "serialize_opt_bytes_as_bstr\|serialize_bytes_as_bstr" src/owner_state_types.rs`). If only the non-`opt` versions exist, add thin `Option` wrappers next to them:
> ```rust
> pub fn serialize_opt_bytes_as_bstr<S, const N: usize>(
>     v: &Option<[u8; N]>, s: S,
> ) -> Result<S::Ok, S::Error>
> where S: serde::Serializer {
>     match v {
>         Some(b) => serialize_bytes_as_bstr(b, s),
>         None => s.serialize_none(),
>     }
> }
> ```
> However `SpaceId` is a newtype, not `[u8; N]`. Simpler and lower-risk: make the field `Option<SpaceId>` and give `SpaceId` itself a `serde` bstr round-trip if it doesn't already have one, then rely on plain `#[serde(rename="si", default, skip_serializing_if="Option::is_none")]` (no custom (de)serializer). Inspect how `SpaceId` is serialized elsewhere in a CBOR struct (e.g. `Space.id`, `owner_state_types.rs:1462` uses `#[serde(rename = "id")]` with no custom serde) — if `SpaceId`'s own `Serialize`/`Deserialize` already emits a bstr, drop the `serialize_with`/`deserialize_with` lines entirely. Choose whichever matches the existing `SpaceId` encoding; the only hard requirement is `None` ⇒ no bytes.

- [ ] **Step 2: Fix all `VoiceSignal { … }` construction sites**

The struct gained a field, so every literal construction breaks the build. Find them:

```bash
cd src-tauri && grep -rn "VoiceSignal {" src/ tests/
```

Add `space_id: None,` to each existing 1:1 construction site (in `send_voice_signal` in `lib.rs`, in `voice_signal.rs`'s own unit tests, and in `wire_format_voice_fixtures.rs`'s existing invite fixture). The group path (Task 7) sets it to `Some(...)`.

- [ ] **Step 3: Run the existing 1:1 fixture to prove byte-identity**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures -E 'test(voice_signal_invite)'`
Expected: `voice_signal_invite_signed_inner_wire_bytes_pinned` **PASSES unchanged** — the pinned hex `a2626264a3616b66696e76697465626369502222222222222222222222222222222262636c5033333333333333333333333333333333627367584054b358000dd6c00fe0614b13bcf707df035a330f1031d93b5423d56124748b55c35f831c13dcc16440c53be125be52df9acb80d01c9c17a4225d6cde2cb6a10f` still matches because `space_id: None` is skipped. **If this fails, the skip-when-None is wrong — fix before proceeding.**

- [ ] **Step 4: Write the new group-invite fixture (failing, to be pinned)**

Add to `src-tauri/tests/wire_format_voice_fixtures.rs`, modeled exactly on `voice_signal_invite_signed_inner_wire_bytes_pinned` but with `space_id: Some(SpaceId([0x44; 16]))`. Pin against a placeholder first so the test prints the actual:

```rust
/// ZEB-360: pin the signed-inner CBOR of a GROUP-DM voice Invite (space_id set).
/// Distinct from the 1:1 fixture by exactly the added `si` bstr(16) field; the
/// 1:1 fixture's byte-identity is the back-compat guard that proves the field is
/// skipped when None.
#[test]
fn voice_signal_group_invite_signed_inner_wire_bytes_pinned() {
    let (signing_key, identity_pub, device_hash) = fixture_caller_identity(0x42);
    let body = VoiceSignal {
        kind: VoiceSignalKind::Invite,
        call_id: [0x22; 16],
        caller: OwnerAddr([0x33; 16]),
        space_id: Some(SpaceId([0x44; 16])),
        decline_reason: None,
    };
    let mut body_bytes = Vec::new();
    ciborium::into_writer(&body, &mut body_bytes).expect("CBOR serialize VoiceSignal");
    let sig = sign_dm_packet(&body_bytes, &signing_key);
    let signed = SignedVoiceSignal { body, sig };
    let mut signed_bytes = Vec::new();
    ciborium::into_writer(&signed, &mut signed_bytes).expect("CBOR serialize SignedVoiceSignal");
    assert_eq!(
        hex::encode(&signed_bytes),
        "PIN_ME",
        "GroupDm SignedVoiceSignal signed-inner CBOR wire format drifted"
    );
    // Self-consistency: the pinned bytes must verify with the caller identity.
    verify_dm_packet_signature(&body_bytes, &sig, &identity_pub, device_hash)
        .expect("group Invite signature must verify");
}
```

Add `SpaceId` to the existing `use harmony_app::owner_state_types::{…}` import line in this file (it already imports `OwnerAddr`, `Hlc`, etc.).

- [ ] **Step 5: Pin the actual hex**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures -E 'test(voice_signal_group_invite)'`
Expected: FAIL with a left/right hex mismatch. Copy the **actual** (left) hex from the failure output and replace `"PIN_ME"` with it. Re-run; expected: PASS. (This is the repo's standard fixture-pinning procedure — the value is generated by the code, then locked.) Sanity-check the new hex is the 1:1 hex **plus** a `6273690…` (`"si"` + bstr-16) insertion and the outer map header bumped `a3…a3` (3 fields → with sig wrapper); it must differ from the 1:1 hex.

- [ ] **Step 6: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/voice_signal.rs src/lib.rs tests/wire_format_voice_fixtures.rs src/owner_state_types.rs
git commit -m "feat(zeb-360): optional space_id on VoiceSignal + group-invite fixture

1:1 invite fixture stays byte-identical (skip-when-None back-compat guard).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `derive_groupdm_presence_key`

A call-independent presence key derived from the group's shared `content_key`, so a member can decrypt presence beacons **before** joining a call (the banner needs this). Domain-separated from the per-call media key.

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (next to `derive_dm_voice_key`, ~lines 83–94)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src-tauri/src/community_channel_log.rs`:

```rust
    #[test]
    fn groupdm_presence_key_is_stable_and_domain_separated() {
        let ck = DmContentKey::new([0x11; 32]);
        // Stable across calls (no per-call salt): same content_key → same key.
        let a = derive_groupdm_presence_key(&ck);
        let b = derive_groupdm_presence_key(&ck);
        assert_eq!(a.as_bytes(), b.as_bytes(), "presence key must be call-independent");
        // Domain-separated from the media key for any call_id.
        let media = derive_dm_voice_key(&ck, &[0x22; 16]);
        assert_ne!(a.as_bytes(), media.as_bytes(), "presence key must differ from media key");
        // Different content_key → different presence key.
        let other = derive_groupdm_presence_key(&DmContentKey::new([0x99; 32]));
        assert_ne!(a.as_bytes(), other.as_bytes());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(groupdm_presence_key)'`
Expected: FAIL to compile — `derive_groupdm_presence_key` not found.

- [ ] **Step 3: Implement `derive_groupdm_presence_key`**

Add directly below `derive_dm_voice_key` in `src-tauri/src/community_channel_log.rs`:

```rust
/// HKDF-SHA256 derivation of the group-DM **presence** key from the group's
/// `DmContentKey`. Unlike `derive_dm_voice_key`, this is **call-independent**
/// (no `call_id` salt) so every member can derive it and decrypt presence
/// beacons BEFORE joining any call — the basis for the join-in-progress banner
/// (ZEB-360 D2). Domain-separated from the media key by a distinct `info`
/// string; the same group content key yields a presence key that is unrelated
/// to any per-call `derive_dm_voice_key` output and survives across successive
/// calls in the group.
pub fn derive_groupdm_presence_key(dm_key: &DmContentKey) -> ChannelKey {
    let mut out = zeroize::Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(None, dm_key.as_bytes())
        .expand(b"voice-presence-groupdm:", out.as_mut())
        .expect("32 <= 8160");
    ChannelKey(*out)
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(groupdm_presence_key)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/community_channel_log.rs
git commit -m "feat(zeb-360): derive_groupdm_presence_key (call-independent presence key)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Group-DM presence packet crypto + beacon seal/open + presence-beacon fixture

The presence beacon is sealed under a `(presence_key, space_id)`-scoped AEAD (parallel to the community `(channel_key, community, channel)` seal). Adds the crypto primitives, the beacon seal/open helpers, and a pinned wire fixture.

**Files:**
- Modify: `src-tauri/src/voice_crypto.rs` (add AAD const + `encrypt/decrypt_groupdm_presence_packet` + `_with_nonce` variant)
- Modify: `src-tauri/src/voice_presence.rs` (add `seal_groupdm_presence_beacon` / `open_groupdm_presence_beacon` + a `_with_nonce` fixture variant)
- Modify: `src-tauri/tests/wire_format_voice_fixtures.rs` (new group presence-beacon fixture)

- [ ] **Step 1: Add the AAD constant + crypto fns**

In `src-tauri/src/voice_crypto.rs`, next to the existing `VOICE_DM_PACKET_AAD` and `encrypt/decrypt_dm_voice_packet`, add:

```rust
/// AAD domain tag for group-DM presence beacons (ZEB-360). Distinct from
/// VOICE_DM_PACKET_AAD / VOICE_PRESENCE_AAD so a beacon can never be confused
/// for media or community-presence even under the same key.
pub const VOICE_GROUPDM_PRESENCE_AAD: &[u8] = b"harmony-voice-groupdm-presence-v1";

/// Seal a group-DM presence payload: AAD = domain || space_id(16). Mirrors
/// `encrypt_dm_voice_packet` but binds the 16-byte `space_id` instead of a
/// `call_id` (presence is group-scoped, not call-scoped).
pub fn encrypt_groupdm_presence_packet(
    key: &ChannelKey,
    space_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::AeadCore;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let nonce_bytes: [u8; NONCE_LEN] = nonce.into();
    seal_inner_groupdm(key, space_id, domain, plaintext, nonce_bytes)
}

/// Deterministic-nonce variant for wire-format fixtures ONLY. NEVER call from
/// production — a fixed nonce under a reused key is catastrophic nonce reuse.
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub fn encrypt_groupdm_presence_packet_with_nonce(
    key: &ChannelKey,
    space_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    seal_inner_groupdm(key, space_id, domain, plaintext, nonce)
}

fn seal_inner_groupdm(
    key: &ChannelKey,
    space_id: &[u8; 16],
    domain: &[u8],
    plaintext: &[u8],
    nonce_bytes: [u8; NONCE_LEN],
) -> Result<Vec<u8>, VoiceCryptoError> {
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let mut aad = Vec::with_capacity(domain.len() + 16);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(space_id);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: plaintext, aad: &aad })
        .map_err(|_| VoiceCryptoError::SealFailed)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a group-DM presence packet sealed by `encrypt_groupdm_presence_packet`.
pub fn decrypt_groupdm_presence_packet(
    key: &ChannelKey,
    space_id: &[u8; 16],
    domain: &[u8],
    packet: &[u8],
) -> Result<Vec<u8>, VoiceCryptoError> {
    if packet.len() < MIN_PACKET_LEN {
        return Err(VoiceCryptoError::TooShort(packet.len()));
    }
    let (nonce_bytes, ct) = packet.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
    let mut aad = Vec::with_capacity(domain.len() + 16);
    aad.extend_from_slice(domain);
    aad.extend_from_slice(space_id);
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), Payload { msg: ct, aad: &aad })
        .map_err(|_| VoiceCryptoError::OpenFailed)
}
```

> Verify the exact names of `NONCE_LEN`, `MIN_PACKET_LEN`, `Payload`, `Nonce`, `VoiceCryptoError::{SealFailed,OpenFailed,TooShort}`, and `ChannelKey::as_bytes` against the existing `seal_inner` / `encrypt_dm_voice_packet` in this file and mirror them exactly (the DM functions are the template).

- [ ] **Step 2: Add a crypto round-trip unit test**

In `voice_crypto.rs`'s test module:

```rust
    #[test]
    fn groupdm_presence_packet_round_trips_and_binds_space() {
        let key = ChannelKey([0x33; 32]);
        let sp = [0x44u8; 16];
        let pt = b"beacon-bytes".to_vec();
        let sealed = encrypt_groupdm_presence_packet(&key, &sp, VOICE_GROUPDM_PRESENCE_AAD, &pt).unwrap();
        assert_eq!(decrypt_groupdm_presence_packet(&key, &sp, VOICE_GROUPDM_PRESENCE_AAD, &sealed).unwrap(), pt);
        // Wrong space_id in AAD must fail.
        assert!(decrypt_groupdm_presence_packet(&key, &[0x45; 16], VOICE_GROUPDM_PRESENCE_AAD, &sealed).is_err());
    }
```

- [ ] **Step 3: Add beacon seal/open helpers in `voice_presence.rs`**

The existing `VoicePresenceBeacon` / `SignedVoicePresenceBeacon` / `sign_presence_beacon` / `verify_presence_beacon_sig` are **reused as-is**. Add group-scoped seal/open siblings next to `seal_presence_beacon` / `open_presence_beacon`:

```rust
/// Seal a signed presence beacon for a group-DM space (ZEB-360). Mirrors
/// `seal_presence_beacon` but scopes the AEAD to `space_id` under the
/// group-DM presence key (`derive_groupdm_presence_key`).
pub fn seal_groupdm_presence_beacon(
    key: &ChannelKey,
    space_id: &SpaceId,
    signed: &SignedVoicePresenceBeacon,
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_groupdm_presence_packet(
        key, &space_id.0, crate::voice_crypto::VOICE_GROUPDM_PRESENCE_AAD, &plain,
    )
    .map_err(|_| BeaconError::Encode)
}

/// Open + decode a sealed group-DM presence beacon. Returns `None` on any
/// failure (drop), matching `open_presence_beacon`.
pub fn open_groupdm_presence_beacon(
    key: &ChannelKey,
    space_id: &SpaceId,
    packet: &[u8],
) -> Option<SignedVoicePresenceBeacon> {
    let plain = crate::voice_crypto::decrypt_groupdm_presence_packet(
        key, &space_id.0, crate::voice_crypto::VOICE_GROUPDM_PRESENCE_AAD, packet,
    )
    .ok()?;
    ciborium::from_reader(plain.as_slice()).ok()
}

/// Deterministic-nonce seal for wire-format fixtures ONLY.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn seal_groupdm_presence_beacon_with_nonce(
    key: &ChannelKey,
    space_id: &SpaceId,
    signed: &SignedVoicePresenceBeacon,
    nonce: [u8; 12],
) -> Result<Vec<u8>, BeaconError> {
    let plain = canonical_cbor_encode(signed).map_err(|_| BeaconError::Encode)?;
    crate::voice_crypto::encrypt_groupdm_presence_packet_with_nonce(
        key, &space_id.0, crate::voice_crypto::VOICE_GROUPDM_PRESENCE_AAD, &plain, nonce,
    )
    .map_err(|_| BeaconError::Encode)
}
```

Ensure `SpaceId` is imported in `voice_presence.rs` (it imports `ChannelId`, `SpaceId` already per the community presence code — verify).

- [ ] **Step 4: Add the new presence-beacon wire fixture**

In `src-tauri/tests/wire_format_voice_fixtures.rs`, add a group presence-beacon pin modeled on `presence_beacon_wire_bytes_pinned`, using the **derived presence key** and the group seal:

```rust
/// ZEB-360: pin the sealed group-DM presence beacon wire format. Sealed under
/// the call-independent derive_groupdm_presence_key, scoped to a space_id.
#[test]
fn groupdm_presence_beacon_wire_bytes_pinned() {
    use harmony_app::community_channel_log::derive_groupdm_presence_key;
    use harmony_app::owner_state_types::DmContentKey;
    use harmony_app::voice_presence::{open_groupdm_presence_beacon, seal_groupdm_presence_beacon_with_nonce};
    let key = derive_groupdm_presence_key(&DmContentKey::new([0x11; 32]));
    let space = SpaceId([0x44; 16]);
    let signed = fixture_signed_beacon(); // reuse the existing deterministic beacon
    let sealed = seal_groupdm_presence_beacon_with_nonce(&key, &space, &signed, [0u8; 12]).expect("seal");
    assert_eq!(hex::encode(&sealed), "PIN_ME", "group-DM presence-beacon wire format drifted");
    // Round-trip back-compat: pinned bytes must reopen to the same beacon.
    let opened = open_groupdm_presence_beacon(&key, &space, &sealed).expect("open pinned beacon");
    assert_eq!(opened, signed, "pinned group beacon decoded to a different value");
}
```

- [ ] **Step 5: Pin the actual hex + run all voice fixtures**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures -E 'test(groupdm_presence_beacon)'`
Expected: FAIL (mismatch). Replace `"PIN_ME"` with the actual (left) hex; re-run → PASS.
Then run the whole file to confirm nothing else drifted:
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures`
Expected: ALL pass (including the unchanged `voice_packet_wire_bytes_pinned`, `presence_beacon_wire_bytes_pinned`, `dm_voice_packet_wire_bytes_pinned`, `voice_signal_invite_signed_inner_wire_bytes_pinned`).

- [ ] **Step 6: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/voice_crypto.rs src/voice_presence.rs tests/wire_format_voice_fixtures.rs
git commit -m "feat(zeb-360): group-DM presence packet crypto + beacon seal/open + fixture

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `voice.rs` group request variants + payload structs

Adds the control-plane vocabulary the IPC layer and event loop speak. Media reuses the existing `JoinDmCall`/`LeaveDmCall`/`VoiceOutbound::Dm`/`SetDmCallMuted`; this task adds only the **presence** variants and the group payload DTOs.

**Files:**
- Modify: `src-tauri/src/voice.rs` (the `VoiceChannelRequest` enum ~lines 71–79, and the payload structs ~lines 115–126)

- [ ] **Step 1: Add the presence `VoiceChannelRequest` variants**

Add to the `VoiceChannelRequest` enum (alongside `JoinDmCall`/`LeaveDmCall`/`SetDmCallMuted`). These carry the derived presence key + the signing caps so the event loop can spawn the publisher/subscriber. `VoiceGroupPresenceCaps` is a new struct (define it below the enum):

```rust
    /// ZEB-360: start a READ-ONLY group-DM presence subscriber for the banner
    /// (no beacon publishing). Idempotent per space_id.
    WatchGroupCall {
        space_id: [u8; 16],
        presence_key: std::sync::Arc<ChannelKey>,
    },
    /// ZEB-360: stop the read-only subscriber for a space (banner closed).
    UnwatchGroupCall { space_id: [u8; 16] },
    /// ZEB-360: start the group-DM presence PUBLISHER for our own beacon on a
    /// call we are joining. The read subscriber (WatchGroupCall) is reused for
    /// the in-call roster; this only adds the publisher.
    StartGroupPresence {
        space_id: [u8; 16],
        call_id: [u8; 16],
        presence_key: std::sync::Arc<ChannelKey>,
        caps: VoiceGroupPresenceCaps,
    },
    /// ZEB-360: publish a `left` tombstone beacon and stop the publisher (we are
    /// leaving the call). The read subscriber persists if the DM is still open.
    StopGroupPresence { space_id: [u8; 16], call_id: [u8; 16] },
    /// ZEB-360: flip the mute bit for an active group call — updates BOTH the
    /// media mute flag (so frames stop) AND the presence beacon's muted bit.
    SetGroupCallMuted { space_id: [u8; 16], call_id: [u8; 16], muted: bool },
```

Define the caps struct (mirror `VoiceJoinCaps`'s fields used by the publisher):

```rust
/// ZEB-360: capabilities needed to publish a group-DM presence beacon.
#[derive(Clone)]
pub struct VoiceGroupPresenceCaps {
    pub signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    pub self_owner: crate::owner_state_types::OwnerAddr,
    pub self_device: [u8; 32],
    pub joined_hlc: crate::owner_state_types::Hlc,
}
```

- [ ] **Step 2: Add the group payload structs**

Next to `SendDmVoiceFramePayload` / `SetDmCallMutedPayload`:

```rust
/// ZEB-360: a media frame for a group-DM call. Identical shape to the DM
/// payload — group media reuses the DM media path (topic + key + AAD).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendGroupVoiceFramePayload {
    pub call_id: String,
    pub frame_bytes: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetGroupCallMutedPayload {
    pub call_id: String,
    pub space_id: String,
    pub muted: bool,
}
```

- [ ] **Step 3: Build-check**

Run: `cd src-tauri && cargo check --locked -p harmony-app --features test-fixtures`
Expected: compiles (the new variants are unused until Task 6/8 — a dead-code warning is acceptable here, or add `#[allow(dead_code)]` temporarily and remove it in Task 6). If the match on `VoiceChannelRequest` in `event_loop.rs` is non-exhaustive now, that's expected — Task 6 adds the arms; for THIS task's gate, a `todo!()`-free temporary `_ => {}` is NOT acceptable, so instead defer the exhaustiveness break by gating: add the arms as stubs in Task 6. For this task, only `cargo check` of the lib needs to pass; if the existing match is exhaustive and now breaks, add minimal `Self::WatchGroupCall { .. } | Self::UnwatchGroupCall { .. } | Self::StartGroupPresence { .. } | Self::StopGroupPresence { .. } | Self::SetGroupCallMuted { .. } => { /* wired in Task 6 */ }` placeholder arm to the event-loop match and replace it in Task 6.

- [ ] **Step 4: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/voice.rs src/event_loop.rs
git commit -m "feat(zeb-360): voice.rs group presence request variants + payloads

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Group-DM presence publisher/subscriber + membership check

Adds the spawn functions modeled on the community-voice presence tasks, plus the group membership check. The `VoicePresenceMap`, `sign_presence_beacon`, `verify_presence_beacon_sig`, `RosterEntry`, and roster/eviction logic are **reused verbatim** — only the topic, seal key, and membership source differ.

**Files:**
- Modify: `src-tauri/src/voice_presence.rs`

- [ ] **Step 1: Add the group membership check**

The community check `beacon_signer_is_member(registry, community, owner, device)` resolves against a `CommunitySyncRegistry`. Group DMs aren't communities; membership lives in the CRDT `spaces[space_id].members` + the `owner_device_cache`. Add a CRDT-backed check. Inspect the `OwnerState` type (the `Arc<Mutex<OwnerState>>` `crdt_state`) for the exact field/accessor names (`spaces`, `owner_device_cache.devices`, `device_identity_pubs`) — they appear verbatim in `event_loop.rs`'s invite handler. Add:

```rust
/// ZEB-360: a group-DM presence beacon is admissible iff its signer
/// (`owner` + `device`) is an enrolled device of a current member of
/// `space_id`. `device` here is the 32-byte Ed25519 verifying key; an enrolled
/// device's identity_pub is `[X25519(32) || Ed25519(32)]`, so we compare the
/// beacon `device` against bytes [32..64] of each cached identity_pub.
pub async fn groupdm_beacon_signer_is_member(
    crdt_state: &tokio::sync::Mutex<crate::owner_state_types::OwnerState>,
    space_id: &SpaceId,
    owner: &OwnerAddr,
    device: &[u8; 32],
) -> bool {
    let os = crdt_state.lock().await;
    let Some(space) = os.spaces.get(space_id) else { return false; };
    if !space.members.contains(owner) {
        return false;
    }
    let Some(entry) = os.owner_device_cache.devices.get(owner) else { return false; };
    entry.device_identity_pubs.iter().flatten().any(|ip| &ip[32..64] == device.as_slice())
}
```

> Verify `OwnerState`'s module path and that `spaces`/`owner_device_cache` are directly accessible (the invite handler reads `g.spaces` and `g.owner_device_cache.devices` on a `crdt_for_signal.lock().await`, so the fields are public/visible). Adjust the type of the `crdt_state` param to match what the subscriber spawn will hold (likely `Arc<tokio::sync::Mutex<OwnerState>>`).

- [ ] **Step 2: Add the group presence publisher spawn fn**

Model on `spawn_voice_presence_publisher` (reconnaissance: `voice_presence.rs:581-635`). Differences: group topic, group seal, no `self_kicked` (D8 no moderation). Use `build_heartbeat_beacon` (reused) — the beacon now also needs the `call_id`. **The beacon struct must carry `call_id`** so the subscriber and frontend can correlate; add a `call_id: [u8; 16]` field to `VoicePresenceBeacon`? **No — do not mutate the shared beacon** (it would drift the community presence fixture). Instead the group beacons travel on a space topic but the **roster key** is `(space_id, call_id)` and the publisher embeds `call_id` by **sealing per-call**: the group presence map is keyed by `(space_id, call_id)`, and the subscriber learns the `call_id` from the **topic is space-scoped** — so the publisher must include `call_id` in the sealed payload. The lowest-risk way that avoids touching `VoicePresenceBeacon`: define a thin group wrapper.

Add a group beacon wrapper type + heartbeat builder:

```rust
/// ZEB-360: a presence beacon plus the call_id it belongs to. The group
/// presence topic is space-scoped (so a banner can discover the active call
/// without holding the call_id), therefore the call_id must ride INSIDE the
/// sealed payload. Wrapping (rather than adding a field to VoicePresenceBeacon)
/// keeps the community-voice beacon + its pinned fixture byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSignedPresenceBeacon {
    #[serde(rename = "ci", serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
            deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr")]
    pub call_id: [u8; 16],
    #[serde(rename = "sb")]
    pub signed: SignedVoicePresenceBeacon,
}
```

Update the group seal/open helpers from Task 3 Step 3 to seal/open a `GroupSignedPresenceBeacon` instead of a bare `SignedVoicePresenceBeacon` (change the `signed: &SignedVoicePresenceBeacon` params to `wrapped: &GroupSignedPresenceBeacon` and the open return to `Option<GroupSignedPresenceBeacon>`). **Re-pin the Task 3 fixture** with the wrapper (the fixture builds a `GroupSignedPresenceBeacon { call_id: [0x22;16], signed: fixture_signed_beacon() }`). This wrapper decision supersedes Task 3's bare-beacon seal; if Task 3 already shipped, this task amends it — note that in the commit.

Then the publisher:

```rust
#[allow(clippy::too_many_arguments)]
pub fn spawn_groupdm_presence_publisher(
    session: zenoh::Session,
    topic: String,
    presence_key: Arc<ChannelKey>,
    space_id: SpaceId,
    call_id: [u8; 16],
    signing_key: Arc<ed25519_dalek::SigningKey>,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: Hlc,
    muted: Arc<AtomicBool>,
    seq_counter: Arc<AtomicU64>,
    interval: std::time::Duration,
    closing: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if closing.load(Ordering::SeqCst) { break; }
            let seq = seq_counter.fetch_add(1, Ordering::SeqCst);
            let beacon = build_heartbeat_beacon(self_owner, self_device, &joined_hlc, seq, muted.load(Ordering::SeqCst));
            let Ok(signed) = sign_presence_beacon(beacon, &signing_key) else { continue; };
            let wrapped = GroupSignedPresenceBeacon { call_id, signed };
            let Ok(sealed) = seal_groupdm_presence_beacon(&presence_key, &space_id, &wrapped) else { continue; };
            if let Err(e) = session.put(&topic, sealed).await {
                tracing::warn!(%topic, err = %e, "group presence publish failed");
            }
        }
    })
}
```

Add a tombstone publish helper (one-shot, `left: true`, `seq = u64::MAX`) modeled on the community immediate-beacon path:

```rust
/// ZEB-360: publish a single `left` tombstone for our beacon, so peers evict us
/// from the roster immediately rather than waiting out the TTL.
pub async fn publish_groupdm_leave_tombstone(
    session: &zenoh::Session,
    topic: &str,
    presence_key: &ChannelKey,
    space_id: &SpaceId,
    call_id: [u8; 16],
    signing_key: &ed25519_dalek::SigningKey,
    self_owner: OwnerAddr,
    self_device: [u8; 32],
    joined_hlc: &Hlc,
) {
    let mut beacon = build_heartbeat_beacon(self_owner, self_device, joined_hlc, u64::MAX, true);
    beacon.left = true;
    let Ok(signed) = sign_presence_beacon(beacon, signing_key) else { return; };
    let wrapped = GroupSignedPresenceBeacon { call_id, signed };
    if let Ok(sealed) = seal_groupdm_presence_beacon(presence_key, space_id, &wrapped) {
        let _ = session.put(topic, sealed).await;
    }
}
```

> Verify `build_heartbeat_beacon`'s signature and whether `VoicePresenceBeacon.left` is settable (it is a public bool field per the struct in reconnaissance §3a). Confirm `Hlc`, `OwnerAddr`, `SpaceId`, `ChannelKey`, `AtomicBool/U64`, `Ordering`, `JoinHandle` are imported.

- [ ] **Step 3: Add the group presence subscriber spawn fn**

Model on `spawn_voice_presence_subscriber` (`voice_presence.rs:446-511`). Differences: group topic, group open, group membership check, roster keyed by `(space_id, call_id)`, emits `group-call-presence-changed { spaceId, callId, roster }`. Reuse `VoicePresenceMap` keyed by `(SpaceId, ChannelId)` where the `ChannelId` slot holds `ChannelId(call_id)`:

```rust
#[allow(clippy::too_many_arguments)]
pub fn spawn_groupdm_presence_subscriber<R: tauri::Runtime>(
    session: zenoh::Session,
    topic: String,
    presence_key: Arc<ChannelKey>,
    space_id: SpaceId,
    crdt_state: Arc<tokio::sync::Mutex<crate::owner_state_types::OwnerState>>,
    map: Arc<tokio::sync::Mutex<VoicePresenceMap>>,
    app: tauri::AppHandle<R>,
    closing: Arc<AtomicBool>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let sub = match session.declare_subscriber(&topic).await {
            Ok(s) => s,
            Err(e) => { tracing::error!(%topic, err = %e, "group presence subscribe failed"); return; }
        };
        while let Ok(sample) = sub.recv_async().await {
            if sample.payload().len() > crate::voice_crypto::MAX_VOICE_PACKET_BYTES { continue; }
            let bytes = sample.payload().to_bytes().to_vec();
            let Some(wrapped) = open_groupdm_presence_beacon(&presence_key, &space_id, &bytes) else { continue; };
            if verify_presence_beacon_sig(&wrapped.signed).is_err() { continue; }
            let owner = OwnerAddr(wrapped.signed.beacon.owner);
            if !groupdm_beacon_signer_is_member(&crdt_state, &space_id, &owner, &wrapped.signed.beacon.device).await { continue; }
            let call_chan = ChannelId(wrapped.call_id);
            let changed = {
                let mut g = map.lock().await;
                g.apply(&space_id, &call_chan, &wrapped.signed.beacon, (now_ms)())
            };
            if changed {
                let roster = { let g = map.lock().await; g.roster(&space_id, &call_chan) };
                let _ = app.emit("group-call-presence-changed", serde_json::json!({
                    "spaceId": hex::encode(space_id.0),
                    "callId": hex::encode(wrapped.call_id),
                    "roster": roster,
                }));
            }
        }
        if !closing.load(Ordering::SeqCst) {
            tracing::warn!(%topic, "group presence subscriber closed unexpectedly");
        }
    })
}
```

> `VoicePresenceMap::apply/roster/sweep/remove_channel` take `(&SpaceId, &ChannelId, …)` — passing `ChannelId(call_id)` reuses them unchanged. `OwnerAddr.0` is `[u8;16]`; the beacon's `owner` is `[u8;16]` — matches.

- [ ] **Step 4: Add unit tests**

```rust
    #[tokio::test]
    async fn group_beacon_rejected_for_non_member() {
        // Build an OwnerState with a space whose members do NOT include the
        // beacon signer's owner → groupdm_beacon_signer_is_member == false.
        // (Construct via the test helpers used elsewhere in this module/tests.)
    }
```

Fill this with the module's existing test-construction idiom for `OwnerState` + a `Space` (see `tests/dm_unicast_integration.rs::make_dm_space` for the Space shape; build a `GroupDm` space with two members, check a third owner is rejected and a member with a matching enrolled device is accepted). If constructing `OwnerState` inline is heavy, defer the positive/negative membership coverage to the 3-engine integration test (Task 9) and keep only a compile-level smoke here.

- [ ] **Step 5: Gate**

Run:
```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(group)'
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_voice_fixtures
```
Expected: clippy clean; group unit tests pass; the re-pinned group presence-beacon fixture passes (re-pin if the wrapper changed the bytes).

- [ ] **Step 6: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/voice_presence.rs tests/wire_format_voice_fixtures.rs
git commit -m "feat(zeb-360): group-DM presence publisher/subscriber + membership check

Wrap beacons with call_id (space-scoped topic) without touching the shared
community beacon; reuse VoicePresenceMap/sign/verify/roster verbatim.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Event-loop wiring — invite handler generalization + presence request arms + events

The integration layer. **Media arms are NOT touched** — `join_group_call` reuses `JoinDmCall`. This task: (a) generalizes the inbound invite handler to route `space_id`-present signals to group events; (b) implements the five new presence request arms; (c) maintains per-space presence task handles + maps.

**Files:**
- Modify: `src-tauri/src/event_loop.rs`

This is a **port/wire** task. Each step references the exact existing code the implementer mirrors. Read those anchors before writing.

- [ ] **Step 1: Generalize the inbound invite/decline handler**

Anchor: `event_loop.rs` ~1620–1665 (the `if matches!(signal.kind, VoiceSignalKind::Invite)` block that scans for a 2-member DM and calls `emit_voice_signal_event(..., Some(sh))`). Reconnaissance §2 has the full block.

Change the per-verified-signal logic so that **when `signal.space_id` is `Some(sp)`** it takes the group path, else the existing 1:1 path:

```rust
if let Some(sp) = signal.space_id {
    // ZEB-360 group path: the signal names its space directly — no 2-member
    // scan. Verify the space exists, is a GroupDm, the caller is a member,
    // and it carries a content_key. Then route by kind.
    let ok = {
        let g = crdt_for_signal.lock().await;
        g.spaces.get(&sp).is_some_and(|s| {
            s.kind == crate::owner_state_types::SpaceKind::GroupDm
                && s.content_key.is_some()
                && s.members.contains(&signal.caller)
        })
    };
    if ok {
        emit_group_voice_signal_event(&app_for_signal, &signal, &hex::encode(sp.0));
    } else {
        tracing::debug!("group voice signal dropped: space invalid / caller not a member");
    }
} else if matches!(signal.kind, crate::voice_signal::VoiceSignalKind::Invite) {
    // ... existing 1:1 Invite 2-member-scan block, UNCHANGED ...
} else {
    emit_voice_signal_event(&app_for_signal, &signal, None);
}
```

> Note: for `Decline` the `caller` field is the **decliner** (the responder), so `members.contains(&signal.caller)` still correctly requires the decliner to be a group member. Good.

- [ ] **Step 2: Add `emit_group_voice_signal_event`**

Next to `emit_voice_signal_event` (`event_loop.rs:4069`). Group only ever emits for `Invite` / `Decline`:

```rust
fn emit_group_voice_signal_event<R: Runtime>(
    app: &AppHandle<R>,
    signal: &crate::voice_signal::VoiceSignal,
    space_id_hex: &str,
) {
    use crate::voice_signal::VoiceSignalKind;
    let call_hex = hex::encode(signal.call_id);
    match signal.kind {
        VoiceSignalKind::Invite => {
            let _ = app.emit("incoming-group-call", serde_json::json!({
                "callId": call_hex,
                "callerOwner": hex::encode(signal.caller.0),
                "spaceId": space_id_hex,
            }));
        }
        VoiceSignalKind::Decline => {
            // `caller` on a Decline is the decliner (responder).
            let _ = app.emit("group-call-declined", serde_json::json!({
                "callId": call_hex,
                "spaceId": space_id_hex,
                "owner": hex::encode(signal.caller.0),
            }));
        }
        // Drop-in model: no group Accept/Cancel/End signals are sent.
        _ => {}
    }
}
```

- [ ] **Step 3: Add per-space presence task bookkeeping**

In the voice event-loop state (where `dm_voice_mute_flags` and the community presence map/handles live), add maps keyed by `space_id` for: the read-subscriber `JoinHandle`, the publisher `JoinHandle`, the per-call publisher `muted` `Arc<AtomicBool>` + `seq_counter` `Arc<AtomicU64>`, the shared `VoicePresenceMap`, and the `joined_hlc`/topic needed for the leave tombstone. Use the community presence wiring (`event_loop.rs:3031-3106`) as the structural template, but keyed by `space_id` (16 bytes) and reused across calls in the same space.

- [ ] **Step 4: Implement the five presence request arms**

In the `VoiceChannelRequest` match (replacing the Task 4 placeholder arm):

- `WatchGroupCall { space_id, presence_key }` — if no read subscriber exists for `space_id`, build the topic `format!("harmony/voice-presence/group-dm/{}", hex::encode(space_id))`, ensure a `VoicePresenceMap` exists for the space, and `spawn_groupdm_presence_subscriber(...)`; store the handle. Idempotent (a second watch is a no-op). Snapshot `crdt_state`, `now_ms`, `app`, `closing` from the loop the same way the community subscriber does.
- `UnwatchGroupCall { space_id }` — only stop the read subscriber **if no publisher is active** for that space (an in-call member keeps the roster live). Abort the subscriber handle and drop the map entry via `map.remove_channel`.
- `StartGroupPresence { space_id, call_id, presence_key, caps }` — ensure the read subscriber is running (reuse `WatchGroupCall`'s setup if absent), then create the per-call `muted=Arc::new(AtomicBool::new(true))` (start muted, D7) + `seq_counter=Arc::new(AtomicU64::new(0))`, store the `muted` flag in a `(space_id, call_id)`-keyed map so `SetGroupCallMuted` can flip it, and `spawn_groupdm_presence_publisher(session, topic, presence_key, SpaceId(space_id), call_id, caps.signing_key, caps.self_owner, caps.self_device, caps.joined_hlc, muted, seq_counter, Duration::from_secs(4), closing)`. Keep `caps.joined_hlc` + topic + presence_key around for the tombstone.
- `StopGroupPresence { space_id, call_id }` — `publish_groupdm_leave_tombstone(...).await` with the stored caps, then abort the publisher handle and drop the per-call muted flag. Leave the read subscriber running if the DM view is still watching (track a watch-refcount or a `watching: bool` per space; simplest: keep the subscriber and let `UnwatchGroupCall` stop it).
- `SetGroupCallMuted { space_id, call_id, muted }` — set the **presence** muted `Arc<AtomicBool>` (so the next beacon reflects it) AND forward to the media mute path: send/store the same flag the media uses. Since group media reuses `JoinDmCall`, the media mute is the existing DM mute flag keyed by `call_id` (`dm_voice_mute_flags`) — set it here too (mirror what the `SetDmCallMuted` arm does). If cleaner, have `set_group_call_muted` IPC send BOTH `SetDmCallMuted { call_id, muted }` and `SetGroupCallMuted { space_id, call_id, muted }`; then this arm only flips the presence flag. Pick one and keep media + presence consistent.

- [ ] **Step 5: Confirm media reuse needs no new event**

The DM media subscriber already emits `dm-voice-frame-received`. Per spec, group media frames should arrive on `group-voice-frame-received` to keep the paths cleanly separated. **Decision:** the simplest correct approach is to have `join_group_call` send `JoinDmCall` (media identical) and let frames arrive on `dm-voice-frame-received`, with the frontend `GroupCallSession` filtering by `callId` — BUT the 1:1 `CallSession` also listens on `dm-voice-frame-received` filtered by its own `callId`. Since a node is never in a 1:1 and a group call simultaneously (D6), and `callId`s are globally unique, both controllers filtering by `callId` on the shared event is safe and avoids a backend change. **Choose this**: reuse `dm-voice-frame-received`; the `GroupCallSession` filters by its `callId`. (This overrides the spec's `group-voice-frame-received` note as a deliberate simplification — the spec's intent was clean separation, which `callId` filtering already provides. Document the deviation in the PR description.)

> If the reviewer/spec-compliance pass insists on the separate event: add a `VoiceOutbound::GroupDm`/`JoinGroupCall` media pair mirroring the DM arms and emit `group-voice-frame-received`. Default to reuse unless flagged.

- [ ] **Step 6: Gate**

Run:
```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: clean compile + lib tests green. (End-to-end behavior is proven in Task 9.)

- [ ] **Step 7: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/event_loop.rs
git commit -m "feat(zeb-360): event-loop group invite routing + presence task arms

Generalize inbound handler on space_id; spawn/stop group presence pub/sub;
media reuses the DM path (callId-filtered).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `lib.rs` signaling IPCs — `resolve_group_call_members` + `place_group_call` + `decline_group_call`

The N-party signaling fan-out. Generalizes `resolve_dm_call_peer` to return **all** members' device pubs and `send_voice_signal` to fan to all of them with `space_id` set.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add `resolve_group_call_members`**

Model on `resolve_dm_call_peer` (`lib.rs:11897-12002`). Differences: require `kind == GroupDm` (or at least `members.len() >= 3`), require `caller ∈ members`, and return **every other member's** X25519 pubs (a `Vec<(OwnerAddr, Vec<[u8;32]>)>`), plus the dm content key, signing key, self owner.

```rust
/// ZEB-360: resolve everything needed to fan a group-DM voice signal from a
/// `space_id_hex`. Returns (other_members_with_x25519_pubs, dm_key, signing_key,
/// self_owner). Each member entry is one OwnerAddr + its X25519 pub per enrolled
/// device (signal delivery is owner-scoped, so we fan one sealed envelope per
/// device, mirroring the 1:1 path's per-device fan-out).
async fn resolve_group_call_members(
    state: &std::sync::Mutex<NodeState>,
    space_id_hex: &str,
) -> Result<
    (
        Vec<(crate::owner_state_types::OwnerAddr, Vec<[u8; 32]>)>,
        crate::owner_state_types::DmContentKey,
        std::sync::Arc<ed25519_dalek::SigningKey>,
        crate::owner_state_types::OwnerAddr,
    ),
    String,
> {
    let space_bytes = hex::decode(space_id_hex).map_err(|e| format!("space_id hex: {e}"))?;
    let space_arr: [u8; 16] = space_bytes.as_slice().try_into()
        .map_err(|_| format!("space_id must be 16 bytes, got {}", space_bytes.len()))?;
    let space_id = crate::owner_state_types::SpaceId(space_arr);

    let (crdt_state, dm_outbox, self_owner) = {
        let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        let crdt = g.crdt_state.clone().ok_or(OWNER_NOT_LOADED_MSG)?;
        let outbox = g.dm_outbox.clone().ok_or(OWNER_NOT_LOADED_MSG)?;
        let owner = g.dm_self_owner.ok_or("dm_self_owner missing")?;
        (crdt, outbox, owner)
    };

    let (members, dm_key) = {
        let os = crdt_state.lock().await;
        let space = os.spaces.get(&space_id).ok_or_else(|| "group dm space not found".to_string())?;
        if space.kind != crate::owner_state_types::SpaceKind::GroupDm {
            return Err("not a group-dm space".to_string());
        }
        let dm_key = space.content_key.clone().ok_or_else(|| "group dm has no content key".to_string())?;
        if !space.members.contains(&self_owner) {
            return Err("self is not a member of this group dm".to_string());
        }
        let mut out: Vec<(crate::owner_state_types::OwnerAddr, Vec<[u8; 32]>)> = Vec::new();
        for m in space.members.iter().filter(|m| **m != self_owner) {
            let mut pubs: Vec<[u8; 32]> = os.owner_device_cache.devices.get(m)
                .map(|entry| entry.device_identity_pubs.iter().flatten()
                    .map(|ip| { let mut x = [0u8; 32]; x.copy_from_slice(&ip[..32]); x }).collect())
                .unwrap_or_default();
            pubs.sort_unstable();
            pubs.dedup();
            if !pubs.is_empty() {
                out.push((*m, pubs));
            }
            // Members with no cached device keys are simply unreachable this
            // attempt — the banner + their own watch will still surface the call.
        }
        (out, dm_key)
    };

    let signing_key = dm_outbox.lock().await.community_signing_key.clone();
    Ok((members, dm_key, signing_key, self_owner))
}
```

- [ ] **Step 2: Add `send_group_voice_signal`**

Model on `send_voice_signal` (`lib.rs:12004-12050`) but fan to all members with `space_id` set. For `place_group_call` it sends `Invite` to all other members; for `decline_group_call` it sends `Decline` to a single target (the caller) — so parameterize the recipient set.

```rust
/// ZEB-360: build, sign, seal, and fan a group `VoiceSignal` to a set of
/// member owners (one sealed envelope per enrolled device each). `space_id` is
/// always set so the inbound handler takes the group path.
async fn send_group_voice_signal(
    state: &std::sync::Mutex<NodeState>,
    space_id_hex: &str,
    kind: crate::voice_signal::VoiceSignalKind,
    call_id: [u8; 16],
    reason: Option<crate::voice_signal::DeclineReason>,
    recipients: &[(crate::owner_state_types::OwnerAddr, Vec<[u8; 32]>)],
    signing_key: &std::sync::Arc<ed25519_dalek::SigningKey>,
    self_owner: crate::owner_state_types::OwnerAddr,
) -> Result<(), String> {
    let space_bytes = hex::decode(space_id_hex).map_err(|e| format!("space_id hex: {e}"))?;
    let space_arr: [u8; 16] = space_bytes.as_slice().try_into().map_err(|_| "space_id must be 16 bytes".to_string())?;
    let signal = crate::voice_signal::VoiceSignal {
        kind,
        call_id,
        caller: self_owner,
        space_id: Some(crate::owner_state_types::SpaceId(space_arr)),
        decline_reason: reason,
    };
    let voice_signal_tx = {
        let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.voice_signal_tx.clone().ok_or_else(|| "voice signal not running".to_string())?
    };
    for (owner, pubs) in recipients {
        let callee_owner_hex = hex::encode(owner.0);
        for p in pubs {
            let sealed = crate::voice_signal::build_sealed_signal(&signal, signing_key, p)
                .map_err(|e| format!("seal signal: {e}"))?;
            voice_signal_tx.send(crate::voice_signal::VoiceSignalRequest {
                callee_owner_hex: callee_owner_hex.clone(),
                sealed,
            }).await.map_err(|_| "event loop not running".to_string())?;
        }
    }
    Ok(())
}
```

- [ ] **Step 3: Add `place_group_call` + `decline_group_call` IPCs**

```rust
/// ZEB-360: place a group-DM voice call. Mints a 16-byte call_id, fans a sealed
/// Invite (with space_id) to every other member, and returns the call_id hex.
/// The frontend then immediately joins media + presence (drop-in, D1).
#[tauri::command]
async fn place_group_call(
    space_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String> {
    use rand::RngCore;
    let (members, _dm_key, signing_key, self_owner) = resolve_group_call_members(&state, &space_id).await?;
    let mut call_id = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut call_id);
    send_group_voice_signal(
        &state, &space_id, crate::voice_signal::VoiceSignalKind::Invite, call_id, None,
        &members, &signing_key, self_owner,
    ).await?;
    Ok(hex::encode(call_id))
}

/// ZEB-360: decline an incoming group-DM call — seals a Decline back to the
/// caller only (parity; lets the caller's roster show "declined" before the 30s
/// ring timeout). The caller's owner is provided by the frontend (from the
/// incoming-group-call event's callerOwner).
#[tauri::command]
async fn decline_group_call(
    call_id: String,
    space_id: String,
    caller_owner: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let call = parse_call_id(&call_id)?;
    // Resolve the caller's device pubs so we can seal the Decline to them.
    let caller_bytes = hex::decode(&caller_owner).map_err(|_| "caller_owner not hex".to_string())?;
    let caller_arr: [u8; 16] = caller_bytes.as_slice().try_into().map_err(|_| "caller_owner must be 16 bytes".to_string())?;
    let caller = crate::owner_state_types::OwnerAddr(caller_arr);
    let (members, _dm_key, signing_key, self_owner) = resolve_group_call_members(&state, &space_id).await?;
    // `members` is everyone-but-self; the caller is one of them. Find their pubs.
    let Some(target) = members.into_iter().find(|(o, _)| *o == caller) else {
        return Err("caller not a member of this group dm".to_string());
    };
    send_group_voice_signal(
        &state, &space_id, crate::voice_signal::VoiceSignalKind::Decline, call,
        Some(crate::voice_signal::DeclineReason::User), &[target], &signing_key, self_owner,
    ).await
}
```

- [ ] **Step 4: Build-check**

Run: `cd src-tauri && cargo check --locked -p harmony-app --features test-fixtures`
Expected: compiles (the new commands are registered in Task 8). `parse_call_id` already exists (`lib.rs:11893`).

- [ ] **Step 5: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/lib.rs
git commit -m "feat(zeb-360): place_group_call + decline_group_call + N-member fan-out

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: `lib.rs` media+presence IPCs + handler registration

The remaining six IPCs (media reuses the DM path; presence routes to the Task 6 arms) and registration in both `generate_handler!` lists.

**Files:**
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: Add `watch_group_call` / `unwatch_group_call`**

```rust
/// ZEB-360: start a READ-ONLY group presence subscription (banner). Derives the
/// group presence key and tells the event loop to watch the space topic.
#[tauri::command]
async fn watch_group_call(
    space_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let (_members, dm_key, _sk, _self) = resolve_group_call_members(&state, &space_id).await?;
    let presence_key = std::sync::Arc::new(crate::community_channel_log::derive_groupdm_presence_key(&dm_key));
    let space_arr = parse_space_id_16(&space_id)?;
    let tx = voice_channel_tx(&state)?;
    tx.send(voice::VoiceChannelRequest::WatchGroupCall { space_id: space_arr, presence_key })
        .await.map_err(|_| "event loop not running".to_string())
}

/// ZEB-360: stop the read-only banner subscription for a space.
#[tauri::command]
async fn unwatch_group_call(
    space_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let space_arr = parse_space_id_16(&space_id)?;
    let tx = voice_channel_tx(&state)?;
    tx.send(voice::VoiceChannelRequest::UnwatchGroupCall { space_id: space_arr })
        .await.map_err(|_| "event loop not running".to_string())
}
```

Add the two small helpers if absent (mirror `parse_call_id` + the `voice_channel_tx` snapshot used by `join_dm_call`):

```rust
fn parse_space_id_16(s: &str) -> Result<[u8; 16], String> { parse_voice_id_16("spaceId", s) }

fn voice_channel_tx(state: &std::sync::Mutex<NodeState>) -> Result<tokio::sync::mpsc::Sender<voice::VoiceChannelRequest>, String> {
    let g = state.lock().map_err(|e| format!("lock: {e}"))?;
    g.voice_channel_tx.clone().ok_or_else(|| "not connected".to_string())
}
```

> Confirm the channel's element type matches `voice_channel_tx`'s real type in `NodeState`; adjust the return type accordingly.

- [ ] **Step 2: Add `join_group_call` / `leave_group_call`**

`join_group_call` reuses the DM media path (`JoinDmCall`) AND starts the presence publisher. It needs the same caps `join_dm_call` builds (k_voice from `derive_dm_voice_key`, joined_hlc, self_device, signing_key, self_owner) plus the presence key. Mirror `join_dm_call` (`lib.rs:12157-12217`) for the media half, then send `StartGroupPresence`:

```rust
/// ZEB-360: join (or drop into) a group-DM call. Reuses the DM media path
/// (JoinDmCall) and additionally starts the group presence publisher.
#[tauri::command]
async fn join_group_call(
    call_id: String,
    space_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let call = parse_call_id(&call_id)?;
    let (_members, dm_key, signing_key, self_owner) = resolve_group_call_members(&state, &space_id).await?;
    let space_arr = parse_space_id_16(&space_id)?;

    // Snapshot device + hlc the same way join_dm_call does.
    let (voice_channel_tx, device_hex, self_device, hlc_tracker) = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        let tx = guard.voice_channel_tx.clone().ok_or_else(|| "not connected".to_string())?;
        let device_hex = guard.dm_device_id.clone().ok_or_else(|| "no device id".to_string())?;
        let self_device = <[u8; 32]>::try_from(hex::decode(&device_hex).map_err(|_| "device id not hex".to_string())?.as_slice())
            .map_err(|_| "device id must be 32 bytes".to_string())?;
        let hlc_tracker = guard.hlc_tracker.clone().ok_or_else(|| "no hlc tracker".to_string())?;
        (tx, device_hex, self_device, hlc_tracker)
    };

    let k_voice = crate::community_channel_log::derive_dm_voice_key(&dm_key, &call);
    let presence_key = std::sync::Arc::new(crate::community_channel_log::derive_groupdm_presence_key(&dm_key));
    let wall_now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let joined_hlc = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_hex, wall_now_ms).await;

    // Media: identical to a DM call (same topic/key/AAD), addressed by call_id.
    voice_channel_tx.send(voice::VoiceChannelRequest::JoinDmCall {
        call_id: call,
        caps: voice::VoiceJoinCaps {
            channel_key: std::sync::Arc::new(k_voice),
            signing_key: signing_key.clone(),
            self_owner,
            self_device,
            joined_hlc: joined_hlc.clone(),
        },
    }).await.map_err(|_| "event loop not running".to_string())?;

    // Presence: publish our beacon + ensure the read subscriber is running.
    voice_channel_tx.send(voice::VoiceChannelRequest::StartGroupPresence {
        space_id: space_arr,
        call_id: call,
        presence_key,
        caps: voice::VoiceGroupPresenceCaps { signing_key, self_owner, self_device, joined_hlc },
    }).await.map_err(|_| "event loop not running".to_string())
}

/// ZEB-360: leave a group-DM call — tombstone our presence beacon + tear down
/// media. The read subscription persists if the DM view is still watching.
#[tauri::command]
async fn leave_group_call(
    call_id: String,
    space_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let call = parse_call_id(&call_id)?;
    let space_arr = parse_space_id_16(&space_id)?;
    let tx = voice_channel_tx(&state)?;
    tx.send(voice::VoiceChannelRequest::StopGroupPresence { space_id: space_arr, call_id: call })
        .await.map_err(|_| "event loop not running".to_string())?;
    tx.send(voice::VoiceChannelRequest::LeaveDmCall { call_id: call })
        .await.map_err(|_| "event loop not running".to_string())
}
```

- [ ] **Step 3: Add `send_group_voice_frame` / `set_group_call_muted`**

```rust
/// ZEB-360: send a group-call media frame — reuses the DM media outbound
/// (same per-call topic + key). Filtered to this call by callId on receipt.
#[tauri::command]
async fn send_group_voice_frame(
    payload: voice::SendGroupVoiceFramePayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let call = parse_call_id(&payload.call_id)?;
    let voice_tx = {
        let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
        guard.voice_tx.clone().ok_or_else(|| "not connected".to_string())?
    };
    voice_tx.send(voice::VoiceOutbound::Dm { call_id: call, frame: payload.frame_bytes })
        .await.map_err(|_| "event loop not running".to_string())
}

/// ZEB-360: flip the mute bit for an active group call (media + presence).
#[tauri::command]
async fn set_group_call_muted(
    payload: voice::SetGroupCallMutedPayload,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    let call = parse_call_id(&payload.call_id)?;
    let space_arr = parse_space_id_16(&payload.space_id)?;
    let tx = voice_channel_tx(&state)?;
    // Media mute (reuses the DM mute flag keyed by call_id).
    tx.send(voice::VoiceChannelRequest::SetDmCallMuted { call_id: call, muted: payload.muted })
        .await.map_err(|_| "event loop not running".to_string())?;
    // Presence mute (next beacon reflects it).
    tx.send(voice::VoiceChannelRequest::SetGroupCallMuted { space_id: space_arr, call_id: call, muted: payload.muted })
        .await.map_err(|_| "event loop not running".to_string())
}
```

> If Task 6 Step 4 chose to have the `SetGroupCallMuted` arm flip BOTH flags, drop the `SetDmCallMuted` send here. Keep media + presence consistent with whatever Task 6 implemented.

- [ ] **Step 4: Register all eight commands in both `generate_handler!` lists**

In the production list (`lib.rs:~32928`) and the `add_dm_ipc_handlers` test-fixtures list (`lib.rs:~33133`), add after the existing `// ZEB-352 Voice V4` block:

```rust
            // ZEB-360 Group-DM voice calls.
            place_group_call,
            decline_group_call,
            watch_group_call,
            unwatch_group_call,
            join_group_call,
            leave_group_call,
            send_group_voice_frame,
            set_group_call_muted,
```

- [ ] **Step 5: Gate**

Run:
```bash
cd src-tauri && cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```
Expected: clean. (`unused` warnings on the new commands should be gone now that they're registered.)

- [ ] **Step 6: Commit**

```bash
cd src-tauri && cargo fmt --all
git add src/lib.rs
git commit -m "feat(zeb-360): group call media+presence IPCs + handler registration

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: 3-engine group-DM voice integration test

End-to-end proof: three engines in a group DM; place fans invites; two join; presence converges to 3; media seals/relays across all three under the shared `K_voice`; a leave tombstones; last-leave clears the roster; negatives (wrong-callId key, non-member rejected).

**Files:**
- Create: `src-tauri/tests/group_dm_voice_three_engine_integration.rs`

- [ ] **Step 1: Write the test**

Model on `tests/voice_presence_two_engine_integration.rs` (reconnaissance §3) and `tests/voice_dm_two_engine_integration.rs`. Build three Zenoh sessions on loopback; three minted identities (`mint_test_owner`); a `GroupDm` `OwnerState`/`Space` with all three as members + a shared `content_key` (use the `make_dm_space` shape from `tests/dm_unicast_integration.rs:122-163`, but `SpaceKind::GroupDm` + 3 sorted members). Derive `K_voice = derive_dm_voice_key(content_key, call_id)` and `presence_key = derive_groupdm_presence_key(content_key)`. Spawn the group presence subscriber on engines B + C (each with its own `OwnerState` seeded with all three members enrolled, so `groupdm_beacon_signer_is_member` resolves) and the group publisher on A, B, C. Assert:

1. **Roster converges to 3** on B's map for `(space, call_id)` (wait_until, 10 s).
2. **Media relay:** A seals a frame with `encrypt_dm_voice_packet(&k_voice, &call_id, VOICE_DM_PACKET_AAD, frame)` on `harmony/voice/dm/{call}/{deviceA}`; C subscribed to `harmony/voice/dm/{call}/*` opens it with the same key. (Reuse the DM two-engine relay assertion verbatim, third engine.)
3. **Leave tombstone:** `publish_groupdm_leave_tombstone` from C → B's roster drops to 2 (wait_until).
4. **Last-leave clears:** A and B tombstone → B's `roster(space, call_id)` is empty.
5. **Negative — wrong call_id key:** a frame sealed under `derive_dm_voice_key(content_key, call_id_other)` fails to open under `k_voice` (`assert_eq!(.., Err(VoiceCryptoError::OpenFailed))`).
6. **Negative — non-member beacon dropped:** a rogue identity NOT in `members` signs + seals a valid beacon under `presence_key`; B's subscriber must drop it (roster unchanged). (Mirror the community test's rogue-identity assertion.)

Wrap the whole thing in `tokio::time::timeout(Duration::from_secs(30), run_inner())` with `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]` and an injectable monotonic clock (Arc<AtomicU64>) for the eviction/TTL phase, exactly like the community two-engine test.

- [ ] **Step 2: Run it**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test group_dm_voice_three_engine_integration`
Expected: PASS. If a loopback timing flake appears (presence convergence), bump the `wait_until` budget and the post-declare settle sleep to match the community test (1 s settle, 10 s converge); these loopback tests are inherently timing-sensitive. Do NOT reduce the assertions to make it pass.

- [ ] **Step 3: Commit**

```bash
cd src-tauri && cargo fmt --all
git add tests/group_dm_voice_three_engine_integration.rs
git commit -m "test(zeb-360): 3-engine group-DM voice integration (place/join/relay/presence/leave)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: `GroupCallSession` frontend controller

The new controller. Fuses ring-all signaling (from `CallSession`) with a VoiceSession-style roster + N-stream mix. Media core (connect/teardown/gate/transport/setMuted/setPttMode/setDeafened/destroy) is **ported** from `src/lib/call-session.ts` with the IPC names swapped to the `*_group_call` variants; the roster merge is new.

**Files:**
- Create: `src/lib/group-call-session.ts`

- [ ] **Step 1: Write the file**

Port `call-session.ts` wholesale, then layer the group differences. The complete novel surface:

```typescript
// src/lib/group-call-session.ts
//
// GroupCallSession — controller for group-DM voice calls (3–16 participants).
//
// Generalizes the 1:1 CallSession to N parties: ring-all signaling + a
// presence-driven participant roster (VoiceSession-style) + the V3 N-stream
// media engine, addressed by a `callId`. The media core (connect/teardown/
// talk-gate/transport/mute/PTT/deafen) is PORTED from CallSession; the roster
// merge (presence beacons ∪ full membership) is new. Drop-in + ring (D1): the
// caller joins media immediately and rings all others; the last participant to
// leave ends the call (emergent).

import { writable, get, type Readable } from 'svelte/store';
import { VoiceActivityDetector } from './voice/vad';
import { makeTalkGate } from './voice/talk-gate';
import { VoiceSender } from './voice/voice-sender';
import { VoiceReceiver } from './voice/voice-receiver';
import { VoiceMixer } from './voice/voice-mixer';
import { AudioCapture } from './voice/audio-capture';
import { OpusCodec } from './voice/opus-codec';
import { Codec2Codec } from './voice/codec2-codec';
import type { CodecType } from './voice/voice-codec';

export type GroupCallPhase = 'idle' | 'incoming' | 'connecting' | 'active' | 'leaving';

export interface Participant {
  ownerHex: string;
  deviceHex: string;
  muted: boolean;
  speaking: boolean;
  displayName?: string;
  avatarUrl?: string;
  state: 'in-call' | 'ringing' | 'declined';
}

export interface GroupCallSessionState {
  phase: GroupCallPhase;
  callId: string | null;
  spaceId: string | null;
  participants: Participant[];
  muted: boolean;
  pttMode: boolean;
  pttHeld: boolean;
  deafened: boolean;
  startedAt: number | null;
  reconnecting: boolean;
  /** Owner hex of the caller while phase==='incoming' (for the ring toast). */
  callerOwnerHex: string | null;
}

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
type Listen = (ev: string, h: (e: { payload: unknown }) => void) => Promise<() => void>;
type FrameGate = (pcm: Float32Array) => { send: boolean; ptt: boolean };

function bytesToHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

const RING_TIMEOUT_MS = 30_000;

export interface GroupCallSessionDeps {
  invoke: Invoke;
  listen: Listen;
  selfOwnerHex: string;
  selfDeviceHex: string;
  senderHash: Uint8Array;
  vadThreshold?: number;
  makeSender?: (gate: FrameGate) => Pick<VoiceSender, 'start' | 'stop'>;
  makeReceiver?: () => Pick<VoiceReceiver, 'init' | 'destroy' | 'getActiveSenders' | 'isSpeaking'>;
  makeMixer?: () => Pick<VoiceMixer, 'init' | 'pushFrame' | 'drain' | 'setDeafened' | 'destroy'>;
  resolveCard?: (ownerHex: string) => { displayName?: string; avatarUrl?: string } | undefined;
  /** Resolve a group DM space → its full member owner-hex list (for the roster
   *  "ringing"/"declined" rows that have no live beacon yet). */
  resolveMembers?: (spaceId: string) => string[];
  onRosterOwners?: (ownerHexes: string[]) => void;
}

const INITIAL: GroupCallSessionState = {
  phase: 'idle', callId: null, spaceId: null, participants: [],
  muted: true, pttMode: false, pttHeld: false, deafened: false,
  startedAt: null, reconnecting: false, callerOwnerHex: null,
};
```

Then the class. **Port these members verbatim from `call-session.ts`, swapping IPC names** (`place_call`→n/a, `join_dm_call`→`join_group_call`, `leave_dm_call`→`leave_group_call`, `send_dm_voice_frame`→`send_group_voice_frame`, `set_dm_call_muted`→`set_group_call_muted`) and the frame event filter target (still `dm-voice-frame-received`, filtered by `this.callId` — per Task 6 Step 5 the media path is reused): `coreGate`/`gate`, `connect(spaceId)`, `subscribeTransport()`, `teardownMedia(callId)`, `setMuted`, `setPttMode`, `setPttHeld`, `setDeafened`, `destroy`, `patch`, `clearRingTimer`, the VAD/sender/receiver/mixer fields, the `drainTimer`/`ringTimer`/`unlisteners` fields.

The **group-specific** methods (write these new):

```typescript
export class GroupCallSession {
  readonly state: Readable<GroupCallSessionState>;
  private store = writable<GroupCallSessionState>({ ...INITIAL });
  // … (ported fields: deps, vad, coreGate, sender, receiver, mixer, muted,
  //    deafened, pttMode, pttHeld, callId, spaceId, drainTimer, ringTimer,
  //    unlisteners, plus:)
  /** Beacons from group-call-presence-changed, keyed nothing — replaced wholesale. */
  private liveRoster: { ownerHex: string; deviceHex: string; muted: boolean }[] = [];
  private declinedOwners = new Set<string>();
  private lastSelfSpeaking = false;

  // constructor: identical to CallSession's (build vad + coreGate). The DM
  // call had no roster self-speaking side-effect; the group gate DOES need it
  // (mirror VoiceSession.gate): wrap coreGate to setSelfSpeaking.

  /** Caller (place): mint+fan happens in the backend; we immediately drop in. */
  async placeGroupCall(spaceId: string): Promise<string> {
    if (get(this.store).phase !== 'idle') throw new Error('A call is already in progress');
    const callId = (await this.deps.invoke('place_group_call', { spaceId })) as string;
    this.callId = callId; this.spaceId = spaceId;
    this.declinedOwners = new Set(); this.liveRoster = [];
    this.patch({ phase: 'connecting', callId, spaceId, callerOwnerHex: null, startedAt: null });
    await this.connect(spaceId); // drop straight into media (active, possibly alone)
    return callId;
  }

  /** Callee (rung): an incoming group invite arrived. */
  onIncomingGroup(callId: string, callerOwnerHex: string, spaceId: string): void {
    if (get(this.store).phase !== 'idle') return; // busy: silently ignore (D6)
    this.callId = callId; this.spaceId = spaceId;
    this.declinedOwners = new Set(); this.liveRoster = [];
    this.patch({ phase: 'incoming', callId, spaceId, callerOwnerHex, startedAt: null });
    this.clearRingTimer();
    this.ringTimer = setTimeout(() => { void this.decline(); }, RING_TIMEOUT_MS);
  }

  /** Callee accept → join media + presence (no Accept signal; presence announces us). */
  async accept(): Promise<void> {
    if (get(this.store).phase !== 'incoming') return;
    this.clearRingTimer();
    const spaceId = this.spaceId;
    if (!spaceId) throw new Error('missing space for incoming group call');
    this.patch({ phase: 'connecting' });
    await this.connect(spaceId);
  }

  /** Callee decline → Decline signal to caller (parity); stay idle. */
  async decline(): Promise<void> {
    if (get(this.store).phase !== 'incoming') return;
    this.clearRingTimer();
    const callId = this.callId, spaceId = this.spaceId, caller = get(this.store).callerOwnerHex;
    if (callId && spaceId && caller) {
      await this.deps.invoke('decline_group_call', { callId, spaceId, callerOwner: caller }).catch(() => {});
    }
    this.resetToIdle();
  }

  /** Join-in-progress via the banner → join media + presence, no ring. */
  async joinActive(callId: string, spaceId: string): Promise<void> {
    if (get(this.store).phase !== 'idle') throw new Error('A call is already in progress');
    this.callId = callId; this.spaceId = spaceId;
    this.declinedOwners = new Set(); this.liveRoster = [];
    this.patch({ phase: 'connecting', callId, spaceId, callerOwnerHex: null });
    await this.connect(spaceId);
  }

  /** Leave the call (tombstone + tear down media). Last-leave ends it for all. */
  async leave(): Promise<void> {
    const phase = get(this.store).phase;
    if (phase === 'idle' || phase === 'leaving') return;
    this.patch({ phase: 'leaving' });
    const callId = this.callId;
    await this.teardownMedia(callId); // teardownMedia invokes leave_group_call
    this.resetToIdle();
  }

  // Remote handlers:
  onPresenceChanged(callId: string, roster: { owner: string; device: string; muted: boolean }[]): void {
    if (this.callId !== callId) return;
    this.liveRoster = roster.map((r) => ({ ownerHex: r.owner, deviceHex: r.device, muted: r.muted }));
    this.deps.onRosterOwners?.(this.liveRoster.map((r) => r.ownerHex));
    this.refreshParticipants();
  }
  onDeclined(callId: string, ownerHex: string): void {
    if (this.callId !== callId) return;
    this.declinedOwners.add(ownerHex);
    this.refreshParticipants();
  }

  /** Merge live beacons (in-call) with full membership (ringing/declined). */
  private refreshParticipants(): void {
    const members = this.spaceId ? (this.deps.resolveMembers?.(this.spaceId) ?? []) : [];
    const live = new Map(this.liveRoster.map((r) => [r.ownerHex, r]));
    const out: Participant[] = [];
    for (const r of this.liveRoster) {
      const isSelf = r.deviceHex.slice(0, 32) === this.deps.selfDeviceHex.slice(0, 32);
      const speaking = isSelf
        ? (!this.muted && !this.deafened && this.lastSelfSpeaking)
        : (this.receiver?.isSpeaking(r.deviceHex.slice(0, 32)) ?? false);
      const card = this.deps.resolveCard?.(r.ownerHex);
      out.push({ ownerHex: r.ownerHex, deviceHex: r.deviceHex, muted: r.muted, speaking,
        state: 'in-call', ...(card?.displayName ? { displayName: card.displayName } : {}),
        ...(card?.avatarUrl ? { avatarUrl: card.avatarUrl } : {}) });
    }
    for (const ownerHex of members) {
      if (live.has(ownerHex) || ownerHex === this.deps.selfOwnerHex) continue;
      const card = this.deps.resolveCard?.(ownerHex);
      out.push({ ownerHex, deviceHex: '', muted: true, speaking: false,
        state: this.declinedOwners.has(ownerHex) ? 'declined' : 'ringing',
        ...(card?.displayName ? { displayName: card.displayName } : {}),
        ...(card?.avatarUrl ? { avatarUrl: card.avatarUrl } : {}) });
    }
    this.patch({ participants: out });
  }

  private setSelfSpeaking(v: boolean): void {
    if (v !== this.lastSelfSpeaking) { this.lastSelfSpeaking = v; this.refreshParticipants(); }
  }
}

let _groupCallSingleton: GroupCallSession | null = null;
let _groupCallIdentity: string | null = null;
export function getGroupCallSession(deps: GroupCallSessionDeps): GroupCallSession {
  const identity = `${deps.selfOwnerHex}:${deps.selfDeviceHex}`;
  if (!_groupCallSingleton || _groupCallIdentity !== identity) {
    _groupCallSingleton?.destroy();
    _groupCallSingleton = new GroupCallSession(deps);
    _groupCallIdentity = identity;
  }
  return _groupCallSingleton;
}
```

In `connect(spaceId)`, the receiver's `frameEvent` stays `'dm-voice-frame-received'` filtered by `this.callId` (media reuse), the sender's `publishFrame` calls `send_group_voice_frame`, and `join_group_call`/`leave_group_call` replace the DM verbs. After media is up, call `this.refreshParticipants()` once so the roster shows immediately. `resetToIdle()` mirrors CallSession's but resets `participants: []`, `declinedOwners`, `liveRoster`, `callerOwnerHex`.

- [ ] **Step 2: Type-check**

Run: `npx tsc --noEmit`
Expected: clean (no errors in `group-call-session.ts`).

- [ ] **Step 3: Commit**

```bash
git add src/lib/group-call-session.ts
git commit -m "feat(zeb-360): GroupCallSession controller (ring-all + roster + N-stream mix)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: `GroupCallSession` vitest

**Files:**
- Create: `src/lib/group-call-session.test.ts`

- [ ] **Step 1: Write the tests**

Model on `src/lib/call-session.test.ts` (mock-injection: `vi.fn()` invoke/listen, captured-listener emit, injected `makeSender`/`makeReceiver`/`makeMixer`). Cover:

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { GroupCallSession, type GroupCallSessionDeps } from './group-call-session';
// makeDeps(): invoke returns a fresh callId hex for place_group_call; listen
// captures handlers; makeSender/Receiver/Mixer are no-op stubs; resolveMembers
// returns ['self','alice','bob','carol']; senderHash = first 16 bytes of self.

describe('GroupCallSession', () => {
  // 1. placeGroupCall: drop-in — invokes place_group_call then join_group_call,
  //    phase goes idle→connecting→active (no ringingOut), startedAt set.
  // 2. onIncomingGroup → phase 'incoming' + callerOwnerHex set; ring timeout
  //    fires decline_group_call after 30s (fake timers).
  // 3. accept(): incoming→connecting→active, invokes join_group_call (NO accept
  //    signal IPC).
  // 4. decline(): invokes decline_group_call {callId, spaceId, callerOwner},
  //    returns to idle.
  // 5. joinActive(callId, spaceId): idle→connecting→active, invokes
  //    join_group_call, no ring.
  // 6. onPresenceChanged: roster merge — a beacon owner becomes 'in-call'; a
  //    non-beacon member is 'ringing'; self is excluded from the ringing rows.
  // 7. onDeclined(callId, owner): that owner flips 'ringing'→'declined'.
  // 8. mute/PTT/deafen rollback-on-reject: set_group_call_muted rejects →
  //    muted rolls back (port the CallSession rollback test).
  // 9. leave(): active→leaving→idle, invokes leave_group_call; participants
  //    cleared.
  // 10. busy: onIncomingGroup while active is a no-op (stays active).
  // 11. identity switch: getGroupCallSession with a new identity destroys the
  //     old instance (spy destroy) and returns a fresh one.
  // 12. transport: voice-transport-lost/restored (filtered by callId) toggles
  //     reconnecting.
});
```

Write each as a concrete test with assertions (no placeholders) — port the structure from `call-session.test.ts` for the shared verbs (mute/PTT/deafen/transport/identity), and write the roster-merge + drop-in + decline tests fresh.

- [ ] **Step 2: Run**

Run: `npx vitest run src/lib/group-call-session.test.ts`
Expected: all PASS.

- [ ] **Step 3: Commit**

```bash
git add src/lib/group-call-session.test.ts
git commit -m "test(zeb-360): GroupCallSession vitest (drop-in/roster-merge/decline/rollback)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: App.svelte wiring

Wire the three group listeners + the alerter + the `getGroupCallSession` singleton, alongside the existing 1:1 `incoming-call` wiring.

**Files:**
- Modify: `src/App.svelte`

- [ ] **Step 1: Construct the session + listeners**

Where the 1:1 `getCallSession` is wired and the `incoming-call`/`call-accepted`/`call-declined` listeners are registered, add the group equivalents. Build deps mirroring the 1:1 ones (same `senderHash`, `selfOwnerHex`, `selfDeviceHex`, `resolveCard`) plus `resolveMembers: (spaceId) => navService.groupDmMembers(spaceId)` (resolve the group DM space → its member owner-hex list; use whatever NavService/space accessor exists for GroupDm members — grep for how the group DM view lists its members). Register:

```typescript
const groupCall = getGroupCallSession(groupCallDeps);
unlisteners.push(await listen('incoming-group-call', (e) => {
  const p = e.payload as { callId: string; callerOwner: string; spaceId: string };
  groupCall.onIncomingGroup(p.callId, p.callerOwner, p.spaceId);
  const name = resolveCard?.(p.callerOwner)?.displayName ?? 'Someone';
  const groupName = navService.spaceName?.(p.spaceId) ?? 'a group';
  void incomingCallAlerter.notify({ id: p.callId, title: 'Incoming group call', body: `${name} is calling ${groupName}` });
}));
unlisteners.push(await listen('group-call-presence-changed', (e) => {
  const p = e.payload as { spaceId: string; callId: string; roster: { owner: string; device: string; muted: boolean }[] };
  groupCall.onPresenceChanged(p.callId, p.roster);
}));
unlisteners.push(await listen('group-call-declined', (e) => {
  const p = e.payload as { callId: string; spaceId: string; owner: string };
  groupCall.onDeclined(p.callId, p.owner);
}));
```

- [ ] **Step 2: Clear the alerter when the group call leaves `incoming`**

Mirror the existing 1:1 spot that calls `incomingCallAlerter.clear(callId)` when `incomingCall` clears: subscribe to `groupCall.state` and call `incomingCallAlerter.clear(callId)` when the group phase transitions out of `'incoming'` (accepted/declined/timed-out). Reuse the existing pattern verbatim.

- [ ] **Step 3: Type-check**

Run: `npx tsc --noEmit`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add src/App.svelte
git commit -m "feat(zeb-360): App.svelte group-call listeners + alerter wiring

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 13: Group-DM UI — Call/Join button, active-call banner, watch/unwatch, in-call bar

The visible surface. Reuses the existing ring toast + in-call bar + participant tiles; adds the header Call/Join button, the active-call banner, and the watch/unwatch lifecycle.

**Files:**
- Create: `src/lib/components/GroupCallBanner.svelte`
- Modify: the group-DM conversation view component (grep for where `SpaceKind` GroupDm conversations render — likely a `DmView.svelte`/`ConversationView.svelte`; find the component that renders a GroupDm thread header)
- Modify: the in-call bar component if it's 1:1-specific (reuse the N-party tiles from `VoiceChannelView` if shareable; otherwise render `groupCall.state.participants`)

- [ ] **Step 1: Watch/unwatch on mount/unmount**

In the group-DM view component, on mount call `invoke('watch_group_call', { spaceId })` and on unmount `invoke('unwatch_group_call', { spaceId })` (Svelte `onMount` returning a cleanup, or `onDestroy`). Guard to only run for `SpaceKind.GroupDm` spaces and only inside Tauri.

- [ ] **Step 2: GroupCallBanner.svelte**

A small component subscribed to `group-call-presence-changed` for the current `spaceId` (or to a store the view maintains from the watch subscription). When a roster is non-empty for the space and the local `groupCall` phase is `idle`, render: "📞 Call in progress — {N} in call" + avatars + a **Join** button → `groupCall.joinActive(callId, spaceId)`. Hidden when the local user is already in the call (phase !== 'idle' for this callId) or when no call is active.

> The banner needs the active `callId` + roster for the space even when the local user isn't in the call. The cleanest source is the watch subscription's `group-call-presence-changed` events. Maintain a `Map<spaceId, { callId, roster }>` store updated by the App-level `group-call-presence-changed` listener (Task 12), and have the banner read it. Add that store in Task 12 if not present; wire the banner to it here.

- [ ] **Step 3: Header Call/Join button**

In the group-DM header, add a button: when no active call for the space → **"Call"** → `groupCall.placeGroupCall(spaceId)`; when an active call exists (banner store has an entry) and the user isn't in it → **"Join call"** → `groupCall.joinActive(callId, spaceId)`; when the user is in the call → hide (the in-call bar takes over). Disable when `getVoiceSession`/`getCallSession` is busy (D6) — mirror however the 1:1 Call button checks busy state.

- [ ] **Step 4: In-call bar + tiles**

Render the in-call bar from `groupCall.state` when phase ∈ {connecting, active, leaving}: participant tiles for `participants` (in-call = normal, ringing/declined = greyed with the state label), mute/PTT/deafen controls wired to `groupCall.setMuted`/`setPttMode`/`setPttHeld`/`setDeafened`, a "Reconnecting…" indicator on `reconnecting`, and a Leave button → `groupCall.leave()`. Reuse the existing N-party tile component from `VoiceChannelView` if it accepts a `RosterMember`-like shape; otherwise a minimal tile list keyed by `ownerHex`+`deviceHex`. Reuse the existing incoming-call ring toast for phase `'incoming'` (accept → `groupCall.accept()`, decline → `groupCall.decline()`), body "{caller} is calling {group name}".

- [ ] **Step 5: Type-check + manual-ish vitest (optional)**

Run: `npx tsc --noEmit`
Expected: clean. (Svelte component logic is exercised by the controller's vitest; component rendering is covered by the manual smoke checklist in Task 14.)

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/GroupCallBanner.svelte src/App.svelte src/lib/  # + the modified view/in-call-bar files
git commit -m "feat(zeb-360): group-DM Call/Join button + active-call banner + in-call bar

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 14: Final gate sweep + smoke checklist + push + PR

**Files:**
- Modify: `docs/specs/2026-06-02-zeb-360-group-dm-voice-calls-design.md` is the source of the manual smoke checklist; no doc change needed unless a deviation (Task 6 Step 5 frame-event reuse) needs noting — add a short "Implementation notes / deviations" section to the PR body instead.

- [ ] **Step 1: Full Rust sweep (`--all-targets`)**

Run (each with a generous timeout; these relink all integration binaries):
```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean; clippy 0 warnings; nextest green except the 6 known iroh/zenoh loopback orphan-flakes (`reachability_publisher::force_notify_triggers_publish`, `zeb_321_connectivity_ipc_tests::force_republish_wakes_publisher`, `zenoh_iroh_link::paired_stream_roundtrip_via_loopback`, two `zenoh_iroh_transport` tests, `community_reachability_two_engine_integration`) — re-run those with `--failed` to confirm they're transient, not ZEB-360 breakage. The new `group_dm_voice_three_engine_integration` must pass (re-run if it loopback-flakes; it must pass on retry, not be quarantined).

- [ ] **Step 2: MSRV check**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures` (with the declared MSRV toolchain if available locally; otherwise note it runs in CI).
Expected: compiles.

- [ ] **Step 3: Frontend sweep**

Run:
```bash
npx tsc --noEmit
npx vitest run
```
Expected: tsc clean; all vitest pass (including `group-call-session.test.ts` and the unchanged `call-session.test.ts`/`voice-session.test.ts`/`incoming-call-alert.test.ts`).

- [ ] **Step 4: Push + open PR**

```bash
git push -u origin zeb-360-group-dm-voice-calls
```

Open the PR with `gh pr create`. Title: `ZEB-360: group-DM voice calls (3+ participants)`. Body must cover:
- Summary: extends V4 1:1 DM signaling to N-party + group-space presence roster; reuses DM media path + crypto verbatim.
- Spec link (`docs/specs/2026-06-02-zeb-360-group-dm-voice-calls-design.md`) + plan link.
- Parent epic **ZEB-348**; builds on ZEB-352/351/356/228 (mention as plain text, NOT "Closes", to avoid the Linear auto-close cascade closing the parent).
- What changed: optional `space_id` on `VoiceSignal` (1:1 fixture byte-identical), `derive_groupdm_presence_key`, group presence crypto + pub/sub, generalized invite handler, 8 new IPCs, `GroupCallSession` + UI, 3-engine integration test + 2 new wire fixtures.
- **Implementation deviation note:** group media frames reuse the `dm-voice-frame-received` event filtered by `callId` (a node is never in a 1:1 + group call simultaneously per D6, and callIds are globally unique), rather than a separate `group-voice-frame-received` event — a deliberate simplification preserving clean separation via callId filtering.
- Manual smoke checklist (the 7 steps from the spec §"Manual smoke checklist").
- Test plan checklist: fmt/clippy/nextest (`--all-targets --features test-fixtures`)/tsc/vitest/MSRV; the 6 known orphan-flakes are non-blocking.

End the PR body with:
```
🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

- [ ] **Step 5: Hand off to the autonomous bot-review loop**

After the PR is open, enter the autonomous bot-review loop (CodeRabbit / Cursor / CodeAnt / Qodo — **never** trigger Greptile). Scan all THREE comment buckets (inline review threads + PR issue-comments + reviews) each round, bundle fixes into ONE push per round, self-pace with ScheduleWakeup (~1200s), wait for CI green AND bots addressed, then pushover Jake at ready-to-merge. Do NOT self-merge (Jake's gate). Post-merge: verify the Linear cascade (ZEB-360→Done; parent ZEB-348 + siblings ZEB-354/355/357/359/362/363/364 stay open; reopen if over-closed).

---

## Self-review (run by the plan author after writing)

**Spec coverage check:**
- D1 drop-in+ring → T7 `place_group_call` (mint+fan) + T10 `placeGroupCall` (immediate connect) + 30s ring timeout in `onIncomingGroup`. ✓
- D2 banner/join-in-progress → T8 `watch_group_call`/`unwatch_group_call`, T5 read-only subscriber, T13 banner + `joinActive`. ✓
- D3 new `GroupCallSession` → T10/T11. ✓
- D4 separate IPCs → T7/T8 all `*_group_call`. ✓
- D5 cap=membership + member gate → T7 `resolve_group_call_members` (`caller ∈ members`, GroupDm kind), T6 invite-handler membership check, T5 `groupdm_beacon_signer_is_member`. ✓
- D6 one-session → T10 busy-block (phase guards) + T13 button disable. ✓
- D7 start muted → T6 `StartGroupPresence` muted=true + T10 connect starts muted (ported). ✓
- D8 no moderation → group publisher has no `self_kicked`; no moderate IPC. ✓
- Signaling state machine (idle→incoming→connecting→active→leaving) → T10 phases. ✓
- Presence topic group-scoped + derived key + beacon contents + concurrent-place lowest-callId → T5 (topic/key/wrapper), T10 (frontend reconciles lowest callId — **note:** the lowest-callId reconciliation is described in the spec as frontend; the plan's T10 doesn't explicitly implement multi-callId reconciliation. **Gap → addressed:** the banner store keyed by spaceId should keep only the lowest active callId when two appear; add this as a one-line tiebreak in T13 Step 2's banner store update — `if existing && existing.callId < callId: ignore`. Folded into T13.)
- Crypto/media reuse + optional space_id → T1/T2/T3. ✓
- Backend IPCs + events → T6/T7/T8. ✓
- Frontend controller + UI → T10–T13. ✓
- Testing (1:1 byte-identity, group fixtures, 3-engine, vitest) → T1/T3/T9/T11. ✓

**Type consistency check:** `Participant.state` ∈ {'in-call','ringing','declined'} consistent T10↔T13; event payloads `{callId, callerOwner, spaceId}` / `{spaceId, callId, roster}` / `{callId, spaceId, owner}` consistent T6↔T12; `VoiceGroupPresenceCaps` fields consistent T4↔T5↔T8; `GroupSignedPresenceBeacon` consistent T5↔T9; IPC names (`place_group_call`/`join_group_call`/`leave_group_call`/`watch_group_call`/`unwatch_group_call`/`send_group_voice_frame`/`set_group_call_muted`/`decline_group_call`) consistent T7/T8↔T10↔T12. ✓

**Placeholder scan:** the two `"PIN_ME"` fixture values are the repo's standard generate-then-pin procedure (the value is produced by the code and locked in the same step), not design placeholders — each has an explicit "run, copy actual, replace, re-run" step. All code steps contain concrete code or precise port-anchors with file:line. ✓
