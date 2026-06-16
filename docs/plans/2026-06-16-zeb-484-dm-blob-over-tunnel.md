# ZEB-484 (Move 1c): tunnel-inline DM content-blob carrier — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the encrypted DM message blob live over the PQ iroh tunnel so two online peers with no butler exchange DM content end-to-end (un-ignore `s2_dm_delivery_over_tunnel_hard_assert`).

**Architecture:** A new `DmPacket::CidNotifyWithBlob` variant (disc `0x04`) carries the signed `CidNotify` plus the encrypted `storage_blob` inline. The sender's existing best-effort "attempt-tunnel" rung (`IrohTunnelDmTransport`) reads the blob from CAS and inlines it when it fits a frame-safety ceiling; the receiver CAS-puts the inline blob (content-addressed, fails closed) then runs the **existing** CidNotify ingest, whose Phase-3 fetch now hits local CAS. The butler deposit rung is unchanged (durability). Mirrors ZEB-473's always-deposit + attempt-tunnel split.

**Tech Stack:** Rust, tokio, `cargo nextest`, ed25519-dalek, `harmony_content::cid` (content-addressing), the `harmony-tunnel` PQ session.

**Spec:** `docs/specs/2026-06-16-zeb-484-dm-blob-over-tunnel-design.md`

---

## File Structure

| File | Responsibility / change |
|---|---|
| `src-tauri/src/dm_envelope.rs` | New `DmPacket::CidNotifyWithBlob` variant + `build_signed_cidnotify_with_blob` + length-delimited `encode_packet`/`decode_packet` path (disc `0x04`) + unit tests |
| `src-tauri/src/dm_outbox.rs` | New `build_dm_packet_with_blob` wrapper (parallel to `build_dm_packet`) |
| `src-tauri/src/iroh_tunnel_dm_transport.rs` | `cas` field + ctor param; `send` inlines the blob when it fits; `INLINE_BLOB_MAX` const + `build_tunnel_dm_packet` helper + unit tests; update the 3 existing tests' `make_transport` calls |
| `src-tauri/src/lib.rs` | Thread `content_store` into the production `IrohTunnelDmTransport::new` call (`:7569`) |
| `src-tauri/src/dm_inbox_ingest.rs` | New `CidNotifyWithBlob` dispatch arm in `ingest_dm_packet`: `cas_put` (content-addressed) then delegate to the existing CidNotify ingest + a unit test |
| `e2e-harness/tests/e2e_two_node.rs` | Un-ignore `s2_dm_delivery_over_tunnel_hard_assert`; fix the ZEB-483→ZEB-484 comment |

**Task order & dependencies:** Task 1 (envelope) → Task 2 (receive) and Task 3 (send) both depend on Task 1 → Task 4 (e2e) depends on Tasks 2 + 3.

**Gates (run from `src-tauri/`):**
- Targeted tests: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(<name>)'`
- Lint: `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- Format: `cargo fmt --all -- --check`

---

## Task 1: `DmPacket::CidNotifyWithBlob` variant + length-delimited codec

**Files:**
- Modify: `src-tauri/src/dm_envelope.rs` (enum ~`182-199`, after `build_signed_ack` ~`368`, `encode_packet` ~`262`, `decode_packet` ~`370`)
- Test: `src-tauri/src/dm_envelope.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the variant to the `DmPacket` enum**

In `dm_envelope.rs`, add a fourth variant to `pub enum DmPacket` (after the `Ack { … }` variant, currently ending ~line 198):

```rust
    /// ZEB-484 (Move 1c): a `CidNotify` carrying the encrypted DM `storage_blob`
    /// inline, for live peer-to-peer delivery over the PQ tunnel when the
    /// recipient has no butler. `signed`/`signature`/`signed_bytes` are IDENTICAL
    /// to a bare `CidNotify` (the same Ed25519 signature authenticates them); the
    /// `storage_blob` carries no separate signature because it is bound by
    /// content-addressing — the receiver recomputes
    /// `ContentId::for_book(storage_blob)` and rejects a mismatch. The wire layout
    /// is length-delimited (two variable-length fields), distinct from the
    /// `[disc][body][64-sig]` layout of the other three variants. See
    /// `encode_packet` / `decode_packet`.
    CidNotifyWithBlob {
        signed: DmCidNotifySigned,
        signature: [u8; 64],
        signed_bytes: Vec<u8>,
        storage_blob: Vec<u8>,
    },
```

- [ ] **Step 2: Add the builder `build_signed_cidnotify_with_blob`**

After `build_signed_ack` (ends ~line 368), add:

```rust
/// ZEB-484: build a `CidNotifyWithBlob` packet — a signed `CidNotify` (signed
/// EXACTLY like `build_signed_cidnotify`) plus the encrypted `storage_blob`
/// carried inline. The blob is NOT signed; it is bound by content-addressing at
/// the receiver (`ContentId::for_book(storage_blob) == signed.message_cid`).
pub fn build_signed_cidnotify_with_blob(
    signed: DmCidNotifySigned,
    signing_key: &ed25519_dalek::SigningKey,
    storage_blob: Vec<u8>,
) -> Result<DmPacket, EncodeError> {
    let signed_bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed)
        .map_err(|e| EncodeError::Cbor(e.to_string()))?;
    let signature = crate::dm_signing::sign_dm_packet(&signed_bytes, signing_key);
    Ok(DmPacket::CidNotifyWithBlob {
        signed,
        signature,
        signed_bytes,
        storage_blob,
    })
}
```

- [ ] **Step 3: Handle the variant in `encode_packet` (early, length-delimited)**

At the TOP of `encode_packet` (before the existing `let (disc, signed_bytes, signature) = match packet { … }`), add an early-return for the new variant, and add an `unreachable!` arm to the existing match so it stays exhaustive:

```rust
pub fn encode_packet(packet: &DmPacket) -> Result<Vec<u8>, EncodeError> {
    // ZEB-484: the blob-carrying variant has two variable-length fields, so it
    // uses an explicit length-delimited layout instead of the shared
    // [disc][signed_bytes][64-sig] layout. Handle + return early.
    if let DmPacket::CidNotifyWithBlob {
        signed,
        signature,
        signed_bytes,
        storage_blob,
    } = packet
    {
        let re_encoded = crate::owner_state_crypto::canonical_cbor_encode(signed)
            .map_err(|e| EncodeError::ReSerialize(format!("re-encode signed body: {e}")))?;
        if re_encoded != *signed_bytes {
            return Err(EncodeError::SignedMutated(
                "DmPacket CidNotifyWithBlob variant: signed mutated post-build (re-encode \
                 mismatches cached signed_bytes; signature would not cover wire body)"
                    .to_string(),
            ));
        }
        let body_len = u32::try_from(signed_bytes.len()).map_err(|_| {
            EncodeError::Cbor("CidNotifyWithBlob signed_bytes length exceeds u32".to_string())
        })?;
        let mut out = Vec::with_capacity(1 + 4 + signed_bytes.len() + 64 + storage_blob.len());
        out.push(0x04);
        out.extend_from_slice(&body_len.to_be_bytes());
        out.extend_from_slice(signed_bytes);
        out.extend_from_slice(signature);
        out.extend_from_slice(storage_blob);
        return Ok(out);
    }
    let (disc, signed_bytes, signature): (u8, &Vec<u8>, &[u8; 64]) = match packet {
        // ... EXISTING Invite (0x01) / CidNotify (0x02) / Ack (0x03) arms unchanged ...
        DmPacket::CidNotifyWithBlob { .. } => {
            unreachable!("CidNotifyWithBlob is handled by the early return above")
        }
    };
    // ... rest of existing encode_packet unchanged ...
}
```

- [ ] **Step 4: Handle disc `0x04` in `decode_packet` + add `decode_cidnotify_with_blob`**

In `decode_packet`, immediately after `let (disc, rest) = bytes.split_first().ok_or(DecodeError::Empty)?;`, add the special-case before the generic `rest.len() < 64 + 1` check:

```rust
    // ZEB-484: the blob-carrying variant is length-delimited (two variable
    // fields); it does NOT use the "sig = last 64 bytes" split used below.
    if *disc == 0x04 {
        return decode_cidnotify_with_blob(rest);
    }
```

Then add the free function (place it directly after `decode_packet`):

```rust
/// ZEB-484: decode the length-delimited `CidNotifyWithBlob` body (everything
/// after the `0x04` discriminant):
/// `[u32 BE len(signed_bytes)][signed_bytes][64 sig][storage_blob]`.
/// Applies the same CidNotify structural invariants as the `0x02` arm and
/// requires a non-empty blob (an empty blob is malformed — the sender would use
/// a bare `CidNotify`).
fn decode_cidnotify_with_blob(rest: &[u8]) -> Result<DmPacket, DecodeError> {
    if rest.len() < 4 {
        return Err(DecodeError::TooShortForSignature);
    }
    let (len_bytes, after_len) = rest.split_at(4);
    let body_len = u32::from_be_bytes(
        len_bytes
            .try_into()
            .expect("split_at(4) yields exactly 4 bytes"),
    ) as usize;
    // Need the signed body + a 64-byte signature; storage_blob is the remainder.
    if after_len.len() < body_len + 64 {
        return Err(DecodeError::TooShortForSignature);
    }
    let (body_bytes, after_body) = after_len.split_at(body_len);
    let (signature_bytes, storage_blob) = after_body.split_at(64);
    let signature: [u8; 64] = signature_bytes
        .try_into()
        .expect("split_at(64) yields exactly 64 bytes");
    if storage_blob.is_empty() {
        return Err(DecodeError::Invalid(
            "CidNotifyWithBlob.storage_blob must be non-empty",
        ));
    }
    let signed: DmCidNotifySigned = decode_body(body_bytes)?;
    ensure_canonical_body(&signed, body_bytes)?;
    if signed.sender_devices.len() > MAX_DEVICES_PER_OWNER {
        return Err(DecodeError::Invalid(
            "CidNotifyWithBlob.sender_devices exceeds MAX_DEVICES_PER_OWNER",
        ));
    }
    if !signed.sender_devices.contains(&signed.signing_device_hash) {
        return Err(DecodeError::Invalid(
            "CidNotifyWithBlob.signing_device_hash must be in sender_devices",
        ));
    }
    Ok(DmPacket::CidNotifyWithBlob {
        signed,
        signature,
        signed_bytes: body_bytes.to_vec(),
        storage_blob: storage_blob.to_vec(),
    })
}
```

- [ ] **Step 5: Write the failing tests**

Add to `dm_envelope.rs`'s `#[cfg(test)] mod tests` (the module already `use super::*` and constructs `DmCidNotifySigned`, `SpaceId`, `ContentId`, `OwnerAddr`, `DeviceIdentityHash`):

```rust
    #[test]
    fn dm_packet_cidnotify_with_blob_round_trip() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let device_hash = DeviceIdentityHash([0x11; 16]);
        let signed = DmCidNotifySigned {
            space_id: SpaceId([0x77; 16]),
            message_cid: ContentId::from_bytes([0xab; 32]),
            sender_owner_addr: OwnerAddr([0xA1; 16]),
            sender_devices: vec![device_hash],
            signing_device_hash: device_hash,
        };
        let storage_blob = vec![0xCDu8; 4096];
        let packet =
            build_signed_cidnotify_with_blob(signed.clone(), &signing_key, storage_blob.clone())
                .expect("build with-blob packet");
        let wire = encode_packet(&packet).expect("encode");
        assert_eq!(wire[0], 0x04, "discriminant byte is 0x04");
        let decoded = decode_packet(&wire).expect("decode");
        match decoded {
            DmPacket::CidNotifyWithBlob {
                signed: d_signed,
                storage_blob: d_blob,
                ..
            } => {
                assert_eq!(d_signed, signed, "signed body round-trips");
                assert_eq!(d_blob, storage_blob, "storage_blob round-trips");
            }
            other => panic!("expected CidNotifyWithBlob, got {other:?}"),
        }
    }

    #[test]
    fn dm_packet_cidnotify_with_blob_empty_blob_rejected() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]);
        let device_hash = DeviceIdentityHash([0x11; 16]);
        let signed = DmCidNotifySigned {
            space_id: SpaceId([0x77; 16]),
            message_cid: ContentId::from_bytes([0xab; 32]),
            sender_owner_addr: OwnerAddr([0xA1; 16]),
            sender_devices: vec![device_hash],
            signing_device_hash: device_hash,
        };
        let packet = build_signed_cidnotify_with_blob(signed, &signing_key, Vec::new()).unwrap();
        let wire = encode_packet(&packet).unwrap();
        let err = decode_packet(&wire).unwrap_err();
        assert!(
            matches!(err, DecodeError::Invalid(m) if m.contains("storage_blob must be non-empty")),
            "an empty inline blob must be rejected, got {err:?}"
        );
    }

    #[test]
    fn dm_packet_cidnotify_with_blob_truncated_rejected() {
        // A 0x04 packet shorter than [4 len][body][64 sig] must not panic.
        let err = decode_packet(&[0x04, 0x00, 0x00]).unwrap_err();
        assert!(matches!(err, DecodeError::TooShortForSignature), "got {err:?}");
    }
```

- [ ] **Step 6: Run the tests to verify they fail, then pass**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(cidnotify_with_blob)'`
Expected before Steps 1-4: FAIL (variant/builder undefined). After: PASS (3 tests).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/dm_envelope.rs
git commit -m "feat(zeb-484): DmPacket::CidNotifyWithBlob variant + length-delimited codec"
```

---

## Task 2: Receive-side dispatch — CAS-put the inline blob then ingest

**Files:**
- Modify: `src-tauri/src/dm_inbox_ingest.rs` (`ingest_dm_packet` dispatch match, ~`405-463`)
- Test: `src-tauri/src/dm_inbox_ingest.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the `CidNotifyWithBlob` dispatch arm**

In `ingest_dm_packet`, inside the `match crate::dm_envelope::decode_packet(...)?` block, add a new arm AFTER the `DmPacket::Ack { .. }` arm (before the closing `}` of the match at ~line 463):

```rust
        crate::dm_envelope::DmPacket::CidNotifyWithBlob {
            signed,
            signature,
            signed_bytes,
            storage_blob,
        } => {
            // ZEB-484 (Move 1c): the live tunnel carried the encrypted blob
            // inline. CAS-put it FIRST under its content-addressed CID — the SAME
            // `for_book` flags the butler sweeper uses (`cas_put`, this file) — so
            // the Phase 3 fetch below hits local CAS instead of the (deliberately
            // refusing) content-serve queryable. The CID is recomputed FROM the
            // blob, so a blob that does NOT hash to the signed `message_cid` lands
            // under a different key: Phase 3's `get(message_cid)` then misses and
            // delivery FAILS CLOSED (Phase 3b re-checks the binding too).
            let cid = harmony_content::cid::ContentId::for_book(
                &storage_blob,
                harmony_content::cid::ContentFlags {
                    encrypted: true,
                    ..Default::default()
                },
            )
            .map_err(|e| format!("CidNotifyWithBlob for_book: {e:?}"))?;
            content_store
                .put(cid, storage_blob)
                .await
                .map_err(|e| format!("CidNotifyWithBlob CAS put: {e:?}"))?;
            (signed, signature, signed_bytes)
        }
```

(The arm yields `(signed, signature, signed_bytes)` exactly like the `CidNotify` arm, so it falls through to the existing Phase 2 admission / Phase 3 CAS fetch / Phase 3b binding / Phase 4 decrypt+apply / Phase 6 emit — all reused verbatim.)

- [ ] **Step 2: Write the failing test**

Add to `dm_inbox_ingest.rs`'s test module (the one with `ingest_dm_packet_applies_a_tunnel_delivered_invite`; it imports `build_dm_ingest_fixture`, `OwnerAddr`, `SpaceId`, `ContentId`, and via the fixture module `InMemoryStub`). Place it near the other `ingest_dm_packet_*` tests:

```rust
    /// ZEB-484: a `CidNotifyWithBlob` delivers the DM live — the inline blob is
    /// CAS-put (no zenoh content query) and `dm-received` fires. Proven by using
    /// a FRESH (empty) content store: if the inline path didn't CAS-put the blob,
    /// Phase 3's `get(message_cid)` would miss and no `dm-received` would emit.
    #[tokio::test]
    async fn ingest_dm_packet_cidnotify_with_blob_delivers_live() {
        let fx = build_dm_ingest_fixture(b"hello-over-tunnel").await;

        // Pull the encrypted blob the fixture stored, and re-wrap the fixture's
        // VALID signed CidNotify (decoded from fx.packet, so the signature still
        // verifies against fx.crdt_state) as a CidNotifyWithBlob carrying it.
        let blob = fx
            .content_store
            .get(&fx.message_cid)
            .await
            .unwrap()
            .expect("fixture stored the encrypted blob");
        let (signed, signature, signed_bytes) =
            match crate::dm_envelope::decode_packet(&fx.packet).unwrap() {
                crate::dm_envelope::DmPacket::CidNotify {
                    signed,
                    signature,
                    signed_bytes,
                } => (signed, signature, signed_bytes),
                other => panic!("fixture packet must be a bare CidNotify, got {other:?}"),
            };
        let with_blob = crate::dm_envelope::DmPacket::CidNotifyWithBlob {
            signed,
            signature,
            signed_bytes,
            storage_blob: blob,
        };
        let wire = crate::dm_envelope::encode_packet(&with_blob).unwrap();

        // FRESH empty store: only the inline CAS-put can make the blob fetchable.
        let fresh_store: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        assert!(
            fresh_store.get(&fx.message_cid).await.unwrap().is_none(),
            "precondition: the fresh store does not yet hold the blob"
        );

        let applied = ingest_dm_packet(
            &fx.crdt_state,
            &fresh_store,
            &fx.sink,
            fx.bob,
            &fx.bob_device_id,
            [0u8; 32],
            &wire,
        )
        .await
        .expect("a CidNotifyWithBlob from an admitted sender must deliver");
        assert!(applied, "a delivered DM emits dm-received (Ok(true))");

        // The inline blob was CAS-put under message_cid (content-addressed).
        assert!(
            fresh_store.get(&fx.message_cid).await.unwrap().is_some(),
            "the inline blob must be CAS-put so the recipient can read it"
        );
        // dm-received emitted.
        assert!(
            fx.sink_handle
                .frames()
                .iter()
                .any(|(n, _)| n == crate::dm_outbox::DM_RECEIVED_EVENT),
            "a CidNotifyWithBlob must emit dm-received"
        );
    }
```

- [ ] **Step 3: Run the test — verify it fails, then passes**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(ingest_dm_packet_cidnotify_with_blob_delivers_live)'`
Expected before Step 1: FAIL (non-exhaustive match / unknown variant). After: PASS.

> If `applied`/`DM_RECEIVED_EVENT`/fixture field names differ from the existing `ingest_dm_packet_applies_inbox_and_emits_dm_received` test, mirror that test's exact assertions (same module) — the structure (fresh store, decode-rewrap, assert CAS-put + emit) is the load-bearing part.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/dm_inbox_ingest.rs
git commit -m "feat(zeb-484): ingest CidNotifyWithBlob — CAS-put inline blob then deliver"
```

---

## Task 3: Send-side blob inlining in `IrohTunnelDmTransport`

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (add `build_dm_packet_with_blob` after `build_dm_packet` ~`343`; update the test ctor call at ~`8295`)
- Modify: `src-tauri/src/iroh_tunnel_dm_transport.rs` (struct field + ctor + `send` + const + helper + tests)
- Modify: `src-tauri/src/lib.rs` (production ctor call ~`7569`)

- [ ] **Step 1: Add `build_dm_packet_with_blob` to `dm_outbox.rs`**

Directly after `build_dm_packet` (ends ~line 343):

```rust
/// ZEB-484: build a `CidNotifyWithBlob` wire packet — the signed CidNotify plus
/// the encrypted `storage_blob` inline. Parallel to `build_dm_packet`.
pub(crate) fn build_dm_packet_with_blob(
    signed: crate::dm_envelope::DmCidNotifySigned,
    signing_key: &ed25519_dalek::SigningKey,
    storage_blob: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let packet =
        crate::dm_envelope::build_signed_cidnotify_with_blob(signed, signing_key, storage_blob)
            .map_err(|e| format!("build_signed_cidnotify_with_blob: {e}"))?;
    crate::dm_envelope::encode_packet(&packet).map_err(|e| format!("encode_packet: {e}"))
}
```

- [ ] **Step 2: Add the `cas` field + const + helper to `iroh_tunnel_dm_transport.rs`**

Update the `use` line (top of file) from
`use crate::dm_outbox::{build_dm_packet, DmTransport, TransportError};`
to
`use crate::dm_outbox::{build_dm_packet, build_dm_packet_with_blob, DmTransport, TransportError};`
and add `use crate::content_store::ContentStore;`.

Add the const near the top of the file (after the `use`s):

```rust
/// ZEB-484: inline-blob ceiling on the ASSEMBLED `CidNotifyWithBlob` packet size.
/// Comfortably below the tunnel frame cap (`DATA_MAX_MESSAGE = 2 MiB`,
/// `tunnel_task.rs`): a full 1 MiB CAS book + storage envelope + the CidNotify +
/// framing fit with ~0.5 MiB headroom, and a single book never exceeds it. Over
/// this, `send` falls back to a bare `CidNotify` and the deposit rung carries it.
pub(crate) const INLINE_BLOB_MAX: usize = 1_572_864; // 1.5 MiB
```

Add the `cas` field to the struct:

```rust
pub struct IrohTunnelDmTransport {
    mgr: Arc<TunnelManager>,
    crdt_state: Arc<tokio::sync::Mutex<OwnerState>>,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    self_owner: OwnerAddr,
    our_signing_device_hash: DeviceIdentityHash,
    /// ZEB-484: the local CAS, read on `send` to inline the encrypted DM blob
    /// over the tunnel for live delivery (when it fits `INLINE_BLOB_MAX`).
    cas: Arc<dyn ContentStore>,
}
```

Update `new` to take + store it (add `cas: Arc<dyn ContentStore>` as the last param, set `cas` in the struct literal).

Add the helper as a free function in the module:

```rust
/// ZEB-484: build the tunnel DM packet for one DM — `CidNotifyWithBlob` (inline
/// blob) when the blob is in CAS and the assembled packet fits `INLINE_BLOB_MAX`,
/// else the bare `CidNotify` (durability is the deposit rung's job either way).
async fn build_tunnel_dm_packet(
    cas: &Arc<dyn ContentStore>,
    signed: &crate::dm_envelope::DmCidNotifySigned,
    signing_key: &ed25519_dalek::SigningKey,
    message_cid: crate::owner_state_types::ContentId,
) -> Result<Vec<u8>, String> {
    if let Ok(Some(blob)) = cas.get(&message_cid).await {
        match build_dm_packet_with_blob(signed.clone(), signing_key, blob) {
            Ok(packet) if packet.len() <= INLINE_BLOB_MAX => return Ok(packet),
            Ok(_) => {
                // Oversize for the frame budget — fall through to bare CidNotify.
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "ZEB-484: with-blob packet build failed; sending bare CidNotify"
                );
            }
        }
    }
    build_dm_packet(signed.clone(), signing_key)
}
```

- [ ] **Step 3: Use the helper in `send`**

In `impl DmTransport for IrohTunnelDmTransport`'s `send`, replace the `match build_dm_packet(signed, &self.signing_key) { … }` block (currently ~`171-189`) so it builds via the helper:

```rust
        if !targets.is_empty() {
            let signed = crate::dm_envelope::DmCidNotifySigned {
                space_id: entry.space_id,
                message_cid: entry.message_cid,
                sender_owner_addr: self.self_owner,
                sender_devices: vec![self.our_signing_device_hash],
                signing_device_hash: self.our_signing_device_hash,
            };
            // ZEB-484: inline the encrypted blob when it fits; else bare CidNotify.
            match build_tunnel_dm_packet(
                &self.cas,
                &signed,
                &self.signing_key,
                entry.message_cid,
            )
            .await
            {
                Ok(packet) => {
                    for (node_id, contact) in &targets {
                        self.mgr.send_dm(*node_id, contact, packet.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        recipient = ?recipient,
                        error = %e,
                        "ZEB-484: tunnel DM packet build failed; deposit rung still covers this DM"
                    );
                }
            }
        }
```

(The `Err(TransportError::Transient(...))` tail is UNCHANGED — always-deposit still holds.)

- [ ] **Step 4: Thread `content_store` into the production ctor (`lib.rs`)**

At `lib.rs:7569`, add the new arg to the `IrohTunnelDmTransport::new(...)` call (the `content_store` Arc is in scope here — it is moved at `:7586`, AFTER this call):

```rust
                            crate::iroh_tunnel_dm_transport::IrohTunnelDmTransport::new(
                                std::sync::Arc::clone(tunnel_mgr),
                                std::sync::Arc::clone(&crdt_state),
                                signing_key_arc.clone(),
                                self_owner,
                                our_signing_device_hash,
                                std::sync::Arc::clone(&content_store),
                            ),
```

- [ ] **Step 5: Update the existing ctor callers + `make_transport` test helper**

In `iroh_tunnel_dm_transport.rs` tests, change `make_transport` to take a `cas` and forward it:

```rust
    fn make_transport(
        mgr: Arc<TunnelManager>,
        state: Arc<tokio::sync::Mutex<OwnerState>>,
        cas: Arc<dyn crate::content_store::ContentStore>,
    ) -> IrohTunnelDmTransport {
        let signing_key = Arc::new(ed25519_dalek::SigningKey::from_bytes(&[0x42u8; 32]));
        IrohTunnelDmTransport::new(
            mgr,
            state,
            signing_key,
            OwnerAddr([0xff; 16]),
            DeviceIdentityHash([0xaa; 16]),
            cas,
        )
    }
```

Update its three existing callers to pass an EMPTY store (so they keep asserting bare-CidNotify routing):
- `send_resolves_contact_and_routes_to_manager`: `let transport = make_transport(Arc::clone(&mgr), state, Arc::new(crate::content_store::InMemoryStub::default()));`
- `send_without_contact_still_returns_transient_for_deposit`: same change.
- `send_unknown_recipient_returns_transient`: `let transport = make_transport(mgr, state, Arc::new(crate::content_store::InMemoryStub::default()));`

Also update the `dm_outbox.rs:8295` `IrohTunnelDmTransport::new(...)` test call: add a final arg `std::sync::Arc::new(crate::content_store::InMemoryStub::default())` (or the content store already in scope in that test).

- [ ] **Step 6: Write the failing send tests**

Add to `iroh_tunnel_dm_transport.rs` tests:

```rust
    /// ZEB-484: a recipient with a tunnel contact AND the blob in CAS → `send`
    /// routes a `CidNotifyWithBlob` carrying that exact blob.
    #[tokio::test]
    async fn send_with_blob_in_cas_routes_cidnotify_with_blob() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc { wall_ms: 1, logical: 0, device_id: "peer".into() },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));

        let cid = ContentId::from_bytes([0xee; 32]);
        let blob = vec![0xCDu8; 2048];
        let cas: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        cas.put(cid, blob.clone()).await.expect("seed blob in CAS");

        let transport = make_transport(Arc::clone(&mgr), state, Arc::clone(&cas));
        let entry = synthetic_outbox_entry(SpaceId([0xcc; 16]), cid, recipient);
        let _ = transport
            .send(&entry, recipient, Vec::new())
            .await
            .expect_err("always-deposit: send returns Transient");

        let pending = mgr
            .test_pending_packets(&expected_node_id)
            .expect("a session was registered for the derived NodeId");
        assert_eq!(pending.len(), 1);
        match crate::dm_envelope::decode_packet(&pending[0]).expect("decode routed packet") {
            crate::dm_envelope::DmPacket::CidNotifyWithBlob { signed, storage_blob, .. } => {
                assert_eq!(signed.message_cid, cid, "carries the DM's message_cid");
                assert_eq!(storage_blob, blob, "inlines the exact CAS blob");
            }
            other => panic!("expected CidNotifyWithBlob, got {other:?}"),
        }
    }

    /// ZEB-484: a blob larger than the frame budget → `send` falls back to a bare
    /// `CidNotify` (deposit rung carries durability).
    #[tokio::test]
    async fn send_oversize_blob_falls_back_to_bare_cidnotify() {
        let mgr = test_manager().await;
        let recipient = OwnerAddr([0x11; 16]);
        let dsa_pubkey = vec![0x07u8; 1952];
        let contact = DeviceTunnelContact {
            iroh_node_id: [0x09; 32],
            home_relay_url: None,
            pq_dsa_pubkey: dsa_pubkey.clone(),
            pq_kem_pubkey: vec![0x08u8; 1184],
        };
        let expected_node_id = node_id_from_dsa_pubkey(&dsa_pubkey);
        let mut owner_state = OwnerState::default();
        owner_state.owner_device_cache.devices.insert(
            recipient,
            OwnerDeviceEntry {
                devices: vec![DeviceIdentityHash([0x33; 16])],
                device_identity_pubs: vec![None],
                learned_at: Hlc { wall_ms: 1, logical: 0, device_id: "peer".into() },
                device_tunnel_contacts: vec![Some(contact)],
            },
        );
        let state = Arc::new(tokio::sync::Mutex::new(owner_state));

        let cid = ContentId::from_bytes([0xee; 32]);
        let blob = vec![0x00u8; INLINE_BLOB_MAX]; // assembled packet exceeds the ceiling
        let cas: Arc<dyn crate::content_store::ContentStore> =
            Arc::new(crate::content_store::InMemoryStub::default());
        cas.put(cid, blob).await.expect("seed oversize blob");

        let transport = make_transport(Arc::clone(&mgr), state, Arc::clone(&cas));
        let entry = synthetic_outbox_entry(SpaceId([0xcc; 16]), cid, recipient);
        let _ = transport.send(&entry, recipient, Vec::new()).await.expect_err("Transient");

        let pending = mgr.test_pending_packets(&expected_node_id).expect("session registered");
        assert_eq!(pending.len(), 1);
        assert!(
            matches!(
                crate::dm_envelope::decode_packet(&pending[0]).unwrap(),
                crate::dm_envelope::DmPacket::CidNotify { .. }
            ),
            "an oversize blob must fall back to a bare CidNotify"
        );
    }
```

- [ ] **Step 7: Run the send tests + the existing transport tests**

Run: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(iroh_tunnel_dm_transport)'`
Expected: PASS (the 2 new tests + the 3 updated existing tests all green).

- [ ] **Step 8: Build-check the whole workspace (lib.rs ctor wiring)**

Run: `cargo check --locked --all-targets --features test-fixtures`
Expected: clean (catches the `lib.rs:7569` ctor + the `dm_outbox.rs:8295` test ctor).

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/iroh_tunnel_dm_transport.rs src-tauri/src/dm_outbox.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-484): inline the DM blob over the tunnel when it fits the frame budget"
```

---

## Task 4: Un-ignore the S2 e2e hard-assert + fix the comment

**Files:**
- Modify: `e2e-harness/tests/e2e_two_node.rs` (`s2_dm_delivery_over_tunnel_hard_assert`, ~`418-450`)

- [ ] **Step 1: Remove the `#[ignore]` attribute**

Delete the entire `#[ignore = "ZEB-482 landed … (ZEB-483)."]` attribute (the multi-line attribute at ~`444-449`), leaving the `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]` and the `async fn s2_dm_delivery_over_tunnel_hard_assert()` intact.

- [ ] **Step 2: Update the lead comment to reflect ZEB-484 closing the gap**

Replace the "REMAINING gap" paragraph + the "So this stays a HARD ASSERT … (ZEB-483)" paragraph (~`426-441`) with a note that ZEB-484 closed it via the tunnel-inline blob carrier:

```rust
// CLOSED by ZEB-484 (Move 1c): the encrypted DM *blob* now rides the PQ tunnel
// inline (`DmPacket::CidNotifyWithBlob`) alongside the CidNotify, so two
// co-located peers with NO butler deliver DM content live — the receiver CAS-puts
// the inline blob (content-addressed; fails closed on mismatch) and the existing
// CidNotify ingest finds it locally instead of hitting the (refusing)
// content-serve queryable. The butler deposit rung is unchanged (durability).
//
// This is a HARD ASSERT (no longer ignored). Run it explicitly with:
//   cargo nextest run --features e2e -E 'test(s2_dm_delivery_over_tunnel_hard_assert)'
// ─────────────────────────────────────────────────────────────────────────────
```

- [ ] **Step 3: Run the e2e test (the DoD)**

Run (from `src-tauri/`): `cargo nextest run --features e2e -E 'test(s2_dm_delivery_over_tunnel_hard_assert)'`
Expected: PASS — Bob fires `dm-received` for `b"hard-assert-dm-over-tunnel"` and the plaintext lands in Bob's DM thread.

> This test spins two full nodes + iroh first-contact; allow **~3-5 min** wall-clock. The friend handshake is racy (~75-90s, the test retries internally). If it fails ONLY on the 120s friend-handshake deadline (not on the `dm-received` asserts), re-run — that is pre-existing first-contact flakiness, not a blob-delivery regression. The blob-delivery asserts (the new behavior) are deterministic once friendship is Active.

- [ ] **Step 4: Commit**

```bash
git add e2e-harness/tests/e2e_two_node.rs
git commit -m "test(zeb-484): un-ignore s2_dm_delivery_over_tunnel_hard_assert (blob delivers live)"
```

---

## Final gates (after all tasks)

- [ ] `cargo fmt --all -- --check` — clean
- [ ] `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings` — clean
- [ ] `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(cidnotify_with_blob) | test(ingest_dm_packet_cidnotify_with_blob_delivers_live) | test(iroh_tunnel_dm_transport)'` — all green
- [ ] `cargo check --locked --all-targets --features test-fixtures` — clean
- [ ] e2e DoD: `s2_dm_delivery_over_tunnel_hard_assert` green under `--features e2e`

The full `--all-targets` clippy + nextest sweep runs in CI; the per-task `--lib` gates above keep local iteration fast (the e2e feature is NOT in the 4-job CI, so the DoD test is verified manually here).
