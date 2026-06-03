//! ZEB-350 Voice V2 / ZEB-352 Voice V4 wire-format pins. Locks the sealed
//! voice-packet framing (and the signed+sealed presence beacon). A drift here
//! means the on-the-wire format changed — bump the version domain and re-pin
//! deliberately, never silently.
#![cfg(feature = "test-fixtures")]

use ed25519_dalek::SigningKey;
use harmony_app::community_channel_log::{derive_channel_key, derive_dm_voice_key};
use harmony_app::community_membership::ChannelId;
use harmony_app::dm_signing::{
    derive_device_hash_from_identity_pub, sign_dm_packet, verify_dm_packet_signature,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{
    DeviceIdentityHash, DmContentKey, EpochKey, Hlc, OwnerAddr, SpaceId,
};
use harmony_app::voice_crypto::{
    encrypt_dm_voice_packet_with_nonce, encrypt_voice_packet_with_nonce, VOICE_DM_PACKET_AAD,
    VOICE_PACKET_AAD,
};
use harmony_app::voice_moderation::{
    seal_directive_with_nonce, sign_directive, ModAction, VoiceModerationDirective,
};
use harmony_app::voice_presence::{
    open_presence_beacon, seal_presence_beacon_with_nonce, sign_presence_beacon,
    SignedVoicePresenceBeacon, VoicePresenceBeacon,
};
use harmony_app::voice_signal::{SignedVoiceSignal, VoiceSignal, VoiceSignalKind};

#[test]
fn voice_packet_wire_bytes_pinned() {
    let key = derive_channel_key(
        &EpochKey::new([0x11; 32]),
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
    );
    // 23-byte header (flags|seq|ts|senderHash) + a short opus payload, all zeros
    // except markers — the relay seals the whole frame opaquely.
    let frame: Vec<u8> = (0u8..30).collect();
    let sealed = encrypt_voice_packet_with_nonce(
        &key,
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
        VOICE_PACKET_AAD,
        &frame,
        [0u8; 12],
    )
    .expect("seal");
    let actual = hex::encode(&sealed);
    let expected = "000000000000000000000000ac5b18940b0bae8a9581f7fe741e457ecacd15abbe96c2fe579c73777fc6b4542adb6477613e68651bd4237c586c";
    assert_eq!(actual, expected, "sealed voice-packet wire format drifted");
}

/// Fully deterministic signed+sealed presence beacon: fixed device-#2 key
/// `[7u8; 32]`, fixed `joined_hlc`, `seq = 1`, sealed with a zeroed nonce.
fn fixture_signed_beacon() -> SignedVoicePresenceBeacon {
    let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let beacon = VoicePresenceBeacon {
        owner: [0xa1; 16],
        device: sk.verifying_key().to_bytes(),
        muted: true,
        joined_hlc: Hlc {
            wall_ms: 1000,
            logical: 0,
            device_id: "aa".repeat(32),
        },
        seq: 1,
        left: false,
    };
    sign_presence_beacon(beacon, &sk).expect("sign")
}

#[test]
fn presence_beacon_wire_bytes_pinned() {
    let key = derive_channel_key(
        &EpochKey::new([0x11; 32]),
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
    );
    let signed = fixture_signed_beacon();
    let sealed = seal_presence_beacon_with_nonce(
        &key,
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
        &signed,
        [0u8; 12],
    )
    .expect("seal");
    let actual = hex::encode(&sealed);
    let expected = "0000000000000000000000000e3878f4aa6cc7facd295c54d9b2ead07b7da6190b227548eee70d1a3bfb1a62432a5f6b46b27fa0a1a1cf8ea5ff27cf90893c2046dcbd473b2f4a6e69cc3094336f6cd54117eb973e6c9e1c18c23fccdff02a868b284897577a96163281e6eddc67c2a47da57b2b938ef9bbbdadc266f939bf5e5a54614313b1d70941bb27261068d5070606039a01f2c5308184bddf60ea0684e9db40867cc001209223b99ba529ab5980077bbd429679a05471ea51da8cdf57754623a2f68c60093d5750ea241e18297557bcd5ab8464431dfac8173db8a49d2fc414d4022287148b61d9f4297c5353c37b9deefe2910";
    assert_eq!(
        actual, expected,
        "sealed presence-beacon wire format drifted"
    );

    // Back-compat decode: the pinned bytes must still open to the expected
    // beacon (guards against silent struct/field drift).
    let opened = open_presence_beacon(&key, &SpaceId([0xc0; 16]), &ChannelId([0xc1; 16]), &sealed)
        .expect("open pinned beacon");
    assert_eq!(opened, signed, "pinned beacon decoded to a different value");
}

/// ZEB-360: pin the sealed group-DM presence beacon wire format. Wrapped with a
/// `call_id` and sealed under the call-independent `derive_groupdm_presence_key`,
/// scoped to a `space_id`.
#[test]
fn groupdm_presence_beacon_wire_bytes_pinned() {
    use harmony_app::community_channel_log::derive_groupdm_presence_key;
    use harmony_app::voice_presence::{
        open_groupdm_presence_beacon, seal_groupdm_presence_beacon_with_nonce,
        GroupSignedPresenceBeacon,
    };
    let key = derive_groupdm_presence_key(&DmContentKey::new([0x11; 32]));
    let space = SpaceId([0x44; 16]);
    let wrapped = GroupSignedPresenceBeacon {
        call_id: [0x22; 16],
        signed: fixture_signed_beacon(),
    };
    let sealed =
        seal_groupdm_presence_beacon_with_nonce(&key, &space, &wrapped, [0u8; 12]).expect("seal");
    assert_eq!(
        hex::encode(&sealed),
        "0000000000000000000000000f3487b70aeee9f5cef22160d2b0ad5a0979b3eb01cc5e76501622e7497d8551e46b97eeec02cdf8f11fed60b3bbd6a08428d6e304ec9c5ab627a1bbcf1e16856caa769b096446a4df4c2b49d1fa740942b17173f9149d0c21c59d1596a4c09d8c09899efc9122ec6375c78526a51c0a8ca29e6414d29550b12bc0162d6267a039904a0f42d11dac6e76466d665f27100817f9076fd132e60c2c10edb256d68e4629c897486d113933657ee7af357132dc6e531d3180aed9fd27eb537844b25330d44b9976210e168c7f9b04185c7481fde745cb31881463a929d039c1af0f6e527bc04eb35efa23afac8ab0ca86073e53ab5e68a53b7f49dfd36f57e94fc77d4a4b08",
        "group-DM presence-beacon wire format drifted"
    );
    // Round-trip back-compat: pinned bytes must reopen to the same wrapped beacon.
    let opened = open_groupdm_presence_beacon(&key, &space, &sealed).expect("open pinned beacon");
    assert_eq!(
        opened, wrapped,
        "pinned group beacon decoded to a different value"
    );
}

/// Pin the DM voice-packet wire format. A drift here means the AEAD
/// framing or AAD construction changed — re-pin deliberately.
#[test]
fn dm_voice_packet_wire_bytes_pinned() {
    let k = derive_dm_voice_key(&DmContentKey::new([0x11; 32]), &[0x22u8; 16]);
    let frame: Vec<u8> = (0u8..30).collect();
    let sealed = encrypt_dm_voice_packet_with_nonce(
        &k,
        &[0x22u8; 16],
        VOICE_DM_PACKET_AAD,
        &frame,
        [0u8; 12],
    )
    .expect("seal DM voice packet");
    assert_eq!(
        hex::encode(&sealed),
        "000000000000000000000000cd09efe51676e64ad943d44c33fbb6725e68212e87f719879d5f5b1147459b32d7e0bb54f49c35ae7499d9c6a27e",
        "DM voice-packet wire format drifted"
    );
}

/// Build the caller identity (SigningKey + identity_pub + DeviceIdentityHash)
/// from a fixed seed byte, using the same HKDF derivation as
/// `PrivateIdentity::from_seed` in harmony-identity.
///
/// This is bit-identical to the `make_caller_identity` helper in
/// `voice_signal.rs`'s tests; replicated here so the integration test has no
/// coupling to private test helpers.
fn fixture_caller_identity(seed_byte: u8) -> (SigningKey, [u8; 64], DeviceIdentityHash) {
    use hkdf::Hkdf;
    use sha2::Sha256;
    let seed = [seed_byte; 32];
    let private = harmony_identity::PrivateIdentity::from_seed(&seed);
    let public = private.public_identity();
    let identity_pub = public.to_public_bytes();
    let device_hash = DeviceIdentityHash(public.address_hash);

    // Derive the signing key via HKDF — bit-identical to PrivateIdentity::sign.
    let hk = Hkdf::<Sha256>::new(None, &seed);
    let mut ed_arr = [0u8; 32];
    hk.expand(b"harmony-identity-ed25519-v1", &mut ed_arr)
        .expect("HKDF 32 bytes always succeeds");
    let signing_key = SigningKey::from_bytes(&ed_arr);

    (signing_key, identity_pub, device_hash)
}

/// Pin the signed inner CBOR of a voice-signal Invite. Because the envelope
/// uses an ephemeral X25519 key it is non-deterministic; instead we pin the
/// `SignedVoiceSignal` CBOR directly — that is fully deterministic for a
/// fixed signing key + body. A drift here means either the CBOR field-key
/// renaming or the Ed25519 signing path changed.
///
/// The pinned bytes are also verified end-to-end with
/// `verify_dm_packet_signature`, so the fixture is meaningful rather than an
/// opaque byte blob.
#[test]
fn voice_signal_invite_signed_inner_wire_bytes_pinned() {
    // Build a deterministic signing identity from seed 0x42.
    let (signing_key, identity_pub, device_hash) = fixture_caller_identity(0x42);

    // Construct the VoiceSignal body.
    let body = VoiceSignal {
        kind: VoiceSignalKind::Invite,
        call_id: [0x22; 16],
        caller: OwnerAddr([0x33; 16]),
        space_id: None,
        decline_reason: None,
    };

    // Canonical-CBOR encode the body (replicate voice_signal::canonical_cbor inline).
    let mut body_bytes = Vec::new();
    ciborium::into_writer(&body, &mut body_bytes)
        .expect("CBOR serialize VoiceSignal: infallible for well-typed values");

    // Sign with device-#2 key.
    let sig = sign_dm_packet(&body_bytes, &signing_key);

    // Build SignedVoiceSignal and encode as canonical CBOR.
    let signed = SignedVoiceSignal { body, sig };
    let mut signed_bytes = Vec::new();
    ciborium::into_writer(&signed, &mut signed_bytes)
        .expect("CBOR serialize SignedVoiceSignal: infallible for well-typed values");

    // Assert the pinned hex value.
    assert_eq!(
        hex::encode(&signed_bytes),
        "a2626264a3616b66696e76697465626369502222222222222222222222222222222262636c5033333333333333333333333333333333627367584054b358000dd6c00fe0614b13bcf707df035a330f1031d93b5423d56124748b55c35f831c13dcc16440c53be125be52df9acb80d01c9c17a4225d6cde2cb6a10f",
        "SignedVoiceSignal signed-inner CBOR wire format drifted"
    );

    // Verify round-trip: the pinned bytes must verify with the matching
    // identity_pub + device_hash, proving the fixture is self-consistent.
    verify_dm_packet_signature(&body_bytes, &sig, &identity_pub, device_hash)
        .expect("SignedVoiceSignal: signature must verify with matching identity");

    // Verify the device_hash round-trips through derive_device_hash_from_identity_pub.
    let derived_hash =
        derive_device_hash_from_identity_pub(&identity_pub).expect("identity_pub must be valid");
    assert_eq!(
        derived_hash, device_hash,
        "device_hash must match derived value"
    );
}

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
        "a2626264a4616b66696e76697465626369502222222222222222222222222222222262636c50333333333333333333333333333333336273695044444444444444444444444444444444627367584007d34024a63e25c951df586b6e179a9e5fdf3b4cdb47ae5fdf9819b0ba24cb30d360ab2c88e499f5cbc0c3b2f87f4611cacf761d269ef0b2453abeedbf53f10c",
        "GroupDm SignedVoiceSignal signed-inner CBOR wire format drifted"
    );
    verify_dm_packet_signature(&body_bytes, &sig, &identity_pub, device_hash)
        .expect("group Invite signature must verify");
}

// ---------------------------------------------------------------------------
// ZEB-358 — voice moderation directive wire-format pins
// ---------------------------------------------------------------------------

/// Fully deterministic unsigned directive: fixed actor/target bytes, fixed HLC,
/// seq 3. All fields are compile-time constants so the CBOR is stable.
fn fixture_directive() -> VoiceModerationDirective {
    VoiceModerationDirective {
        actor_owner: [0xA1; 16],
        actor_device: [0xD2; 32],
        target_owner: [0xB3; 16],
        action: ModAction::Mute,
        issued_hlc: Hlc {
            wall_ms: 42,
            logical: 7,
            device_id: "fixture-dev".into(),
        },
        seq: 3,
    }
}

/// Pin the canonical-CBOR encoding of an unsigned `VoiceModerationDirective`.
/// A drift here means a field rename (serde rename attr), a field added/removed,
/// or the CBOR codec changed. Re-pin deliberately — never silently.
#[test]
fn voice_moderation_directive_canonical_cbor_is_pinned() {
    let bytes = canonical_cbor_encode(&fixture_directive()).unwrap();
    assert_eq!(
        hex::encode(&bytes),
        "a662616f50a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a16261645820d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d262746f50b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b362616300626968a36177182a616c0761646b666978747572652d64657662737103",
        "VoiceModerationDirective canonical-CBOR wire format drifted"
    );
}

/// Pin the sealed (signed + ChannelKey-encrypted) directive wire format.
/// Signing key is `[7u8; 32]`, nonce is `[9u8; 12]`, channel key/community/channel
/// match the existing presence-beacon fixture exactly.
///
/// Unlike the unsigned-CBOR pin above (which keeps the synthetic `[0xD2; 32]`
/// actor_device to lock serialization), this sealed fixture sets
/// `actor_device = verifying_key([7u8; 32])` so it is a VALID wire example: the
/// embedded device key matches the signer, so the packet passes
/// `verify_directive_sig` (CodeRabbit Minor).
#[test]
fn voice_moderation_sealed_directive_is_pinned() {
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let mut directive = fixture_directive();
    directive.actor_device = signing.verifying_key().to_bytes();
    let signed = sign_directive(directive, &signing).expect("sign");
    let key = derive_channel_key(
        &EpochKey::new([0x11; 32]),
        &SpaceId([0xc0; 16]),
        &ChannelId([0xc1; 16]),
    );
    let community = SpaceId([0xc0; 16]);
    let channel = ChannelId([0xc1; 16]);
    let sealed =
        seal_directive_with_nonce(&key, &community, &channel, &signed, [9u8; 12]).expect("seal");
    assert_eq!(
        hex::encode(&sealed),
        "090909090909090909090909f76ed9e6ed6f16b7af2d4cdeba79cd9b2b52875c6c8dd9f55e6a6d3821f4eeac9766925d2973eb0c91014bd56686fe5976fe41136c9daba59323e9d50551fa78f3b765450ad1ebcb42c289f12efb521c3095d4204761a6235672205ac88454a330218cf5e02f8710576af956a769a71b5e1b2657eb6af19d43c5f0f7b0f6f9a4dec3298f7901d7979496c8a3f5583bbc5af6476b76332a0dc6b2e2fe15062fa509a2c1e8066233fa4ef0398626296a5d10440184ad8b9a19f02d566de18e35546864bc95e6788401",
        "sealed VoiceModerationDirective wire format drifted"
    );
}
