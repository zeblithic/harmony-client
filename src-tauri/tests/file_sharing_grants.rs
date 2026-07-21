//! ZEB-674 Task 2 (C2): per-device seal fan-out + owner-local grant records.
//!
//! Exercises the sharing primitives that seal a file's DEK to each of a
//! grantee's bound devices (`seal_grant_for_devices`), plus the owner-local
//! `GrantEntry` records that back the "Shared with" list and their
//! persistence round-trip.
//!
//! No owner is minted and the keychain is never touched: X25519 device
//! keypairs are derived deterministically via HKDF and `KeyTree`-free, so the
//! ZEB-428 keychain-isolation rule is satisfied by avoidance (mirrors
//! `file_sharing_dek.rs`).

use harmony_app::dm_signing::{open_from_owner_with_info, DmSignError};
use harmony_app::file_sharing::{seal_grant_for_devices, FileGrantInner, FILE_GRANT_SEAL_INFO};
use harmony_app::owner_state_crypto::canonical_cbor_decode;

/// Deterministic test X25519 keypair (mirrors `butler_deposit`'s
/// `make_x25519_keypair`). Returns (priv_scalar, pub).
fn make_x25519_keypair(seed_byte: u8) -> ([u8; 32], [u8; 32]) {
    use hkdf::Hkdf;
    use sha2::Sha256;
    use x25519_dalek::{PublicKey, StaticSecret};

    let seed = [seed_byte; 32];
    let hk = Hkdf::<Sha256>::new(None, &seed);
    let mut scalar = [0u8; 32];
    hk.expand(b"harmony-zeb-674-test-x25519-scalar", &mut scalar)
        .expect("HKDF 32 bytes always works");

    let secret = StaticSecret::from(scalar);
    let public = PublicKey::from(&secret);
    (scalar, *public.as_bytes())
}

fn sample_inner() -> FileGrantInner {
    FileGrantInner {
        cid: [0xAB; 32],
        file_name: "quarterly-report.pdf".to_string(),
        file_size: 4096,
        mime: "application/pdf".to_string(),
        dek: [0xCD; 32],
    }
}

/// N known devices → N sealed blobs (one per device, in order). Each blob
/// opens ONLY with its own device's X25519 private key and parses back to the
/// exact `FileGrantInner`; opening device-0's blob with device-1's key fails
/// with `DecryptionFailed` (the seal is device-scoped, not owner-scoped).
#[test]
fn seal_fanout_one_per_known_device() {
    let (priv0, pub0) = make_x25519_keypair(0x10);
    let (priv1, pub1) = make_x25519_keypair(0x20);
    assert_ne!(pub0, pub1, "distinct device keys");

    let inner = sample_inner();
    let devices = [pub0, pub1];
    let blobs = seal_grant_for_devices(&inner, &devices).expect("fan-out seal");

    // (1) One sealed blob per device, in device order.
    assert_eq!(blobs.len(), 2, "one seal per known device");

    // (2) blob[0] opens with device-0's private key and parses to `inner`.
    let opened0 = open_from_owner_with_info(&priv0, &blobs[0], FILE_GRANT_SEAL_INFO)
        .expect("device-0 opens its own blob");
    let parsed0: FileGrantInner =
        canonical_cbor_decode(&opened0).expect("plaintext is canonical FileGrantInner CBOR");
    assert_eq!(parsed0, inner, "round-tripped grant inner matches");

    // (3) A foreign device (device-1) cannot open blob[0].
    let foreign = open_from_owner_with_info(&priv1, &blobs[0], FILE_GRANT_SEAL_INFO);
    assert!(
        matches!(foreign, Err(DmSignError::DecryptionFailed)),
        "device-1 must NOT open device-0's blob (got {foreign:?})"
    );

    // Sanity: blob[1] belongs to device-1.
    let opened1 = open_from_owner_with_info(&priv1, &blobs[1], FILE_GRANT_SEAL_INFO)
        .expect("device-1 opens its own blob");
    let parsed1: FileGrantInner = canonical_cbor_decode(&opened1).expect("device-1 blob parses");
    assert_eq!(parsed1, inner);
}

/// Owner-local grant records append and revoke on `OwnerState.file_grants` —
/// the data behind the "Shared with" list. Revoke TOMBSTONES (ZEB-725): the
/// entry is retained (for cross-device convergence) but hidden from the list.
#[test]
fn grant_record_append_revoke_tombstones() {
    use harmony_app::file_sharing::{list_grants_inner, record_grant, revoke_grant_inner};
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_types::OwnerAddr;

    let cid = [0x33u8; 32];
    let alice = OwnerAddr([1u8; 16]);
    let bob = OwnerAddr([2u8; 16]);

    let mut state = OwnerState::default();
    record_grant(&mut state, cid, alice, 100);
    record_grant(&mut state, cid, bob, 200);
    assert_eq!(list_grants_inner(&state, cid).len(), 2, "two active grants");

    // Revoke Alice: hidden from the list, but the entry is KEPT (tombstone).
    assert!(
        revoke_grant_inner(&mut state, cid, alice, 300),
        "Alice's active grant was revoked"
    );
    let listed: Vec<String> = list_grants_inner(&state, cid)
        .iter()
        .map(|g| g.grantee_address.clone())
        .collect();
    assert_eq!(
        listed,
        vec![hex::encode(bob.0)],
        "only Bob's active grant is listed after revoking Alice"
    );
    assert_eq!(
        state.file_grants[&cid].len(),
        2,
        "the revoked entry is retained (tombstone), not removed — needed for convergence"
    );
    assert!(
        state.file_grants[&cid]
            .iter()
            .any(|g| g.grantee_owner == alice && g.granted_at <= g.revoked_at),
        "Alice's entry is present and inactive (granted_at <= revoked_at)"
    );
}

/// Grant records survive a `save_crdt` → `load_crdt` cycle (persisted like
/// `file_deks`, via the `CrdtFileV2` field + both `From` impls).
#[test]
fn grant_records_persist_reload() {
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_persist::{load_crdt, save_crdt};
    use harmony_app::owner_state_types::{GrantEntry, OwnerAddr};

    let cid = [0x77u8; 32];
    let mut state = OwnerState::default();
    state.file_grants.insert(
        cid,
        vec![
            GrantEntry {
                grantee_owner: OwnerAddr([5u8; 16]),
                granted_at: 42,
                revoked_at: 0,
            },
            // A tombstoned (revoked) entry — `revoked_at` must survive the
            // round-trip so the revoke stays converged after a reload.
            GrantEntry {
                grantee_owner: OwnerAddr([6u8; 16]),
                granted_at: 43,
                revoked_at: 99,
            },
        ],
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("crdt-v2.bin");
    save_crdt(&path, &state).expect("save_crdt");
    let reloaded = load_crdt(&path).expect("load_crdt");

    assert_eq!(
        reloaded.file_grants.get(&cid),
        state.file_grants.get(&cid),
        "grant records survive save→reload"
    );
}
