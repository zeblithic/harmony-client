use crate::identity::KeychainStore;
use crate::owner_commands::OWNER_STATE_WRITE_LOCK;
use crate::pairing::state_machine::{InviterEnrollResult, JoinerEnrollResult};
use harmony_owner::trust::DEFAULT_ACTIVE_WINDOW_SECS;
use std::path::Path;

/// Persist the Joiner's signing key + OwnerState to disk. Mirrors the
/// atomicity contract from ZEB-170: keychain first, .cbor last.
///
/// On failure mid-write, the keychain entry may remain orphaned; subsequent
/// `load_owner_state` will treat the absence of `.cbor` as un-bound and
/// re-pairing will overwrite the keychain entry.
///
/// The Joiner has NO master_seed (cert-only model — see ZEB-197 design).
/// Passing `master_seed: None` ensures `save_owner_state_atomic` clears any
/// stale `master_seed.enc`/keychain entry from a previous identity, so
/// `load_owner_state` correctly reports `canBackUp: false` for this device.
///
/// Production callers go through this function, which acquires a real
/// `KeychainStore::new().ok()` — mirroring the mint path in
/// `owner_commands.rs`. PR #63 review found the prior `keychain: None`
/// caller forced the encrypted-file fallback on every desktop, silently
/// failing without `HARMONY_PASSPHRASE`.
///
/// Tests bypass this wrapper and call [`install_joiner_state_inner`]
/// directly with `keychain: None`, since the real keychain is a single
/// global shared resource and would leak state across `#[tokio::test]`
/// invocations even with per-test tempdirs.
pub fn install_joiner_state(identity_dir: &Path, result: JoinerEnrollResult) -> Result<(), String> {
    // `KeychainStore` is not `Clone`, so we acquire two handles: one consumed by
    // `save_owner_state_atomic` (owner state + device key), one borrowed by the
    // ZEB-492 fleet-keytree persist/clear that runs AFTER the atomic save.
    install_joiner_state_inner(
        identity_dir,
        result,
        KeychainStore::new().ok(),
        KeychainStore::new().ok(),
    )
}

/// Test seam for [`install_joiner_state`]: the public wrapper hard-codes
/// `KeychainStore::new().ok()` (the real OS keychain), but tests need to
/// pass `None` so they exercise the encrypted-file fallback under
/// `HARMONY_PASSPHRASE` without polluting the developer's actual keychain.
#[doc(hidden)]
pub fn install_joiner_state_inner(
    identity_dir: &Path,
    result: JoinerEnrollResult,
    keychain: Option<KeychainStore>,
    fleet_keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // ZEB-199: serialize against every other writer of owner_state.cbor +
    // its companion seed/key entries. Joiner does NOT merge — the result
    // represents a fresh identity binding (the device is establishing
    // membership in this Owner's device set), so clobbering any pre-existing
    // owner_state.cbor on this device is intentional and correct.
    let _guard = OWNER_STATE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    // ZEB-492 (Qodo/CodeAnt round 1, FIX D): persist the fleet KeyTree FIRST,
    // BEFORE the owner_state + device-key commit. The earlier order (owner_state
    // first, fleet second) left a partial commit on a fleet-persist failure: the
    // device booted "paired but with no fleet material." With the fleet block
    // first:
    //   - a fleet-persist failure aborts BEFORE any owner_state.cbor / device-key
    //     write, leaving the device unchanged (clean abort); and
    //   - an owner_state failure after the fleet write leaves at most a harmless
    //     orphan `fleet_keytree.enc` — with no owner_state.cbor, `load_owner_state`
    //     returns `None`, so the orphan is ignored and overwritten on the next
    //     successful install.
    // The inviter sealed this so the cert-only device can build the fleet engines
    // + act as a butler on next boot. When the inviter sent NO material
    // (pre-ZEB-492 inviter), defensively CLEAR any stale `fleet_keytree.enc`/vault
    // slot from a prior identity so it can't masquerade as this device's KeyTree
    // (mirrors save_owner_state_atomic's master_seed == None clear). Runs inside
    // the held OWNER_STATE_WRITE_LOCK, same as the owner_state write below.
    match result.fleet_keytree.as_ref() {
        Some(materials) => {
            // ZEB-668 S5: the slot persists the multi-epoch set encoding —
            // a pre-bump handover carries one epoch, a post-bump one two
            // (epoch-0 + current).
            let buf = crate::owner_state_crypto::encode_fleet_material_set(materials)
                .map_err(|e| format!("encode fleet keytree for persist: {e}"))?;
            crate::owner_state::save_fleet_keytree(&fleet_keychain, identity_dir, &buf)?;
        }
        None => {
            crate::owner_state::clear_fleet_keytree(&fleet_keychain, identity_dir)?;
        }
    }

    crate::owner_state::save_owner_state_atomic(
        identity_dir,
        &result.owner_state,
        &result.our_signing_key,
        None, // no master_seed on Joiner — clears any stale prior seed
        keychain,
    )?;
    Ok(())
}

/// Apply a freshly-signed enrollment cert to the Inviter's on-disk
/// `OwnerState` and write the result back atomically.
///
/// **Merge-on-load semantics (ZEB-199):** unlike the Joiner path, the
/// Inviter already has its own signing key on disk (from the original
/// mint) and may have other enrollments that the SM doesn't know about
/// (e.g., a parallel pairing flow that completed and persisted while this
/// SM was running). We:
///
/// 1. Acquire `OWNER_STATE_WRITE_LOCK` — serializes against every other
///    writer of `owner_state.cbor` (mint, other pairing-Completes).
/// 2. Load the freshest on-disk state.
/// 3. Apply the new cert via `add_enrollment` to the loaded state — NOT
///    to the SM's snapshot. This is the load-bearing difference: if we
///    wrote the SM's snapshot directly, any enrollment that landed
///    between the SM's snapshot and our write would be silently
///    overwritten.
/// 4. Save the merged state via `save_owner_state_atomic`.
///
/// Errors if no existing owner state is on disk: an Inviter that never
/// minted should never have reached `Complete`, so this is an inconsistent
/// state that surfaces as a hard failure.
///
/// Production wrapper: each call site (load + save) acquires a fresh
/// `KeychainStore::new().ok()`. `KeychainStore` is not `Clone`, so we can't
/// reuse a single instance across both calls.
pub fn install_inviter_state(
    identity_dir: &Path,
    result: InviterEnrollResult,
) -> Result<(), String> {
    install_inviter_state_inner(
        identity_dir,
        result,
        KeychainStore::new().ok(),
        KeychainStore::new().ok(),
    )
}

/// Test seam for [`install_inviter_state`]. See
/// [`install_joiner_state_inner`] for the rationale.
#[doc(hidden)]
pub fn install_inviter_state_inner(
    identity_dir: &Path,
    result: InviterEnrollResult,
    load_keychain: Option<KeychainStore>,
    save_keychain: Option<KeychainStore>,
) -> Result<(), String> {
    // ZEB-199: hold the lock across the entire load+merge+save window.
    // Without this, two writers could both observe the pre-mutation
    // OwnerState, each apply only their own cert, and one writer's
    // enrollment is silently lost when the second save overwrites the
    // first. See module-level comment in owner_commands.rs.
    let _guard = OWNER_STATE_WRITE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());

    let mut loaded = crate::owner_state::load_owner_state(identity_dir, load_keychain)?
        .ok_or_else(|| "no existing owner state to update".to_string())?;

    // Merge-on-load: apply the cert to the FRESHEST disk state, not the
    // SM's possibly-stale snapshot. `add_enrollment` re-verifies the cert
    // signature against the loaded state's owner_id, so a cert for a
    // different owner (e.g., from a stale drainer queue across a wipe)
    // surfaces as a hard error rather than corrupting the new identity.
    loaded
        .state
        .add_enrollment(result.cert, result.now, DEFAULT_ACTIVE_WINDOW_SECS)
        .map_err(|e| format!("apply enrollment to loaded state: {e}"))?;

    crate::owner_state::save_owner_state_atomic(
        identity_dir,
        &loaded.state,
        &loaded.device_signing_key,    // the EXISTING signing key
        result.master_seed.as_deref(), // Some: seed-holder keeps master; None: seedless quorum inviter stays cert-only
        save_keychain,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::state_machine::JoinerEnrollResult;
    use ed25519_dalek::SigningKey;
    use harmony_owner::lifecycle::{mint_owner, MintResult};
    use harmony_owner::pubkey_bundle::PubKeyBundle;
    use rand::rngs::OsRng;
    use serial_test::serial;
    use tempfile::tempdir;

    /// Reuse the EnvVarGuard pattern (and replace once ZEB-193 lands).
    struct EnvVarGuard {
        name: &'static str,
    }
    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            std::env::set_var(name, value);
            Self { name }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.name);
        }
    }

    #[tokio::test]
    #[serial]
    async fn install_writes_owner_state_cbor() {
        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
        let dir = tempdir().unwrap();

        let MintResult { state, .. } = mint_owner(1_700_000_000).unwrap();
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let joiner_id = joiner_pubkey.identity_hash();

        let result = JoinerEnrollResult {
            our_signing_key: joiner_sk,
            owner_state: state,
            our_device_id: joiner_id,
            fleet_keytree: None,
        };
        // Use the inner variant with `keychain: None` so the encrypted-file
        // fallback under `HARMONY_PASSPHRASE` is exercised — the production
        // wrapper would write to the developer's actual OS keychain.
        install_joiner_state_inner(dir.path(), result, None, None).unwrap();

        let cbor_path = dir.path().join("owner_state.cbor");
        assert!(cbor_path.exists(), "OwnerState cbor written");

        // The Joiner's master_seed.enc must NOT exist (cert-only model).
        let master_path = dir.path().join("master_seed.enc");
        assert!(
            !master_path.exists(),
            "master_seed must not exist on Joiner"
        );

        // The Joiner's device_sk.enc MUST exist (signing key persisted via fallback).
        let device_path = dir.path().join("device_sk.enc");
        assert!(device_path.exists(), "device_sk.enc written");

        // ZEB-492: no fleet material was sent, so no fleet_keytree.enc.
        assert!(
            !dir.path().join("fleet_keytree.enc").exists(),
            "fleet_keytree.enc must not exist when no material was sent"
        );
    }

    /// ZEB-492: a joiner that received the inviter's sealed fleet KeyTree must
    /// persist it on install, and a subsequent `load_fleet_keytree` must
    /// reconstruct a KeyTree byte-identical to the inviter's — proven by
    /// decrypting an entry the inviter wrote under the same KeyTree.
    #[tokio::test]
    #[serial]
    async fn install_joiner_persists_fleet_keytree_and_decrypts_inviter_entry() {
        use crate::owner_state_crypto::{decrypt_entry, encrypt_entry, space_lookup_key, KeyTree};

        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "zeb492-persist-test");
        let dir = tempdir().unwrap();

        // Inviter side: derive a KeyTree from a seed, write an entry under it,
        // and export the material the way the inviter seals it at pairing.
        let seed = [0x5Au8; 32];
        let kt = KeyTree::derive(&seed).unwrap();
        let lk = space_lookup_key(&kt, b"dm-inbox-v1");
        let ciphertext = encrypt_entry(&kt, &lk, b"butler payload").unwrap();
        let material = kt.to_fleet_material();

        // Build a JoinerEnrollResult carrying the material + persist it (keychain
        // None → encrypted-file fallback under HARMONY_PASSPHRASE).
        let MintResult { state, .. } = mint_owner(1_700_000_000).unwrap();
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_id =
            PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes()).identity_hash();
        let result = JoinerEnrollResult {
            our_signing_key: joiner_sk,
            owner_state: state,
            our_device_id: joiner_id,
            fleet_keytree: Some(vec![material]),
        };
        install_joiner_state_inner(dir.path(), result, None, None).unwrap();

        // The encrypted file must be on disk.
        assert!(
            dir.path().join("fleet_keytree.enc").exists(),
            "fleet_keytree.enc written on install"
        );

        // Reload the material, rebuild the KeyTree, and confirm it decrypts the
        // inviter's entry — i.e. the joiner holds the IDENTICAL fleet keys.
        let loaded = crate::owner_state::load_fleet_keytree(&None, dir.path())
            .unwrap()
            .expect("fleet keytree present after install");
        // ZEB-668 S5: the slot persists the multi-epoch set encoding; a
        // pairing handover installs a 1-set.
        let set = crate::owner_state_crypto::decode_fleet_material_set(loaded.as_slice()).unwrap();
        assert_eq!(set.len(), 1, "pairing handover installs exactly one epoch");
        let kt2 = KeyTree::from_fleet_material(&set[0]).unwrap();
        let lk2 = space_lookup_key(&kt2, b"dm-inbox-v1");
        assert_eq!(
            decrypt_entry(&kt2, &lk2, &ciphertext).unwrap(),
            b"butler payload",
            "reloaded fleet KeyTree decrypts the inviter's entry"
        );
    }

    /// ZEB-492 stale-masquerade guard: a re-pairing that carries NO fleet
    /// material must ACTIVELY REMOVE any `fleet_keytree.enc` a prior identity
    /// left in the same identity_dir — otherwise the stale file would
    /// masquerade as this device's KeyTree on next boot. This exercises the
    /// `clear_fleet_keytree` path (the no-material install branch), which the
    /// empty-dir test does not reach.
    #[tokio::test]
    #[serial]
    async fn reinstall_without_material_clears_stale_fleet_keytree() {
        use crate::owner_state_crypto::KeyTree;

        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "zeb492-clear-test");
        let dir = tempdir().unwrap();

        let mk_result = |fleet_keytree| {
            let MintResult { state, .. } = mint_owner(1_700_000_000).unwrap();
            let joiner_sk = SigningKey::generate(&mut OsRng);
            let joiner_id =
                PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes()).identity_hash();
            JoinerEnrollResult {
                our_signing_key: joiner_sk,
                owner_state: state,
                our_device_id: joiner_id,
                fleet_keytree,
            }
        };

        // First install: WITH material → fleet_keytree.enc lands on disk.
        let material = KeyTree::derive(&[0x11u8; 32]).unwrap().to_fleet_material();
        install_joiner_state_inner(dir.path(), mk_result(Some(vec![material])), None, None)
            .unwrap();
        assert!(
            dir.path().join("fleet_keytree.enc").exists(),
            "precondition: fleet_keytree.enc written by the WITH-material install"
        );

        // Second install into the SAME dir: WITHOUT material → the stale file
        // must be cleared, and load must report None.
        install_joiner_state_inner(dir.path(), mk_result(None), None, None).unwrap();
        assert!(
            !dir.path().join("fleet_keytree.enc").exists(),
            "stale fleet_keytree.enc must be removed by the no-material install"
        );
        assert!(
            crate::owner_state::load_fleet_keytree(&None, dir.path())
                .unwrap()
                .is_none(),
            "load_fleet_keytree must return None after the stale file is cleared"
        );
    }

    /// Inviter-side persistence (single-writer happy path): simulates the
    /// Inviter having already minted (so device_sk.enc + master_seed.enc +
    /// owner_state.cbor exist on disk), then a pairing handshake completes
    /// and produces an `InviterEnrollResult` carrying the freshly-signed
    /// cert. After `install_inviter_state`, the on-disk `.cbor` MUST
    /// reflect the new enrollment AND preserve the original mint device's
    /// enrollment, and the existing signing key + master seed MUST remain
    /// in place.
    #[tokio::test]
    #[serial]
    async fn install_inviter_writes_updated_owner_state() {
        use crate::owner_state::{load_owner_state, save_owner_state_atomic};
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        use zeroize::Zeroizing;

        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
        let dir = tempdir().unwrap();

        // Simulate a freshly-minted Inviter: original state, signing key,
        // and master seed all on disk.
        let MintResult {
            state: original_state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed_bytes: [u8; 32] = *recovery_artifact.as_bytes();
        save_owner_state_atomic(
            dir.path(),
            &original_state,
            &device_signing_key,
            Some(&master_seed_bytes),
            None,
        )
        .unwrap();
        let original_signing_bytes = device_signing_key.to_bytes();
        let original_device_id = *original_state
            .enrollments
            .keys()
            .next()
            .expect("mint produces one enrollment for the originating device");

        // Sign a fresh joiner enrollment cert against the original state.
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let now = 1_700_000_001;
        let master_seed_for_sign = Zeroizing::new(master_seed_bytes);
        let cert = crate::pairing::cert::sign_enrollment_for_joiner(
            &master_seed_for_sign,
            &original_state,
            joiner_pubkey.clone(),
            now,
        )
        .unwrap();
        let new_device_id = joiner_pubkey.identity_hash();

        // ZEB-199: InviterEnrollResult carries the cert + signing-time `now`,
        // and persist applies it on top of the freshest disk state.
        let result = InviterEnrollResult {
            cert,
            now,
            master_seed: Some(Zeroizing::new(master_seed_bytes)),
        };
        // Use the inner variant — see install_writes_owner_state_cbor for why.
        install_inviter_state_inner(dir.path(), result, None, None).unwrap();

        // Reload from disk and assert BOTH the original mint device AND the
        // new joiner enrollment are present (merge-on-load preserved both).
        let reloaded = load_owner_state(dir.path(), None)
            .unwrap()
            .expect("loaded state");
        assert!(
            reloaded.state.enrollments.contains_key(&original_device_id),
            "merge-on-load preserved the original mint enrollment"
        );
        assert!(
            reloaded.state.enrollments.contains_key(&new_device_id),
            "persisted state contains the new joiner enrollment"
        );
        // Signing key on disk MUST be unchanged.
        assert_eq!(
            reloaded.device_signing_key.to_bytes(),
            original_signing_bytes,
            "Inviter's signing key preserved across persist"
        );
        // Master seed on disk MUST be preserved (Inviter keeps master).
        let reloaded_seed = reloaded.master_seed.expect("master seed preserved");
        assert_eq!(
            *reloaded_seed, master_seed_bytes,
            "Inviter's master seed preserved across persist"
        );
    }

    /// ZEB-199 invariant: two threads each calling `install_inviter_state_inner`
    /// with a DIFFERENT cert must both end up persisted on disk. Without
    /// the lock + merge-on-load, the second writer's `save_owner_state_atomic`
    /// would silently overwrite the first writer's enrollment because the
    /// SM's snapshot doesn't know about the other writer's cert.
    ///
    /// The threads synchronize on a `Barrier` so both enter the load → mutate
    /// → save window concurrently — without the lock, the race window is
    /// the full `load_owner_state` → `save_owner_state_atomic` interval,
    /// which is plenty wide enough for both writers to load the pre-mutation
    /// state and produce conflicting writes. The `Barrier` makes the race
    /// deterministic regardless of executor scheduling.
    #[tokio::test]
    #[serial]
    async fn concurrent_inviter_persists_both_enrollments() {
        use crate::owner_state::{load_owner_state, save_owner_state_atomic};
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        use std::sync::{Arc, Barrier};
        use std::thread;
        use zeroize::Zeroizing;

        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
        let dir = tempdir().unwrap();

        let MintResult {
            state: original_state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed_bytes: [u8; 32] = *recovery_artifact.as_bytes();
        save_owner_state_atomic(
            dir.path(),
            &original_state,
            &device_signing_key,
            Some(&master_seed_bytes),
            None,
        )
        .unwrap();

        // Sign two distinct enrollment certs against the same original
        // state — modeling two SMs (or one SM signing two pair flows) that
        // each see the disk state pre-merge and produce a cert based on it.
        let master_seed_for_sign = Zeroizing::new(master_seed_bytes);
        let mk_cert = |joiner_sk: &SigningKey, now: u64| {
            let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
            let cert = crate::pairing::cert::sign_enrollment_for_joiner(
                &master_seed_for_sign,
                &original_state,
                joiner_pubkey.clone(),
                now,
            )
            .unwrap();
            (joiner_pubkey.identity_hash(), cert)
        };
        let joiner_a_sk = SigningKey::generate(&mut OsRng);
        let joiner_b_sk = SigningKey::generate(&mut OsRng);
        let (joiner_a_id, cert_a) = mk_cert(&joiner_a_sk, 1_700_000_001);
        let (joiner_b_id, cert_b) = mk_cert(&joiner_b_sk, 1_700_000_002);
        assert_ne!(
            joiner_a_id, joiner_b_id,
            "preconditions: distinct joiner identities"
        );

        let result_a = InviterEnrollResult {
            cert: cert_a,
            now: 1_700_000_001,
            master_seed: Some(Zeroizing::new(master_seed_bytes)),
        };
        let result_b = InviterEnrollResult {
            cert: cert_b,
            now: 1_700_000_002,
            master_seed: Some(Zeroizing::new(master_seed_bytes)),
        };

        // Synchronize the two threads so they enter install_inviter_state_inner
        // at the same instant — without the lock, both would call
        // load_owner_state at ~the same time, each producing a
        // pre-mutation state and racing to save.
        let barrier = Arc::new(Barrier::new(2));
        let dir_a = dir.path().to_path_buf();
        let dir_b = dir.path().to_path_buf();
        let barrier_a = barrier.clone();
        let handle_a = thread::spawn(move || {
            barrier_a.wait();
            install_inviter_state_inner(&dir_a, result_a, None, None)
        });
        let handle_b = thread::spawn(move || {
            barrier.wait();
            install_inviter_state_inner(&dir_b, result_b, None, None)
        });
        handle_a.join().unwrap().expect("thread A persist");
        handle_b.join().unwrap().expect("thread B persist");

        // The lock + merge-on-load invariant: BOTH joiner enrollments are
        // on disk. Without the lock, exactly one would be missing (the
        // loser of the second-write race).
        let reloaded = load_owner_state(dir.path(), None).unwrap().expect("loaded");
        assert!(
            reloaded.state.enrollments.contains_key(&joiner_a_id),
            "joiner A enrollment must survive concurrent persist (got keys: {:?})",
            reloaded.state.enrollments.keys().collect::<Vec<_>>()
        );
        assert!(
            reloaded.state.enrollments.contains_key(&joiner_b_id),
            "joiner B enrollment must survive concurrent persist (got keys: {:?})",
            reloaded.state.enrollments.keys().collect::<Vec<_>>()
        );
    }

    /// Calling `install_inviter_state` against a directory with no existing
    /// owner state must fail rather than silently create one — an Inviter
    /// that never minted should never reach `Complete`.
    #[tokio::test]
    #[serial]
    async fn install_inviter_errors_when_no_existing_state() {
        use harmony_owner::pubkey_bundle::PubKeyBundle;
        use zeroize::Zeroizing;

        let _guard = EnvVarGuard::set("HARMONY_PASSPHRASE", "test-pp");
        let dir = tempdir().unwrap();

        // Build a valid cert against a freshly-minted state. The cert
        // never gets applied — install_inviter_state_inner errors at the
        // load step before reaching add_enrollment — but constructing
        // the cert requires master_seed and state to match (owner_id
        // check inside sign_enrollment_for_joiner).
        let MintResult {
            state,
            recovery_artifact,
            ..
        } = mint_owner(1_700_000_000).unwrap();
        let master_seed_bytes: [u8; 32] = *recovery_artifact.as_bytes();
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let joiner_pubkey = PubKeyBundle::classical_only(joiner_sk.verifying_key().to_bytes());
        let cert = crate::pairing::cert::sign_enrollment_for_joiner(
            &Zeroizing::new(master_seed_bytes),
            &state,
            joiner_pubkey,
            1_700_000_001,
        )
        .unwrap();
        let result = InviterEnrollResult {
            cert,
            now: 1_700_000_001,
            master_seed: Some(Zeroizing::new(master_seed_bytes)),
        };
        let err =
            install_inviter_state_inner(dir.path(), result, None, None).expect_err("must error");
        assert!(
            err.contains("no existing owner state"),
            "expected 'no existing owner state' error, got: {err}"
        );
    }
}
