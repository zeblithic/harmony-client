# ZEB-214 Opt-in Per-DM Read Receipts — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in, per-1:1-DM read receipt — a signed, ephemeral (live-only) "I've read up to time T" watermark the peer renders as "Seen HH:MM".

**Architecture:** A new device-Ed25519-signed `DmPacket::ReadReceipt` (discriminant `0x06`) rides the existing live iroh tunnel (never the deposit rung), verified through the same admission chain as `CidNotify`. An owner-local `Space.read_receipt_pref` (synced across the owner's own devices, off by default) gates emission. The frontend supplies the read watermark (the newest rendered message's ms timestamp) via a `mark_dm_read` command; the peer's client emits a `dm-read-receipt` Tauri event that a small frontend service turns into a per-space watermark, rendered as a "Seen HH:MM" line under the newest acknowledged own message. A "peer became reachable" re-send reuses the inbound-DM ingest as the liveness signal.

**Tech Stack:** Rust (owner-state CRDT, `DmPacket` CBOR envelope, Ed25519 via `ed25519_dalek`, Tauri commands), Svelte 5 runes frontend, `@testing-library/svelte` + vitest, `cargo nextest`.

## Global Constraints

- Frontend CI gates (repo root): `npx tsc --noEmit` and `npx vitest run`.
- Rust CI gates (from `src-tauri/`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- Tauri IPC: Rust params are `snake_case`; JS callers pass `camelCase` (auto-converted). Error extraction on the JS side: `e instanceof Error ? e.message : String(e)`.
- Owner-state additive fields MUST preserve byte-compat: `Option` fields use `#[serde(rename = "xx", skip_serializing_if = "Option::is_none", default)]`; the rename code MUST be exactly 2 chars (the `Space` struct requires equal-length keys — see `owner_state_types.rs:1853-1857`).
- Any new wire type passing through `canonical_cbor_encode`/`_decode` MUST be registered as a `CanonicalPayload` (the `impl_canonical!` list in `owner_state_types.rs:1512` for owner-state types; the dm-envelope signed bodies are registered near their definitions in `dm_envelope.rs`).
- Never call deterministic-nonce crypto helpers outside `#[cfg(any(test, feature = "test-fixtures"))]`.
- Discriminant bytes in `dm_envelope.rs` are inline literals, not named constants — `0x06` follows the `0x02` (`CidNotify`) shared-layout pattern.
- Read receipts are **ephemeral**: they MUST NOT create an `OutboxEntry` and MUST NOT be deposited.

## File Structure

**New files:**
- `src-tauri/src/dm_read_receipt.rs` — read-receipt module: the `dm-read-receipt` event constant + payload builder, watermark→packet build, the pref gate, and the "prepare what to send" pure core. One responsibility: turning a read event into a signed receipt to push (and deciding whether to).
- `src/lib/read-receipt-service.ts` — frontend listener + per-space watermark store.
- `src/lib/components/__tests__/read-receipt.test.ts` — frontend tests (service + TextFeed toggle + Seen indicator).

**Modified files:**
- `src-tauri/src/owner_state_types.rs` — `ReadReceiptPref` enum, `Space.read_receipt_pref` field, `impl_canonical!` registration.
- `src-tauri/src/owner_state_crdt.rs` — `lww_merge_space` carries the field; `OwnerState::set_read_receipt_pref` / `read_receipt_pref` helpers.
- `src-tauri/src/dm_envelope.rs` — `DmReadReceiptSigned` body, `DmPacket::ReadReceipt` variant (`0x06`), encode/decode arms, `build_signed_read_receipt`, `CanonicalPayload` registration.
- `src-tauri/src/dm_outbox.rs` — `verify_read_receipt_admission`.
- `src-tauri/src/dm_inbox_ingest.rs` — `0x06` ingest arm → emit event; `resolve_owner_for_peer` made `pub(crate)`.
- `src-tauri/src/lib.rs` — `set_space_read_receipt_pref`, `get_space_read_receipt_pref`, `mark_dm_read` commands; NodeState handles (tunnel manager, DM signing material, watermark map); reconnect re-send in the tunnel-ingest drain; `mod dm_read_receipt;`; handler registration.
- `src/App.svelte` — instantiate the service, thread the watermark to `TextFeed`, call `mark_dm_read` on DM open + focused inbound, fetch/toggle the pref.
- `src/lib/components/TextFeed.svelte` — read-receipt toggle in the 1:1 DM header; compute the newest acknowledged own-message; pass the seen prop.
- `src/lib/components/TextMessage.svelte` — "Seen HH:MM" line.
- `src/lib/types.ts` — `seenAt` prop plumbing (via component props, not the wire `Message`).

---

### Task 1: Owner-state `read_receipt_pref` field + merge

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs` (add enum ~after line 1494; add field ~after line 1877; register ~line 1524)
- Modify: `src-tauri/src/owner_state_crdt.rs` (add to `lww_merge_space` returned literal ~line 1295)
- Test: inline `#[cfg(test)]` in `owner_state_types.rs` and `owner_state_crdt.rs`

**Interfaces:**
- Produces: `pub enum ReadReceiptPref { Off, Broadcast }` (Copy, serde `"o"`/`"b"`); `Space.read_receipt_pref: Option<ReadReceiptPref>` (serde `"rr"`).

- [ ] **Step 1: Write the failing test** (append to the `owner_state_types.rs` test module)

```rust
#[test]
fn read_receipt_pref_roundtrips_and_omits_when_none() {
    use crate::owner_state_crypto::{canonical_cbor_encode, canonical_cbor_decode};
    // A DM space with the pref set round-trips.
    let mut space = crate::owner_state_crdt::test_support::dm_space_fixture();
    space.read_receipt_pref = Some(ReadReceiptPref::Broadcast);
    let bytes = canonical_cbor_encode(&space).unwrap();
    let back: Space = canonical_cbor_decode(&bytes).unwrap();
    assert_eq!(back.read_receipt_pref, Some(ReadReceiptPref::Broadcast));
    // None is omitted from the wire (additive back-compat): the "rr" key
    // MUST NOT appear in the encoding.
    space.read_receipt_pref = None;
    let bytes_none = canonical_cbor_encode(&space).unwrap();
    assert!(
        !bytes_none.windows(2).any(|w| w == b"rr"),
        "read_receipt_pref=None must not serialize the 'rr' key"
    );
    let back_none: Space = canonical_cbor_decode(&bytes_none).unwrap();
    assert_eq!(back_none.read_receipt_pref, None);
}
```

If `crate::owner_state_crdt::test_support::dm_space_fixture()` does not exist, instead build the `Space` inline by copying the construction used by an existing DM-space test in `owner_state_crdt.rs` (search `SpaceKind::Dm` in `#[cfg(test)]`); the assertions above are what matters.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(read_receipt_pref_roundtrips_and_omits_when_none)'`
Expected: FAIL to compile (`ReadReceiptPref` / `read_receipt_pref` undefined).

- [ ] **Step 3: Add the enum + registration + field**

In `owner_state_types.rs`, after the `NotificationPref` enum (line 1494):

```rust
/// Per-DM read-receipt preference (owner-local; NOT propagated to other
/// members — like `NotificationPref`). `None` ≡ `Off`. ZEB-214.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadReceiptPref {
    #[serde(rename = "o")]
    Off,
    #[serde(rename = "b")]
    Broadcast,
}
```

In the `impl_canonical!(...)` list, add `ReadReceiptPref,` immediately after `NotificationPref,` (line 1524).

In the `Space` struct, immediately after the `notification_pref` field (line 1877):

```rust
    /// ZEB-214: per-DM opt-in read-receipt broadcast preference. Owner-local
    /// (NOT propagated to other members, like `notification_pref`); syncs
    /// across the owner's own devices via owner-state Flow A. `None` ≡ `Off`.
    /// Additive `Option` keeps pre-ZEB-214 owner-state wire bytes identical.
    #[serde(rename = "rr", skip_serializing_if = "Option::is_none", default)]
    pub read_receipt_pref: Option<ReadReceiptPref>,
```

- [ ] **Step 4: Make `lww_merge_space` carry the field**

In `owner_state_crdt.rs`, in the `Space { ... }` literal returned by `lww_merge_space`, immediately after `notification_pref: newer.notification_pref,` (line 1295):

```rust
        // read_receipt_pref is LWW like the other owner-local per-Space prefs
        // (notification_pref, custom_name): newer updated_at wins, preserving
        // the cross-device opt-in.
        read_receipt_pref: newer.read_receipt_pref,
```

- [ ] **Step 5: Write the merge test** (append to `owner_state_crdt.rs` test module)

```rust
#[test]
fn lww_merge_space_carries_newer_read_receipt_pref() {
    let mut a = test_dm_space(/* older */);
    let mut b = a.clone();
    a.read_receipt_pref = None;
    b.read_receipt_pref = Some(crate::owner_state_types::ReadReceiptPref::Broadcast);
    b.updated_at = bump(&a.updated_at); // strictly newer
    let merged = lww_merge_space(&a, &b);
    assert_eq!(
        merged.read_receipt_pref,
        Some(crate::owner_state_types::ReadReceiptPref::Broadcast)
    );
}
```

Use whatever DM-space constructor and HLC-bump helper the neighboring `lww_merge_space` tests already use (search `lww_merge_space` in the test module); `test_dm_space` / `bump` are placeholders for those existing helpers.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(read_receipt_pref)'`
Expected: PASS (both tests).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/owner_state_types.rs src-tauri/src/owner_state_crdt.rs
git commit -m "feat(zeb-214): owner-local read_receipt_pref on Space (additive + LWW)"
```

---

### Task 2: `DmPacket::ReadReceipt` (0x06) wire frame

**Files:**
- Modify: `src-tauri/src/dm_envelope.rs` (struct near `DmCidNotifySigned`; variant in `DmPacket`; `encode_packet` shared arm; `decode_packet` `0x06` arm; `build_signed_read_receipt`; `CanonicalPayload` registration)
- Test: inline `#[cfg(test)]` in `dm_envelope.rs`

**Interfaces:**
- Consumes: `crate::dm_signing::sign_dm_packet` (Task exists), `SpaceId`/`OwnerAddr`/`Hlc`/`DeviceIdentityHash` (`owner_state_types`).
- Produces: `pub struct DmReadReceiptSigned { space_id: SpaceId, sender_owner_addr: OwnerAddr, signing_device_hash: DeviceIdentityHash, read_up_to: Hlc, sent_at: Hlc }`; `DmPacket::ReadReceipt { signed, signature: [u8;64], signed_bytes: Vec<u8> }`; `pub fn build_signed_read_receipt(signed, &SigningKey) -> Result<DmPacket, EncodeError>`.

- [ ] **Step 1: Write the failing round-trip + signature test**

```rust
#[test]
fn read_receipt_packet_roundtrips_and_signs() {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let signed = DmReadReceiptSigned {
        space_id: SpaceId([1u8; 16]),
        sender_owner_addr: OwnerAddr([2u8; 16]),
        signing_device_hash: DeviceIdentityHash([3u8; 16]),
        read_up_to: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: "dev".into() },
        sent_at: Hlc { wall_ms: 1_700_000_005_000, logical: 0, device_id: "dev".into() },
    };
    let packet = build_signed_read_receipt(signed.clone(), &sk).unwrap();
    let wire = encode_packet(&packet).unwrap();
    assert_eq!(wire[0], 0x06, "read-receipt discriminant");
    let decoded = decode_packet(&wire).unwrap();
    match decoded {
        DmPacket::ReadReceipt { signed: got, signature, signed_bytes } => {
            assert_eq!(got, signed);
            // Signature covers the canonical body bytes.
            let vk = sk.verifying_key();
            vk.verify_strict(&signed_bytes, &ed25519_dalek::Signature::from_bytes(&signature))
                .expect("signature must verify over signed_bytes");
        }
        other => panic!("expected ReadReceipt, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(read_receipt_packet_roundtrips_and_signs)'`
Expected: FAIL to compile.

- [ ] **Step 3: Add the signed body + registration**

Near `DmCidNotifySigned` (line 126) in `dm_envelope.rs`:

```rust
/// ZEB-214: an opt-in read-receipt watermark. `read_up_to` is the HLC of the
/// newest message the sender has read in `space_id`; the recipient marks its
/// own sent messages at or before it as "seen". Signed with the sender's
/// per-device Ed25519 key (same key as `CidNotify`) and verified via the same
/// admission chain. Ephemeral: pushed over the live tunnel only, never
/// deposited; carries no message body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmReadReceiptSigned {
    #[serde(rename = "si")]
    pub space_id: SpaceId,
    #[serde(rename = "so")]
    pub sender_owner_addr: OwnerAddr,
    #[serde(rename = "dh")]
    pub signing_device_hash: DeviceIdentityHash,
    #[serde(rename = "ru")]
    pub read_up_to: Hlc,
    #[serde(rename = "sa")]
    pub sent_at: Hlc,
}
```

Register it as a `CanonicalPayload` the same way `DmCidNotifySigned` is registered — find the `impl_canonical!`/`impl CanonicalPayload for DmCidNotifySigned` site in `dm_envelope.rs` and add `DmReadReceiptSigned` alongside it.

- [ ] **Step 4: Add the enum variant**

In `DmPacket` (after the `RevocationPush` variant, ~line 250):

```rust
    /// ZEB-214: an opt-in read-receipt watermark control frame. Uses the shared
    /// `[disc 0x06][CBOR body][64-byte sig]` layout, device-Ed25519-signed like
    /// `CidNotify`. Never a chat message — ingest emits `dm-read-receipt` only.
    ReadReceipt {
        signed: DmReadReceiptSigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
    },
```

- [ ] **Step 5: Add the encode arm**

In `encode_packet`, add a match arm alongside `Invite`/`CidNotify`/`Ack` in the shared-layout tuple match (after the `Ack` arm, ~line 415):

```rust
        DmPacket::ReadReceipt {
            signed,
            signature,
            signed_bytes,
        } => {
            let re_encoded = crate::owner_state_crypto::canonical_cbor_encode(signed)
                .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?;
            if re_encoded != *signed_bytes {
                return Err(EncodeError::SignedMutated(
                    "DmPacket ReadReceipt variant: signed mutated post-build (re-encode \
                     mismatches cached signed_bytes; signature would not cover wire body)"
                        .to_string(),
                ));
            }
            (0x06, signed_bytes, signature)
        }
```

- [ ] **Step 6: Add the decode arm**

In `decode_packet`, add an arm to the `match disc { ... }` after `0x03` (~line 640):

```rust
        0x06 => {
            let signed: DmReadReceiptSigned = decode_body(body_bytes)?;
            ensure_canonical_body(&signed, body_bytes)?;
            DmPacket::ReadReceipt {
                signed,
                signature,
                signed_bytes,
            }
        }
```

- [ ] **Step 7: Add the build helper**

After `build_signed_cidnotify` (~line 487):

```rust
/// Build + sign a complete read-receipt packet ready for `encode_packet`.
pub fn build_signed_read_receipt(
    signed: DmReadReceiptSigned,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<DmPacket, EncodeError> {
    let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed)
        .map_err(|e| EncodeError::Cbor(e.to_string()))?;
    let signature = crate::dm_signing::sign_dm_packet(&signed_bytes, signing_key);
    Ok(DmPacket::ReadReceipt {
        signed,
        signature,
        signed_bytes,
    })
}
```

- [ ] **Step 8: Run test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(read_receipt_packet_roundtrips_and_signs)'`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/dm_envelope.rs
git commit -m "feat(zeb-214): DmPacket::ReadReceipt (0x06) signed wire frame"
```

---

### Task 3: Receipt admission (verify)

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add `verify_read_receipt_admission` near `verify_cidnotify_admission`, line 3420)
- Test: inline `#[cfg(test)]` in `dm_outbox.rs`

**Interfaces:**
- Consumes: `lookup_pubkey_for_device`, `resolve_signed_origin_owner` (both `pub(crate)` in `dm_outbox`), `crate::dm_signing::verify_dm_packet_signature`, `DmReadReceiptSigned` (Task 2), `DmReceiveError`.
- Produces: `pub(crate) fn verify_read_receipt_admission(state: &OwnerState, signed: &DmReadReceiptSigned, signature: &[u8;64], signed_bytes: &[u8], revoked: &RevokedDeviceProjection) -> Result<OwnerAddr, DmReceiveError>` (returns the resolved sender owner).

- [ ] **Step 1: Write the failing admission tests**

Adapt the setup from the existing `verify_cidnotify_sender_binding` unit tests in this module (search `verify_cidnotify` in `#[cfg(test)]`) — they seed an `OwnerState.owner_device_cache` with a signing device and a `Dm` space. Build a receipt signed by that device:

```rust
#[test]
fn read_receipt_admission_accepts_valid_and_rejects_tampered() {
    let f = cidnotify_admission_fixture(); // existing helper: cache + Dm space + signing key
    let signed = DmReadReceiptSigned {
        space_id: f.space_id,
        sender_owner_addr: f.sender_owner,
        signing_device_hash: f.signing_device_hash,
        read_up_to: Hlc { wall_ms: 1000, logical: 0, device_id: "d".into() },
        sent_at: Hlc { wall_ms: 1500, logical: 0, device_id: "d".into() },
    };
    let bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
    let sig = crate::dm_signing::sign_dm_packet(&bytes, &f.signing_key);

    // Valid → returns the resolved owner.
    let owner = verify_read_receipt_admission(&f.state, &signed, &sig, &bytes, &f.revoked).unwrap();
    assert_eq!(owner, f.sender_owner);

    // Tampered signed_bytes → signature fails.
    let mut bad = bytes.clone();
    bad[0] ^= 0xFF;
    assert!(matches!(
        verify_read_receipt_admission(&f.state, &signed, &sig, &bad, &f.revoked),
        Err(DmReceiveError::SignatureVerificationFailed)
    ));

    // Owner-field mismatch.
    let mut wrong = signed.clone();
    wrong.sender_owner_addr = OwnerAddr([0x99; 16]);
    let wb = crate::owner_state_crypto::canonical_cbor_encode(&wrong).unwrap();
    let ws = crate::dm_signing::sign_dm_packet(&wb, &f.signing_key);
    assert!(matches!(
        verify_read_receipt_admission(&f.state, &wrong, &ws, &wb, &f.revoked),
        Err(DmReceiveError::OwnerFieldMismatch)
    ));
}
```

If the existing helper has a different name/shape, reuse whatever the `verify_cidnotify` tests use to obtain `(state, space_id, sender_owner, signing_device_hash, signing_key, revoked)`; only the three assertions above are load-bearing.

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(read_receipt_admission_accepts_valid_and_rejects_tampered)'`
Expected: FAIL to compile (`verify_read_receipt_admission` undefined).

- [ ] **Step 3: Implement the verifier**

After `verify_cidnotify_space` (~line 3501) in `dm_outbox.rs`:

```rust
/// ZEB-214: verify an inbound read-receipt frame against the CURRENT
/// OwnerDeviceCache — mirrors `verify_cidnotify_admission` (sender-binding +
/// space-binding) for `DmReadReceiptSigned`. Returns the resolved sender owner.
pub(crate) fn verify_read_receipt_admission(
    state: &OwnerState,
    signed: &crate::dm_envelope::DmReadReceiptSigned,
    signature: &[u8; 64],
    signed_bytes: &[u8],
    revoked: &crate::revoked_device_projection::RevokedDeviceProjection,
) -> Result<OwnerAddr, DmReceiveError> {
    let identity_pub =
        lookup_pubkey_for_device(&state.owner_device_cache, signed.signing_device_hash)
            .ok_or(DmReceiveError::UnknownSigningKey)?;
    crate::dm_signing::verify_dm_packet_signature(
        signed_bytes,
        signature,
        &identity_pub,
        signed.signing_device_hash,
    )?;
    let resolved_owner =
        resolve_signed_origin_owner(&state.owner_device_cache, signed.signing_device_hash)?;
    if signed.sender_owner_addr != resolved_owner {
        return Err(DmReceiveError::OwnerFieldMismatch);
    }
    let ed25519: [u8; 32] = identity_pub[32..64].try_into().expect("64 - 32 == 32");
    if revoked.is_revoked(&resolved_owner, &ed25519) {
        return Err(DmReceiveError::SignerDeviceRevoked);
    }
    let space = state
        .spaces
        .get(&signed.space_id)
        .ok_or(DmReceiveError::SpaceNotFound)?;
    if !matches!(space.kind, SpaceKind::Dm | SpaceKind::GroupDm) {
        return Err(DmReceiveError::SpaceKindMismatch);
    }
    if !space.members.contains(&resolved_owner) {
        return Err(DmReceiveError::SenderNotInSpaceMembers);
    }
    Ok(resolved_owner)
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(read_receipt_admission_accepts_valid_and_rejects_tampered)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "feat(zeb-214): verify_read_receipt_admission (CidNotify-parity admission)"
```

---

### Task 4: Ingest arm + `dm-read-receipt` event

**Files:**
- Create: `src-tauri/src/dm_read_receipt.rs` (event const + payload builder only, this task)
- Modify: `src-tauri/src/lib.rs` (add `mod dm_read_receipt;`)
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`0x06` arm in `ingest_dm_packet`)
- Test: inline `#[cfg(test)]` in `dm_inbox_ingest.rs`

**Interfaces:**
- Consumes: `verify_read_receipt_admission` (Task 3); `crate::node_event_sink::emit_ser`.
- Produces: `pub(crate) const DM_READ_RECEIPT_EVENT: &str = "dm-read-receipt";`; `pub(crate) fn dm_read_receipt_event_payload(space_id: SpaceId, from: OwnerAddr, read_up_to: &Hlc, at_ms: u64) -> serde_json::Value`.

- [ ] **Step 1: Create the module skeleton**

`src-tauri/src/dm_read_receipt.rs`:

```rust
//! ZEB-214 — opt-in per-DM read receipts (ephemeral, live-only). This module
//! owns the `dm-read-receipt` UI event shape and (in a later task) the emit
//! decision + packet build. Receipts never touch the outbox/deposit rung.

use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

/// The `dm-read-receipt` UI event name — a peer told us they've read our DM up
/// to a watermark. Emitted from the tunnel ingest path only (receipts are
/// live-only, so there is no deposit/sweeper path to duplicate).
pub(crate) const DM_READ_RECEIPT_EVENT: &str = "dm-read-receipt";

/// Shared payload builder — single source of truth for the event shape.
/// `readUpTo` and `at` are exposed as wall-ms (like `sentAt` on `dm-received`)
/// so the frontend compares them against `Message.timestamp` directly.
/// `readUpTo` = the watermark (which of the viewer's messages are seen);
/// `at` = the receipt's send time (the "Seen HH:MM" clock).
pub(crate) fn dm_read_receipt_event_payload(
    space_id: SpaceId,
    from: OwnerAddr,
    read_up_to: &Hlc,
    at_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "spaceId": hex::encode(space_id.0),
        "from": hex::encode(from.0),
        "readUpTo": read_up_to.wall_ms,
        "at": at_ms,
    })
}
```

Add `mod dm_read_receipt;` to `lib.rs` next to the other `mod dm_*;` declarations.

- [ ] **Step 2: Write the failing ingest test**

Adapt an existing `ingest_dm_packet` unit test (search `ingest_dm_packet(` in the `dm_inbox_ingest.rs` test module) for the harness (a recording `NodeEventSink`, a seeded `crdt_state`, `revoked`, `self_owner`, `device_id`). Build a valid receipt (as in Task 3) into wire bytes and assert the sink recorded a `dm-read-receipt`:

```rust
#[tokio::test]
async fn ingest_read_receipt_emits_event_and_no_inbox_write() {
    let h = ingest_harness_with_dm(); // existing helper: crdt_state + recording sink + revoked + peer
    let signed = /* DmReadReceiptSigned signed by h.peer_device, space = h.space_id */;
    let wire = crate::dm_envelope::encode_packet(
        &crate::dm_envelope::build_signed_read_receipt(signed.clone(), &h.peer_key).unwrap(),
    ).unwrap();

    let emitted = crate::dm_inbox_ingest::ingest_dm_packet(
        &h.crdt_state, &h.content_store, &h.sink, None, h.self_owner,
        &h.device_id, h.peer_node_id, &wire, &h.revoked, None,
    ).await.unwrap();

    assert!(!emitted, "a receipt is not a chat message (returns Ok(false))");
    let ev = h.sink.last_event();
    assert_eq!(ev.name, "dm-read-receipt");
    assert_eq!(ev.payload["readUpTo"], signed.read_up_to.wall_ms);
    assert!(h.crdt_state.lock().await.inbox.is_empty(), "receipt must not write the inbox");
}
```

Match the recording-sink accessor (`last_event`) to whatever the existing tests use.

- [ ] **Step 3: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(ingest_read_receipt_emits_event_and_no_inbox_write)'`
Expected: FAIL (the `0x06` arm returns `UnknownDiscriminant` or the decode arm from Task 2 exists but ingest has no handler → decode error path).

- [ ] **Step 4: Add the ingest arm**

In `ingest_dm_packet`, add an arm to the `match crate::dm_envelope::decode_packet(...)` alongside the other early-returning arms (`Invite`, `Ack`, `RevocationPush`), e.g. after the `RevocationPush` arm (~line 780):

```rust
        crate::dm_envelope::DmPacket::ReadReceipt {
            signed,
            signature,
            signed_bytes,
        } => {
            // ZEB-214: an opt-in read-receipt watermark. Verify against the
            // CURRENT cache (same admission chain as CidNotify), then emit
            // `dm-read-receipt`. A control frame: never a chat message, so
            // this returns Ok(false) without touching the inbox.
            let resolved = {
                let state = crdt_state.lock().await;
                crate::dm_outbox::verify_read_receipt_admission(
                    &state, &signed, &signature, &signed_bytes, revoked,
                )
            };
            match resolved {
                Ok(resolved_owner) => {
                    crate::node_event_sink::emit_ser(
                        sink.as_ref(),
                        crate::dm_read_receipt::DM_READ_RECEIPT_EVENT,
                        &crate::dm_read_receipt::dm_read_receipt_event_payload(
                            signed.space_id,
                            resolved_owner,
                            &signed.read_up_to,
                            signed.sent_at.wall_ms,
                        ),
                    );
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "ZEB-214: rejected read receipt; dropping");
                }
            }
            return Ok(false);
        }
```

- [ ] **Step 5: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(ingest_read_receipt)'`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/dm_read_receipt.rs src-tauri/src/dm_inbox_ingest.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-214): ingest read-receipt frame + dm-read-receipt event"
```

---

### Task 5: Emit decision + packet build (pure core)

**Files:**
- Modify: `src-tauri/src/dm_read_receipt.rs` (add the pref gate, packet build, and `prepare_read_receipt`)
- Test: inline `#[cfg(test)]` in `dm_read_receipt.rs`

**Interfaces:**
- Consumes: `Space`, `SpaceKind`, `ReadReceiptPref`, `DeviceIdentityHash`, `crate::dm_envelope::{DmReadReceiptSigned, build_signed_read_receipt, encode_packet}`, `crate::owner_state_crdt::OwnerState`.
- Produces:
  - `pub(crate) fn dm_peer_of(space: &Space, self_owner: OwnerAddr) -> Option<OwnerAddr>` (the single other member of a 1:1 `Dm`, else `None`).
  - `pub(crate) fn prepare_read_receipt(state: &OwnerState, self_owner: OwnerAddr, signing_device_hash: DeviceIdentityHash, signing_key: &ed25519_dalek::SigningKey, device_id: &str, space_id: SpaceId, up_to_ms: u64, now_ms: u64) -> Option<(OwnerAddr, Vec<u8>)>` — `Some((peer, wire_bytes))` iff the space is a 1:1 `Dm` with `read_receipt_pref == Broadcast`; else `None`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod prepare_tests {
    use super::*;
    use crate::owner_state_types::{ReadReceiptPref, SpaceKind};

    fn dm(pref: Option<ReadReceiptPref>, kind: SpaceKind) -> (crate::owner_state_crdt::OwnerState, SpaceId, OwnerAddr) {
        // Build an OwnerState with one space of `kind`, members [self, peer],
        // read_receipt_pref = pref. Reuse the DM-space test constructor from
        // owner_state_crdt tests.
        todo!("use existing dm-space fixture; set kind + read_receipt_pref")
    }

    #[test]
    fn prepares_only_for_1to1_dm_broadcast() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let (state, space_id, me) = dm(Some(ReadReceiptPref::Broadcast), SpaceKind::Dm);
        let out = prepare_read_receipt(&state, me, DeviceIdentityHash([1;16]), &sk, "dev", space_id, 1234, 5678);
        let (peer, wire) = out.expect("Broadcast 1:1 DM prepares a receipt");
        assert_ne!(peer, me);
        // The wire decodes to a read receipt carrying the watermark.
        match crate::dm_envelope::decode_packet(&wire).unwrap() {
            crate::dm_envelope::DmPacket::ReadReceipt { signed, .. } => {
                assert_eq!(signed.read_up_to.wall_ms, 1234);
                assert_eq!(signed.sent_at.wall_ms, 5678);
                assert_eq!(signed.space_id, space_id);
                assert_eq!(signed.sender_owner_addr, me);
            }
            _ => panic!("expected ReadReceipt"),
        }
    }

    #[test]
    fn no_receipt_when_off_group_or_channel() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        for (pref, kind) in [
            (Some(ReadReceiptPref::Off), SpaceKind::Dm),
            (None, SpaceKind::Dm),
            (Some(ReadReceiptPref::Broadcast), SpaceKind::GroupDm), // 1:1 only this cut
            (Some(ReadReceiptPref::Broadcast), SpaceKind::Channel),
        ] {
            let (state, space_id, me) = dm(pref, kind);
            assert!(
                prepare_read_receipt(&state, me, DeviceIdentityHash([1;16]), &sk, "dev", space_id, 1, 2).is_none(),
                "pref={pref:?} kind={kind:?} must not prepare a receipt"
            );
        }
    }
}
```

Replace the `todo!()` fixture with the existing DM-space constructor (search `SpaceKind::Dm` in `owner_state_crdt.rs` tests) — set its `kind`, `members = [me, peer]`, and `read_receipt_pref`.

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(prepare)'`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the core**

Append to `dm_read_receipt.rs`:

```rust
use crate::owner_state_types::{ReadReceiptPref, Space, SpaceKind};

/// The single other member of a 1:1 `Dm` space, or `None` for any other shape
/// (group DM, channel, or a degenerate member set). 1:1 only this cut.
pub(crate) fn dm_peer_of(space: &Space, self_owner: OwnerAddr) -> Option<OwnerAddr> {
    if space.kind != SpaceKind::Dm || space.members.len() != 2 {
        return None;
    }
    space.members.iter().copied().find(|m| *m != self_owner)
}

/// Decide whether to emit a read receipt for `space_id` and, if so, build the
/// signed wire packet. `Some((peer, wire))` iff the space is a 1:1 `Dm` with
/// `read_receipt_pref == Broadcast`. Pure: reads state, mints no HLC, touches
/// no outbox — the ephemerality invariant is structural (there is simply no
/// outbox write anywhere in this path).
pub(crate) fn prepare_read_receipt(
    state: &crate::owner_state_crdt::OwnerState,
    self_owner: OwnerAddr,
    signing_device_hash: crate::owner_state_types::DeviceIdentityHash,
    signing_key: &ed25519_dalek::SigningKey,
    device_id: &str,
    space_id: SpaceId,
    up_to_ms: u64,
    now_ms: u64,
) -> Option<(OwnerAddr, Vec<u8>)> {
    let space = state.spaces.get(&space_id)?;
    if space.read_receipt_pref != Some(ReadReceiptPref::Broadcast) {
        return None;
    }
    let peer = dm_peer_of(space, self_owner)?;
    let signed = crate::dm_envelope::DmReadReceiptSigned {
        space_id,
        sender_owner_addr: self_owner,
        signing_device_hash,
        read_up_to: Hlc { wall_ms: up_to_ms, logical: 0, device_id: device_id.to_string() },
        sent_at: Hlc { wall_ms: now_ms, logical: 0, device_id: device_id.to_string() },
    };
    let packet = crate::dm_envelope::build_signed_read_receipt(signed, signing_key).ok()?;
    let wire = crate::dm_envelope::encode_packet(&packet).ok()?;
    Some((peer, wire))
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(prepare)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_read_receipt.rs
git commit -m "feat(zeb-214): read-receipt emit gate + packet build (pure core)"
```

---

### Task 6: Commands + NodeState wiring + emit hook

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` (`set_read_receipt_pref` / `read_receipt_pref` helpers on `OwnerState`)
- Modify: `src-tauri/src/dm_read_receipt.rs` (`maybe_send_read_receipt` thin wrapper)
- Modify: `src-tauri/src/lib.rs` (NodeState fields; three commands; handler registration; populate handles at the transport-construction site)
- Test: inline `#[cfg(test)]` in `owner_state_crdt.rs` (the pure helper); command tests deferred to the whole-branch review (they need a NodeState harness — the pure helper carries the gating logic).

**Interfaces:**
- Consumes: `prepare_read_receipt` (Task 5); `crate::iroh_tunnel_dm_transport::send_packet_to_owner_tunnels`; `crate::tunnel_manager::TunnelManager`; `reserve_next_hlc_for_device`.
- Produces:
  - `OwnerState::set_read_receipt_pref(&mut self, space_id: SpaceId, pref: ReadReceiptPref, new_hlc: Hlc) -> Result<bool, String>` (Err if space missing or not `Dm`/`GroupDm`; `Ok(false)` = no-op, `Ok(true)` = changed).
  - `OwnerState::read_receipt_pref(&self, space_id: SpaceId) -> Option<ReadReceiptPref>`.
  - `pub(crate) async fn maybe_send_read_receipt(...)` in `dm_read_receipt.rs`.
  - Tauri commands `set_space_read_receipt_pref`, `get_space_read_receipt_pref`, `mark_dm_read`.
  - NodeState fields: `tunnel_manager: Option<Arc<TunnelManager>>`, `dm_tunnel_sign_key: Option<Arc<ed25519_dalek::SigningKey>>`, `dm_tunnel_sign_hash: Option<DeviceIdentityHash>`, `read_receipt_watermarks: Arc<tokio::sync::Mutex<std::collections::HashMap<SpaceId, u64>>>`.

- [ ] **Step 1: Write the failing helper tests**

Append to `owner_state_crdt.rs` tests:

```rust
#[test]
fn set_read_receipt_pref_gates_kind_and_is_idempotent() {
    use crate::owner_state_types::{ReadReceiptPref, SpaceKind};
    let mut st = OwnerState::default();
    let dm = test_dm_space();       // kind Dm
    let ch = test_channel_space();  // kind Channel
    let dm_id = dm.id; let ch_id = ch.id;
    st.spaces.insert(dm_id, dm);
    st.spaces.insert(ch_id, ch);

    let h1 = hlc(10);
    assert_eq!(st.set_read_receipt_pref(dm_id, ReadReceiptPref::Broadcast, h1.clone()).unwrap(), true);
    assert_eq!(st.read_receipt_pref(dm_id), Some(ReadReceiptPref::Broadcast));
    assert_eq!(st.spaces[&dm_id].updated_at, h1);
    // Idempotent: same value → no-op, no HLC change.
    assert_eq!(st.set_read_receipt_pref(dm_id, ReadReceiptPref::Broadcast, hlc(20)).unwrap(), false);
    assert_eq!(st.spaces[&dm_id].updated_at, h1);
    // Non-DM kind → Err.
    assert!(st.set_read_receipt_pref(ch_id, ReadReceiptPref::Broadcast, hlc(30)).is_err());
    // Missing space → Err.
    assert!(st.set_read_receipt_pref(SpaceId([0xAB;16]), ReadReceiptPref::Off, hlc(40)).is_err());
}
```

Use the existing `test_dm_space` / `test_channel_space` / `hlc` helpers from the module's tests (or the closest equivalents).

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(set_read_receipt_pref_gates_kind_and_is_idempotent)'`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the OwnerState helpers**

In `owner_state_crdt.rs` (near `apply_marker`):

```rust
    /// ZEB-214: set the owner-local per-DM read-receipt preference. Gated to
    /// `Dm`/`GroupDm` (the field is meaningless elsewhere). `Ok(true)` on a
    /// real change (caller has already reserved `new_hlc` and will notify the
    /// sync engine), `Ok(false)` on a no-op (unchanged — no HLC burned).
    pub fn set_read_receipt_pref(
        &mut self,
        space_id: crate::owner_state_types::SpaceId,
        pref: crate::owner_state_types::ReadReceiptPref,
        new_hlc: crate::owner_state_types::Hlc,
    ) -> Result<bool, String> {
        let space = self
            .spaces
            .get_mut(&space_id)
            .ok_or_else(|| format!("space not found: {space_id:?}"))?;
        if !matches!(
            space.kind,
            crate::owner_state_types::SpaceKind::Dm | crate::owner_state_types::SpaceKind::GroupDm
        ) {
            return Err(format!("read_receipt_pref is DM-only (kind={:?})", space.kind));
        }
        if space.read_receipt_pref == Some(pref) {
            return Ok(false);
        }
        space.read_receipt_pref = Some(pref);
        space.updated_at = new_hlc;
        Ok(true)
    }

    /// ZEB-214: read the per-DM read-receipt preference (`None` ≡ Off).
    pub fn read_receipt_pref(
        &self,
        space_id: crate::owner_state_types::SpaceId,
    ) -> Option<crate::owner_state_types::ReadReceiptPref> {
        self.spaces.get(&space_id).and_then(|s| s.read_receipt_pref)
    }
```

- [ ] **Step 4: Run to verify the helper passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(set_read_receipt_pref_gates_kind_and_is_idempotent)'`
Expected: PASS.

- [ ] **Step 5: Add the `maybe_send_read_receipt` wrapper**

In `dm_read_receipt.rs`:

```rust
/// Orchestrate a single read-receipt push: prepare (pref gate + packet build),
/// record the watermark for later reconnect re-sends, and push over the live
/// tunnel. No-op when `prepare_read_receipt` returns `None`. Never writes the
/// outbox (ephemeral).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn maybe_send_read_receipt(
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    mgr: &std::sync::Arc<crate::tunnel_manager::TunnelManager>,
    signing_key: &std::sync::Arc<ed25519_dalek::SigningKey>,
    signing_device_hash: crate::owner_state_types::DeviceIdentityHash,
    self_owner: OwnerAddr,
    device_id: &str,
    watermarks: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<SpaceId, u64>>>,
    space_id: SpaceId,
    up_to_ms: u64,
    now_ms: u64,
) {
    let prepared = {
        let state = crdt_state.lock().await;
        prepare_read_receipt(
            &state, self_owner, signing_device_hash, signing_key, device_id, space_id, up_to_ms, now_ms,
        )
    };
    let Some((peer, wire)) = prepared else { return };
    watermarks.lock().await.insert(space_id, up_to_ms);
    crate::iroh_tunnel_dm_transport::send_packet_to_owner_tunnels(crdt_state, mgr, peer, &wire).await;
}
```

- [ ] **Step 6: Add NodeState fields + populate them**

Add the four fields to the `NodeState` struct (next to the existing `dm_self_owner` / `dm_transport` handles). Default `tunnel_manager`/`dm_tunnel_sign_key`/`dm_tunnel_sign_hash` to `None` and `read_receipt_watermarks` to `Arc::new(Mutex::new(HashMap::new()))` wherever `NodeState` is constructed. At the transport-construction site (`lib.rs:12190-12208`, where `tunnel_manager_for_state`, `dm_tunnel_sign_key_arc`, `dm_tunnel_sign_hash`, and `self_owner` are all in scope), publish them back into `NodeState` alongside the existing `transport` assignment:

```rust
                        // ZEB-214: expose the tunnel manager + DM signing material
                        // so mark_dm_read / the reconnect re-send can push receipts.
                        g_nodestate.tunnel_manager = tunnel_manager_for_state.clone();
                        g_nodestate.dm_tunnel_sign_key = Some(dm_tunnel_sign_key_arc.clone());
                        g_nodestate.dm_tunnel_sign_hash = Some(dm_tunnel_sign_hash);
```

(Use the actual `NodeState` write path used for the sibling `transport` field at that site.)

- [ ] **Step 7: Add the three Tauri commands**

In `lib.rs`, modeled on `set_space_shared_in_profile` (line 44189) but using the owner-state helper + the owner-state sync engine for `notify_dirty`:

```rust
/// ZEB-214: toggle the per-DM read-receipt preference (owner-local, synced
/// across the owner's own devices). Gated to Dm/GroupDm by the helper.
#[tauri::command]
async fn set_space_read_receipt_pref(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    space_id: String,
    enabled: bool,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&space_id)
        .map_err(|e| format!("invalid space_id hex: {e}"))?
        .as_slice().try_into()
        .map_err(|_| "space_id must be 16 bytes".to_string())?;
    let sid = crate::owner_state_types::SpaceId(id_bytes);
    let pref = if enabled {
        crate::owner_state_types::ReadReceiptPref::Broadcast
    } else {
        crate::owner_state_types::ReadReceiptPref::Off
    };
    let (crdt_state, hlc_tracker, adopt_floor, device_id, sync_engine, snapshot_generation) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state.clone().ok_or_else(|| g.owner_not_loaded_msg())?,
            g.hlc_tracker.clone().ok_or_else(|| g.owner_not_loaded_msg())?,
            g.hlc_adopt_floor.clone(),
            g.dm_device_id.clone().ok_or_else(|| g.owner_not_loaded_msg())?,
            g.sync_engine.clone(),
            g.generation,
        )
    };
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let new_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
        &hlc_tracker, &adopt_floor, &device_id, wall_now_ms,
    ).await;
    let changed = {
        let mut g = crdt_state.lock().await;
        g.set_read_receipt_pref(sid, pref, new_hlc)?
    };
    // Generation post-check (detached-node safety), mirroring set_space_shared_in_profile.
    {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        if g.generation != snapshot_generation {
            return Err("node generation changed during set_space_read_receipt_pref".to_string());
        }
    }
    if changed {
        if let Some(engine) = sync_engine {
            engine.notify_dirty();
        }
    }
    Ok(())
}

/// ZEB-214: read the current per-DM read-receipt preference (true = Broadcast).
#[tauri::command]
async fn get_space_read_receipt_pref(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    space_id: String,
) -> Result<bool, String> {
    let id_bytes: [u8; 16] = hex::decode(&space_id)
        .map_err(|e| format!("invalid space_id hex: {e}"))?
        .as_slice().try_into()
        .map_err(|_| "space_id must be 16 bytes".to_string())?;
    let sid = crate::owner_state_types::SpaceId(id_bytes);
    let crdt_state = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        g.crdt_state.clone().ok_or_else(|| g.owner_not_loaded_msg())?
    };
    let g = crdt_state.lock().await;
    Ok(matches!(g.read_receipt_pref(sid), Some(crate::owner_state_types::ReadReceiptPref::Broadcast)))
}

/// ZEB-214: the user read a DM up to `up_to_ms` (the newest rendered message's
/// ms timestamp). If opted in (1:1 Dm, Broadcast) and the peer is reachable,
/// push a signed watermark receipt. No-op when the tunnel isn't up.
#[tauri::command]
async fn mark_dm_read(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    space_id: String,
    up_to_ms: u64,
) -> Result<(), String> {
    let id_bytes: [u8; 16] = hex::decode(&space_id)
        .map_err(|e| format!("invalid space_id hex: {e}"))?
        .as_slice().try_into()
        .map_err(|_| "space_id must be 16 bytes".to_string())?;
    let sid = crate::owner_state_types::SpaceId(id_bytes);
    let (crdt_state, mgr, key, hash, self_owner, device_id, watermarks) = {
        let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state.clone(),
            g.tunnel_manager.clone(),
            g.dm_tunnel_sign_key.clone(),
            g.dm_tunnel_sign_hash,
            g.dm_self_owner,
            g.dm_device_id.clone(),
            g.read_receipt_watermarks.clone(),
        )
    };
    // Any missing handle ⇒ node not fully up / no iroh ⇒ best-effort no-op.
    let (Some(crdt_state), Some(mgr), Some(key), Some(hash), Some(self_owner), Some(device_id)) =
        (crdt_state, mgr, key, hash, self_owner, device_id)
    else {
        return Ok(());
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    crate::dm_read_receipt::maybe_send_read_receipt(
        &crdt_state, &mgr, &key, hash, self_owner, &device_id, &watermarks, sid, up_to_ms, now_ms,
    ).await;
    Ok(())
}
```

Register all three in the `tauri::generate_handler![...]` list (search for `set_space_shared_in_profile` in that macro and add the three names next to it).

- [ ] **Step 8: Full gate**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(read_receipt) + test(set_read_receipt_pref)'` then `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all -- --check`.
Expected: PASS / clean.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/owner_state_crdt.rs src-tauri/src/dm_read_receipt.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-214): read-receipt commands + NodeState handles + emit hook"
```

---

### Task 7: Reconnect re-send on peer inbound activity

**Files:**
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`resolve_owner_for_peer` → `pub(crate)`)
- Modify: `src-tauri/src/dm_read_receipt.rs` (`resend_watermarks_to_peer` core)
- Modify: `src-tauri/src/lib.rs` (call from the tunnel-ingest drain after `ingest_dm_packet`)
- Test: inline `#[cfg(test)]` in `dm_read_receipt.rs`

**Interfaces:**
- Consumes: `resolve_owner_for_peer` (now `pub(crate)`); the `read_receipt_watermarks` map; `prepare_read_receipt`.
- Produces: `pub(crate) fn plan_reconnect_resends(state: &OwnerState, self_owner: OwnerAddr, peer: OwnerAddr, watermarks: &HashMap<SpaceId, u64>) -> Vec<SpaceId>` — the opted-in 1:1 DM spaces with `peer` for which we hold a stored watermark; and a `resend_watermarks_to_peer(...)` async wrapper that pushes each.

- [ ] **Step 1: Write the failing planner test**

```rust
#[test]
fn plan_reconnect_resends_only_opted_in_dms_with_stored_watermark() {
    use crate::owner_state_types::{ReadReceiptPref, SpaceKind};
    let me = OwnerAddr([1;16]); let peer = OwnerAddr([2;16]); let other = OwnerAddr([3;16]);
    let mut st = crate::owner_state_crdt::OwnerState::default();
    let a = dm_space_between(me, peer, Some(ReadReceiptPref::Broadcast)); // opted-in, with peer
    let b = dm_space_between(me, peer, Some(ReadReceiptPref::Off));       // peer, but off
    let c = dm_space_between(me, other, Some(ReadReceiptPref::Broadcast)); // opted-in, wrong peer
    let (a_id, b_id, c_id) = (a.id, b.id, c.id);
    for s in [a, b, c] { st.spaces.insert(s.id, s); }
    let mut wm = std::collections::HashMap::new();
    wm.insert(a_id, 999u64); // stored watermark for a
    wm.insert(b_id, 5u64);   // b is off → excluded regardless
    // c has no stored watermark → excluded
    let plan = plan_reconnect_resends(&st, me, peer, &wm);
    assert_eq!(plan, vec![a_id]);
    let _ = (b_id, c_id);
}
```

Use a `dm_space_between(me, peer, pref)` built from the existing DM-space fixture (members `[me, peer]` sorted, kind `Dm`).

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(plan_reconnect_resends_only_opted_in_dms_with_stored_watermark)'`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the planner + wrapper**

In `dm_read_receipt.rs`:

```rust
/// The 1:1 DM spaces with `peer` that are opted in (Broadcast) AND have a
/// stored watermark to re-send. Used when `peer` proves reachable (an inbound
/// DM just arrived over a live tunnel).
pub(crate) fn plan_reconnect_resends(
    state: &crate::owner_state_crdt::OwnerState,
    self_owner: OwnerAddr,
    peer: OwnerAddr,
    watermarks: &std::collections::HashMap<SpaceId, u64>,
) -> Vec<SpaceId> {
    state
        .spaces
        .values()
        .filter(|s| s.read_receipt_pref == Some(ReadReceiptPref::Broadcast))
        .filter(|s| dm_peer_of(s, self_owner) == Some(peer))
        .filter(|s| watermarks.contains_key(&s.id))
        .map(|s| s.id)
        .collect()
}

/// Re-send our stored watermark to `peer` for each opted-in 1:1 DM. Best-effort
/// (live tunnel just proven by the inbound DM that triggered this).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resend_watermarks_to_peer(
    crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    mgr: &std::sync::Arc<crate::tunnel_manager::TunnelManager>,
    signing_key: &std::sync::Arc<ed25519_dalek::SigningKey>,
    signing_device_hash: crate::owner_state_types::DeviceIdentityHash,
    self_owner: OwnerAddr,
    device_id: &str,
    watermarks: &std::sync::Arc<tokio::sync::Mutex<std::collections::HashMap<SpaceId, u64>>>,
    peer: OwnerAddr,
    now_ms: u64,
) {
    let plan = {
        let state = crdt_state.lock().await;
        let wm = watermarks.lock().await;
        plan_reconnect_resends(&state, self_owner, peer, &wm)
    };
    for space_id in plan {
        let up_to_ms = match watermarks.lock().await.get(&space_id) {
            Some(v) => *v,
            None => continue,
        };
        maybe_send_read_receipt(
            crdt_state, mgr, signing_key, signing_device_hash, self_owner, device_id,
            watermarks, space_id, up_to_ms, now_ms,
        ).await;
    }
}
```

- [ ] **Step 4: Make `resolve_owner_for_peer` reachable**

In `dm_inbox_ingest.rs`, change `fn resolve_owner_for_peer` (line 510) to `pub(crate) fn resolve_owner_for_peer`.

- [ ] **Step 5: Wire the drain**

In `lib.rs` at the tunnel-ingest drain (the `tokio::spawn` recv loop, ~line 11142), capture the extra handles before the spawn (they are all in scope where the drain is spawned): `drain_tunnel_mgr = tunnel_manager_for_state.clone()`, `drain_rr_key = dm_tunnel_sign_key_arc.clone()`, `drain_rr_hash = dm_tunnel_sign_hash`, `drain_rr_watermarks = read_receipt_watermarks_for_state.clone()`, `drain_rr_device = device_id.clone()`. After the `ingest_dm_packet(...).await` match, add:

```rust
                                    // ZEB-214: the peer just proved reachable
                                    // (a DM arrived over a live tunnel). Re-send
                                    // our stored read watermark for any opted-in
                                    // 1:1 DM with them. Best-effort; no-op if the
                                    // tunnel manager isn't present.
                                    if let Some(mgr) = drain_tunnel_mgr.as_ref() {
                                        if let Some(peer) = {
                                            let st = drain_crdt_state.lock().await;
                                            crate::dm_inbox_ingest::resolve_owner_for_peer(&st, dm.peer_node_id)
                                        } {
                                            let now_ms = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap_or_default().as_millis() as u64;
                                            crate::dm_read_receipt::resend_watermarks_to_peer(
                                                &drain_crdt_state, mgr, &drain_rr_key, drain_rr_hash,
                                                drain_self_owner, &drain_rr_device, &drain_rr_watermarks,
                                                peer, now_ms,
                                            ).await;
                                        }
                                    }
```

`read_receipt_watermarks_for_state` is the same `Arc` stored on `NodeState.read_receipt_watermarks` (clone it into a local before the acceptor block, next to the other `*_for_state` handles). `drain_rr_key`/`drain_rr_hash` are the `dm_tunnel_sign_key_arc`/`dm_tunnel_sign_hash` computed at `lib.rs:5680-5705`.

- [ ] **Step 6: Run tests + gate**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(plan_reconnect_resends)'` then clippy + fmt as in Task 6 Step 8.
Expected: PASS / clean.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/dm_inbox_ingest.rs src-tauri/src/dm_read_receipt.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-214): re-send read watermark when peer proves reachable"
```

---

### Task 8: Frontend read-receipt service

**Files:**
- Create: `src/lib/read-receipt-service.ts`
- Create: `src/lib/components/__tests__/read-receipt.test.ts`
- Modify: `src/App.svelte` (instantiate + init + destroy; thread `getWatermark`/`getSeenAt` to `TextFeed`)

**Interfaces:**
- Produces: `class ReadReceiptService { init(adapter): Promise<void>; getWatermark(spaceId): number | undefined; getSeenAt(spaceId): number | undefined; onChange?: () => void; destroy(): void }` where `getWatermark` = the peer's `readUpTo` ms and `getSeenAt` = the receipt `at` ms.

- [ ] **Step 1: Write the failing test**

`src/lib/components/__tests__/read-receipt.test.ts`:

```ts
import { describe, it, expect } from 'vitest';
import { ReadReceiptService } from '../../read-receipt-service';

function fakeAdapter() {
  let cb: ((e: { payload: unknown }) => void) | undefined;
  return {
    listen: async (_name: string, fn: (e: { payload: unknown }) => void) => { cb = fn; return () => {}; },
    emit: (payload: unknown) => cb?.({ payload }),
  };
}

describe('ReadReceiptService', () => {
  it('tracks the latest per-space watermark and seen-time, monotonically', async () => {
    const a = fakeAdapter();
    const svc = new ReadReceiptService();
    await svc.init(a as never);
    a.emit({ spaceId: 'aa', from: 'bb', readUpTo: 100, at: 150 });
    expect(svc.getWatermark('aa')).toBe(100);
    expect(svc.getSeenAt('aa')).toBe(150);
    // A newer watermark advances.
    a.emit({ spaceId: 'aa', from: 'bb', readUpTo: 200, at: 250 });
    expect(svc.getWatermark('aa')).toBe(200);
    // A stale (older) watermark is ignored.
    a.emit({ spaceId: 'aa', from: 'bb', readUpTo: 50, at: 999 });
    expect(svc.getWatermark('aa')).toBe(200);
    expect(svc.getSeenAt('aa')).toBe(250);
    // Unknown space → undefined.
    expect(svc.getWatermark('zz')).toBeUndefined();
  });
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/read-receipt.test.ts`
Expected: FAIL (module not found).

- [ ] **Step 3: Implement the service**

`src/lib/read-receipt-service.ts`:

```ts
/**
 * ZEB-214 — receives `dm-read-receipt` events (a peer told us they've read our
 * DM up to a watermark) and exposes a per-space watermark + seen-time. The
 * watermark (`readUpTo`, ms) gates which of our sent messages show "Seen";
 * `at` (ms) is the receipt send-time shown as "Seen HH:MM". Both advance
 * monotonically — a stale/duplicate receipt never regresses the display.
 */
interface DmReadReceiptPayload {
  spaceId: string;
  from: string;
  readUpTo: number;
  at: number;
}

interface ListenAdapter {
  listen(name: string, cb: (e: { payload: unknown }) => void): Promise<() => void>;
}

export class ReadReceiptService {
  private watermarks = new Map<string, number>();
  private seenAt = new Map<string, number>();
  private unlisten?: () => void;
  onChange?: () => void;

  async init(adapter: ListenAdapter): Promise<void> {
    this.unlisten = await adapter.listen('dm-read-receipt', (event) => {
      const p = event.payload as DmReadReceiptPayload;
      if (!p || typeof p.spaceId !== 'string' || typeof p.readUpTo !== 'number') return;
      const prev = this.watermarks.get(p.spaceId) ?? -1;
      if (p.readUpTo <= prev) return; // monotonic: ignore stale/duplicate
      this.watermarks.set(p.spaceId, p.readUpTo);
      if (typeof p.at === 'number') this.seenAt.set(p.spaceId, p.at);
      this.onChange?.();
    });
  }

  getWatermark(spaceId: string): number | undefined { return this.watermarks.get(spaceId); }
  getSeenAt(spaceId: string): number | undefined { return this.seenAt.get(spaceId); }
  destroy(): void { this.unlisten?.(); this.unlisten = undefined; }
}
```

- [ ] **Step 4: Wire into App.svelte**

Instantiate near the other services, `await readReceiptService.init(adapter)` where the app listens for DM events, set `readReceiptService.onChange = () => (readReceiptVersion += 1)` (a `$state` counter to force reactive re-read), and `destroy()` in teardown. Thread two accessors to `TextFeed` for the active DM: `peerReadUpToMs={ (void readReceiptVersion, readReceiptService.getWatermark(activeChannel)) }` and `peerSeenAtMs={ (void readReceiptVersion, readReceiptService.getSeenAt(activeChannel)) }`.

- [ ] **Step 5: Run test + tsc**

Run: `npx vitest run src/lib/components/__tests__/read-receipt.test.ts` and `npx tsc --noEmit`
Expected: PASS / clean.

- [ ] **Step 6: Commit**

```bash
git add src/lib/read-receipt-service.ts src/lib/components/__tests__/read-receipt.test.ts src/App.svelte
git commit -m "feat(zeb-214): frontend read-receipt watermark service"
```

---

### Task 9: DM-header toggle + mark_dm_read invocation

**Files:**
- Modify: `src/lib/components/TextFeed.svelte` (toggle in the 1:1 `.dm-header`; new props)
- Modify: `src/App.svelte` (fetch pref → pass `readReceiptOn`; `onToggleReadReceipt` → `set_space_read_receipt_pref`; call `mark_dm_read` on DM open + focused inbound)
- Modify: `src/lib/components/__tests__/read-receipt.test.ts` (add the toggle test)

**Interfaces:**
- Consumes: commands `get_space_read_receipt_pref`, `set_space_read_receipt_pref`, `mark_dm_read`.
- Produces: `TextFeed` props `readReceiptOn?: boolean`, `onToggleReadReceipt?: (on: boolean) => void`.

- [ ] **Step 1: Write the failing toggle test**

Append to `read-receipt.test.ts`:

```ts
import { render, fireEvent } from '@testing-library/svelte';
import TextFeed from '../TextFeed.svelte';

it('renders a read-receipt toggle in a 1:1 DM header and reports changes', async () => {
  const calls: boolean[] = [];
  const { getByTestId } = render(TextFeed, {
    props: {
      messages: [], channelType: 'dm', channelName: 'Alice', channelId: 'aa'.repeat(16),
      ownAddress: 'me', readReceiptOn: false,
      onToggleReadReceipt: (on: boolean) => calls.push(on),
    },
  });
  const toggle = getByTestId('read-receipt-toggle');
  await fireEvent.click(toggle);
  expect(calls).toEqual([true]); // off → on
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/read-receipt.test.ts -t 'read-receipt toggle'`
Expected: FAIL (`read-receipt-toggle` not found).

- [ ] **Step 3: Add the props + toggle**

In `TextFeed.svelte`'s `$props()` block add `readReceiptOn = false` and `onToggleReadReceipt` with types `readReceiptOn?: boolean;` and `onToggleReadReceipt?: (on: boolean) => void;`. In the 1:1 `.dm-header` (line 201-212), before the Call button:

```svelte
        <button
          type="button"
          class="btn-receipt"
          data-testid="read-receipt-toggle"
          aria-pressed={readReceiptOn}
          title={readReceiptOn ? 'Read receipts on — click to stop sending' : 'Read receipts off — click to send'}
          onclick={() => onToggleReadReceipt?.(!readReceiptOn)}
        >
          {readReceiptOn ? '👁 Receipts on' : '👁 Receipts off'}
        </button>
```

(Style `.btn-receipt` next to `.btn-call` in the component's `<style>`.)

- [ ] **Step 4: Wire App.svelte**

When a DM becomes active, `invoke('get_space_read_receipt_pref', { spaceId: activeChannel })` → bind `readReceiptOn`. `onToggleReadReceipt={(on) => invoke('set_space_read_receipt_pref', { spaceId: activeChannel, enabled: on }).then(() => readReceiptOn = on).catch((e) => console.warn('set read receipt pref:', e instanceof Error ? e.message : String(e)))}`. At the DM-open read site (`App.svelte:3617`, next to `dmUnread?.markThreadRead(node.id)`), for a `dm`/`group-chat` node also call `invoke('mark_dm_read', { spaceId: node.id, upToMs: newestDmTimestamp(node.id) })` where `newestDmTimestamp` is the max `timestamp` among the loaded messages for that space (0 if none). Also call it when a `dm-received` for the currently-open DM arrives.

- [ ] **Step 5: Run test + tsc**

Run: `npx vitest run src/lib/components/__tests__/read-receipt.test.ts` and `npx tsc --noEmit`
Expected: PASS / clean.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/TextFeed.svelte src/App.svelte src/lib/components/__tests__/read-receipt.test.ts
git commit -m "feat(zeb-214): DM-header read-receipt toggle + mark_dm_read"
```

---

### Task 10: "Seen HH:MM" indicator

**Files:**
- Modify: `src/lib/components/TextFeed.svelte` (compute the newest acknowledged own-message; pass `seenAt` to that `TextMessage`)
- Modify: `src/lib/components/TextMessage.svelte` (render the "Seen HH:MM" line)
- Modify: `src/lib/components/__tests__/read-receipt.test.ts` (add the render test)

**Interfaces:**
- Consumes: `TextFeed` props `peerReadUpToMs?: number`, `peerSeenAtMs?: number` (from Task 8), `ownAddress`.
- Produces: `TextMessage` prop `seenAt?: number` (ms; when set, renders "Seen HH:MM").

- [ ] **Step 1: Write the failing render test**

```ts
it('shows "Seen HH:MM" under the newest own message within the watermark', async () => {
  const mk = (id: string, ts: number, self: boolean) => ({
    id, text: id, timestamp: ts, media: [], priority: 'standard',
    sender: { address: self ? 'self' : 'peer', displayName: self ? 'Me' : 'Them' },
  });
  const { getAllByText, queryAllByText } = render(TextFeed, {
    props: {
      channelType: 'dm', channelId: 'aa'.repeat(16), ownAddress: 'me',
      // own@100 (seen), peer@150, own@200 (NOT seen — after watermark)
      messages: [mk('a', 100, true), mk('b', 150, false), mk('c', 200, true)],
      peerReadUpToMs: 150, peerSeenAtMs: 160,
    },
  });
  // Exactly one "Seen …" line, attached to the own message at ts=100.
  const seen = queryAllByText(/^Seen /);
  expect(seen.length).toBe(1);
});
```

- [ ] **Step 2: Run to verify it fails**

Run: `npx vitest run src/lib/components/__tests__/read-receipt.test.ts -t 'Seen HH:MM'`
Expected: FAIL (no "Seen" text).

- [ ] **Step 3: Compute the acknowledged message in TextFeed**

Add props `peerReadUpToMs?: number` and `peerSeenAtMs?: number`. Compute the id of the newest own message at or before the watermark (1:1 DM only):

```svelte
  let seenMessageId = $derived.by(() => {
    if (channelType !== 'dm' || peerReadUpToMs === undefined) return null;
    const isOwn = (m: Message) =>
      m.sender.address === 'self' || (ownAddress && m.sender.address === ownAddress);
    let best: Message | null = null;
    for (const m of messages) {
      if (isOwn(m) && m.timestamp <= peerReadUpToMs && (!best || m.timestamp > best.timestamp)) best = m;
    }
    return best?.id ?? null;
  });
```

Where each message renders `<TextMessage ... />` (line ~271-284), pass `seenAt={item.message.id === seenMessageId ? (peerSeenAtMs ?? peerReadUpToMs) : undefined}`.

- [ ] **Step 4: Render the line in TextMessage**

Add prop `seenAt` to `TextMessage.svelte`'s `$props()` (`seenAt?: number;`). Add a derived label and a line after `.message-text` (line 134), mirroring the `CallEventLine` HH:MM idiom:

```svelte
  let seenStr = $derived(
    seenAt === undefined
      ? ''
      : new Date(seenAt).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }),
  );
```

```svelte
    {#if seenAt !== undefined}
      <div class="seen-indicator" data-testid="seen-indicator">Seen {seenStr}</div>
    {/if}
```

(Style `.seen-indicator`: small, right-aligned, `color: var(--text-secondary)`.)

- [ ] **Step 5: Run test + tsc + full vitest**

Run: `npx vitest run src/lib/components/__tests__/read-receipt.test.ts`, then `npx tsc --noEmit`, then `npx vitest run`.
Expected: PASS / clean / whole suite green.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/TextFeed.svelte src/lib/components/TextMessage.svelte src/lib/components/__tests__/read-receipt.test.ts
git commit -m "feat(zeb-214): Seen HH:MM indicator under acknowledged own messages"
```

---

## Final verification (before PR)

- [ ] Rust full gate (CI parity): `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] Frontend full gate: from repo root, `npx tsc --noEmit && npx vitest run`
- [ ] Confirm no `OutboxEntry`/deposit path was added for receipts (grep `dm_read_receipt` + the ingest arm — the only send is `send_packet_to_owner_tunnels`).

## Self-review notes (planner)

- **Spec coverage:** §3 pref field → T1/T6; §4 wire+crypto → T2/T3; §5 emit/ingest/render → T4/T5/T6/T8/T9/T10; §5 reconnect → T7; §6 toggle UI → T9; §7 invariants (never-when-off, no-outbox, group-inert, verify-fail-drop) → T3/T4/T5 tests; §8 tests → each task.
- **Deliberate refinements vs spec (flag for reviewer):** (a) the watermark is **frontend-supplied** via `mark_dm_read(spaceId, upToMs)` rather than backend-computed — `InboxEntry` does not persist message `sent_at`, and the frontend already holds authoritative per-message timestamps; (b) "reconnect re-send" is realized as **re-send on the peer's next inbound DM** (the only in-repo liveness proof without changing the frozen `harmony-tunnel-iroh` crate). Both preserve the approved behavior.
- **Type consistency:** `ReadReceiptPref`/`read_receipt_pref` (T1) used identically in T5/T6/T7; `DmReadReceiptSigned` field set (T2) matches `prepare_read_receipt` construction (T5) and `verify_read_receipt_admission` reads (T3); event payload keys `spaceId`/`from`/`readUpTo`/`at` (T4) match the frontend `DmReadReceiptPayload` (T8) and the "Seen" wiring (T10).
- **Test-harness reuse:** several Rust tests (T3/T4/T6) reuse existing crypto/cache/ingest fixtures rather than re-deriving identities inline; the task text names the fixture to copy. If a named fixture differs, use the nearest existing one — the assertions are the contract.
