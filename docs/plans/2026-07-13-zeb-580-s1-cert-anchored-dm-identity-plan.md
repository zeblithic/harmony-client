# ZEB-580 S1 — Cert-anchored #2 DM identity (dual-path) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate DM packet body signing from the Reticulum identity key (#3) to the cert-attested enrolled device key (#2), with the receiver resolving the sender's #2 identity from a master-attested `EnrollmentCert` (friend handshake — already on the wire; or a new additive `DmInvite` field), keeping legacy #3 verification alive (dual-path).

**Architecture:** The DM identity for device #2 is the 64-byte combined pub `x25519_pub ‖ ed25519_verify` read from its `EnrollmentCert`; its DM hash is the existing `SHA256(X25519‖Ed25519)[:16]` scheme, so `verify_dm_packet_signature` is unchanged. Senders sign with `community_signing_key` (#2) and stamp #2's DM hash; receivers cache the #2 combined pub (from the friend-handshake cert, already verified but today discarded; or from a new `inviter_enrollment` field on `DmInvite`). Rollout is a hard flag-day (§4.7 of the spec) — no identity-refresh machinery.

**Tech Stack:** Rust (Tauri backend), `ed25519-dalek`, `harmony-owner` (git dep @ `1ecb4160`, checkout dir `1ecb416`), CBOR via `owner_state_crypto::canonical_cbor_encode`.

**Spec:** `docs/specs/2026-07-13-zeb-580-dm-signing-migration-design.md` (§3–§4, §7–§9). This plan implements **S1 only**; S2 (revocation cutoff) is a separate plan.

## Global Constraints

- **No `FILE_VERSION` / packet-version bumps.** All wire changes are additive optional fields (`#[serde(default, skip_serializing_if = ...)]`). The DM packet layer has no version byte (routing discriminants only); do not add one.
- **Never construct `KeychainStore::new()` in test-reachable code.** Inject via `*_inner`/`*_with_keychain` seams. Set `HARMONY_PASSPHRASE` in identity-touching tests (CLAUDE.md ZEB-428).
- **Gates (CI-parity, run from `src-tauri/`):** `cargo fmt --all -- --check` · `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` · `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Frontend (repo root): `npx tsc --noEmit` · `npx vitest run`. Iterative: `scripts/test-select --context task`.
- **`--all-targets` and `--locked` are load-bearing** — always include both.
- **Keep #3 alive.** This slice does not remove `signing_key` (#3), `private_identity` (#3), or `verify_dm_packet_signature`'s generality — #3 verify is retained (dual-path). Only DM *body signing* moves to #2.
- **Expiry-agnostic cert verification** for DM identity: verify structurally (`cert.verify(0)` / `verify_enrollment_any_issuer(..., now_secs=0)` where a non-expiry check is wanted), matching `DmOutbox::new`'s existing invariant.
- **Master-issued only on the `DmInvite` path** (N3): the new `DmInvite` field carries a single `EnrollmentCert`, no signer bundle. A quorum-issued cert bootstrapping via DmInvite-only degrades to the legacy #3 path (not a hard failure). The friend-handshake path already carries `signer_certs` and handles quorum.

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `src-tauri/src/dm_signing.rs` | #2 combined-pub + #2 DM-hash helpers | Add `device2_combined_pub`, `device2_signing_hash` + tests |
| `src-tauri/src/dm_envelope.rs` | `DmInviteSigned` wire body | Add additive `inviter_enrollment` field + round-trip tests |
| `src-tauri/src/dm_outbox.rs` | build/verify/cache DM packets | Verify+cache #2 in `apply_invite`/`run_invite_accept_tail`; flip signing sites to #2; add `our_device2_signing_hash` |
| `src-tauri/src/iroh_friend_acceptor.rs` | friend handshake | Cache peer's #2 combined pub (from the already-verified cert) on both request+accept sides |
| `src-tauri/src/lib.rs` | node construction + DM invite send | Pass #2 material to outbox+transports; attach `inviter_enrollment` at the invite send site |
| `src-tauri/tests/dm/dm_send_integration.rs` (or a new `dm_cert_identity_integration.rs`) | end-to-end #2 round-trip | New cross-WAN-shaped integration test (ZEB-504 gate) |

---

## Task 1: `device2_combined_pub` + `device2_signing_hash` helpers

**Files:**
- Modify: `src-tauri/src/dm_signing.rs` (add fns after `derive_device_hash_from_identity_pub`, ~line 289; add tests in the existing `mod tests`)

**Interfaces:**
- Consumes: `harmony_owner::certs::EnrollmentCert` (fields `device_pubkeys.classical.{x25519_pub, ed25519_verify}: [u8;32]`); `crate::owner_state_types::DeviceIdentityHash`; existing `derive_device_hash_from_identity_pub(&[u8;64]) -> Option<DeviceIdentityHash>`.
- Produces:
  - `pub fn device2_combined_pub(cert: &harmony_owner::certs::EnrollmentCert) -> [u8; 64]`
  - `pub fn device2_signing_hash(cert: &harmony_owner::certs::EnrollmentCert) -> Option<DeviceIdentityHash>`

- [ ] **Step 1: Write the failing tests**

Add to `dm_signing.rs`'s `#[cfg(test)] mod tests`:

```rust
    /// ZEB-580 S1: the #2 combined pub is x25519_pub ‖ ed25519_verify from
    /// the cert, and its DM hash differs from the same device's #3 hash.
    #[test]
    fn device2_combined_pub_and_hash_from_mint() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted
            .state
            .enrollments
            .values()
            .find(|c| {
                c.device_pubkeys.classical.ed25519_verify
                    == minted.device_signing_key.verifying_key().to_bytes()
            })
            .expect("device cert");

        let combined = device2_combined_pub(cert);
        assert_eq!(&combined[..32], &cert.device_pubkeys.classical.x25519_pub);
        assert_eq!(&combined[32..], &cert.device_pubkeys.classical.ed25519_verify);

        let h2 = device2_signing_hash(cert).expect("real cert yields a #2 hash");
        // Deterministic + equals the direct derivation.
        assert_eq!(
            h2,
            derive_device_hash_from_identity_pub(&combined).unwrap()
        );
    }

    /// A cert with an all-zero X25519 half (the pre-ZEB-372 stub / a
    /// degenerate synthetic cert) yields no usable #2 identity — callers
    /// must degrade rather than cache a degenerate combined pub.
    #[test]
    fn device2_signing_hash_rejects_zeroed_x25519() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let mut cert = minted.state.enrollments.values().next().unwrap().clone();
        cert.device_pubkeys.classical.x25519_pub = [0u8; 32];
        assert!(device2_signing_hash(&cert).is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(device2_)'`
Expected: FAIL — `device2_combined_pub` / `device2_signing_hash` not found.

- [ ] **Step 3: Write the implementation**

Add after `derive_device_hash_from_identity_pub` (dm_signing.rs:289):

```rust
/// ZEB-580 S1: build the 64-byte DM combined pub (`X25519_pub(32) ‖
/// Ed25519_pub(32)`) for an enrolled device (#2) from its EnrollmentCert's
/// classical pubkeys. This is the same layout as
/// `harmony_identity::Identity::to_public_bytes()`, so its DM device hash is
/// `derive_device_hash_from_identity_pub(&combined)` and
/// `verify_dm_packet_signature` accepts it unchanged.
pub fn device2_combined_pub(cert: &harmony_owner::certs::EnrollmentCert) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&cert.device_pubkeys.classical.x25519_pub);
    out[32..].copy_from_slice(&cert.device_pubkeys.classical.ed25519_verify);
    out
}

/// ZEB-580 S1: the DM device hash for a device's #2 identity, or `None` when
/// the cert lacks a usable X25519 pub (all-zero pre-ZEB-372 stub or a
/// degenerate synthetic cert) or the combined pub is not a valid Identity
/// point. Callers treat `None` as "no #2 identity available" and degrade to
/// the legacy #3 path.
pub fn device2_signing_hash(
    cert: &harmony_owner::certs::EnrollmentCert,
) -> Option<DeviceIdentityHash> {
    let combined = device2_combined_pub(cert);
    if combined[..32] == [0u8; 32] {
        return None;
    }
    derive_device_hash_from_identity_pub(&combined)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(device2_)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_signing.rs
git commit -m "ZEB-580 S1 Task 1: device2 combined-pub + DM-hash helpers"
```

---

## Task 2: additive `inviter_enrollment` field on `DmInviteSigned`

**Files:**
- Modify: `src-tauri/src/dm_envelope.rs` (`DmInviteSigned` struct ~67-115; tests in `mod tests`)

**Interfaces:**
- Consumes: `harmony_owner::certs::EnrollmentCert` (derives `Serialize`/`Deserialize`).
- Produces: `DmInviteSigned.inviter_enrollment: Option<harmony_owner::certs::EnrollmentCert>` (serde key `"ie"`, `default`, `skip_serializing_if = "Option::is_none"`). `build_signed_invite`, `encode_packet`, `decode_packet` are **unchanged** (the field rides inside the already-signed body); only construction sites populate it.

- [ ] **Step 1: Write the failing tests**

Add to `dm_envelope.rs`'s `mod tests`:

```rust
    /// ZEB-580 S1: a DmInvite carrying an inviter_enrollment round-trips
    /// byte-exactly and the signature still covers the cert.
    #[test]
    fn dm_invite_with_inviter_enrollment_round_trips() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted.state.enrollments.values().next().unwrap().clone();
        let sk = minted.device_signing_key;

        let mut signed = sample_invite_signed(); // existing helper in this test mod
        signed.inviter_enrollment = Some(cert.clone());

        let packet = build_signed_invite(signed.clone(), &sk).expect("build");
        let wire = encode_packet(&packet).expect("encode");
        let decoded = decode_packet(&wire).expect("decode");
        match decoded {
            DmPacket::Invite { signed: got, .. } => {
                assert_eq!(got.inviter_enrollment, Some(cert));
                assert_eq!(got, signed);
            }
            _ => panic!("expected Invite"),
        }
    }

    /// Back-compat: a DmInvite WITHOUT the field decodes with None, and the
    /// canonical bytes are identical to a pre-ZEB-580 invite (skip_serializing_if
    /// omits the key entirely when None).
    #[test]
    fn dm_invite_without_inviter_enrollment_is_none_and_byte_stable() {
        let signed = sample_invite_signed(); // inviter_enrollment defaults to None
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&signed).unwrap();
        let back: DmInviteSigned = crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(back.inviter_enrollment, None);
        // The "ie" key must be absent from the map when None.
        assert!(!bytes.windows(2).any(|w| w == b"ie"));
    }
```

If `sample_invite_signed()` does not exist, add a minimal builder in the test module mirroring the fields shown in `dm_envelope.rs:67-115` (with `inviter_enrollment: None`), reusing whatever `SpaceId`/`OwnerAddr`/`Hlc` fixtures the surrounding tests already use. If a `canonical_cbor_decode` helper name differs, use the module's existing decode entry point (grep `canonical_cbor_decode|from_canonical` in `owner_state_crypto.rs`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dm_invite_with_inviter_enrollment) + test(dm_invite_without_inviter_enrollment)'`
Expected: FAIL — no field `inviter_enrollment`.

- [ ] **Step 3: Add the field**

In `DmInviteSigned` (after `inviter_identity_pub`, dm_envelope.rs:114):

```rust
    /// ZEB-580 S1: the inviter's enrolled-device (#2) EnrollmentCert. When
    /// present, the receiver verifies master→#2 (+ owner_id match + that the
    /// derived #2 DM hash equals `signing_device_hash`) and caches the #2 DM
    /// identity — no prior friend handshake needed. Absent for legacy #3
    /// senders (then the receiver falls back to `inviter_identity_pub`).
    /// Additive: the map key is omitted entirely when None, so pre-ZEB-580
    /// invites are byte-stable and decode with None.
    #[serde(rename = "ie", default, skip_serializing_if = "Option::is_none")]
    pub inviter_enrollment: Option<harmony_owner::certs::EnrollmentCert>,
```

Fix every existing `DmInviteSigned { .. }` literal in non-test code and fixtures to add `inviter_enrollment: None` (grep `DmInviteSigned {` across `src/`; the live construction site is `lib.rs` ~13402 and `dm_outbox.rs::build_invite_packet_from_space` ~467 — Task 5/6 set them to `Some`, set them to `None` here to compile).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dm_invite_with_inviter_enrollment) + test(dm_invite_without_inviter_enrollment)'`
Expected: PASS.

- [ ] **Step 5: Run the envelope round-trip suite (no regression)**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dm_packet) + test(build_signed_invite)'`
Expected: PASS (existing invite round-trips unaffected — the field defaults None).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/dm_envelope.rs src-tauri/src/lib.rs src-tauri/src/dm_outbox.rs
git commit -m "ZEB-580 S1 Task 2: additive inviter_enrollment field on DmInviteSigned"
```

---

## Task 3: verify + cache #2 in `apply_invite` / `run_invite_accept_tail`

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (`apply_invite` 2268-2372; `run_invite_accept_tail` 2378-2504; tests in `mod revoke_tests`/the dm_outbox test module)

**Interfaces:**
- Consumes: `dm_signing::{device2_combined_pub, device2_signing_hash}` (Task 1); `harmony_owner::certs::EnrollmentCert::verify`; `crate::enrollment_verify::verify_enrollment_any_issuer`; `DmInviteSigned.inviter_enrollment` (Task 2).
- Produces: `apply_invite` gains no new params (resolves the signer pub internally); `run_invite_accept_tail` gains a trailing param `signer_identity_pub: [u8; 64]` (the pub to cache — #2 when the cert path was taken, else the legacy `inviter_identity_pub`).

- [ ] **Step 1: Write the failing tests**

Add to the dm_outbox test module (adapt fixtures to the existing `two_device_fixture`/mint helpers; the invite must be from an ACTIVE friend so `apply_invite` reaches the accept tail — seed a `FriendEntry` with `status = Active` for `signed.inviter`):

```rust
    /// ZEB-580 S1: an invite carrying a valid inviter_enrollment verifies via
    /// the #2 combined pub and caches the #2 DM identity (not #3).
    #[test]
    fn apply_invite_with_cert_caches_device2_identity() {
        let minted = harmony_owner::lifecycle::mint_owner(1_700_000_000).expect("mint");
        let cert = minted.state.enrollments.values().next().unwrap().clone();
        let sk2 = minted.device_signing_key; // #2 key
        let inviter = OwnerAddr(cert.owner_id);
        let device2_pub = crate::dm_signing::device2_combined_pub(&cert);
        let device2_hash = crate::dm_signing::device2_signing_hash(&cert).unwrap();

        let mut state = /* OwnerState with self_owner ∈ members and inviter as Active friend */;
        // Build a Dm invite signed by #2, with signing_device_hash = device2_hash,
        // sender_devices = [device2_hash], inviter_enrollment = Some(cert), and a
        // real #2 signature over the canonical body.
        let signed = build_dm_invite_signed_with_cert(&mut state, inviter, device2_hash, cert.clone());
        let signature = sk2.sign(&canonical(&signed)).to_bytes();
        let signed_bytes = canonical(&signed);

        let out = apply_invite(
            &mut state, /*self_owner*/ SELF, "dev", signed, signature, &signed_bytes,
            1_700_000_100, Some(inviter), true,
        ).expect("apply");
        assert!(matches!(out, ApplyInviteOutcome::Accepted));

        // The cache now holds the #2 combined pub keyed by the #2 DM hash.
        let entry = state.owner_device_cache.devices.get(&inviter).unwrap();
        let idx = entry.devices.iter().position(|d| *d == device2_hash).unwrap();
        assert_eq!(entry.device_identity_pubs[idx], Some(device2_pub));
    }

    /// Owner mismatch (cert.owner_id != signed.inviter) rejects.
    #[test]
    fn apply_invite_cert_owner_mismatch_rejects() {
        /* build an invite whose inviter_enrollment.owner_id != signed.inviter → expect
           Err(DmReceiveError::SignatureVerificationFailed or a dedicated variant) */
    }

    /// signing_device_hash != device2 hash rejects (cert/hash desync).
    #[test]
    fn apply_invite_cert_hash_mismatch_rejects() {
        /* set signed.signing_device_hash to a bogus hash while carrying a valid cert
           → expect Err */
    }

    /// Legacy invite (inviter_enrollment = None) still verifies via #3 and caches #3.
    #[test]
    fn apply_invite_legacy_no_cert_caches_device3() {
        /* existing #3 path unchanged: build with inviter_enrollment = None, sign with #3,
           assert the #3 pub is cached */
    }
```

The helpers `build_dm_invite_signed_with_cert` / `canonical` are thin test builders; if equivalents exist in the module, reuse them. Use `DmReceiveError` variants that already exist; if a distinct reason is warranted for the cert checks, add a variant in Step 3 and assert it.

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(apply_invite_with_cert) + test(apply_invite_cert_owner) + test(apply_invite_cert_hash) + test(apply_invite_legacy_no_cert)'`
Expected: FAIL — no cert handling in `apply_invite`.

- [ ] **Step 3: Implement cert-anchored verification**

In `apply_invite` (dm_outbox.rs), replace the single `verify_dm_packet_signature(... &signed.inviter_identity_pub ...)` call (2320-2325) with a resolved signer pub:

```rust
    // ZEB-580 S1: resolve the signer's combined pub. If the invite carries an
    // inviter_enrollment (#2 cert), verify master→#2 + owner binding + hash
    // agreement and use the #2 combined pub; otherwise fall back to the legacy
    // inline #3 pub.
    let signer_identity_pub: [u8; 64] = if let Some(cert) = &signed.inviter_enrollment {
        // Master-issued only on this path (N3): no signer bundle carried.
        // Expiry-agnostic (now_secs = 0), matching DmOutbox::new's invariant.
        crate::enrollment_verify::verify_enrollment_any_issuer(cert, &[], Some(&signed.inviter.0), 0)
            .map_err(|_| DmReceiveError::SignatureVerificationFailed)?;
        let expected = crate::dm_signing::device2_signing_hash(cert)
            .ok_or(DmReceiveError::SignatureVerificationFailed)?;
        if expected != signed.signing_device_hash {
            return Err(DmReceiveError::SigningKeyDoesNotMatchDeviceHash);
        }
        let d2_pub = crate::dm_signing::device2_combined_pub(cert);
        // Defense: the cert and the self-consistent inline pub (Task 5 sets
        // inviter_identity_pub = device2_combined_pub on the #2 path) must agree,
        // so a mismatched invite (cert for one #2, inline pub for another) is
        // rejected rather than silently trusting the cert over the signed pub.
        if d2_pub != signed.inviter_identity_pub {
            return Err(DmReceiveError::SigningKeyDoesNotMatchDeviceHash);
        }
        d2_pub
    } else {
        signed.inviter_identity_pub
    };
    crate::dm_signing::verify_dm_packet_signature(
        signed_bytes,
        &signature,
        &signer_identity_pub,
        signed.signing_device_hash,
    )?;
```

Thread `signer_identity_pub` into the accept tail. Change the `run_invite_accept_tail(...)` call (2364-2370) to pass it, and change the fn signature (2378-2385) to accept `signer_identity_pub: [u8; 64]`. Inside `run_invite_accept_tail`, replace the two uses of `signed.inviter_identity_pub` (in the `device_identity_pubs[signer_idx] = Some(...)` write, 2478) with `signer_identity_pub`.

Note: `verify_enrollment_any_issuer` expects `now_secs`; passing `0` short-circuits the expiry check for a `Some(expires_at)` cert only if `0 <= expires_at` (always true) — confirm it does not *reject* a non-expiring cert at `now_secs = 0` (it does not: expiry is `now_secs > exp`). If a Quorum cert reaches this path with an empty `signer_certs`, `verify_enrollment_any_issuer` errors → mapped to `SignatureVerificationFailed` → the invite is dropped (N3: quorum-via-DmInvite-only is unsupported; the sender should re-bootstrap via the friend handshake). This is the intended degrade, not a bug.

- [ ] **Step 4: Run to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(apply_invite)'`
Expected: PASS (new + existing apply_invite tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "ZEB-580 S1 Task 3: apply_invite verifies + caches the inviter #2 cert identity"
```

---

## Task 4: friend handshake caches the peer's #2 combined pub

**Files:**
- Modify: `src-tauri/src/iroh_friend_acceptor.rs` (`process_friend_request` device-cache write ~1119-1142; the symmetric accept-processing path that consumes `FriendLinkAccepted.enrollment` — grep `apply_owner_device_update` in this file for both sites)

**Interfaces:**
- Consumes: `dm_signing::{device2_combined_pub, device2_signing_hash}`; the already-verified `req.enrollment` / accepted-side `enrollment`; `apply_owner_device_update(addr, devices, device_identity_pubs, device_tunnel_contacts, learned_at)`.
- Produces: after a successful handshake, `OwnerDeviceCache` holds the peer's **#2** DM identity (combined pub keyed by #2 DM hash) with the tunnel contact — replacing the previously-cached #3 wire bundle for that owner.

- [ ] **Step 1: Write the failing test**

Add to the friend-acceptor test module (reuse its existing handshake fixtures; both sides mint real owners so the certs carry real X25519):

```rust
    /// ZEB-580 S1: after processing a friend request, the acceptor caches the
    /// requester's #2 DM identity (derived from the verified enrollment cert),
    /// keyed by the #2 DM hash — not the wire #3 bundle.
    #[test]
    fn process_friend_request_caches_requester_device2_identity() {
        let (mut state, req, /* self materials */ ..) = friend_request_fixture();
        let expect_pub = crate::dm_signing::device2_combined_pub(&req.enrollment);
        let expect_hash = crate::dm_signing::device2_signing_hash(&req.enrollment).unwrap();

        process_friend_request(&mut state, learned_at(), &req, /* self args */ ..).expect("ok");

        let entry = state.owner_device_cache.devices.get(&req.from_addr).unwrap();
        let idx = entry.devices.iter().position(|d| *d == expect_hash).expect("#2 device cached");
        assert_eq!(entry.device_identity_pubs[idx], Some(expect_pub));
        // The tunnel contact rode along on the (sole) cached device.
        assert!(entry.device_tunnel_contacts.get(idx).map(|c| c.is_some()).unwrap_or(false));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(process_friend_request_caches_requester_device2)'`
Expected: FAIL — the cache holds the #3 hash, not `expect_hash`.

- [ ] **Step 3: Cache #2 from the cert**

In `process_friend_request`, replace the device-cache write (the `if !req.sender_devices.is_empty() { ... apply_owner_device_update(req.from_addr, req.sender_devices.clone(), req.device_identity_pubs.clone(), device_tunnel_contacts, learned_at) ... }` block, ~1119-1142) so it caches the **#2** identity derived from the verified cert:

```rust
    // ZEB-580 S1: cache the requester's cert-attested #2 DM identity (was: the
    // wire #3 bundle, now discarded for DM signing). The cert was already
    // verified above (verify_enrolled_device). Degrade to the legacy #3 bundle
    // only if the cert has no usable X25519 (test/synthetic cert).
    let device_tunnel_contacts = vec![crate::dm_tunnel_contact::peer_handshake_contact(
        req.iroh_node_id,
        req.home_relay_url.clone(),
        req.pq_dsa_pubkey.clone(),
        req.pq_kem_pubkey.clone(),
    )];
    let (devices, pubs): (Vec<DeviceIdentityHash>, Vec<Option<[u8; 64]>>) =
        match crate::dm_signing::device2_signing_hash(&req.enrollment) {
            Some(h2) => (
                vec![h2],
                vec![Some(crate::dm_signing::device2_combined_pub(&req.enrollment))],
            ),
            None if !req.sender_devices.is_empty() => {
                (req.sender_devices.clone(), req.device_identity_pubs.clone())
            }
            None => (Vec::new(), Vec::new()),
        };
    if !devices.is_empty() {
        if let crate::owner_state_crdt::ApplyOutcome::Rejected(reason) = state
            .apply_owner_device_update(req.from_addr, devices, pubs, device_tunnel_contacts, learned_at)
        {
            return Err(FriendHandshakeError::ApplyRejected(format!("device cache: {reason:?}")));
        }
    }
```

Apply the symmetric change on the accept-processing path (where `FriendLinkAccepted.enrollment` is verified and the acceptor's #2 should be cached by the requester). Keep the `learned_at` = local-HLC anti-forgery rule.

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(process_friend_request) + test(friend_link_accept)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/iroh_friend_acceptor.rs
git commit -m "ZEB-580 S1 Task 4: friend handshake caches the peer #2 DM identity from the cert"
```

---

## Task 5: `DmOutbox` signs with #2 + attaches the cert on invites

**Files:**
- Modify: `src-tauri/src/dm_outbox.rs` (struct fields ~600-651; `new`/`new_synthetic` 654-737; `build_cidnotify_packet_bytes` 1511-1534; the test transport `send` 256-273; `build_invite_packet_from_space` ~455-482)

**Interfaces:**
- Consumes: `dm_signing::{device2_combined_pub, device2_signing_hash}`; `self.community_signing_key` (#2), `self.enrollment_cert`.
- Produces: `DmOutbox.our_device2_signing_hash: Option<DeviceIdentityHash>` (computed in `new`/`new_synthetic` from `enrollment_cert`; `None` degrades signing to legacy #3). A private helper `fn dm_signing_material(&self) -> (&Arc<SigningKey>, DeviceIdentityHash)` returning (#2 key, #2 hash) when `our_device2_signing_hash` is `Some`, else (#3 key, #3 hash).

- [ ] **Step 1: Write the failing tests**

```rust
    /// ZEB-580 S1: a DmOutbox built from a real (minted) enrollment cert signs
    /// its CidNotify with #2 and stamps the #2 DM hash, verifiable against the
    /// #2 combined pub.
    #[test]
    fn dm_outbox_signs_cidnotify_with_device2() {
        let (outbox, cert) = outbox_from_mint(); // helper: DmOutbox::new with minted #2 cert
        let device2_hash = crate::dm_signing::device2_signing_hash(&cert).unwrap();
        assert_eq!(outbox.our_device2_signing_hash, Some(device2_hash));

        let bytes = outbox.build_cidnotify_packet_bytes(&state_with_message(), &entry()).unwrap();
        let packet = crate::dm_envelope::decode_packet(&bytes).unwrap();
        match packet {
            crate::dm_envelope::DmPacket::CidNotify { signed, signature, signed_bytes } => {
                assert_eq!(signed.signing_device_hash, device2_hash);
                crate::dm_signing::verify_dm_packet_signature(
                    &signed_bytes, &signature,
                    &crate::dm_signing::device2_combined_pub(&cert),
                    signed.signing_device_hash,
                ).expect("verifies against #2");
            }
            _ => panic!("expected CidNotify"),
        }
    }

    /// A built invite carries inviter_enrollment = Some(self cert).
    #[test]
    fn dm_outbox_invite_attaches_enrollment_cert() {
        let (outbox, cert) = outbox_from_mint();
        let packet = outbox.build_invite_packet_from_space(&dm_space(), &content_key()).unwrap();
        match packet { /* DmPacket::Invite { signed, .. } => assert_eq!(signed.inviter_enrollment, Some(cert)) */ }
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dm_outbox_signs_cidnotify_with_device2) + test(dm_outbox_invite_attaches_enrollment)'`
Expected: FAIL — no `our_device2_signing_hash`, still signs #3.

- [ ] **Step 3: Add the field + compute in constructors**

Add to `DmOutbox` (after `enrollment_cert`, 627):

```rust
    /// ZEB-580 S1: this device's #2 DM hash, computed from `enrollment_cert`.
    /// `None` when the cert has no usable X25519 (synthetic/test certs) — then
    /// DM body signing degrades to the legacy #3 (`signing_key` / `our_signing_device_hash`).
    pub(crate) our_device2_signing_hash: Option<DeviceIdentityHash>,
```

In both `new` (687-701) and `new_synthetic` (722-736), compute and set it:

```rust
            our_device2_signing_hash: crate::dm_signing::device2_signing_hash(&enrollment_cert),
```

(Compute BEFORE the struct literal if the borrow checker complains about `enrollment_cert` being moved — bind `let d2 = crate::dm_signing::device2_signing_hash(&enrollment_cert);` before the `Self { .. }`.)

Add the private helper (impl DmOutbox):

```rust
    /// ZEB-580 S1: the (key, device-hash) pair for DM body signing — #2 when a
    /// usable enrolled identity exists, else legacy #3.
    fn dm_signing_material(&self) -> (&Arc<ed25519_dalek::SigningKey>, DeviceIdentityHash) {
        match self.our_device2_signing_hash {
            Some(h) => (&self.community_signing_key, h),
            None => (&self.signing_key, self.our_signing_device_hash),
        }
    }
```

- [ ] **Step 4: Flip the build sites**

`build_cidnotify_packet_bytes` (1522-1533):

```rust
        let (key, dh) = self.dm_signing_material();
        let signed = crate::dm_envelope::DmCidNotifySigned {
            space_id: entry.space_id,
            message_cid,
            sender_owner_addr: self.self_owner,
            sender_devices: resolve_sender_devices(state, self.self_owner, dh),
            signing_device_hash: dh,
        };
        build_dm_packet(signed, key)
```

Test transport `send` (256-273): same pattern — `let (key, dh) = self.dm_signing_material();` then use `dh` for `sender_devices`/`signing_device_hash` and `key` for `build_dm_packet`. (The test transport's `signing_key`/`our_signing_device_hash` fields are its own; give it a `dm_signing_material` equivalent or inline the same match if it lacks the #2 fields — simplest: seed the test transport's `signing_key` with #2 material at its construction and stamp the #2 hash. Keep this test-only struct minimal.)

`build_invite_packet_from_space` (~455-482): use `let (key, dh) = self.dm_signing_material();`, set `signing_device_hash: dh`, `sender_devices` to include `dh`, `inviter_enrollment: Some(self.enrollment_cert.clone())`, and sign with `key`. **Populate `inviter_identity_pub` with the pub that matches the signing key**: when signing #2 (`our_device2_signing_hash.is_some()`), set it to `device2_combined_pub(&self.enrollment_cert)` so the invite is self-consistent (`derive_device_hash_from_identity_pub(inviter_identity_pub) == signing_device_hash == dh`). This keeps the invite verifiable both ways — an updated receiver verifies via the cert (master-attested), a transitional receiver can TOFU-verify via the raw #2 pub — so the invite bootstrap does not hard-break during the flag-day. On the #3 fallback path, `inviter_identity_pub` stays the #3 pub as today.

Also enumerate and flip the **live tunnel** CidNotify builder `IrohTunnelDmTransport::send` (grep `DmCidNotifySigned` / `resolve_sender_devices` in `dm_outbox.rs`): it must sign with #2 too. If it holds its own `signing_key`/`our_signing_device_hash`, either give it the #2 pair at construction (Task 6) or thread `dm_signing_material` from the outbox.

- [ ] **Step 5: Run to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dm_outbox)'`
Expected: PASS (new tests + existing `dm_outbox_*` — update any that asserted #3 signing to expect #2, keeping a #3-fallback test that builds via `new_synthetic` with a zeroed-X25519 cert to pin the degrade path).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/dm_outbox.rs
git commit -m "ZEB-580 S1 Task 5: DmOutbox signs DM bodies with #2 + attaches the cert on invites"
```

---

## Task 6: lib.rs wiring — #2 material to outbox/transports + attach cert at the invite send site

**Files:**
- Modify: `src-tauri/src/lib.rs` (DmOutbox construction ~4712-4722; the transport constructions right after ~4728-4735 and where `IrohTunnelDmTransport` is built; the DM invite send site `add_space_dm_inner` ~13402-13412)

**Interfaces:**
- Consumes: `own_enrollment_cert` (#2 cert, already in scope at construction, `lib.rs:4619`), `community_signing_key_arc` (#2). No new material needs sourcing — everything is already present (that is why the friend-handshake primary bootstrap is zero-new-wire).
- Produces: the outbox + transports sign #2; outbound `DmInvite`s carry `inviter_enrollment: Some(own #2 cert)`.

- [ ] **Step 1: Write the failing test**

Prefer to cover this via the Task 7 integration test (a full node build + DM send), since lib.rs construction is not unit-addressable. Add a focused assertion there (Task 7 Step 1) that a sent invite carries the cert and a sent CidNotify verifies against #2. If a `start_node`-level harness test exists (grep `start_node` in `tests/`), assert `dm_outbox.our_device2_signing_hash.is_some()` after a real mint boot.

- [ ] **Step 2: Wire #2 material into the transports**

`DmOutbox::new` already receives `community_signing_key_arc` + `own_enrollment_cert` (4719-4720) and now computes `our_device2_signing_hash` itself (Task 5) — no change needed at the outbox call.

For the transports (grep `IrohTunnelDmTransport` construction + the `signing_key_arc` / `our_signing_device_hash` uses near 4708-4735 and wherever the tunnel transport is built before the NodeState lift-out): pass `community_signing_key_arc.clone()` as the signing key and the **#2 DM hash** as `our_signing_device_hash`. Compute the #2 hash once near 4710:

```rust
    // ZEB-580 S1: #2 DM identity for the transports (mirrors DmOutbox's own
    // computation). Falls back to the #3 hash if the cert lacks a usable X25519.
    let our_device2_hash = crate::dm_signing::device2_signing_hash(&own_enrollment_cert);
    let (dm_sign_key_arc, dm_sign_hash) = match our_device2_hash {
        Some(h) => (community_signing_key_arc.clone(), h),
        None => (signing_key_arc.clone(), our_signing_device_hash),
    };
```

Then hand `(dm_sign_key_arc, dm_sign_hash)` to each transport that builds+signs DM packets. (`own_enrollment_cert` is moved into `DmOutbox::new` at 4720 — compute `our_device2_hash` BEFORE that move, or from the `own_enrollment_cert_for_friend`/`_for_device_intro` clones already taken at 4631/4637.)

- [ ] **Step 3: Attach the cert at the DM invite send site**

At `add_space_dm_inner` (lib.rs ~13402-13412) where `DmInviteSigned { ... inviter_identity_pub: *inviter_identity_pub ... }` is built and signed with `signing_key`: set `inviter_enrollment: Some(own_enrollment_cert.clone())` (thread the cert into `add_space_dm_inner` if not already available — it is the same `own_enrollment_cert`/`NodeState`-held cert), and sign with the #2 key + stamp the #2 hash (mirror `dm_signing_material`). If the invite is built via `DmOutbox::build_invite_packet_from_space` (Task 5 already handles it), route this send site through that builder instead of hand-constructing the packet, to avoid two divergent invite builders.

- [ ] **Step 4: Run the build + type check**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: PASS (no compile errors from the moved `own_enrollment_cert`).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-580 S1 Task 6: wire #2 DM signing material into transports + attach cert at invite send"
```

---

## Task 7: end-to-end #2 round-trip integration (ZEB-504 non-regression gate)

**Files:**
- Create: `src-tauri/tests/dm/dm_cert_identity_integration.rs` (or extend `dm_send_integration.rs`)

**Interfaces:**
- Consumes: the whole S1 stack (mint → friend handshake caches #2 → DM send signs #2 → receive verifies via cached #2).
- Produces: a passing two-owner DM round-trip proving #2 identity works end to end (the shape ZEB-504 validated cross-WAN).

- [ ] **Step 1: Write the failing integration test**

Mirror the existing `tests/dm/*` harness (grep an existing DM round-trip test for the mint + friend-handshake + send/receive scaffolding; reuse it — do NOT hand-roll node boot). Assert:

```rust
// 1. Two owners A, B mint real identities; A and B complete a friend handshake.
// 2. After the handshake, B's OwnerDeviceCache holds A's #2 DM identity
//    (device2_signing_hash(A_cert)); likewise A holds B's.
// 3. A sends a DM to B. The CidNotify B receives is signed by A's #2 and
//    B verifies it against the cached #2 combined pub (no #3 involved).
// 4. B receives + decrypts the message (delivery not regressed).
// 5. A DmInvite from A to B carries inviter_enrollment = Some(A #2 cert), and
//    B (with NO prior handshake — fresh state) accepts it via the cert path.
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test '*dm_cert_identity*'`
Expected: FAIL initially if any wiring gap remains; use it to shake out integration bugs.

- [ ] **Step 3: Fix any integration gaps surfaced**

Iterate until green. Common gaps: a transport still signing #3 (Task 5/6 enumeration miss); `resolve_sender_devices` returning a #3 hash for self (ensure the self device set contains the #2 hash — for single-device alpha, the fallback singleton is `dm_signing_material().1`).

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures --test '*dm_cert_identity*'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/dm/dm_cert_identity_integration.rs
git commit -m "ZEB-580 S1 Task 7: end-to-end #2 DM identity round-trip (ZEB-504 non-regression)"
```

---

## Task 8: sweep existing #3-pinning tests + full CI-parity gate

**Files:**
- Modify: `dm_signing.rs`, `dm_envelope.rs`, `dm_outbox.rs`, `owner_state_crdt.rs` test modules; any `tests/dm/*` that asserted #3.

**Interfaces:** none (test-only + gate).

- [ ] **Step 1: Identify #3-pinning tests**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb580-s1-run.txt`
Review failures. Expected touch points (from the spec §7 inventory): `dm_outbox_community_signing_key_and_enrollment_cert` (still valid — #2≠#3), the `dm_signing.rs` #3-equivalence pins (`sign_dm_packet_matches_private_identity_sign`, `derive_device_hash_equals_harmony_identity_address_hash` — KEEP, they pin the retained #3 path), `owner_state_crdt.rs` cache-ingest tests, and `tests/dm/*` round-trips.

- [ ] **Step 2: Update assertions**

For each failure, decide: (a) it pins a still-valid #3 fact → keep; (b) it asserted #3 signing where #2 now applies → update to #2, keeping a `new_synthetic` degrade-path test for #3. Do NOT delete a pin — retarget it. Add a #2 analogue where a #3 pin has no #2 counterpart.

- [ ] **Step 3: Frontend gate (unaffected, confirm)**

Run (repo root): `npx tsc --noEmit && npx vitest run`
Expected: PASS (S1 is backend-only; no frontend change until S2's honesty copy).

- [ ] **Step 4: Full CI-parity sweep**

```bash
cd src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "ZEB-580 S1 Task 8: retarget #3-pinning tests to #2 + full gate green"
```

---

## Self-review notes (for the executor)

- **Dual-path is asymmetric by design.** A #2-signed packet reaching a receiver that only cached #3 for that sender **drops** (lookup miss / sig-vs-#3 fail). That is the accepted hard-flag-day break (spec §4.7) — the cross-WAN gate (Task 7) runs on freshly-handshaked (#2-cached) peers.
- **`resolve_sender_devices` self-set.** For single-device alpha nodes the self device set is the singleton `dm_signing_material().1` (#2 hash). Multi-device #2 self-sets (siblings' #2 hashes in the own-owner cache) are a follow-up, not S1.
- **No new `DmReceiveError` variant is strictly required** — reuse `SignatureVerificationFailed` / `SigningKeyDoesNotMatchDeviceHash`. Add one only if a test needs to distinguish the cert-owner-mismatch reason.
- **Quorum-via-DmInvite-only degrades to legacy #3** (N3) — verified by the drop-path in Task 3 Step 3.
