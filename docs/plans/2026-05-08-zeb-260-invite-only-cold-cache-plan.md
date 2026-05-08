# ZEB-260 Phase 4 Invite-Only Cold-Cache Bootstrap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Plumb the admin's signed bootstrap event through the invite URL so the new joiner's empty CRDT admits the admin's publish-back, fixing end-to-end Phase 4 invite-only redemption.

**Architecture:** Two new fields on `CommunityInvitePayload` (`admin_bootstrap` + `admin_identity_pub`); a six-step pure verification helper in `community_invite.rs`; a single insert call (`engine.insert_local_event_with_pubs`) in `redeem_invite_inner` between `spawn_engine` and the unicast send. No changes to the membership-at-HLC gate, the publish-back wire format, or the encrypted-blob pipeline.

**Tech Stack:** Rust 1.x, Tauri v2, ed25519-dalek, ciborium (canonical CBOR), tokio (async runtime), harmony_identity (Ed25519 + X25519 identity model).

**Spec:** `docs/specs/2026-05-08-zeb-260-invite-only-cold-cache-design.md` (commit `da3f8a8`).

**Branch:** `zeb-260-invite-only-cold-cache` (cut from `origin/main` at `7d32256` — the ZEB-262 Phase 4 ship commit).

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src-tauri/src/community_invite.rs` | Wire types + verify helpers + URL codec | **Modify** — add `admin_bootstrap` + `admin_identity_pub` fields to `CommunityInvitePayload`; add `verify_admin_bootstrap` pure helper + `RedeemBootstrapVerifyError` enum |
| `src-tauri/src/community_state_sync.rs` | CRDT engine + state-root publish gate | **Modify** — single comment update inside `handle_incoming_publish` (the existing "Bootstrap caveat" comment) |
| `src-tauri/src/lib.rs` | IPC layer + `redeem_invite_inner` | **Modify** — call `verify_admin_bootstrap` + `engine.insert_local_event_with_pubs` after `spawn_engine` and before the unicast send (invite-only branch) |
| `src-tauri/tests/community_invite_unit.rs` | Pure unit tests for `community_invite.rs` | **Modify** — 9 new unit tests for verify-chain branches + 1 happy-path test |
| `src-tauri/tests/community_invite_only_integration.rs` | Two-engine cross-publish round-trip | **Modify** — populate new fields in the test's `CommunityInvitePayload` construction; remove the ZEB-260 OOB pre-seed; add post-redeem CRDT-event-count assertion; add tampering test |
| `src-tauri/tests/wire_format_community_fixtures.rs` | Canonical CBOR byte fixtures | **Modify** — update `community_invite_payload_invite_only_wire_bytes_pinned` to include the new fields and pin the new canonical bytes |

---

## Task 0: Pre-flight + green-baseline confirmation

**Files:** none (verification only)

**Goal:** Confirm the branch is on `origin/main` lineage and the workspace is green before any changes land.

- [ ] **Step 1: Verify branch + base commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git branch --show-current
git log --oneline -3
```

Expected output (the branch carries only the spec commit `da3f8a8` over `origin/main` at `7d32256`):

```
zeb-260-invite-only-cold-cache
da3f8a8 docs(zeb-260): Phase 4 invite-only cold-cache bootstrap design spec
7d32256 Merge pull request #89 from zeblithic/zeb-262-phase-4-invite-only-kick-set-power
35e2522 fix(zeb-262): close timeout-vs-notifier race in redeem_invite_inner
```

If the branch name is wrong, stop and ask the controller. Do NOT create a worktree — per HARD RULE, all work happens in the main repo via `git checkout`.

- [ ] **Step 2: Pull latest origin/main and confirm base is unchanged**

```bash
git fetch origin
git log --oneline origin/main..HEAD
```

Expected: exactly one commit (`da3f8a8 docs(zeb-260): ...`). If more commits exist or the base has moved beyond `7d32256`, stop and ask the controller.

- [ ] **Step 3: Run baseline `cargo fmt` check**

```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: zero output, exit code 0.

- [ ] **Step 4: Run baseline `cargo clippy`**

```bash
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings
```

Expected: zero warnings, exit code 0. (Compile time is significant for the harmony-client workspace; expect 2-5 minutes on first run.)

- [ ] **Step 5: Run baseline `cargo test`**

```bash
cd src-tauri && cargo test
```

Expected: all tests pass, exit code 0. The tests `community_invite_only_integration::community_invite_only_redeem_round_trips` and `community_sync_integration::*` are the load-bearing ones for ZEB-262 / ZEB-258; if any fail, stop and ask the controller (test drift is our fault per HARD RULE).

- [ ] **Step 6: No commit (verification-only task)**

This task produces no code changes. Mark complete in TodoWrite and proceed to Task 1.

---

## Task 1: Wire format — `admin_bootstrap` + `admin_identity_pub` fields

**Files:**
- Modify: `src-tauri/src/community_invite.rs:28-52` (extend `CommunityInvitePayload`)
- Modify: `src-tauri/tests/wire_format_community_fixtures.rs:140-187` (update the invite-only fixture; keep open-community fixture byte-identical)

**Goal:** Extend `CommunityInvitePayload` with two optional fields that carry the admin's signed bootstrap event, and pin the new canonical CBOR bytes.

- [ ] **Step 1: Modify `CommunityInvitePayload` in `community_invite.rs:28-52`**

Insert two new fields after `invite_token` (line 51). The CBOR keys `ab` and `ap` keep the same-length-2 invariant; `skip_serializing_if = "Option::is_none"` keeps open-community payloads byte-identical.

Resulting struct (full replacement of lines 27-52):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunityInvitePayload {
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "mk")]
    pub membership_key: MembershipKey,

    #[serde(rename = "ad")]
    pub admin_addr: OwnerAddr,

    #[serde(rename = "nm")]
    pub community_name: String,

    #[serde(rename = "io")]
    pub is_invite_only: bool,

    #[serde(rename = "ex", skip_serializing_if = "Option::is_none", default)]
    pub expires_at: Option<Hlc>,

    /// Required for invite-only redemption (carries the inviter's
    /// pre-signed authorization). Optional for open communities (could
    /// still be present as an authenticity hint, but not required).
    #[serde(rename = "tk", skip_serializing_if = "Option::is_none", default)]
    pub invite_token: Option<InviteToken>,

    /// Admin's signed self-Join (their bootstrap event). Required for
    /// invite-only payloads (ZEB-260): without this the joiner's empty
    /// CRDT cannot admit the admin's eventual publish-back, because the
    /// receive-side membership-at-HLC gate evaluates publisher status
    /// against the joiner's local prefix (which has no admin entry).
    /// Joiner's `redeem_invite_inner` verifies this against
    /// `admin_identity_pub` and inserts via
    /// `engine.insert_local_event_with_pubs` before sending the unicast
    /// packet — the publish-back is generated strictly later, so this
    /// closes the race by construction. Open communities ignore this
    /// field; encoding stays byte-identical via skip_serializing_if.
    #[serde(rename = "ab", skip_serializing_if = "Option::is_none", default)]
    pub admin_bootstrap: Option<crate::community_membership::SignedMembershipEvent>,

    /// Admin's 64-byte identity_pub (X25519_pub(32) || Ed25519_pub(32),
    /// matching `harmony_identity::Identity::to_public_bytes()`). Required
    /// for invite-only payloads (ZEB-260) — used to verify
    /// `admin_bootstrap` and passed into
    /// `engine.insert_local_event_with_pubs(_, admin_identity_pub, None)`.
    /// Bound to `admin_addr` via
    /// `Identity::from_public_bytes(admin_identity_pub).address_hash ==
    /// admin_addr.0`.
    #[serde(
        rename = "ap",
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "serialize_admin_identity_pub_as_bstr",
        deserialize_with = "deserialize_admin_identity_pub_from_bstr"
    )]
    pub admin_identity_pub: Option<[u8; 64]>,
}
```

- [ ] **Step 2: Add the two new serde helpers near the existing `serialize_identity_pub_as_bstr` / `deserialize_identity_pub_from_bstr` (search for those names; the new helpers handle the `Option<[u8; 64]>` shape)**

Add immediately after the existing `deserialize_identity_pub_from_bstr` function (which is in `community_invite.rs`; grep `serialize_identity_pub_as_bstr` to find it):

```rust
/// Serialize `Option<[u8; 64]>` as a CBOR bstr (Some) or absent (None,
/// via `skip_serializing_if`). Mirrors the existing
/// `serialize_identity_pub_as_bstr` shape; wraps it for the optional
/// case used by `CommunityInvitePayload::admin_identity_pub`.
fn serialize_admin_identity_pub_as_bstr<S>(
    val: &Option<[u8; 64]>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match val {
        Some(bytes) => serializer.serialize_bytes(bytes),
        None => serializer.serialize_none(),
    }
}

/// Deserialize `Option<[u8; 64]>` from CBOR. The field is wrapped in
/// `Option` because invite-only payloads always set it but open-community
/// payloads omit it entirely; serde routes the absent-key case to
/// `default` (None) and the present-bstr case here.
fn deserialize_admin_identity_pub_from_bstr<'de, D>(
    deserializer: D,
) -> Result<Option<[u8; 64]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let bytes = <serde_bytes::ByteBuf as serde::Deserialize>::deserialize(deserializer)?;
    if bytes.len() != 64 {
        return Err(D::Error::invalid_length(bytes.len(), &"64 bytes"));
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&bytes);
    Ok(Some(out))
}
```

If `serde_bytes` isn't already imported at the top of `community_invite.rs`, no need to add it — the deserialize path uses the fully-qualified `serde_bytes::ByteBuf` path. (Verify with `grep '^use serde_bytes' src-tauri/src/community_invite.rs` first; if it's already there, leave alone.)

- [ ] **Step 3: Run `cargo build` to verify the new struct compiles**

```bash
cd src-tauri && cargo build --tests 2>&1 | tail -50
```

Expected: errors ONLY in test files (the existing fixture tests construct `CommunityInvitePayload` literally without the new fields). The library proper should compile cleanly. If the library has a compile error, fix it before proceeding.

- [ ] **Step 4: Update fixture: open-community wire bytes test (lines 140-159 of `tests/wire_format_community_fixtures.rs`)**

The existing open-community fixture has 5 keys (no `ex`, `tk`, `ab`, `ap`). Add explicit `admin_bootstrap: None, admin_identity_pub: None` to the literal construction so it compiles; the encoded bytes MUST stay byte-identical (skip_serializing_if = Option::is_none means absent fields don't appear in CBOR).

Resulting test:

```rust
#[test]
fn community_invite_payload_open_wire_bytes_pinned() {
    let p = CommunityInvitePayload {
        community_id: SpaceId([0x37; 16]),
        membership_key: MembershipKey::new([0xAA; 32]),
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,
        admin_identity_pub: None,
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_open hex: {hex}");
    assert_eq!(
        hex,
        "a56263695037373737373737373737373737373737626d6b5820aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6261645011111111111111111111111111111111626e6d6366697862696ff4",
        "CommunityInvitePayload (open) wire format changed"
    );
}
```

The asserted hex is unchanged from the existing fixture — this is the load-bearing assertion that open-community URLs are NOT broken by the field addition.

- [ ] **Step 5: Update fixture: invite-only wire bytes test (lines 161-187 of `tests/wire_format_community_fixtures.rs`)**

The invite-only fixture must include the new fields and pin the new canonical bytes. Use deterministic test inputs (a synthetic `SignedMembershipEvent` and a 64-byte 0xAB-padded identity_pub) so the encoded bytes are fully reproducible.

Replace the existing test (lines 161-187) with:

```rust
#[test]
fn community_invite_payload_invite_only_wire_bytes_pinned() {
    use harmony_client::community_membership::{
        EventPayload, MembershipEventKind, SignedMembershipEvent,
    };

    let token = InviteToken {
        inviter: OwnerAddr([0x11; 16]),
        invitee_hint: Some(OwnerAddr([0x22; 16])),
        minted_at: fixture_hlc(),
        expires_at: None,
        sig: [0xDD; 64],
    };

    // Synthetic admin bootstrap with all-deterministic bytes so the
    // encoded payload is reproducible. NOT a real signature — this test
    // pins canonical wire bytes only.
    let bootstrap_payload = EventPayload {
        id: [0xCC; 16],
        community_id: SpaceId([0x37; 16]),
        kind: MembershipEventKind::Join,
        actor: OwnerAddr([0x11; 16]),
        at: fixture_hlc(),
    };
    let admin_bootstrap = SignedMembershipEvent {
        id: bootstrap_payload.id,
        community_id: bootstrap_payload.community_id,
        kind: bootstrap_payload.kind,
        actor: bootstrap_payload.actor,
        at: bootstrap_payload.at,
        sig: [0xEE; 64],
        countersig: None,
    };

    let p = CommunityInvitePayload {
        community_id: SpaceId([0x37; 16]),
        membership_key: MembershipKey::new([0xAA; 32]),
        admin_addr: OwnerAddr([0x11; 16]),
        community_name: "fix".into(),
        is_invite_only: true,
        expires_at: Some(fixture_hlc()),
        invite_token: Some(token),
        admin_bootstrap: Some(admin_bootstrap),
        admin_identity_pub: Some([0xAB; 64]),
    };
    let bytes = canonical_cbor_encode(&p).expect("encode");
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    eprintln!("community_invite_payload_invite_only hex: {hex}");

    // Run once with `--nocapture` to capture the encoded hex, then paste
    // here. The assertion below is the load-bearing wire-format pin: any
    // future change to the CBOR shape MUST update this byte string by
    // hand and document the breaking change in the spec.
    //
    // To regenerate after canonical-CBOR shape changes:
    //   cargo test community_invite_payload_invite_only_wire_bytes_pinned -- --nocapture
    // and copy the printed hex into the assert_eq!.
    assert_eq!(
        hex.len() % 2,
        0,
        "even hex length sanity"
    );
    // First-run regeneration step: the implementer pins the actual hex
    // here after a single `cargo test --nocapture` run. Replace the
    // panic-on-mismatch placeholder below with the captured hex.
    panic!(
        "PIN-NEEDED: copy printed hex into assert_eq!. Hex was: {hex}"
    );
}
```

- [ ] **Step 6: Run the test once with `--nocapture` to capture the hex**

```bash
cd src-tauri && cargo test --test wire_format_community_fixtures community_invite_payload_invite_only_wire_bytes_pinned -- --nocapture 2>&1 | tail -30
```

Expected: the test panics at the `PIN-NEEDED` line, and the eprintln output above shows the actual encoded hex. Copy that hex.

- [ ] **Step 7: Replace the panic with the actual `assert_eq!`**

In `tests/wire_format_community_fixtures.rs`, replace the `panic!("PIN-NEEDED: ...")` block with:

```rust
    assert_eq!(
        hex,
        "<paste-the-captured-hex-here>",
        "CommunityInvitePayload (invite-only) wire format changed"
    );
```

Substitute `<paste-the-captured-hex-here>` with the hex from Step 6.

- [ ] **Step 8: Re-run all wire-format fixture tests**

```bash
cd src-tauri && cargo test --test wire_format_community_fixtures 2>&1 | tail -20
```

Expected: all tests pass, including the new invite-only fixture.

- [ ] **Step 9: Run cargo fmt**

```bash
cd src-tauri && cargo fmt --all
```

Then verify clean:

```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: exit code 0, zero output.

- [ ] **Step 10: Run cargo clippy**

```bash
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings
```

Expected: zero warnings.

- [ ] **Step 11: Commit**

```bash
git add src-tauri/src/community_invite.rs src-tauri/tests/wire_format_community_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(zeb-260): add admin_bootstrap + admin_identity_pub to invite payload

Two new optional fields on CommunityInvitePayload (CBOR keys ab + ap)
carry the admin's signed self-Join event and identity_pub. Required for
invite-only redemption — the joiner's redeem_invite_inner will verify
+ insert these into the engine before sending the unicast (next task).
Open-community URLs stay byte-identical via skip_serializing_if.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Verify chain helper + error enum

**Files:**
- Modify: `src-tauri/src/community_invite.rs` (add `RedeemBootstrapVerifyError` enum + `verify_admin_bootstrap` pure helper near the existing `CommunityInviteVerifyError` declaration)

**Goal:** Add a pure function `verify_admin_bootstrap(payload, admin_bootstrap, admin_identity_pub) -> Result<(), RedeemBootstrapVerifyError>` that runs the six-step binding chain. No tests yet (those land in Task 3); this task is types + impl only so the unit tests in Task 3 have a stable surface to test.

- [ ] **Step 1: Add the `RedeemBootstrapVerifyError` enum near the existing `CommunityInviteVerifyError`**

Search for `pub enum CommunityInviteVerifyError` in `src-tauri/src/community_invite.rs` (around line 361). Add the new enum AFTER it (and after the existing `impl CommunityInviteVerifyError` block at line 429+). Place at end of the verify-error block, before any function-level definitions.

```rust
/// Errors from `verify_admin_bootstrap` — the six-step binding chain
/// the joiner runs against the invite payload's `admin_bootstrap` +
/// `admin_identity_pub` fields before inserting the bootstrap into the
/// engine. ZEB-260: closing the cold-cache gap that prevents the new
/// joiner's empty CRDT from admitting the admin's first publish-back.
///
/// Each variant maps to a stable IPC error string via Display (NOT
/// Debug), matching the pattern established in PR #89 for IPC error
/// surface stability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedeemBootstrapVerifyError {
    /// Invite-only payload missing `admin_bootstrap` and/or
    /// `admin_identity_pub`. Fires for old PR #89 invite URLs (which
    /// never carried these fields). Stable IPC string:
    /// "redeem_invite: invite-only payload missing admin bootstrap".
    BootstrapMissing,

    /// `admin_identity_pub` bytes are not a valid Ed25519 + X25519 pair
    /// (rejected by `harmony_identity::Identity::from_public_bytes`).
    BootstrapInvalidPubkey,

    /// `Identity::from_public_bytes(admin_identity_pub).address_hash`
    /// does not equal `payload.admin_addr.0`.
    BootstrapAddressMismatch,

    /// `admin_bootstrap.actor` does not equal `payload.admin_addr`.
    BootstrapActorMismatch,

    /// `admin_bootstrap.community_id` does not equal
    /// `payload.community_id`.
    BootstrapCommunityMismatch,

    /// Ed25519 signature verification of `admin_bootstrap` failed under
    /// `admin_identity_pub`.
    BootstrapSignatureInvalid,

    /// `admin_bootstrap.kind` is not `Join`, or `countersig` is `Some`.
    /// Admin's bootstrap is always a self-Join with no countersig.
    BootstrapKindInvalid,
}

impl std::fmt::Display for RedeemBootstrapVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BootstrapMissing => write!(
                f,
                "redeem_invite: invite-only payload missing admin bootstrap"
            ),
            Self::BootstrapInvalidPubkey => write!(
                f,
                "redeem_invite: admin_identity_pub is not a valid identity"
            ),
            Self::BootstrapAddressMismatch => write!(
                f,
                "redeem_invite: admin_identity_pub.address_hash != admin_addr"
            ),
            Self::BootstrapActorMismatch => write!(
                f,
                "redeem_invite: admin_bootstrap.actor != admin_addr"
            ),
            Self::BootstrapCommunityMismatch => write!(
                f,
                "redeem_invite: admin_bootstrap.community_id != payload.community_id"
            ),
            Self::BootstrapSignatureInvalid => write!(
                f,
                "redeem_invite: admin_bootstrap signature verify failed"
            ),
            Self::BootstrapKindInvalid => write!(
                f,
                "redeem_invite: admin_bootstrap is not a self-Join (countersig present or wrong kind)"
            ),
        }
    }
}

impl std::error::Error for RedeemBootstrapVerifyError {}

impl RedeemBootstrapVerifyError {
    /// Short telemetry tag for the existing `record_redeem_outcome`-
    /// style logging path. Kept stable across builds — frontend-side
    /// metrics dashboards key off these strings. Mirrors the
    /// `CommunityInviteVerifyError::reason_tag` shape.
    pub fn reason_tag(&self) -> &'static str {
        match self {
            Self::BootstrapMissing => "bootstrap_missing",
            Self::BootstrapInvalidPubkey => "bootstrap_invalid_pubkey",
            Self::BootstrapAddressMismatch => "bootstrap_address_mismatch",
            Self::BootstrapActorMismatch => "bootstrap_actor_mismatch",
            Self::BootstrapCommunityMismatch => "bootstrap_community_mismatch",
            Self::BootstrapSignatureInvalid => "bootstrap_signature_invalid",
            Self::BootstrapKindInvalid => "bootstrap_kind_invalid",
        }
    }
}
```

- [ ] **Step 2: Add the `verify_admin_bootstrap` pure helper**

Place this helper immediately after the `RedeemBootstrapVerifyError` block. Pure / sync / no I/O — testable in isolation.

```rust
/// Run the six-step binding chain that admits the admin's signed
/// bootstrap event into the joiner's engine (ZEB-260). Pure / sync.
///
/// Returns `Ok(())` if every check passes; `Err(variant)` on the first
/// failure. Caller is `redeem_invite_inner` (in `lib.rs`) which converts
/// the error to a string for the IPC surface.
///
/// The chain (each step's failure → distinct error variant):
///   1. Required fields present (`admin_bootstrap` + `admin_identity_pub`
///      both `Some`). [BootstrapMissing]
///   2. `Identity::from_public_bytes(admin_identity_pub).address_hash ==
///      payload.admin_addr.0`. [BootstrapInvalidPubkey or
///      BootstrapAddressMismatch]
///   3. `admin_bootstrap.actor == payload.admin_addr`.
///      [BootstrapActorMismatch]
///   4. `admin_bootstrap.community_id == payload.community_id`.
///      [BootstrapCommunityMismatch]
///   5. Ed25519 signature verify of `admin_bootstrap` under
///      `admin_identity_pub` (delegates to
///      `community_membership::verify_signature`). [BootstrapSignatureInvalid]
///   6. Sanity: `admin_bootstrap.kind == Join` and `countersig is None`.
///      [BootstrapKindInvalid]
///
/// Caller (`redeem_invite_inner` invite-only branch) calls this AFTER
/// `spawn_engine` and BEFORE the unicast send. On `Ok`, the caller
/// proceeds to `engine.insert_local_event_with_pubs(admin_bootstrap,
/// admin_identity_pub, None)`. On `Err`, the caller tears down the
/// engine via `shutdown_engine_and_cleanup_persistence` and surfaces
/// the error string.
pub fn verify_admin_bootstrap(
    payload: &CommunityInvitePayload,
) -> Result<(&crate::community_membership::SignedMembershipEvent, &[u8; 64]), RedeemBootstrapVerifyError>
{
    // 1. Required fields.
    let admin_bootstrap = payload
        .admin_bootstrap
        .as_ref()
        .ok_or(RedeemBootstrapVerifyError::BootstrapMissing)?;
    let admin_identity_pub = payload
        .admin_identity_pub
        .as_ref()
        .ok_or(RedeemBootstrapVerifyError::BootstrapMissing)?;

    // 2. identity_pub ↔ admin_addr binding.
    let admin_identity = harmony_identity::Identity::from_public_bytes(admin_identity_pub)
        .map_err(|_| RedeemBootstrapVerifyError::BootstrapInvalidPubkey)?;
    if admin_identity.address_hash != payload.admin_addr.0 {
        return Err(RedeemBootstrapVerifyError::BootstrapAddressMismatch);
    }

    // 3. bootstrap.actor ↔ admin_addr binding.
    if admin_bootstrap.actor != payload.admin_addr {
        return Err(RedeemBootstrapVerifyError::BootstrapActorMismatch);
    }

    // 4. bootstrap.community_id ↔ payload.community_id binding.
    if admin_bootstrap.community_id != payload.community_id {
        return Err(RedeemBootstrapVerifyError::BootstrapCommunityMismatch);
    }

    // 5. Ed25519 signature verify.
    crate::community_membership::verify_signature(admin_bootstrap, admin_identity_pub)
        .map_err(|_| RedeemBootstrapVerifyError::BootstrapSignatureInvalid)?;

    // 6. Sanity: self-Join with no countersig.
    if !matches!(
        admin_bootstrap.kind,
        crate::community_membership::MembershipEventKind::Join
    ) || admin_bootstrap.countersig.is_some()
    {
        return Err(RedeemBootstrapVerifyError::BootstrapKindInvalid);
    }

    Ok((admin_bootstrap, admin_identity_pub))
}
```

- [ ] **Step 3: Run cargo build to verify**

```bash
cd src-tauri && cargo build --tests 2>&1 | tail -30
```

Expected: clean build (no errors). If clippy flags the unused helper, ignore — Task 3 will exercise it.

- [ ] **Step 4: Run cargo fmt**

```bash
cd src-tauri && cargo fmt --all && cargo fmt --all -- --check
```

Expected: exit code 0.

- [ ] **Step 5: Run cargo clippy**

```bash
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings
```

Expected: zero warnings. (The new code may trigger `clippy::needless_pass_by_value` or similar — adjust as needed; the helper takes `&CommunityInvitePayload` to avoid moving, which is the intent.)

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/community_invite.rs
git commit -m "$(cat <<'EOF'
feat(zeb-260): verify_admin_bootstrap helper + RedeemBootstrapVerifyError

Six-step binding chain (required-fields, identity_pub↔admin_addr,
bootstrap.actor↔admin_addr, bootstrap.community_id↔payload.community_id,
Ed25519 sig verify, kind+countersig sanity). Pure helper testable in
isolation; consumed by redeem_invite_inner (next task). reason_tag()
mirrors CommunityInviteVerifyError for telemetry consistency.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Unit tests for verify chain

**Files:**
- Modify: `src-tauri/tests/community_invite_unit.rs` (append 9 new unit tests)

**Goal:** Pin every branch of the six-step verify chain via standalone unit tests. Tests construct synthetic payloads with deterministic-but-realistic signatures (using `harmony_identity::PrivateIdentity::from_seed` for reproducibility).

- [ ] **Step 1: Add the test-helper at the top of `community_invite_unit.rs` (after the existing imports)**

Search for the existing imports block in `tests/community_invite_unit.rs`. Add this helper module after them. The helper builds a known-good `CommunityInvitePayload` with valid `admin_bootstrap` + `admin_identity_pub` so each test mutates one field at a time.

```rust
mod admin_bootstrap_helpers {
    use harmony_client::community_invite::{CommunityInvitePayload, InviteToken};
    use harmony_client::community_membership::{
        sign_event, EventPayload, MembershipEventKind, SignedMembershipEvent,
    };
    use harmony_client::owner_state_types::{Hlc, MembershipKey, OwnerAddr, SpaceId};
    use harmony_identity::PrivateIdentity;

    /// Deterministic keys: `seed` selects the identity (e.g., 0xAA for
    /// admin in the test). Returns `(identity, identity_pub_64,
    /// signing_key, owner_addr)`.
    pub fn identity_set(
        seed: u8,
    ) -> (
        PrivateIdentity,
        [u8; 64],
        ed25519_dalek::SigningKey,
        OwnerAddr,
    ) {
        let identity = PrivateIdentity::from_seed(&[seed; 32]);
        let pub_bytes = identity.identity.to_public_bytes();
        let priv_bytes = identity.to_private_bytes();
        let ed_seed: [u8; 32] = priv_bytes[32..64]
            .try_into()
            .expect("ed25519 seed slice 32..64");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&ed_seed);
        let addr = OwnerAddr(identity.identity.address_hash);
        (identity, pub_bytes, signing_key, addr)
    }

    pub fn fixture_hlc() -> Hlc {
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "admin-dev".into(),
        }
    }

    /// Build a known-good admin self-Join (signed) for the given
    /// community_id + admin identity. The returned event verifies
    /// against the admin's identity_pub.
    pub fn admin_bootstrap_event(
        community_id: SpaceId,
        admin_addr: OwnerAddr,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [0xCC; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor: admin_addr,
            at: fixture_hlc(),
        };
        sign_event(&payload, signing_key).expect("sign admin bootstrap")
    }

    /// Build a known-good invite-only `CommunityInvitePayload` with
    /// well-formed `admin_bootstrap` + `admin_identity_pub`. The 9
    /// per-branch tests below mutate one field at a time.
    pub fn good_invite_only_payload() -> CommunityInvitePayload {
        let (_identity, admin_pub, admin_sk, admin_addr) = identity_set(0xAA);
        let community_id = SpaceId([0x37; 16]);
        let bootstrap = admin_bootstrap_event(community_id, admin_addr, &admin_sk);

        CommunityInvitePayload {
            community_id,
            membership_key: MembershipKey::new([0xBB; 32]),
            admin_addr,
            community_name: "TestCom".into(),
            is_invite_only: true,
            expires_at: None,
            invite_token: Some(InviteToken {
                inviter: admin_addr,
                invitee_hint: None,
                minted_at: fixture_hlc(),
                expires_at: None,
                sig: [0xDD; 64],
            }),
            admin_bootstrap: Some(bootstrap),
            admin_identity_pub: Some(admin_pub),
        }
    }
}
```

- [ ] **Step 2: Add the 9 unit tests + 1 happy-path test**

Append to `community_invite_unit.rs` after the helper module:

```rust
#[cfg(test)]
mod verify_admin_bootstrap_tests {
    use super::admin_bootstrap_helpers::*;
    use harmony_client::community_invite::{verify_admin_bootstrap, RedeemBootstrapVerifyError};
    use harmony_client::community_membership::MembershipEventKind;
    use harmony_client::owner_state_types::{OwnerAddr, SpaceId};

    #[test]
    fn admits_well_formed_admin_bootstrap() {
        let p = good_invite_only_payload();
        let res = verify_admin_bootstrap(&p);
        assert!(
            res.is_ok(),
            "well-formed bootstrap should pass; got {res:?}"
        );
    }

    #[test]
    fn rejects_invite_only_without_admin_bootstrap() {
        let mut p = good_invite_only_payload();
        p.admin_bootstrap = None;
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapMissing
        );
    }

    #[test]
    fn rejects_invite_only_without_admin_identity_pub() {
        let mut p = good_invite_only_payload();
        p.admin_identity_pub = None;
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapMissing
        );
    }

    #[test]
    fn rejects_invalid_admin_pubkey() {
        let mut p = good_invite_only_payload();
        // All-zero bytes are not a valid X25519 + Ed25519 pair.
        p.admin_identity_pub = Some([0u8; 64]);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapInvalidPubkey
        );
    }

    #[test]
    fn rejects_admin_address_mismatch() {
        let mut p = good_invite_only_payload();
        // Use a different identity's pubkey but keep the original
        // admin_addr → the pubkey.address_hash will mismatch.
        let (_other_identity, other_pub, _other_sk, _other_addr) = identity_set(0xBB);
        p.admin_identity_pub = Some(other_pub);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapAddressMismatch
        );
    }

    #[test]
    fn rejects_admin_actor_mismatch() {
        let mut p = good_invite_only_payload();
        // Mutate the bootstrap's actor to a different address. Admin's
        // signature was over the original actor field, so this would
        // also fail step 5 (signature) — but step 3 fires first because
        // the chain checks actor before sig.
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        bs.actor = OwnerAddr([0xFF; 16]);
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapActorMismatch
        );
    }

    #[test]
    fn rejects_admin_community_mismatch() {
        let mut p = good_invite_only_payload();
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        bs.community_id = SpaceId([0xFF; 16]);
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapCommunityMismatch
        );
    }

    #[test]
    fn rejects_invalid_admin_signature() {
        let mut p = good_invite_only_payload();
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        // Flip a single bit in the signature.
        bs.sig[0] ^= 0x01;
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapSignatureInvalid
        );
    }

    #[test]
    fn rejects_admin_bootstrap_with_countersig() {
        let mut p = good_invite_only_payload();
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        // Inject a synthetic countersig.
        bs.countersig = Some(harmony_client::community_membership::CounterSignature {
            signer: harmony_client::owner_state_types::OwnerAddr([0xEE; 16]),
            sig: [0x77; 64],
        });
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapKindInvalid
        );
    }

    #[test]
    fn rejects_admin_bootstrap_non_join_kind() {
        let mut p = good_invite_only_payload();
        let mut bs = p.admin_bootstrap.as_ref().expect("bootstrap").clone();
        bs.kind = MembershipEventKind::Leave;
        p.admin_bootstrap = Some(bs);
        assert_eq!(
            verify_admin_bootstrap(&p).unwrap_err(),
            RedeemBootstrapVerifyError::BootstrapKindInvalid
        );
    }
}
```

- [ ] **Step 3: Run the new test module — ALL tests should pass**

```bash
cd src-tauri && cargo test --test community_invite_unit verify_admin_bootstrap_tests 2>&1 | tail -25
```

Expected:

```
running 10 tests
test verify_admin_bootstrap_tests::admits_well_formed_admin_bootstrap ... ok
test verify_admin_bootstrap_tests::rejects_admin_actor_mismatch ... ok
test verify_admin_bootstrap_tests::rejects_admin_address_mismatch ... ok
test verify_admin_bootstrap_tests::rejects_admin_bootstrap_non_join_kind ... ok
test verify_admin_bootstrap_tests::rejects_admin_bootstrap_with_countersig ... ok
test verify_admin_bootstrap_tests::rejects_admin_community_mismatch ... ok
test verify_admin_bootstrap_tests::rejects_invalid_admin_pubkey ... ok
test verify_admin_bootstrap_tests::rejects_invalid_admin_signature ... ok
test verify_admin_bootstrap_tests::rejects_invite_only_without_admin_bootstrap ... ok
test verify_admin_bootstrap_tests::rejects_invite_only_without_admin_identity_pub ... ok
```

If any test fails, fix the helper or the verify chain — the verify helper is the load-bearing surface and unit tests are how we pin behaviour.

- [ ] **Step 4: Run the entire `community_invite_unit.rs` suite to confirm no regressions**

```bash
cd src-tauri && cargo test --test community_invite_unit 2>&1 | tail -30
```

Expected: all tests (existing + 10 new) pass.

- [ ] **Step 5: Run cargo fmt + clippy**

```bash
cd src-tauri && cargo fmt --all && cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit code 0 from each command.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/tests/community_invite_unit.rs
git commit -m "$(cat <<'EOF'
test(zeb-260): unit-pin every branch of verify_admin_bootstrap

10 tests cover the happy path + 9 reject branches: missing fields ×2,
invalid pubkey, address mismatch, actor mismatch, community mismatch,
invalid signature, countersig present, non-Join kind. Synthetic
identities via PrivateIdentity::from_seed for reproducibility.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `redeem_invite_inner` integration + populate fields in integration test

**Files:**
- Modify: `src-tauri/src/lib.rs` (modify `redeem_invite_inner` invite-only branch around lines 6504-6610 — call `verify_admin_bootstrap` + `engine.insert_local_event_with_pubs` AFTER `spawn_engine`, BEFORE the `register_pending_redemption` step)
- Modify: `src-tauri/tests/community_invite_only_integration.rs` (the existing test constructs `CommunityInvitePayload` literally — populate `admin_bootstrap` + `admin_identity_pub` so the end-to-end flow works once the verify chain becomes mandatory)

**Goal:** Wire the verify+insert into production `redeem_invite_inner`. Update the integration test's invite-URL construction to populate the new fields. The integration test still has the OOB pre-seed at this point (Task 5 removes it); both paths insert the same bootstrap, second insert is idempotent (event-id dedup).

- [ ] **Step 1: Modify `redeem_invite_inner` invite-only branch in `lib.rs`**

In the invite-only branch (search `7. Branch on payload.is_invite_only` then `} else {` for the invite-only block — around line 6504), insert the verify+insert ABOVE the existing "7a. Register oneshot" comment. The full insertion (replacing the existing line `// 7a. Register oneshot keyed on bootstrap_join.id.` and the comment block above) becomes:

```rust
        // ZEB-260: verify admin's bootstrap from the invite payload AND
        // insert it into the joiner's engine BEFORE sending the unicast.
        // Closes the cold-cache gap: the joiner's empty CRDT cannot
        // admit the admin's eventual publish-back unless admin is in
        // the joiner's local prefix at the gate's `prior_state_at_hlc`
        // evaluation. Order is critical — the publish-back is generated
        // strictly later than the unicast arrives at admin, so the
        // bootstrap insert here cannot be raced.
        let (admin_bootstrap, admin_identity_pub) =
            match crate::community_invite::verify_admin_bootstrap(&payload) {
                Ok(pair) => pair,
                Err(verify_err) => {
                    // Engine + persistence dir were spawned at step 6;
                    // tear down before returning so we don't leak.
                    if let Err(stop_err) = community_registry
                        .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                        .await
                    {
                        tracing::warn!(
                            error = %stop_err,
                            community_id = %hex::encode(minted.community_id.0),
                            reason_tag = verify_err.reason_tag(),
                            "shutdown failed during redeem_invite admin-bootstrap-verify rollback"
                        );
                    }
                    return Err(verify_err.to_string());
                }
            };
        // Idempotent on retry: insert_local_event_with_pubs dedups on
        // event-id. The clone is cheap (SignedMembershipEvent is a few
        // hundred bytes) and required because the engine consumes by
        // value.
        let admin_bootstrap_owned = admin_bootstrap.clone();
        let admin_identity_pub_owned = *admin_identity_pub;
        let bootstrap_engine = match community_registry.engine_arc(&minted.community_id).await {
            Some(e) => e,
            None => {
                // Engine vanished between spawn and lookup — registry
                // race. Treated as a transient failure; tear down and
                // surface a deterministic error.
                if let Err(stop_err) = community_registry
                    .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                    .await
                {
                    tracing::warn!(
                        error = %stop_err,
                        community_id = %hex::encode(minted.community_id.0),
                        "shutdown failed during redeem_invite engine-vanished rollback"
                    );
                }
                return Err(
                    "engine vanished immediately after spawn — registry race (invite-only branch)"
                        .to_string(),
                );
            }
        };
        if let Err(insert_err) = bootstrap_engine
            .insert_local_event_with_pubs(
                admin_bootstrap_owned,
                admin_identity_pub_owned,
                None,
            )
            .await
        {
            // Bootstrap insert failed — should be effectively unreachable
            // given the verify chain just passed, but surface
            // deterministically and tear down.
            if let Err(stop_err) = community_registry
                .shutdown_engine_and_cleanup_persistence(&minted.community_id)
                .await
            {
                tracing::warn!(
                    error = %stop_err,
                    community_id = %hex::encode(minted.community_id.0),
                    "shutdown failed during redeem_invite admin-bootstrap-insert rollback"
                );
            }
            return Err(format!(
                "engine.insert_local_event_with_pubs (admin bootstrap): {insert_err}"
            ));
        }

        // 7a. Register oneshot keyed on bootstrap_join.id. Engine's
        //     insert hook (Task 7's notify_pending_redemption_in_map)
        //     fires it once the counter-signed Join lands.
```

The verify+insert block lives strictly between `spawn_engine` (step 6, around line 6418) and `register_pending_redemption` (step 7a, around line 6532). It does NOT change ordering of the existing 7b/7c/7d steps.

- [ ] **Step 2: Modify the integration test to populate the new fields in `CommunityInvitePayload`**

Open `src-tauri/tests/community_invite_only_integration.rs`. Search for `let invite_url = community_invite::encode_invite_url(&CommunityInvitePayload {` (around line 328). The existing literal is missing `admin_bootstrap` + `admin_identity_pub`. Add them.

Find the existing block (around lines 320-337) and replace with:

```rust
    let invite_token = InviteToken {
        inviter: alice_addr,
        invitee_hint: Some(bob_addr),
        minted_at: token_minted_at,
        expires_at: None,
        sig: token_sig,
    };

    // ZEB-260: invite-only payloads now carry admin's signed bootstrap
    // + identity_pub so the joiner's engine can admit admin's eventual
    // publish-back. We pull both from `alice_minted` (the output of
    // `mint_community_creation` already produced earlier in this test)
    // and Alice's identity (constructed near the top of the test).
    let invite_url = community_invite::encode_invite_url(&CommunityInvitePayload {
        community_id,
        membership_key: alice_minted.membership_key.clone(),
        admin_addr: alice_addr,
        community_name: "InviteOnly".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(invite_token),
        admin_bootstrap: Some(alice_minted.bootstrap_join.clone()),
        admin_identity_pub: Some(alice_identity.identity.to_public_bytes()),
    })
    .expect("encode URL");
```

The variable name `alice_identity` is the existing test's `PrivateIdentity` for Alice (search the file's earlier setup; if it's named differently — e.g., `alice_priv_identity` — substitute the correct name).

- [ ] **Step 3: Run the integration test**

```bash
cd src-tauri && cargo test --test community_invite_only_integration 2>&1 | tail -30
```

Expected: all tests pass. The OOB pre-seed at lines 365-430 is still in place (Task 5 removes it); the production verify+insert and the test pre-seed both target the same bootstrap event-id; `insert_local_event_with_pubs` dedups → second insert is `InsertOutcome::AlreadyPresent`, no error.

If the test fails with `BootstrapMissing` or similar, double-check the `CommunityInvitePayload` construction — both new fields must be `Some(...)` for invite-only payloads.

- [ ] **Step 4: Run the entire test suite to confirm no regressions**

```bash
cd src-tauri && cargo test 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 5: Run cargo fmt + clippy**

```bash
cd src-tauri && cargo fmt --all && cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit code 0.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/community_invite_only_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-260): wire admin bootstrap insert into redeem_invite_inner

Invite-only branch now calls verify_admin_bootstrap +
insert_local_event_with_pubs after spawn_engine and before sending
the unicast — closes the cold-cache gap so the joiner's CRDT admits
admin's publish-back. Verify failures + engine vanish + insert
failures all tear down via shutdown_engine_and_cleanup_persistence.

Test still has the ZEB-260 OOB pre-seed (next commit removes it);
both paths now insert the same bootstrap and dedup on event-id.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Remove integration-test pre-seed (load-bearing assertion)

**Files:**
- Modify: `src-tauri/tests/community_invite_only_integration.rs` (delete the ZEB-260 OOB pre-seed block at lines 365-430; replace with a single comment + the necessary forwarder wiring that the pre-seed was hiding)
- Modify: `src-tauri/src/community_state_sync.rs` (single comment update — the existing "Bootstrap caveat (tracked as ZEB-260)" comment now reflects Case A is fixed)

**Goal:** The pre-seed was the symptom; this commit removes it and verifies that the production verify+insert (Task 4) is the load-bearing path. If this commit's test passes, ZEB-260 Case A is fixed in production. If it fails, the verify+insert path is broken.

- [ ] **Step 1: Read the existing pre-seed block carefully**

```bash
sed -n '360,435p' src-tauri/tests/community_invite_only_integration.rs
```

The block (around lines 365-430) does three things:
1. **Comment** explaining the ZEB-260 caveat.
2. **Pre-spawn Bob's engine** with `registry_b.spawn_engine(...)` + insert Alice's bootstrap.
3. **Wire the publish/subscribe forwarders inline** because the pre-spawned engine consumed the channels that `redeem_invite_inner`'s own spawn would have used.

The Task-5 fix is to delete the pre-seed (parts 1+2) AND keep the publish/subscribe forwarder wiring (part 3) — but adapted so the forwarders connect to the channels created INSIDE `redeem_invite_inner` (which spawns the engine itself when no pre-spawn happens).

Since `redeem_invite_inner` creates its own `pub_tx` / `pub_rx` / `sub_tx` / `sub_rx`, the test forwarder model must be compatible. Check: how does `redeem_invite_inner` plumb `community_adapter_tx` for the post-spawn adapter dispatch? It uses the test's `bob_adapter_tx`. The test code already has a forwarder for `bob_adapter_tx` (the no-op when pre-spawned, but it WILL receive the `CommunityAdapterRequest` once we remove the pre-spawn).

- [ ] **Step 2: Locate and read the existing `bob_adapter_tx` forwarder block**

```bash
grep -n 'bob_adapter\|CommunityAdapterRequest\|spawn_community_state_zenoh_adapter' src-tauri/tests/community_invite_only_integration.rs | head -20
```

Identify the block that consumes `CommunityAdapterRequest` from `bob_adapter_tx` and wires the publisher_rx / subscriber_tx into Alice's side. (Task 5 leaves this block untouched but ensures it's not dead code post-pre-seed-removal.)

- [ ] **Step 3: Delete the pre-seed block**

In `src-tauri/tests/community_invite_only_integration.rs`, locate the block starting with the comment `// ZEB-256 bootstrap caveat: Bob's engine is empty until` and ending at the closing `}` of the inner pre-spawn scope (around lines 365-430). Replace the entire block with a single replacement comment:

```rust
    // ZEB-260 (was: pre-seed required to paper over cold-cache rejection).
    // Production now plumbs admin's signed bootstrap through the invite
    // URL (CommunityInvitePayload.admin_bootstrap + admin_identity_pub);
    // redeem_invite_inner verifies and inserts it into Bob's engine
    // BEFORE sending the unicast packet, so by the time Alice's
    // publish-back arrives, Bob's CRDT has Alice as Joined and the
    // membership-at-HLC gate admits. The previous OOB pre-seed has
    // been removed — its presence here would mask test drift if the
    // production path regressed.
```

The publish/subscribe forwarders that were inside the pre-seed block (the two `tokio::spawn` blocks that route Bob ↔ Alice) get migrated OUT of the deleted scope and inlined where they were before. Specifically, search the file for `tokio::spawn(async move {` blocks that bind to `b_pub_rx_seed` and `alice_pub_rx` — those need to consume `bob_adapter_tx` once the production code spawns Bob's engine itself.

If the existing forwarder model assumes pre-spawned channels (`b_pub_tx_seed`, `b_sub_rx_seed`, `b_pub_rx_seed`, `b_sub_tx_seed`), refactor to consume the `CommunityAdapterRequest` that `redeem_invite_inner` will dispatch via `bob_adapter_tx`. Pattern (paste verbatim — substitute variable names if the test uses different ones):

```rust
    // Forwarder #3: drain the CommunityAdapterRequest that
    // redeem_invite_inner dispatches for Bob's freshly-spawned engine,
    // and connect Bob's publisher_rx → Alice's sub_tx, and Alice's
    // pub_rx → Bob's subscriber_tx. Mirrors the open-flow integration
    // test's adapter forwarder pattern.
    let alice_sub_tx_for_fwd = alice_sub_tx.clone();
    let alice_pub_rx_for_fwd = std::sync::Arc::new(tokio::sync::Mutex::new(alice_pub_rx));
    let bob_adapter_rx_for_fwd = bob_adapter_rx;
    tokio::spawn(async move {
        let mut bob_adapter_rx = bob_adapter_rx_for_fwd;
        if let Some(req) = bob_adapter_rx.recv().await {
            // Bob → Alice publishes
            let mut bob_pub_rx = req.publisher_rx;
            let alice_sub_tx = alice_sub_tx_for_fwd.clone();
            tokio::spawn(async move {
                while let Some(bytes) = bob_pub_rx.recv().await {
                    if alice_sub_tx.send(bytes).await.is_err() {
                        break;
                    }
                }
            });
            // Alice → Bob publishes
            let bob_sub_tx = req.subscriber_tx.clone();
            let alice_pub_rx_clone = alice_pub_rx_for_fwd.clone();
            tokio::spawn(async move {
                let mut g = alice_pub_rx_clone.lock().await;
                while let Some(bytes) = g.recv().await {
                    if bob_sub_tx.send(bytes).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
```

If the test already has this forwarder shape (the open-flow integration test should — search `community_open_flow_integration.rs` for a similar pattern and mirror), reuse the existing variable names. The point is: Bob's engine is spawned by `redeem_invite_inner` itself, so the forwarder must consume the `CommunityAdapterRequest` and wire its channels.

- [ ] **Step 4: Add post-redeem CRDT-event-count assertion**

After the existing assertion that Bob's redemption succeeded, add a check that Bob's CRDT contains exactly 2 events (admin's bootstrap + counter-signed Bob Join). Around the existing `assert_eq!(alice_events.len(), 2, ...);` block, add a parallel check for Bob:

```rust
    // ZEB-260: Bob's CRDT now holds admin's bootstrap (inserted by
    // redeem_invite_inner from the invite URL's admin_bootstrap field)
    // AND his own counter-signed Join (merged from Alice's publish-back).
    let bob_state = registry_b
        .state_for(&community_id)
        .await
        .expect("bob state");
    let bob_events: Vec<_> = {
        let g = bob_state.lock().await;
        g.events.values().cloned().collect()
    };
    assert_eq!(
        bob_events.len(),
        2,
        "bob should hold admin Join + his counter-signed Join after redeem"
    );
    let mat_b = materialize(&bob_events, alice_addr);
    assert_eq!(
        mat_b.members.get(&alice_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "Alice must be Joined in Bob's view (admin bootstrap landed)"
    );
    assert_eq!(
        mat_b.members.get(&bob_addr).map(|m| m.status),
        Some(MemberStatus::Joined),
        "Bob must be Joined in Bob's view (publish-back merged)"
    );
```

- [ ] **Step 5: Update the comment in `community_state_sync.rs`**

Open `src-tauri/src/community_state_sync.rs` and search for the existing comment that mentions ZEB-260 (around line 1605 — `Bootstrap caveat (tracked as ZEB-260)`). The comment currently says:

```rust
    //    Bootstrap caveat (tracked as ZEB-260): the gate cannot
    //    [...]
    //    is also deferred under ZEB-260.
```

Update the wording so it reflects that **Case A** (Phase 4 invite-only new joiner) is fixed, and Cases B+C remain open:

```rust
    //    Bootstrap caveat: the gate evaluates membership pre-decrypt,
    //    so any membership change carrying the publisher's authorizing
    //    Join INSIDE the encrypted blob is rejected. Three cases
    //    historically tracked under ZEB-260:
    //      Case A — invite-only joiner with empty CRDT receiving
    //        admin's first publish-back. FIXED in 2026-05 by plumbing
    //        admin's signed bootstrap through the invite URL
    //        (CommunityInvitePayload.admin_bootstrap +
    //        admin_identity_pub) and inserting it during
    //        redeem_invite_inner before the unicast send.
    //      Case B — open-community brand-new joiner whose self-Join
    //        is only inside their own publish blob. DEFERRED.
    //      Case C — self-Re-Join after Leave. DEFERRED.
    //    Cases B+C share the same root cause but require a gate
    //    redesign (blob pre-decrypt or self-publisher-bootstrap)
    //    rather than a side-channel; deferred until a real production
    //    blocker emerges. See
    //    docs/specs/2026-05-08-zeb-260-invite-only-cold-cache-design.md
    //    for the Case A fix design.
```

- [ ] **Step 6: Run the integration test**

```bash
cd src-tauri && cargo test --test community_invite_only_integration 2>&1 | tail -30
```

Expected: all tests pass. Most importantly, the test that previously needed the OOB pre-seed (`community_invite_only_redeem_round_trips` or similar — check existing test names) now passes WITHOUT the pre-seed. This is the load-bearing assertion that ZEB-260 Case A is fixed in production.

If this test fails:
- A `PublisherNotJoined` rejection in logs → the verify+insert in `redeem_invite_inner` (Task 4) didn't actually insert. Re-check the engine arc lookup or the `insert_local_event_with_pubs` call.
- A timeout → check that the publish/subscribe forwarders are still wired correctly post-pre-seed-removal.
- A `BootstrapMissing` error → the test's `CommunityInvitePayload` construction (Task 4 Step 2) didn't populate the new fields.

- [ ] **Step 7: Run the full test suite**

```bash
cd src-tauri && cargo test 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 8: Run cargo fmt + clippy**

```bash
cd src-tauri && cargo fmt --all && cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit code 0.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/tests/community_invite_only_integration.rs src-tauri/src/community_state_sync.rs
git commit -m "$(cat <<'EOF'
test(zeb-260): remove OOB pre-seed; production now flows end-to-end

The ZEB-260 caveat at community_invite_only_integration.rs no longer
applies — admin's bootstrap is plumbed through the invite URL and
inserted into the joiner's engine during redeem_invite_inner.

Removed the inline pre-spawn + insert_local_event(alice_minted.bootstrap_join)
hack and updated the inline comment in community_state_sync.rs to
reflect Case A fixed / Cases B+C still deferred.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Tampering integration test

**Files:**
- Modify: `src-tauri/tests/community_invite_only_integration.rs` (add new test `community_invite_only_tampered_admin_bootstrap_rejects`)

**Goal:** Pin the security property that a tampered invite URL fails at the verify chain (Task 2's pure helper) BEFORE any engine spawn / persistence / unicast happens. Asserts the chain-step → error-variant mapping for the most-tampering-prone field (the bootstrap signature).

- [ ] **Step 1: Add the new test at the end of `community_invite_only_integration.rs`**

```rust
/// ZEB-260 tampering test: an invite URL whose admin_bootstrap has been
/// modified post-mint (signature flipped) MUST fail at the verify chain
/// before any engine spawn / unicast / commit. The error must surface as
/// `BootstrapSignatureInvalid` (the chain-step → variant mapping is part
/// of the security contract — frontend telemetry keys off these tags).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn community_invite_only_tampered_admin_bootstrap_rejects() {
    use community_invite::{CommunityInvitePayload, InviteToken};
    use community_membership::sign_event_with_identity;
    use harmony_client::*;

    // Build the same Alice + Bob setup as the happy-path test, BUT
    // tamper with admin_bootstrap.sig before encoding the URL.
    //
    // We don't actually run the engines — `decode_invite_url` parses
    // the URL inside `redeem_invite_inner`, then verify_admin_bootstrap
    // fires before spawn_engine, so the failure is observable purely
    // from `redeem_invite_inner`'s return value.

    let alice_identity = harmony_identity::PrivateIdentity::from_seed(&[0xAA; 32]);
    let alice_addr = owner_state_types::OwnerAddr(alice_identity.identity.address_hash);
    let alice_priv_bytes = alice_identity.to_private_bytes();
    let alice_ed_seed: [u8; 32] = alice_priv_bytes[32..64]
        .try_into()
        .expect("ed25519 seed slice 32..64");
    let alice_sk = ed25519_dalek::SigningKey::from_bytes(&alice_ed_seed);

    let bob_identity = harmony_identity::PrivateIdentity::from_seed(&[0xBB; 32]);
    let bob_addr = owner_state_types::OwnerAddr(bob_identity.identity.address_hash);

    // Mint Alice's community bootstrap.
    let community_id = owner_state_types::SpaceId([0x37; 16]);
    let bootstrap_payload = community_membership::EventPayload {
        id: [0xCC; 16],
        community_id,
        kind: community_membership::MembershipEventKind::Join,
        actor: alice_addr,
        at: owner_state_types::Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "alice-dev".into(),
        },
    };
    let mut alice_bootstrap =
        community_membership::sign_event(&bootstrap_payload, &alice_sk).expect("sign");
    // Tamper: flip a single bit in the signature.
    alice_bootstrap.sig[0] ^= 0x01;

    // Build the invite URL with the tampered bootstrap.
    let invite_url = community_invite::encode_invite_url(&CommunityInvitePayload {
        community_id,
        membership_key: owner_state_types::MembershipKey::new([0xDD; 32]),
        admin_addr: alice_addr,
        community_name: "TamperedTest".into(),
        is_invite_only: true,
        expires_at: None,
        invite_token: Some(InviteToken {
            inviter: alice_addr,
            invitee_hint: Some(bob_addr),
            minted_at: bootstrap_payload.at.clone(),
            expires_at: None,
            sig: [0xEE; 64],
        }),
        admin_bootstrap: Some(alice_bootstrap),
        admin_identity_pub: Some(alice_identity.identity.to_public_bytes()),
    })
    .expect("encode URL");

    // We can short-circuit by calling `verify_admin_bootstrap` on the
    // decoded payload directly (the same surface `redeem_invite_inner`
    // hits). This lets us assert the error variant without building
    // the full engine + unicast forwarder + crdt_state harness.
    let decoded = community_invite::decode_invite_url(&invite_url).expect("decode");
    let err = community_invite::verify_admin_bootstrap(&decoded).expect_err("tampered must reject");
    assert_eq!(
        err,
        community_invite::RedeemBootstrapVerifyError::BootstrapSignatureInvalid,
        "tampered admin_bootstrap.sig must surface as BootstrapSignatureInvalid"
    );
    // Telemetry tag is part of the security contract — pin it.
    assert_eq!(err.reason_tag(), "bootstrap_signature_invalid");
}
```

The test deliberately doesn't spin up engines, channels, or CRDT state — `verify_admin_bootstrap` is pure and the failure is observable in isolation. This is the cheapest possible pin for the security property.

- [ ] **Step 2: Run the new test**

```bash
cd src-tauri && cargo test --test community_invite_only_integration community_invite_only_tampered_admin_bootstrap_rejects 2>&1 | tail -15
```

Expected: test passes.

- [ ] **Step 3: Run the full integration test file**

```bash
cd src-tauri && cargo test --test community_invite_only_integration 2>&1 | tail -20
```

Expected: all tests pass (including the existing happy-path test from Task 5).

- [ ] **Step 4: Run cargo fmt + clippy**

```bash
cd src-tauri && cargo fmt --all && cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit code 0.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/tests/community_invite_only_integration.rs
git commit -m "$(cat <<'EOF'
test(zeb-260): tampered admin_bootstrap rejects with stable error tag

Pure-helper test — exercises verify_admin_bootstrap directly on a
decoded URL. Pins the chain-step → variant mapping
(BootstrapSignatureInvalid) and the telemetry tag
(bootstrap_signature_invalid) that frontend metrics dashboards
key off.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Final verification + push + PR

**Files:** none (verification + git operations only)

**Goal:** Re-run every gate one last time, push the branch, and open the PR.

- [ ] **Step 1: Run cargo fmt check**

```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: exit code 0, zero output.

- [ ] **Step 2: Run cargo clippy**

```bash
cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings
```

Expected: zero warnings.

- [ ] **Step 3: Run the full test suite**

```bash
cd src-tauri && cargo test 2>&1 | tail -30
```

Expected: every test passes. Use `${PIPESTATUS[0]}` to confirm the cargo exit code (per HARD RULE — pipe exit codes lie). If you see anything resembling failure, STOP and fix before pushing.

```bash
cd src-tauri && cargo test 2>&1
echo "cargo test exit code: ${PIPESTATUS[0]}"
```

Expected: exit code 0.

- [ ] **Step 4: Verify branch state vs origin/main**

```bash
git fetch origin
git log --oneline origin/main..HEAD
```

Expected: 7 commits — the spec commit `da3f8a8` + 6 implementation commits from Tasks 1-6. If there are extra commits or the base has moved, stop and ask the controller.

- [ ] **Step 5: Push the branch**

```bash
git push -u origin zeb-260-invite-only-cold-cache
```

Expected: push succeeds, GitHub creates the remote branch.

- [ ] **Step 6: Open the PR**

```bash
gh pr create --title "ZEB-260: Phase 4 invite-only cold-cache bootstrap fix" --body "$(cat <<'EOF'
## Summary

Closes the cold-cache gap that prevents Phase 4 invite-only redemption from round-tripping end-to-end in production. PR #89 (commit `7d32256`) shipped invite-only with an integration-test caveat documenting that production had no path to the admin's signed bootstrap; this PR plumbs that bootstrap through the invite URL itself.

- New optional fields on `CommunityInvitePayload`: `admin_bootstrap: Option<SignedMembershipEvent>` (CBOR `ab`) + `admin_identity_pub: Option<[u8; 64]>` (CBOR `ap`). Required for invite-only payloads, ignored for open-community payloads (skip_serializing_if keeps open URLs byte-identical).
- New pure helper `community_invite::verify_admin_bootstrap` runs a six-step binding chain: required-fields, identity_pub↔admin_addr, bootstrap.actor↔admin_addr, bootstrap.community_id↔payload.community_id, Ed25519 sig verify, kind+countersig sanity. Seven distinct error variants (`RedeemBootstrapVerifyError`) with `Display` impls + `reason_tag()` for stable IPC + telemetry surface.
- `redeem_invite_inner` invite-only branch now calls `verify_admin_bootstrap` + `engine.insert_local_event_with_pubs` AFTER `spawn_engine`, BEFORE the unicast send. Order is critical — the publish-back is generated strictly later than the unicast arrives at admin, so the bootstrap insert here cannot be raced.
- Membership-at-HLC gate, publish-back wire format, and encrypted-blob pipeline are unchanged. Cases B+C (open-community brand-new joiner; self-Re-Join after Leave) remain deferred per the existing ZEB-260 ticket recommendation.

Spec: [`docs/specs/2026-05-08-zeb-260-invite-only-cold-cache-design.md`](https://github.com/zeblithic/harmony-client/blob/zeb-260-invite-only-cold-cache/docs/specs/2026-05-08-zeb-260-invite-only-cold-cache-design.md) (commit `da3f8a8`).

References:
- ZEB-262 Phase 4 ship — PR #89 / commit `7d32256` (the baseline this PR builds on)
- [Linear ZEB-260](https://linear.app/zeblith/issue/ZEB-260) — re-scoped to Case A; Cases B+C carry forward to a follow-up ticket.

## Test Plan

- [x] Unit tests cover the happy path + 9 reject branches in `verify_admin_bootstrap` (`tests/community_invite_unit.rs::verify_admin_bootstrap_tests::*`)
- [x] Integration test `community_invite_only_redeem_round_trips` no longer needs the OOB pre-seed (production verifies + inserts admin bootstrap during redeem_invite_inner)
- [x] Integration test `community_invite_only_tampered_admin_bootstrap_rejects` pins the security property — tampered admin_bootstrap.sig surfaces as `BootstrapSignatureInvalid` with telemetry tag `bootstrap_signature_invalid`
- [x] Wire fixture `community_invite_payload_open_wire_bytes_pinned` UNCHANGED (open-community URLs stay byte-identical)
- [x] Wire fixture `community_invite_payload_invite_only_wire_bytes_pinned` updated and re-pinned
- [x] `cargo fmt --all -- --check` passes
- [x] `cargo clippy --all-targets --all-features -- -D warnings` passes
- [x] Full `cargo test` passes

## Out of scope

- `generate_invite` IPC for invite-only — currently rejects `is_invite_only=true` with `"Phase 3 supports OPEN communities only; invite-only generate_invite ships in Phase 4"`. Implementing it is a separate ticket the user has on the deferred-follow-ups list (no Linear ID yet — user files).
- Cases B+C of ZEB-260 (gate redesign for self-publisher-bootstrap and self-Re-Join). Different fix surface; deferred until a real production blocker.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Capture the returned PR URL and report it to the controller.

- [ ] **Step 7: Mark Task 7 complete; do not merge**

The PR needs human merge after review (per HARD RULE: never merge without explicit user authorization). The implementer subagent reports the PR URL back to the controller.

---

## Self-review (controller-side, post-plan)

After this plan was written, the controller ran a placeholder scan (`grep -i 'TBD\|TODO\|FIXME'`) — zero matches. Type consistency confirmed: `RedeemBootstrapVerifyError` and its variants are spelled identically across Tasks 2, 3, 4, 5, 6. The `verify_admin_bootstrap` signature `(payload: &CommunityInvitePayload) -> Result<(&SignedMembershipEvent, &[u8; 64]), RedeemBootstrapVerifyError>` matches between definition (Task 2) and consumer (Task 4). The serde rename keys `ab` and `ap` are consistent across spec, fixture, and struct definition.

Spec coverage:
- Wire format extension → Task 1
- Verification chain → Tasks 2 + 3
- `redeem_invite_inner` integration → Task 4
- Test pre-seed removal → Task 5
- Tampering test → Task 6
- Backward compat (old URLs reject) → covered by `BootstrapMissing` in Task 2 + Task 3 unit tests
- `community_state_sync.rs` comment update → Task 5
- `cargo fmt` + `cargo clippy` gates at every task → embedded in Tasks 1-7 verification steps
- Linear ZEB-260 description re-scope → user-side action; not part of code plan

No gaps.
