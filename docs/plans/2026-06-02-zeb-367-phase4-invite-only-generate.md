# ZEB-367 — Phase 4 invite-only `generate_invite` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship invite-only invite generation (mint a signed `InviteToken`, seal the epoch key, attach the admin bootstrap, publish the case-A pkarr record) so the iroh cross-WAN redeem can run end-to-end — closing the ZEB-366 generate↔redeem gap.

**Architecture:** A new pure-Rust `invite_mint` module holds the three mint primitives (token signing, epoch-key sealing, admin-bootstrap extraction), unit-tested without a node. The `generate_invite` Tauri command gains an invite-only branch that orchestrates them. The redeem side gains a one-line untargeted-decrypt branch. Case-A publish already auto-fires; unregister-on-consume is wired at the two countersign-acceptance points. Two invite models: untargeted (ephemeral key in URL, built first) and targeted (sealed to a specific invitee, built second).

**Tech Stack:** Rust, Tauri v2 commands, `ed25519_dalek`, X25519 sealing (`dm_signing`), `ciborium` CBOR, `cargo nextest`.

**Spec:** `docs/specs/2026-06-02-phase4-invite-only-generate-design.md`. Run Rust tests from `src-tauri/`: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(NAME)'`.

---

## File structure

- **Create** `src-tauri/src/invite_mint.rs` — the three mint primitives + their unit tests. One responsibility: turn community/epoch/identity inputs into the signed, sealed pieces of an invite-only `CommunityInvitePayload`. No Tauri/NodeState deps.
- **Modify** `src-tauri/src/community_invite.rs` — add the `untargeted_decrypt_key` field to `CommunityInvitePayload`; extend `encode_invite_url`/`decode_invite_url` guards; add the `unregister`-on-consume call inside `handle_unicast`.
- **Modify** `src-tauri/src/lib.rs` — register the `invite_mint` module; replace the `generate_invite` invite-only stub (lib.rs:14873) with the real branch; add the untargeted decrypt branch in `mint_redemption` (lib.rs:16409); thread `PkarrInvitePublisher` into `handle_unicast` calls.
- **Modify** `src-tauri/src/iroh_invite_acceptor.rs` — add a `pkarr_invite_publisher` field + unregister call after countersign.
- **Modify** `src-tauri/tests/pkarr_iroh_redeem_full_integration.rs` — add the invite-only generate→redeem end-to-end test (acceptance test).

---

## Task 0: Branch

**Files:** none (git only)

- [ ] **Step 1: Create the implementation branch off the specs branch**

```bash
git checkout phase4-cross-wan-specs
git pull --ff-only
git checkout -b zeb-367-phase4-invite-only-generate
```
Expected: on `zeb-367-phase4-invite-only-generate` with both spec docs present.

---

## Task 1: `invite_mint::mint_invite_token`

**Files:**
- Create: `src-tauri/src/invite_mint.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod invite_mint;`)
- Test: in `invite_mint.rs` `#[cfg(test)] mod tests`

- [ ] **Step 1: Register the module**

In `src-tauri/src/lib.rs`, next to the other `mod` declarations (search for `mod community_invite;`), add:
```rust
mod invite_mint;
```

- [ ] **Step 2: Write the failing test**

Create `src-tauri/src/invite_mint.rs`:
```rust
//! ZEB-367 Phase 4: invite-only invite mint primitives.
//!
//! Pure functions (no Tauri/NodeState) that produce the signed + sealed pieces
//! of an invite-only `CommunityInvitePayload`. The verify/redeem counterparts
//! already live in `community_invite`; this is the mint side.

use crate::community_invite::{canonical_invite_token_bytes, InviteToken};
use crate::owner_state_types::{Hlc, OwnerAddr};

/// Mint + sign an `InviteToken` with the enrolled device-#2 key (ZEB-339).
/// The sig commits to (inviter, invitee_hint?, minted_at, expires_at?) via
/// `canonical_invite_token_bytes`.
pub fn mint_invite_token(
    inviter: OwnerAddr,
    invitee_hint: Option<OwnerAddr>,
    minted_at: Hlc,
    expires_at: Option<u64>,
    device2_signing_key: &ed25519_dalek::SigningKey,
) -> Result<InviteToken, String> {
    let mut token = InviteToken { inviter, invitee_hint, minted_at, expires_at, sig: [0u8; 64] };
    let bytes = canonical_invite_token_bytes(&token)
        .map_err(|e| format!("canonical_invite_token_bytes: {e}"))?;
    use ed25519_dalek::Signer;
    token.sig = device2_signing_key.sign(&bytes).to_bytes();
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_invite::verify_invite_token_sig_device_key;
    use crate::owner_state_types::OwnerAddr;

    fn hlc() -> Hlc { Hlc { wall_ms: 1_000, logical: 0, device_id: "dev2".to_string() } }

    #[test]
    fn minted_token_verifies_against_device_key() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let token = mint_invite_token(OwnerAddr([1u8; 16]), None, hlc(), Some(2_000), &sk).unwrap();
        verify_invite_token_sig_device_key(&token, &sk.verifying_key().to_bytes()).unwrap();
    }

    #[test]
    fn tampered_token_fails_verification() {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut token = mint_invite_token(OwnerAddr([1u8; 16]), None, hlc(), None, &sk).unwrap();
        token.expires_at = Some(999_999); // not covered by the now-stale sig
        assert!(verify_invite_token_sig_device_key(&token, &sk.verifying_key().to_bytes()).is_err());
    }
}
```

> If `verify_invite_token_sig_device_key`'s second argument isn't `&[u8; 32]`, read its exact signature at `community_invite.rs:1617` and match it (it verifies the token sig against the enrolled device's ed25519 verifying-key bytes).

- [ ] **Step 3: Run to verify it fails (module not yet compiling / fn missing)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(minted_token_verifies)'`
Expected: FAIL (compile error: `verify_invite_token_sig_device_key` import path, or assertion). Fix imports until it compiles and the test runs.

- [ ] **Step 4: Run to verify both pass**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(invite_mint)'`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/invite_mint.rs src-tauri/src/lib.rs
git commit -m "ZEB-367: invite_mint::mint_invite_token (device-#2-signed InviteToken)"
```

---

## Task 2: `invite_mint::seal_epoch_key`

**Files:**
- Modify: `src-tauri/src/invite_mint.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

Append to `invite_mint.rs` (above `#[cfg(test)]`):
```rust
use crate::dm_signing::{open_from_owner, seal_to_owner};

/// Who the epoch key is sealed to.
pub enum SealRecipient {
    /// Sealed to a specific invitee's device-#2 X25519 public key (confidential).
    Targeted([u8; 32]),
    /// Sealed to a fresh ephemeral key whose private half ships in the URL
    /// (single-use "controlled open" link).
    Untargeted,
}

pub struct SealedEpochKey {
    /// 92-byte X25519 envelope (32 ephemeral_pub || 12 nonce || 32 ct || 16 tag).
    pub sealed: Vec<u8>,
    /// Untargeted only: the ephemeral X25519 private key the redeemer uses.
    pub untargeted_decrypt_key: Option<[u8; 32]>,
}

pub fn seal_epoch_key(
    epoch_key: &[u8; 32],
    recipient: SealRecipient,
) -> Result<SealedEpochKey, String> {
    match recipient {
        SealRecipient::Targeted(pub_) => {
            let sealed = seal_to_owner(&pub_, epoch_key).map_err(|e| format!("seal_to_owner: {e}"))?;
            Ok(SealedEpochKey { sealed, untargeted_decrypt_key: None })
        }
        SealRecipient::Untargeted => {
            let ephemeral_priv = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
            let ephemeral_pub = x25519_dalek::PublicKey::from(&ephemeral_priv);
            let sealed = seal_to_owner(ephemeral_pub.as_bytes(), epoch_key)
                .map_err(|e| format!("seal_to_owner: {e}"))?;
            Ok(SealedEpochKey { sealed, untargeted_decrypt_key: Some(ephemeral_priv.to_bytes()) })
        }
    }
}
```
Add to the `mod tests`:
```rust
    #[test]
    fn targeted_seal_round_trips() {
        let priv_ = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let pub_ = x25519_dalek::PublicKey::from(&priv_);
        let epoch = [9u8; 32];
        let out = seal_epoch_key(&epoch, SealRecipient::Targeted(*pub_.as_bytes())).unwrap();
        assert!(out.untargeted_decrypt_key.is_none());
        let opened = open_from_owner(&priv_.to_bytes(), &out.sealed).unwrap();
        assert_eq!(opened.as_slice(), &epoch);
    }

    #[test]
    fn untargeted_seal_round_trips_via_url_key() {
        let epoch = [3u8; 32];
        let out = seal_epoch_key(&epoch, SealRecipient::Untargeted).unwrap();
        let key = out.untargeted_decrypt_key.expect("untargeted returns a key");
        let opened = open_from_owner(&key, &out.sealed).unwrap();
        assert_eq!(opened.as_slice(), &epoch);
    }
```

> Confirm `x25519_dalek` + `rand` are already deps of `harmony-app` (search `Cargo.toml`); `dm_signing` uses them, so they are. If `StaticSecret::random_from_rng` differs by version, use `dm_signing`'s own ephemeral-keygen pattern (read `seal_to_owner` body at dm_signing.rs:66).

- [ ] **Step 2: Run to verify it fails, then implement is already inline → run to pass**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(seal)'`
Expected: PASS (2 tests). If the x25519 API mismatches, fix to match `dm_signing`'s usage and re-run.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/invite_mint.rs
git commit -m "ZEB-367: invite_mint::seal_epoch_key (targeted + untargeted)"
```

---

## Task 3: `invite_mint::extract_admin_bootstrap`

**Files:**
- Modify: `src-tauri/src/invite_mint.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

Append to `invite_mint.rs`:
```rust
use crate::community_membership::{MembershipEventKind, SignedMembershipEvent};
use crate::owner_state_types::SpaceId;

#[derive(Debug)]
pub enum InviteMintError { NoAdminBootstrap }
impl std::fmt::Display for InviteMintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::NoAdminBootstrap => write!(f, "admin bootstrap Join not found in community log") }
    }
}

/// Find the admin's bootstrap self-Join (kind=Join, actor=admin, no countersig,
/// carries an enrollment cert) in a community's event set. This is what the
/// redeemer pre-inserts so its empty CRDT can verify the admin's publish-back.
pub fn extract_admin_bootstrap(
    events: &[SignedMembershipEvent],
    community_id: SpaceId,
    admin_addr: OwnerAddr,
) -> Result<SignedMembershipEvent, InviteMintError> {
    events
        .iter()
        .find(|e| {
            e.actor == admin_addr
                && e.community_id == community_id
                && matches!(e.kind, MembershipEventKind::Join)
                && e.countersig.is_none()
                && e.enrollment.is_some()
        })
        .cloned()
        .ok_or(InviteMintError::NoAdminBootstrap)
}
```
Add to `mod tests`:
```rust
    #[test]
    fn extracts_admin_join_with_enrollment() {
        let admin = OwnerAddr([0x6Eu8; 16]);
        let cid = SpaceId([0x11u8; 16]);
        let owner = crate::community_membership::mint_test_owner(0x6E);
        let admin_join = SignedMembershipEvent {
            id: [1u8; 16], community_id: cid, kind: MembershipEventKind::Join, actor: admin,
            at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
            sig: [0u8; 64], countersig: None, enrollment: Some(owner.cert),
        };
        let other = SignedMembershipEvent { actor: OwnerAddr([2u8; 16]), enrollment: None, ..admin_join.clone() };
        let got = extract_admin_bootstrap(&[other, admin_join.clone()], cid, admin).unwrap();
        assert_eq!(got.actor, admin);
        assert!(got.enrollment.is_some());
    }

    #[test]
    fn missing_admin_bootstrap_errors() {
        let r = extract_admin_bootstrap(&[], SpaceId([0u8; 16]), OwnerAddr([0u8; 16]));
        assert!(matches!(r, Err(InviteMintError::NoAdminBootstrap)));
    }
```

> `mint_test_owner(u8) -> {cert, ...}` is the test-fixture helper (community_membership). Confirm its return shape at its definition; `.cert` is the `EnrollmentCert`. The `SignedMembershipEvent` field names are verbatim from community_membership.rs:380.

- [ ] **Step 2: Run to verify pass**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(admin_bootstrap)'`
Expected: PASS (2 tests).

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/invite_mint.rs
git commit -m "ZEB-367: invite_mint::extract_admin_bootstrap"
```

---

## Task 4: `untargeted_decrypt_key` wire field + guards

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (struct + encode/decode guards)
- Test: `community_invite.rs` `#[cfg(test)] mod tests`

- [ ] **Step 1: Add the field**

In `CommunityInvitePayload` (community_invite.rs:89), after `inviter_enrollment`:
```rust
    /// ZEB-367 untargeted invite-only only: the ephemeral X25519 private key the
    /// redeemer uses to open `sealed_epoch_key`. Rides ONLY in the URL — never in
    /// the case-A pkarr record (which publishes routing keyed by token.sig) and
    /// OUTSIDE the token-sig preimage. `None` for targeted + open invites.
    #[serde(rename = "ud", skip_serializing_if = "Option::is_none", default,
        serialize_with = "serialize_bytes_as_bstr_opt",
        deserialize_with = "deserialize_bytes_from_bstr_opt")]
    pub untargeted_decrypt_key: Option<[u8; 32]>,
```
> Reuse the existing optional-bstr serde helpers if present; otherwise add `serialize_bytes_as_bstr_opt`/`deserialize_bytes_from_bstr_opt` mirroring `serialize_admin_identity_pub_as_bstr` (community_invite.rs:627) but for `Option<[u8;32]>`. Update every other `CommunityInvitePayload { .. }` literal in the file (the test fixtures + `build_open_invite_url`) to set `untargeted_decrypt_key: None` — `cargo build` will list them.

- [ ] **Step 2: Write the failing guard test**

In `community_invite.rs` `mod tests`, add:
```rust
    #[test]
    fn untargeted_key_rejected_on_open_payload() {
        let mut p = make_open_payload_correct();
        p.untargeted_decrypt_key = Some([1u8; 32]);
        assert!(encode_invite_url(&p).is_err());
    }

    #[test]
    fn untargeted_key_rejected_on_targeted_invite_only() {
        let mut p = make_invite_only_payload_correct();
        if let Some(t) = p.invite_token.as_mut() { t.invitee_hint = Some(OwnerAddr([5u8; 16])); }
        p.untargeted_decrypt_key = Some([1u8; 32]);
        assert!(encode_invite_url(&p).is_err());
    }

    #[test]
    fn untargeted_key_round_trips_on_untargeted_invite_only() {
        let mut p = make_invite_only_payload_correct(); // invitee_hint = None
        p.untargeted_decrypt_key = Some([7u8; 32]);
        let url = encode_invite_url(&p).unwrap();
        let back = decode_invite_url(&url).unwrap();
        assert_eq!(back.untargeted_decrypt_key, Some([7u8; 32]));
    }
```

- [ ] **Step 3: Add the guard in `encode_invite_url` and `decode_invite_url`**

In `encode_invite_url` (community_invite.rs:831), alongside the existing invite-only/open guards, add:
```rust
    if payload.untargeted_decrypt_key.is_some() {
        let untargeted_ok = payload.is_invite_only
            && payload.invite_token.as_ref().is_some_and(|t| t.invitee_hint.is_none());
        if !untargeted_ok {
            return Err(InviteUrlError::UntargetedKeyNotAllowed);
        }
    }
```
Add the variant to `InviteUrlError` (the error enum near community_invite.rs:199):
```rust
    #[error("untargeted_decrypt_key is only valid on an untargeted invite-only payload")]
    UntargetedKeyNotAllowed,
```
Mirror the same check in `decode_invite_url` (community_invite.rs:892) after deserialization.

- [ ] **Step 4: Run to pass**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(untargeted_key)'`
Expected: PASS (3 tests). Plus `cargo build` clean (all payload literals updated).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_invite.rs
git commit -m "ZEB-367: CommunityInvitePayload.untargeted_decrypt_key + encode/decode guards"
```

---

## Task 5: `mint_redemption` untargeted decrypt branch

**Files:**
- Modify: `src-tauri/src/lib.rs` (mint_redemption decrypt, ~16409-16447)
- Test: the integration test in Task 9 covers the happy path; add a focused unit test if `mint_redemption` is callable in isolation (it is `pub(crate)` per the iroh inner caller).

- [ ] **Step 1: Modify the decrypt branch**

In `mint_redemption` (lib.rs:16409), replace the invite-only decrypt key derivation:
```rust
        use crate::dm_signing::{ed25519_priv_to_x25519, open_from_owner};
        let x25519_priv: [u8; 32] = match payload.untargeted_decrypt_key {
            // Untargeted: the ephemeral private key rode in the URL.
            Some(ephemeral_priv) => ephemeral_priv,
            // Targeted: the invitee's enrolled device-#2 X25519 key.
            None => *ed25519_priv_to_x25519(signing_key),
        };
        let plaintext = open_from_owner(&x25519_priv, &payload.epoch_snapshot.sealed_epoch_key)
            .map_err(|e| format!("invite-only epoch key decryption failed: {e}"))?;
        plaintext.as_slice().try_into().map_err(|_| "decrypted epoch key wrong length".to_string())?
```
(`ed25519_priv_to_x25519` returns `Zeroizing<[u8;32]>`; deref-copy with `*` into the array.)

- [ ] **Step 2: Build**

Run: `cargo build --locked -p harmony-app --features test-fixtures`
Expected: clean. (Behavior is exercised end-to-end in Task 9.)

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-367: mint_redemption decrypts untargeted invites via the URL ephemeral key"
```

---

## Task 6: `generate_invite` invite-only branch — UNTARGETED

**Files:**
- Modify: `src-tauri/src/lib.rs` (`generate_invite`, replace the stub at 14873)

- [ ] **Step 1: Snapshot the extra NodeState handles**

In `generate_invite`'s NodeState snapshot (lib.rs:14827), add `hlc_tracker`, mirroring `create_community` (lib.rs:15571). The block becomes (additions in **bold** intent):
```rust
let (crdt_state, community_registry, dm_outbox, hlc_tracker) = {
    let g = state_lock.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
    (
        g.crdt_state.clone().ok_or("node not started")?,
        g.community_registry.clone().ok_or("no community registry")?,
        g.dm_outbox.clone().ok_or("no dm outbox")?,
        g.hlc_tracker.clone().ok_or("no hlc tracker")?,
    )
};
```

- [ ] **Step 2: Replace the stub with the invite-only branch**

Replace lib.rs:14873-14878 (`if is_invite_only { return Err("...Phase 4") }`) with the branch below. It runs only the untargeted path in this task; the targeted recipient is added in Task 7.
```rust
if is_invite_only {
    // --- gather inviter identity (device-#2 signing key + reticulum identity) ---
    let (self_owner, self_private_identity, community_signing_key, device_id) = {
        let o = dm_outbox.lock().await;
        (o.self_owner, std::sync::Arc::clone(&o.private_identity),
         std::sync::Arc::clone(&o.community_signing_key), o.device_id.clone())
    };
    let admin_identity_pub: [u8; 64] = self_private_identity.identity.to_public_bytes();

    // --- v1: only the admin may generate invite-only invites ---
    if self_owner != admin {
        return Err("only the admin can generate invite-only invites (v1)".to_string());
    }

    // --- power gate (admin always passes; explicit for future non-admin support) ---
    let wall_now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let mat = crate::community_membership::materialize_with_now(&events, admin, Some(wall_now_ms));
    let self_power = mat.power_levels.get(&self_owner).copied().unwrap_or(0);
    if self_power < crate::community_membership::POWER_THRESHOLDS.invite {
        return Err(format!("caller power {self_power} below invite threshold"));
    }

    // --- seal the epoch key (UNTARGETED in this task) ---
    let sealed = crate::invite_mint::seal_epoch_key(
        mk.as_bytes(),
        crate::invite_mint::SealRecipient::Untargeted,
    )?;

    // --- mint the token (invitee_hint = None for untargeted) ---
    let minted_at = crate::dm_outbox::reserve_next_hlc_for_device(&hlc_tracker, &device_id, wall_now_ms).await;
    let effective_expiry = expires_at.or(Some(wall_now_ms + 7 * 24 * 60 * 60 * 1000)); // 7-day default
    let token = crate::invite_mint::mint_invite_token(
        self_owner, None, minted_at, effective_expiry, &community_signing_key,
    )?;

    // --- extract admin bootstrap from the community log ---
    let admin_bootstrap = crate::invite_mint::extract_admin_bootstrap(&events, space_id, admin)
        .map_err(|e| e.to_string())?;

    // --- build the invite-only payload ---
    let epoch_snapshot = crate::community_invite::InviteEpochSnapshot {
        epoch,
        sealed_epoch_key: sealed.sealed,
        state_snapshot, // built above, same as the open path
    };
    let payload = crate::community_invite::CommunityInvitePayload {
        community_id: space_id,
        epoch_snapshot,
        admin_addr: admin,
        community_name: space.name.clone(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(token),
        admin_bootstrap: Some(admin_bootstrap),
        admin_identity_pub: Some(admin_identity_pub),
        forked_from,
        pre_fork_snapshot,
        inviter_enrollment: Some(inviter_enrollment_cert),
        untargeted_decrypt_key: sealed.untargeted_decrypt_key,
    };

    // --- encode FIRST so we never publish a case-A record for a URL that fails
    //     to encode; only register on a successful encode. ---
    let url = crate::community_invite::encode_invite_url(&payload)
        .map_err(|e| format!("encode invite url: {e}"))?;
    // --- case-A pkarr publish (fires because invite_token is Some) ---
    {
        let inv_pub = state_lock.lock().ok().and_then(|g| g.pkarr_invite_publisher.clone());
        match inv_pub {
            Some(inv_pub) => inv_pub.register_invite(&payload).await,
            // An invite-only invite is un-redeemable cross-WAN without its case-A
            // publication — fail at the mint site rather than return a broken URL.
            None => return Err("invite-only invite could not be published for \
                                cross-WAN (case-A) discovery".to_string()),
        }
    }
    return Ok(url);
}
```
> `events`, `state_snapshot`, `forked_from`, `pre_fork_snapshot`, `space`, `admin`, `epoch`, `space_id`, `mk`, `inviter_enrollment_cert` are all already in scope from the open-branch prelude (lib.rs:14827-15033). Reuse them; do NOT recompute. Confirm `materialize_with_now` and the `events` Vec are built before this branch — if the open path builds `state_snapshot`/`events` *after* the stub, hoist that computation above this `if`.

- [ ] **Step 3: Build**

Run: `cargo build --locked -p harmony-app --features test-fixtures`
Expected: clean. (`generate_invite` is a Tauri command; behavior is validated in Task 9's integration test, which calls the same primitives.)

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-367: generate_invite invite-only branch (untargeted) + 7-day default expiry"
```

---

## Task 7: `generate_invite` — TARGETED recipient (+ the device-cache lookup)

> **DEFERRED to ZEB-369 — NOT implemented in this PR.** Targeted invites seal the
> epoch key to the invitee's enrolled **device-#2** X25519 key, but that key is not
> resolvable from `OwnerDeviceCache`, which stores the identity/**#3** key, not
> device-#2. Implementing Steps 1–2 as written would silently mint invites the
> invitee can never decrypt. Instead, `generate_invite` REJECTS `invitee_hint =
> Some(_)` up front (see `invite_only_generation_guard` in lib.rs). The steps below
> are retained verbatim as the ZEB-369 starting point, not as work for this PR.

**Files:**
- Modify: `src-tauri/src/lib.rs` (`generate_invite`)

- [ ] **Step 1: Establish the invitee-X25519 lookup (contained known-unknown)**

The targeted path seals to the invitee's enrolled **device-#2 X25519** pub, derived from their device-#2 **ed25519** verifying key via `crate::dm_signing::ed25519_pub_to_x25519` (used in the integration tests). Locate how an `OwnerAddr` → device-#2 ed25519 pub is resolved: the `OwnerDeviceCache` (used by `dm_outbox::resolve_destinations`, dm_outbox.rs:75) holds per-device records; confirm its method that returns a device's ed25519 pub for an owner (grep `OwnerDeviceCache` in `src-tauri/src/`). If the invitee's device isn't cached, the resolution returns nothing.

- [ ] **Step 2: Implement targeted selection**

Change `generate_invite`'s signature to accept the invitee owner address for targeted invites — reuse the existing `invitee_hint: Option<String>` parameter (currently discarded). When `invitee_hint` is `Some(addr_hex)`, the invite is **targeted**; when `None`, **untargeted** (Task 6). Replace the unconditional `SealRecipient::Untargeted` with:
```rust
let (recipient, token_invitee_hint) = match &invitee_hint {
    Some(hex) => {
        let invitee = parse_owner_addr(hex)?; // existing hex→OwnerAddr helper (grep parse/decode OwnerAddr)
        let device_ed25519 = resolve_invitee_device2_ed25519(&crdt_state, &community_registry, invitee)
            .await
            .ok_or_else(|| format!("can't target {hex}: their devices aren't known yet — use an untargeted link"))?;
        let x25519 = crate::dm_signing::ed25519_pub_to_x25519(&device_ed25519);
        (crate::invite_mint::SealRecipient::Targeted(x25519), Some(invitee))
    }
    None => (crate::invite_mint::SealRecipient::Untargeted, None),
};
let sealed = crate::invite_mint::seal_epoch_key(mk.as_bytes(), recipient)?;
let token = crate::invite_mint::mint_invite_token(
    self_owner, token_invitee_hint, minted_at, effective_expiry, &community_signing_key,
)?;
```
Implement `resolve_invitee_device2_ed25519` as a small private async helper next to `generate_invite` using the `OwnerDeviceCache` method confirmed in Step 1 (it returns the device-#2 ed25519 verifying-key bytes, or `None`). `parse_owner_addr` is the existing hex→`OwnerAddr` decoder used by other IPCs (grep `OwnerAddr` + `hex::decode` in lib.rs).

- [ ] **Step 3: Build + confirm targeted path compiles**

Run: `cargo build --locked -p harmony-app --features test-fixtures`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-367: generate_invite targeted invites (seal to invitee device-2 X25519)"
```

---

## Task 8: Unregister-on-consume (case-A publication teardown)

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (`handle_unicast` — add publisher param + unregister)
- Modify: `src-tauri/src/iroh_invite_acceptor.rs` (add publisher field + thread it)
- Modify: `src-tauri/src/lib.rs` (pass the publisher at the two `handle_unicast` call sites + acceptor construction)

- [ ] **Step 1: Add the publisher parameter to `handle_unicast`**

In `community_invite.rs:1686`, add a parameter:
```rust
pub async fn handle_unicast<H: AppHandleEmit>(
    community_registry: &std::sync::Arc<crate::community_state_sync::CommunitySyncRegistry>,
    dm_outbox: &std::sync::Arc<tokio::sync::Mutex<crate::dm_outbox::DmOutbox>>,
    _crdt_state: &std::sync::Arc<tokio::sync::Mutex<crate::owner_state_crdt::OwnerState>>,
    packet_bytes: Vec<u8>,
    app: Option<&H>,
    pkarr_invite_publisher: Option<&std::sync::Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
) -> Result<(), CommunityInviteVerifyError>
```
After the successful insert (the `Ok(Inserted)` arms at ~1859/1923), before returning `Ok(())`:
```rust
    if let Some(pubr) = pkarr_invite_publisher {
        pubr.unregister_invite(&signed.invite_token.sig).await;
    }
```
(`signed` is the decoded `CommunityInviteSigned`; its `invite_token: InviteToken` carries `sig: [u8;64]`. Confirm the field path `signed.invite_token.sig` against the `CommunityInviteSigned` struct.)

- [ ] **Step 2: Thread it through the iroh acceptor**

In `iroh_invite_acceptor.rs:191`, add a field to `IrohInviteHandshakeAcceptor`:
```rust
    pkarr_invite_publisher: Option<std::sync::Arc<crate::pkarr_invite_publisher::PkarrInvitePublisher>>,
```
Set it in the constructor (add a param), and pass `self.pkarr_invite_publisher.as_ref()` into the `handle_unicast` call at iroh_invite_acceptor.rs:329.

- [ ] **Step 3: Update all call sites in lib.rs**

`cargo build` will flag every `handle_unicast(...)` call and the acceptor constructor. At each, pass the publisher from `NodeState.pkarr_invite_publisher` (the Reticulum inbound dispatch path in lib.rs, and the acceptor construction near lib.rs:2562). For call sites with no node context (tests), pass `None`.

- [ ] **Step 4: Write the failing test**

In `community_invite.rs` `mod tests`, add a test that builds a stub `PkarrInvitePublisher` (or asserts via a tracked flag) confirming `unregister_invite` is invoked on a successful `handle_unicast`. If `PkarrInvitePublisher` can't be constructed in a unit test (it needs a `PkarrPublisher`), assert this at the **integration** layer instead: in Task 9, after the redeem, assert a second redeem of the same token finds no case-A record (publisher state). Mark this step done by adding that assertion to Task 9.

- [ ] **Step 5: Build + test**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(handle_unicast)'`
Expected: PASS (existing `handle_unicast` tests still green with the new `None` arg threaded).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_invite.rs src-tauri/src/iroh_invite_acceptor.rs src-tauri/src/lib.rs
git commit -m "ZEB-367: unregister case-A pkarr publication on invite consumption"
```

---

## Task 9: End-to-end acceptance test (generate→redeem, both kinds)

**Files:**
- Modify: `src-tauri/tests/pkarr_iroh_redeem_full_integration.rs`

- [ ] **Step 1: Write the test, modeled on `bob_joins_alice_via_iroh_handshake_option_a`**

Add `invite_only_untargeted_generate_then_redeem_roundtrip` (and a targeted variant). Reuse that test's two-party setup, but build Alice's invite via the new `invite_mint` primitives instead of a hand-rolled token:
```rust
// Untargeted: seal to ephemeral; token invitee_hint=None.
let sealed = harmony_app::invite_mint::seal_epoch_key(epoch_key.as_bytes(), harmony_app::invite_mint::SealRecipient::Untargeted).unwrap();
let token = harmony_app::invite_mint::mint_invite_token(alice_owner, None, hlc, Some(expiry), &alice_comm_sk).unwrap();
let admin_bootstrap = harmony_app::invite_mint::extract_admin_bootstrap(&alice_events, alice_cid, alice_owner).unwrap();
let payload = CommunityInvitePayload { /* is_invite_only: true, invite_token: Some(token), admin_bootstrap: Some(admin_bootstrap), admin_identity_pub: Some(alice_id_pub), untargeted_decrypt_key: sealed.untargeted_decrypt_key, ... */ };
// Bob redeems via connectivity_redeem_invite_iroh_inner(...) — assert status == "joined" and the community materializes on Bob.
```
Assertions: redeem returns `status == "joined"`; the community appears in Bob's engine with Alice as admin; the epoch key decrypts (a channel/member is visible). Add the **unregister** assertion from Task 8 Step 4: a second `connectivity_redeem_invite_iroh_inner` with the same token no longer resolves the inviter (case-A unregistered). _(The **targeted** variant — seal to Bob's device-#2 X25519, `invitee_hint = Some(bob_owner)` — is **deferred to ZEB-369** along with Task 7; only the untargeted roundtrip ships in this PR.)_

- [ ] **Step 2: Run the integration test**

Run: `cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(invite_only_untargeted_generate_then_redeem)'`
Expected: PASS. Then the targeted variant. These are the spec's acceptance tests — the first time the iroh redeem runs against a generated invite-only invite end-to-end.

- [ ] **Step 3: Full gate + commit**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo fmt --all -- --check
git add src-tauri/tests/pkarr_iroh_redeem_full_integration.rs
git commit -m "ZEB-367: end-to-end invite-only generate->redeem integration test (targeted + untargeted)"
```

---

## Self-review (author)

- **Spec coverage:** `mint_invite_token` (T1), `seal_epoch_key` targeted+untargeted (T2), `extract_admin_bootstrap` (T3), `untargeted_decrypt_key` wire field + guards (T4), redeem untargeted branch (T5), generate invite-only untargeted (T6) + targeted (T7), unregister-on-consume (T8), case-A auto-publish (T6, via existing `register_invite`), 7-day default expiry (T6), admin-only v1 (T6), acceptance tests (T9). All spec sections map to a task.
- **Type consistency:** `SealRecipient`/`SealedEpochKey`/`seal_epoch_key` used identically in T2/T6/T7/T9; `untargeted_decrypt_key: Option<[u8;32]>` consistent across T4/T5/T6/T9; `extract_admin_bootstrap(&[SignedMembershipEvent], SpaceId, OwnerAddr)` consistent T3/T6.
- **Known-unknowns (contained, not placeholders):** (a) `verify_invite_token_sig_device_key`'s exact 2nd-arg type — confirm at community_invite.rs:1617 (T1). (b) The `OwnerDeviceCache` method for invitee device-#2 ed25519 resolution — confirm in T7 Step 1 (isolated to the targeted path, which ships after the self-contained untargeted path). (c) The `CommunityInviteSigned.invite_token.sig` field path — confirm in T8 Step 1. Each is a single signature lookup with a named source, not an open design question.
