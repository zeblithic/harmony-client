# ZEB-254 Pending-Join CRDT Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax for tracking. Each task ends with a commit. After all tasks complete, the final task pushes the branch + opens a PR.

**Goal:** Ship persistent offline counter-signer queue for invite-only community redemption so a joiner can redeem an invite while no admin is online and the redemption completes automatically when an admin returns.

**Architecture:** Two new MembershipEventKind variants (`PendingJoin` joiner-signed + `JoinCountersign` admin-signed, paired by `target_event_id`). Joiner inserts PendingJoin into local community engine which auto-publishes via Zenoh state-root. Admin auto-counter-signs on receipt via either Reticulum unicast (fast path, ≤5s) or CRDT state-root sync (async). Materialize pairs the events; pending Joins >30d auto-hide. `Space.pending_join_at` carries greyed UI state for the joiner.

**Tech Stack:** Rust 2021 (tokio, serde-cbor, ed25519-dalek) on the backend; Svelte + TypeScript + Tauri IPC on the frontend.

**Spec:** `docs/specs/2026-05-15-zeb-254-pending-join-crdt-design.md` (commit `8ca129a`).

**Branch:** `zeb-254-pending-join-crdt` (already cut from `origin/main` at `cd8fc8a`).

---

## Project conventions (read before starting)

- **Cargo commands run from `src-tauri/`.** Frontend commands run from repo root.
- **5 CI gates:**
  1. `cd src-tauri && cargo fmt --all -- --check`
  2. `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  3. `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  4. `npx tsc --noEmit` (from repo root)
  5. `npx vitest run` (from repo root)
- **`test-fixtures` feature** is required for `--all-targets` to compile (gates deterministic-nonce crypto helpers used by `tests/wire_format_*_fixtures.rs`).
- **Tauri IPC param naming:** snake_case Rust ↔ camelCase JS (auto-converted at boundary).
- **Tauri IPC error extraction in TS:** `e instanceof Error ? e.message : String(e)`.
- **Pipe exit codes lie:** use `set -o pipefail` or `${PIPESTATUS[0]}` to verify failure modes — never trust `cmd | tail/grep` exit codes.
- **No worktrees:** all work happens on the existing checked-out branch via `git checkout`.
- **Frequent commits:** every task except Task 0 ends with a commit.

---

## File structure

### New files

| Path | Purpose |
|------|---------|
| `src-tauri/tests/wire_format_zeb254_fixtures.rs` | Byte-pinned canonical CBOR for `PendingJoin`, `JoinCountersign`, `MemberStatus::PendingJoin`, `Space.pending_join_at`. |
| `src-tauri/tests/community_pending_join_integration.rs` | Two-engine integration tests for the full pending → countersign → joined flow. |
| `src/lib/components/PendingJoinsPanel.svelte` | Admin audit feed sub-component used inside `CommunitySettingsPanel`. |
| `src/lib/components/PendingJoinsPanel.test.ts` | Vitest tests for the new component. |

### Modified files

| Path | Change |
|------|--------|
| `src-tauri/src/community_membership.rs` | Add `PendingJoin` + `JoinCountersign` variants to `MembershipEventKind` (line 42+). Add `PendingJoin` variant to `MemberStatus` (line 859+). Extend `verify_event` and `materialize` with the new variants. Add new `VerifyError` variants. |
| `src-tauri/src/owner_state_types.rs` | Add `Space.pending_join_at: Option<Hlc>` field (line 1700+ — `Space` struct). |
| `src-tauri/src/community_invite.rs` | `mint_redemption` (line 9227 in lib.rs — see Task 7) produces `PendingJoin` for invite-only path. `handle_unicast` (line 1471) inserts PendingJoin and triggers auto-counter-sign. |
| `src-tauri/src/community_state_sync.rs` | Add post-Inserted hook for `PendingJoin` (admin auto-counter-sign) and `JoinCountersign` (joiner Space update + nav-updated emit). |
| `src-tauri/src/lib.rs` | Update `mint_redemption` (line 9227), `redeem_invite_inner` (line 9370). Add `list_pending_joins` + `list_recent_counter_signs` IPCs. Add `RedeemInviteResultDto.pending` field. |
| `src/lib/components/RedeemInviteWizard.svelte` | Handle `pending: true` result — toast + dismiss + nav refresh. |
| `src/lib/nav-service.ts` | Greyed-render state for community Spaces with `pending_join_at !== null`; `nav-updated` listener clears greyed on `pending: false`. |
| `src/lib/components/CommunitySettingsPanel.svelte` | Mount `PendingJoinsPanel` for admin-tier viewers. |

---

## Task overview

- **Task 0:** Pre-flight (green-baseline confirm). No commit.
- **Task 1:** Add `MembershipEventKind::PendingJoin` + `JoinCountersign` + `MemberStatus::PendingJoin` variants (types + serde round-trip only — no verify/materialize wiring yet).
- **Task 2:** `verify_event` for `PendingJoin` + new `VerifyError` variants.
- **Task 3:** `verify_event` for `JoinCountersign`.
- **Task 4:** `materialize` updates — PendingJoin → MemberStatus::PendingJoin, pairing with JoinCountersign → Joined, >30d expiry.
- **Task 5:** `Space.pending_join_at: Option<Hlc>` field + round-trip test.
- **Task 6:** Wire fixtures file pinning canonical CBOR.
- **Task 7:** `mint_redemption` produces `PendingJoin` for invite-only path.
- **Task 8:** `redeem_invite_inner` invite-only branch rewrite — 5s timeout, Ok pending, Space commit.
- **Task 9:** `handle_unicast` admin-side update + auto-emit JoinCountersign.
- **Task 10:** Engine post-Inserted hook (`community_state_sync.rs`) — admin auto-counter-sign on CRDT-receipt.
- **Task 11:** Engine post-Inserted hook — joiner-side `JoinCountersign` consumes oneshot + emits Space update + nav-updated.
- **Task 12:** New IPCs `list_pending_joins` + `list_recent_counter_signs`.
- **Task 13:** Frontend — `RedeemInviteWizard` pending handling + `NavService` greyed render.
- **Task 14:** Frontend — `PendingJoinsPanel` component + mount in `CommunitySettingsPanel`.
- **Task 15:** Integration tests file (two-engine end-to-end flow).
- **Task 16:** Final 5-gate sweep + push + PR.

---

### Task 0: Pre-flight (no commit)

**Goal:** Confirm the branch's working tree compiles + tests pass before any new code lands. This is the baseline reference for every subsequent task.

- [ ] **Step 1: Confirm working tree clean**

```bash
git status
```

Expected: `nothing to commit, working tree clean` on branch `zeb-254-pending-join-crdt`.

- [ ] **Step 2: Confirm branch is on top of origin/main**

```bash
git fetch origin && git log --oneline origin/main..HEAD
```

Expected: only the spec commit `docs(zeb-254): persistent offline counter-signer queue design spec`.

- [ ] **Step 3: Run `cargo fmt --check` (gate 1)**

```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: exit 0, no output.

- [ ] **Step 4: Run `cargo clippy` (gate 2)**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: exit 0. Warnings allowed, `-D warnings` makes them fatal — so the only acceptable outcome is zero warnings.

- [ ] **Step 5: Run `cargo nextest` (gate 3)**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all tests pass. Note actual numeric count for the baseline (something near 840+ tests).

- [ ] **Step 6: Run `tsc` (gate 4)**

```bash
npx tsc --noEmit
```

(Run from repo root.) Expected: exit 0, no type errors.

- [ ] **Step 7: Run `vitest` (gate 5)**

```bash
npx vitest run
```

(Run from repo root.) Expected: all tests pass.

**Do NOT commit.** If any gate fails, stop and report — the baseline must be green before starting.

---

### Task 1: Add type variants for PendingJoin + JoinCountersign + MemberStatus::PendingJoin

**Files:**
- Modify: `src-tauri/src/community_membership.rs:42` (`MembershipEventKind` enum)
- Modify: `src-tauri/src/community_membership.rs:859` (`MemberStatus` enum)

**Goal:** Add the new enum variants with serde-rename tags. Pure type additions — no verify or materialize wiring yet. Round-trip tests confirm wire shape.

- [ ] **Step 1: Read the existing `MembershipEventKind` variants**

Read `src-tauri/src/community_membership.rs:42-183` to confirm current variant tags. Already-used 1-char tags: `j, l, i, k, p, u, c, m, d, r, f, x`. ZEB-254 uses `g` (gate/guest) and `y` (yes/approve) — both free.

- [ ] **Step 2: Write the failing round-trip tests**

Add to the existing `#[cfg(test)] mod tests { ... }` block in `community_membership.rs` (locate near bottom of file via `grep -n "^mod tests\|^#\[cfg(test)\]" src-tauri/src/community_membership.rs`):

```rust
#[test]
fn pending_join_variant_canonical_cbor_round_trip() {
    use crate::community_invite::InviteToken;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};

    let token = InviteToken {
        inviter: OwnerAddr([1u8; 16]),
        invitee_hint: Some(OwnerAddr([2u8; 16])),
        minted_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: OwnerAddr([1u8; 16]) },
        expires_at: Some(1_700_000_000_000 + 7 * 86_400_000),
        sig: [3u8; 64],
    };
    let kind = MembershipEventKind::PendingJoin {
        invite_token: token,
        joiner_identity_pub: [4u8; 64],
    };

    let encoded = crate::owner_state_crypto::canonical_cbor_encode(&kind).expect("encode");
    let decoded: MembershipEventKind = ciborium::from_reader(&mut encoded.as_slice()).expect("decode");
    assert_eq!(kind, decoded);
}

#[test]
fn join_countersign_variant_canonical_cbor_round_trip() {
    let kind = MembershipEventKind::JoinCountersign {
        target_event_id: [42u8; 16],
    };
    let encoded = crate::owner_state_crypto::canonical_cbor_encode(&kind).expect("encode");
    let decoded: MembershipEventKind = ciborium::from_reader(&mut encoded.as_slice()).expect("decode");
    assert_eq!(kind, decoded);
}

#[test]
fn member_status_pending_join_canonical_cbor_round_trip() {
    let status = MemberStatus::PendingJoin;
    let encoded = crate::owner_state_crypto::canonical_cbor_encode(&status).expect("encode");
    let decoded: MemberStatus = ciborium::from_reader(&mut encoded.as_slice()).expect("decode");
    assert_eq!(status, decoded);
}
```

- [ ] **Step 3: Verify the tests fail**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(pending_join_variant) | test(join_countersign_variant) | test(member_status_pending_join)' 2>&1 | tail -30
```

Expected: compile error (variants `PendingJoin` and `JoinCountersign` not yet defined on `MembershipEventKind`; `PendingJoin` not yet on `MemberStatus`).

- [ ] **Step 4: Add the `MembershipEventKind` variants**

In `src-tauri/src/community_membership.rs`, in the `MembershipEventKind` enum (line 42), add the variants AFTER the `Fork` variant (line 178-182) and BEFORE the closing `}`:

```rust
    /// ZEB-254: joiner-signed pending join for invite-only communities.
    /// Distributed via the community CRDT (Zenoh) so admins who were
    /// offline at redemption time can counter-sign asynchronously.
    /// Variant code "g" (gate / guest, unused before this). Inner field
    /// keys are 2-char per same-length-keys invariant.
    /// See spec `docs/specs/2026-05-15-zeb-254-pending-join-crdt-design.md` §3.
    #[serde(rename = "g")]
    PendingJoin {
        #[serde(rename = "it")]
        invite_token: crate::community_invite::InviteToken,
        /// 64-byte concatenation of X25519_pub || Ed25519_pub matching
        /// `harmony_identity::Identity::to_public_bytes()`. Same shape
        /// as `CommunityInviteSigned.joiner_identity_pub` (community_invite.rs:258).
        #[serde(
            rename = "jp",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        joiner_identity_pub: [u8; 64],
    },

    /// ZEB-254: admin-signed counter-sign approving a PendingJoin.
    /// Pairs by `target_event_id`. Variant code "y" (yes / approve).
    #[serde(rename = "y")]
    JoinCountersign {
        #[serde(
            rename = "tg",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        target_event_id: EventId,
    },
```

- [ ] **Step 5: Add the `MemberStatus::PendingJoin` variant**

In `src-tauri/src/community_membership.rs:859-869`, extend the `MemberStatus` enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberStatus {
    #[serde(rename = "j")]
    Joined,
    #[serde(rename = "i")]
    Invited,
    #[serde(rename = "l")]
    Left,
    #[serde(rename = "b")]
    Banned,
    /// ZEB-254: joiner has minted a PendingJoin but no JoinCountersign
    /// has yet paired with it. Transitions to Joined when a matching
    /// JoinCountersign is materialized.
    #[serde(rename = "p")]
    PendingJoin,
}
```

- [ ] **Step 6: Verify the tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(pending_join_variant) | test(join_countersign_variant) | test(member_status_pending_join)'
```

Expected: 3 tests pass. (Other tests may have new clippy warnings — fix them in Step 7.)

- [ ] **Step 7: Fix any clippy warnings from the new variants**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -30
```

Common gotchas:
- `match` statements on `MembershipEventKind` elsewhere in the file will now warn about non-exhaustive patterns. Add `MembershipEventKind::PendingJoin { .. } => {}` and `MembershipEventKind::JoinCountersign { .. } => {}` arms with `// ZEB-254 wired in Task 2/3/4` placeholder comments. The actual logic ships in subsequent tasks.
- `match` on `MemberStatus` similarly — add `MemberStatus::PendingJoin => {}` arms.

Verify clippy passes after fixups.

- [ ] **Step 8: Run cargo fmt**

```bash
cd src-tauri && cargo fmt --all
```

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): add PendingJoin + JoinCountersign + MemberStatus::PendingJoin variants

Type additions only — verify_event and materialize wiring ships in
Tasks 2-4. Canonical CBOR round-trip tests confirm wire shape.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: verify_event rules for PendingJoin

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`verify_event` function, around line 1450+)
- Modify: `src-tauri/src/community_membership.rs` (`VerifyError` enum, around line 411+)

**Goal:** Reject PendingJoin events that fail any of the 8 verify rules in spec §3. Reuse existing actor-sig verification; add 4 new verify gates (token signer, token invitee_hint, token expiry, joiner_identity_pub binding) plus the "actor prior state must be None | Left" gate.

- [ ] **Step 1: Read the existing `verify_event` and `VerifyError`**

Read `src-tauri/src/community_membership.rs:411-650` (VerifyError variants) and `src-tauri/src/community_membership.rs:1450-1700` (verify_event body). Locate the invite-only-Join verify block at lines 1589-1614 (that's the existing legacy-Join countersig gate).

- [ ] **Step 2: Find the canonical InviteToken byte-canonicalization helper**

```bash
grep -n "canonical_invite_token_bytes\|fn.*verify.*invite.*token\|fn verify_invite_token" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/community_invite.rs | head -10
```

Note the helper's exact name and signature. The verify code uses it to recompute the bytes the InviteToken signature covers.

- [ ] **Step 3: Add new `VerifyError` variants**

In `src-tauri/src/community_membership.rs:411` (`VerifyError` enum), add these variants alongside the existing ones (before the `#[derive(Debug)]` close):

```rust
    /// ZEB-254: PendingJoin's InviteToken.signer != ctx.admin_addr, OR
    /// invitee_hint does not match the joiner's actor, OR the token's
    /// signature does not verify against the admin's identity_pub.
    PendingJoinTokenInvalid,

    /// ZEB-254: PendingJoin's InviteToken has an `expires_at` value that
    /// is at or before the event's wall_ms.
    PendingJoinTokenExpired,

    /// ZEB-254: PendingJoin's joiner_identity_pub does not hash (via
    /// SHA-256[..16]) to the event's actor address.
    PendingJoinJoinerPubMismatch,

    /// ZEB-254: PendingJoin actor's prior state is Joined or Banned —
    /// cannot accept a pending Join for an already-engaged member.
    PendingJoinAlreadyMember,

    /// ZEB-254: JoinCountersign actor is not currently Joined in the
    /// community.
    JoinCountersignActorNotJoined,

    /// ZEB-254: JoinCountersign actor's power is below invite_threshold.
    JoinCountersignActorPowerInsufficient,
```

In the `impl std::fmt::Display for VerifyError` block (around line 550+ — look for the existing Display impl), add:

```rust
            VerifyError::PendingJoinTokenInvalid => write!(f, "ZEB-254 PendingJoin InviteToken invalid (signer/invitee_hint/sig)"),
            VerifyError::PendingJoinTokenExpired => write!(f, "ZEB-254 PendingJoin InviteToken expired"),
            VerifyError::PendingJoinJoinerPubMismatch => write!(f, "ZEB-254 PendingJoin joiner_identity_pub hash != actor"),
            VerifyError::PendingJoinAlreadyMember => write!(f, "ZEB-254 PendingJoin actor's prior state is Joined or Banned"),
            VerifyError::JoinCountersignActorNotJoined => write!(f, "ZEB-254 JoinCountersign actor is not Joined"),
            VerifyError::JoinCountersignActorPowerInsufficient => write!(f, "ZEB-254 JoinCountersign actor power < invite_threshold"),
```

If the file has a `reason_tag()` method on `VerifyError` (search for `fn reason_tag`), add the corresponding tags:
- `PendingJoinTokenInvalid` → `"zeb_254_pending_join_token_invalid"`
- `PendingJoinTokenExpired` → `"zeb_254_pending_join_token_expired"`
- `PendingJoinJoinerPubMismatch` → `"zeb_254_pending_join_pub_mismatch"`
- `PendingJoinAlreadyMember` → `"zeb_254_pending_join_already_member"`
- `JoinCountersignActorNotJoined` → `"zeb_254_join_countersign_actor_not_joined"`
- `JoinCountersignActorPowerInsufficient` → `"zeb_254_join_countersign_actor_power_low"`

- [ ] **Step 4: Write failing unit tests**

Add to the `#[cfg(test)] mod tests` block in `community_membership.rs`:

```rust
#[cfg(test)]
mod zeb_254_pending_join_verify_tests {
    use super::*;
    use crate::community_invite::InviteToken;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;

    fn make_admin_identity() -> (SigningKey, OwnerAddr, [u8; 64]) {
        let signing = SigningKey::generate(&mut OsRng);
        let mut pub_bytes = [0u8; 64];
        // The first 32 bytes are X25519, the second 32 are Ed25519.
        // For tests we can use any X25519-shaped padding because verify only
        // cares about the Ed25519 half being correct.
        let verifying = signing.verifying_key();
        pub_bytes[32..].copy_from_slice(verifying.as_bytes());
        // Derive admin OwnerAddr = SHA256(pub_bytes)[..16]
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(pub_bytes);
        let mut addr = [0u8; 16];
        addr.copy_from_slice(&hash[..16]);
        (signing, OwnerAddr(addr), pub_bytes)
    }

    fn make_invite_token(
        admin_signing: &SigningKey,
        admin_addr: OwnerAddr,
        community_id: SpaceId,
        invitee_hint: Option<OwnerAddr>,
        expires_at: Option<u64>,
    ) -> InviteToken {
        let mut tok = InviteToken {
            inviter: admin_addr,
            invitee_hint,
            minted_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: admin_addr },
            expires_at,
            sig: [0u8; 64],
        };
        // Sign the canonical bytes — use the helper from community_invite.
        let bytes = crate::community_invite::canonical_invite_token_bytes(&tok, &community_id);
        let sig = admin_signing.sign(&bytes);
        tok.sig = sig.to_bytes();
        tok
    }

    fn make_pending_join_event(
        joiner_signing: &SigningKey,
        joiner_addr: OwnerAddr,
        joiner_pub: [u8; 64],
        community_id: SpaceId,
        token: InviteToken,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [9u8; 16],
            community_id,
            kind: MembershipEventKind::PendingJoin {
                invite_token: token,
                joiner_identity_pub: joiner_pub,
            },
            actor: joiner_addr,
            at: Hlc { wall_ms: 1_700_000_001_000, logical: 0, device_id: joiner_addr },
        };
        sign_event(&payload, joiner_signing).expect("sign PendingJoin")
    }

    #[test]
    fn pending_join_event_signs_and_verifies() {
        let (admin_sk, admin_addr, _admin_pub) = make_admin_identity();
        let (joiner_sk, joiner_addr, joiner_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(&admin_sk, admin_addr, community_id, Some(joiner_addr), Some(1_700_000_100_000));
        let event = make_pending_join_event(&joiner_sk, joiner_addr, joiner_pub, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
        };
        let mat = MaterializedMembership::default();
        assert!(verify_event(&event, &ctx, &mat).is_ok());
    }

    #[test]
    fn pending_join_rejected_when_token_not_for_actor() {
        let (admin_sk, admin_addr, _) = make_admin_identity();
        let (joiner_sk, joiner_addr, joiner_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        // Hint addresses someone else, not the joiner.
        let token = make_invite_token(&admin_sk, admin_addr, community_id, Some(OwnerAddr([99u8; 16])), Some(1_700_000_100_000));
        let event = make_pending_join_event(&joiner_sk, joiner_addr, joiner_pub, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
        };
        let mat = MaterializedMembership::default();
        assert!(matches!(verify_event(&event, &ctx, &mat), Err(VerifyError::PendingJoinTokenInvalid)));
    }

    #[test]
    fn pending_join_rejected_when_token_expired() {
        let (admin_sk, admin_addr, _) = make_admin_identity();
        let (joiner_sk, joiner_addr, joiner_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        // expires_at is BEFORE the event's wall_ms (1_700_000_001_000).
        let token = make_invite_token(&admin_sk, admin_addr, community_id, Some(joiner_addr), Some(1_700_000_000_500));
        let event = make_pending_join_event(&joiner_sk, joiner_addr, joiner_pub, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
        };
        let mat = MaterializedMembership::default();
        assert!(matches!(verify_event(&event, &ctx, &mat), Err(VerifyError::PendingJoinTokenExpired)));
    }

    #[test]
    fn pending_join_rejected_when_token_signer_not_admin() {
        let (_, admin_addr, _) = make_admin_identity();
        // A different "rogue" identity signs the token.
        let (rogue_sk, _, _) = make_admin_identity();
        let (joiner_sk, joiner_addr, joiner_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        // Use the rogue's signing key but claim the admin as inviter.
        let mut tok = InviteToken {
            inviter: admin_addr, // claims to be from admin
            invitee_hint: Some(joiner_addr),
            minted_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: admin_addr },
            expires_at: Some(1_700_000_100_000),
            sig: [0u8; 64],
        };
        let bytes = crate::community_invite::canonical_invite_token_bytes(&tok, &community_id);
        tok.sig = rogue_sk.sign(&bytes).to_bytes();
        let event = make_pending_join_event(&joiner_sk, joiner_addr, joiner_pub, community_id, tok);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
        };
        let mat = MaterializedMembership::default();
        // Sig verifies against rogue's pub, not admin's identity_pub —
        // but the verify code computes admin's identity_pub from admin_addr
        // via the resolver, so this either fails with sig-mismatch or
        // with token-signer-not-admin depending on the resolver path. For
        // unit-test purposes the verify accepts the InviteToken IFF its
        // sig verifies against the admin's known identity_pub. Since we
        // don't have a resolver here, the verify path uses the inviter
        // field directly and checks sig against admin_addr's pub —
        // which fails.
        assert!(matches!(verify_event(&event, &ctx, &mat), Err(VerifyError::PendingJoinTokenInvalid)));
    }

    #[test]
    fn pending_join_rejected_when_actor_already_joined() {
        let (admin_sk, admin_addr, _) = make_admin_identity();
        let (joiner_sk, joiner_addr, joiner_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(&admin_sk, admin_addr, community_id, Some(joiner_addr), Some(1_700_000_100_000));
        let event = make_pending_join_event(&joiner_sk, joiner_addr, joiner_pub, community_id, token);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(joiner_addr, MemberState {
            status: MemberStatus::Joined,
            joined_at: None,
            left_at: None,
        });
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
        };
        assert!(matches!(verify_event(&event, &ctx, &mat), Err(VerifyError::PendingJoinAlreadyMember)));
    }

    #[test]
    fn pending_join_rejected_when_actor_banned() {
        let (admin_sk, admin_addr, _) = make_admin_identity();
        let (joiner_sk, joiner_addr, joiner_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(&admin_sk, admin_addr, community_id, Some(joiner_addr), Some(1_700_000_100_000));
        let event = make_pending_join_event(&joiner_sk, joiner_addr, joiner_pub, community_id, token);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(joiner_addr, MemberState {
            status: MemberStatus::Banned,
            joined_at: None,
            left_at: None,
        });
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
        };
        assert!(matches!(verify_event(&event, &ctx, &mat), Err(VerifyError::PendingJoinAlreadyMember)));
    }

    #[test]
    fn pending_join_accepted_when_actor_was_left() {
        let (admin_sk, admin_addr, _) = make_admin_identity();
        let (joiner_sk, joiner_addr, joiner_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(&admin_sk, admin_addr, community_id, Some(joiner_addr), Some(1_700_000_100_000));
        let event = make_pending_join_event(&joiner_sk, joiner_addr, joiner_pub, community_id, token);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(joiner_addr, MemberState {
            status: MemberStatus::Left,
            joined_at: None,
            left_at: None,
        });
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &joiner_pub,
            countersigner_identity_pub: None,
        };
        assert!(verify_event(&event, &ctx, &mat).is_ok());
    }

    #[test]
    fn pending_join_rejected_when_identity_pub_does_not_hash_to_actor() {
        let (admin_sk, admin_addr, _) = make_admin_identity();
        let (joiner_sk, joiner_addr, _correct_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let token = make_invite_token(&admin_sk, admin_addr, community_id, Some(joiner_addr), Some(1_700_000_100_000));
        // Wrong pub — does NOT hash to joiner_addr.
        let wrong_pub = [0xFFu8; 64];
        let event = make_pending_join_event(&joiner_sk, joiner_addr, wrong_pub, community_id, token);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &wrong_pub,
            countersigner_identity_pub: None,
        };
        let mat = MaterializedMembership::default();
        // The pubkey-to-actor binding check fires first.
        assert!(matches!(
            verify_event(&event, &ctx, &mat),
            Err(VerifyError::PendingJoinJoinerPubMismatch) | Err(VerifyError::SignatureInvalid)
        ));
    }
}
```

Note: if `MemberState` has additional fields beyond `status / joined_at / left_at`, adjust the literal accordingly — check the struct definition near `community_membership.rs:840-857`.

- [ ] **Step 5: Verify the tests fail**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(zeb_254_pending_join_verify_tests)' 2>&1 | tail -40
```

Expected: 8 tests fail (or compile errors if `VerifyContext` / `MaterializedMembership` don't have `Default` — see step 6).

- [ ] **Step 6: Wire the PendingJoin verify rules into `verify_event`**

Locate the existing invite-only-Join verify block at `community_membership.rs:1589-1614`. Add a NEW block that handles `MembershipEventKind::PendingJoin` BEFORE the existing match-on-kind in the Joined-membership-check section. The new block:

```rust
    // ZEB-254: PendingJoin verify rules. Must run BEFORE the actor-sig
    // check (or alongside it) because the sig still binds the same
    // (id, community_id, kind, actor, at) tuple. The kind-specific
    // rules below add token + pubkey-binding gates on top.
    if let MembershipEventKind::PendingJoin {
        invite_token,
        joiner_identity_pub,
    } = &event.kind
    {
        // P1: joiner_identity_pub hashes to actor (SHA256[..16]).
        use sha2::{Digest, Sha256};
        let derived_addr = {
            let hash = Sha256::digest(joiner_identity_pub);
            let mut a = [0u8; 16];
            a.copy_from_slice(&hash[..16]);
            a
        };
        if derived_addr != event.actor.0 {
            return Err(VerifyError::PendingJoinJoinerPubMismatch);
        }

        // P2: token.inviter == ctx.admin_addr.
        if invite_token.inviter != ctx.admin_addr {
            return Err(VerifyError::PendingJoinTokenInvalid);
        }

        // P3: token.invitee_hint matches actor (if hint present).
        if let Some(hint) = invite_token.invitee_hint {
            if hint != event.actor {
                return Err(VerifyError::PendingJoinTokenInvalid);
            }
        }

        // P4: token expiry check — strictly less than. Mirrors
        // verify_packet_pure's rule.
        if let Some(exp) = invite_token.expires_at {
            if event.at.wall_ms >= exp {
                return Err(VerifyError::PendingJoinTokenExpired);
            }
        }

        // P5: token signature verifies against the admin's known
        // identity_pub. Reuse the canonical-bytes helper from
        // community_invite. The admin's pubkey for verifying the
        // token IS the actor_identity_pub stored alongside the
        // community's admin_addr — but in this verify scope the
        // caller passes only actor_identity_pub (= joiner's pub),
        // so we need a separate path. ZEB-262 already plumbs the
        // admin-identity-pub through verify_packet_pure for the
        // Reticulum path; for Zenoh-CRDT replay the admin's pub
        // is resolved via the engine's identity_resolver. Pass it
        // through VerifyContext.admin_identity_pub (new field).
        //
        // For unit tests that don't go through a resolver, the
        // helper signature is:
        //   crate::community_invite::verify_invite_token_signature(
        //       invite_token,
        //       community_id,
        //       admin_identity_pub,
        //   ) -> Result<(), ()>
        //
        // If the helper does NOT yet exist, factor it out of
        // verify_packet_pure (community_invite.rs:1277+).
        if let Some(admin_pub) = ctx.admin_identity_pub {
            if crate::community_invite::verify_invite_token_signature(
                invite_token,
                &event.community_id,
                admin_pub,
            ).is_err()
            {
                return Err(VerifyError::PendingJoinTokenInvalid);
            }
        }
        // If admin_identity_pub is None, the engine context cannot verify
        // the token signature. This is a setup error — the engine MUST
        // plumb the admin pub. Reject defensively.
        else {
            return Err(VerifyError::PendingJoinTokenInvalid);
        }

        // P6: prior state must be None | Some(Left).
        let prior_status = prior_state.members.get(&event.actor).map(|m| m.status);
        match prior_status {
            None | Some(MemberStatus::Left) => { /* ok */ }
            _ => return Err(VerifyError::PendingJoinAlreadyMember),
        }
    }
```

**IMPORTANT:** This block depends on `VerifyContext` having an `admin_identity_pub: Option<&[u8; 64]>` field. If the current `VerifyContext` (around line 478+) does NOT have this field, add it now. Update every existing caller (search via `grep -n VerifyContext src-tauri/src/`) to pass `None` or the admin's pub as appropriate. Most callers will pass `None` for legacy paths; the engine's verify path (community_state_sync.rs Phase 2) will need to thread it through.

Also: ensure the existing invite-only-Join block at lines 1589-1614 does NOT fire when `event.kind` is `PendingJoin` (it currently matches `MembershipEventKind::Join`). The new PendingJoin block above runs first.

- [ ] **Step 7: Factor out `verify_invite_token_signature` helper if needed**

If the helper does NOT exist (check with `grep -n "fn verify_invite_token_signature" src-tauri/src/community_invite.rs`), extract it from `verify_packet_pure`. The helper signature:

```rust
/// ZEB-254: pure helper extracted from verify_packet_pure for use by
/// SignedMembershipEvent verify on PendingJoin. Verifies the token's
/// signature covers (canonical token bytes including community_id) and
/// was produced by `admin_identity_pub`.
pub fn verify_invite_token_signature(
    token: &InviteToken,
    community_id: &SpaceId,
    admin_identity_pub: &[u8; 64],
) -> Result<(), CommunityInviteVerifyError> {
    let bytes = canonical_invite_token_bytes(token, community_id);
    let verifying_key_bytes: [u8; 32] = admin_identity_pub[32..].try_into()
        .map_err(|_| CommunityInviteVerifyError::EnvelopeSigInvalid)?;
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&verifying_key_bytes)
        .map_err(|_| CommunityInviteVerifyError::EnvelopeSigInvalid)?;
    let sig = ed25519_dalek::Signature::from_bytes(&token.sig);
    verifying_key.verify_strict(&bytes, &sig)
        .map_err(|_| CommunityInviteVerifyError::TokenSignatureInvalid)
}
```

If a similar verify already exists with a different name, use that name instead — adjust the verify_event call site accordingly.

- [ ] **Step 8: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(zeb_254_pending_join_verify_tests)'
```

Expected: 8 tests pass.

- [ ] **Step 9: Run full clippy + fmt**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
```

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): verify_event rules for PendingJoin

Adds 6 verify gates on PendingJoin: token signer is admin, invitee_hint
matches actor, token not expired, joiner_pub hashes to actor, prior
state is None|Left. Threads admin_identity_pub through VerifyContext
so the engine can verify the token's admin signature on Zenoh-replay.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: verify_event rules for JoinCountersign

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`verify_event` function)

**Goal:** JoinCountersign verifies its actor's sig over the (id, community_id, kind=JoinCountersign{tgt}, actor, at) tuple AND requires the actor to be Joined with power ≥ invite_threshold at this event's HLC. Target event existence is NOT a verify-time check — that's deferred to materialize to allow out-of-order delivery.

- [ ] **Step 1: Write failing tests**

Append to the `zeb_254_pending_join_verify_tests` module (or add a new sibling module):

```rust
    fn make_join_countersign_event(
        admin_signing: &SigningKey,
        admin_addr: OwnerAddr,
        community_id: SpaceId,
        target_event_id: EventId,
    ) -> SignedMembershipEvent {
        let payload = EventPayload {
            id: [88u8; 16],
            community_id,
            kind: MembershipEventKind::JoinCountersign { target_event_id },
            actor: admin_addr,
            at: Hlc { wall_ms: 1_700_000_002_000, logical: 0, device_id: admin_addr },
        };
        sign_event(&payload, admin_signing).expect("sign JoinCountersign")
    }

    #[test]
    fn join_countersign_event_signs_and_verifies() {
        let (admin_sk, admin_addr, admin_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let target = [9u8; 16];
        let event = make_join_countersign_event(&admin_sk, admin_addr, community_id, target);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(admin_addr, MemberState {
            status: MemberStatus::Joined,
            joined_at: None,
            left_at: None,
        });
        mat.power_levels.insert(admin_addr, POWER_THRESHOLDS.invite);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        assert!(verify_event(&event, &ctx, &mat).is_ok());
    }

    #[test]
    fn join_countersign_rejected_when_actor_not_joined() {
        let (admin_sk, admin_addr, admin_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let target = [9u8; 16];
        let event = make_join_countersign_event(&admin_sk, admin_addr, community_id, target);
        let mat = MaterializedMembership::default(); // actor not in members
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        assert!(matches!(verify_event(&event, &ctx, &mat), Err(VerifyError::JoinCountersignActorNotJoined)));
    }

    #[test]
    fn join_countersign_rejected_when_actor_lacks_power() {
        let (admin_sk, admin_addr, admin_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let target = [9u8; 16];
        let event = make_join_countersign_event(&admin_sk, admin_addr, community_id, target);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(admin_addr, MemberState {
            status: MemberStatus::Joined,
            joined_at: None,
            left_at: None,
        });
        // Power is below invite threshold. With invite_threshold=0 in v1,
        // this can only happen if we explicitly set negative-like behavior,
        // so use a contrived threshold-bump test: when invite_threshold
        // is 0 there's no way for a Joined member to have insufficient
        // power. Skip-test this case for v1 — the check exists for ZEB-251.
        let _ = (event, mat, ctx); // silence unused warnings
        // Test stub — verify the gate exists structurally:
        assert_eq!(POWER_THRESHOLDS.invite, 0, "invite_threshold is 0 in v1");
    }

    #[test]
    fn join_countersign_accepted_when_target_missing() {
        // Out-of-order delivery — JoinCountersign arrives before its
        // target PendingJoin. Verify must accept it (target existence
        // is materialize-time, not verify-time).
        let (admin_sk, admin_addr, admin_pub) = make_admin_identity();
        let community_id = SpaceId([7u8; 16]);
        let target = [0xDEu8; 16]; // does not exist in prior state
        let event = make_join_countersign_event(&admin_sk, admin_addr, community_id, target);
        let mut mat = MaterializedMembership::default();
        mat.members.insert(admin_addr, MemberState {
            status: MemberStatus::Joined,
            joined_at: None,
            left_at: None,
        });
        mat.power_levels.insert(admin_addr, POWER_THRESHOLDS.invite);
        let ctx = VerifyContext {
            expected_community_id: community_id,
            admin_addr,
            is_invite_only: true,
            actor_identity_pub: &admin_pub,
            countersigner_identity_pub: None,
            admin_identity_pub: Some(&admin_pub),
        };
        assert!(verify_event(&event, &ctx, &mat).is_ok());
    }
```

- [ ] **Step 2: Verify the tests fail**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(join_countersign_event_signs_and_verifies) | test(join_countersign_rejected_when_actor_not_joined) | test(join_countersign_accepted_when_target_missing)' 2>&1 | tail -20
```

Expected: tests fail (the JoinCountersign verify path doesn't yet exist).

- [ ] **Step 3: Wire the JoinCountersign verify rules**

In `verify_event`, locate the existing match-on-kind section after the actor-sig check (around the Joined-membership-check at line 1620+). Add the new arm:

```rust
        MembershipEventKind::JoinCountersign { .. } => {
            // ZEB-254: actor must be Joined + power ≥ invite_threshold.
            // Target event existence is a materialize concern (allow
            // out-of-order delivery).
            if !is_joined_member(prior_state, &event.actor) {
                return Err(VerifyError::JoinCountersignActorNotJoined);
            }
            let actor_power = prior_state.power_levels.get(&event.actor).copied().unwrap_or(0);
            if actor_power < POWER_THRESHOLDS.invite {
                return Err(VerifyError::JoinCountersignActorPowerInsufficient);
            }
        }
```

Insert this arm alongside the other kind-specific arms (e.g. `Invite { .. }`, `Kick { .. }`).

- [ ] **Step 4: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(join_countersign_event_signs_and_verifies) | test(join_countersign_rejected_when_actor_not_joined) | test(join_countersign_accepted_when_target_missing)'
```

Expected: 3 tests pass (+ the no-op power test).

- [ ] **Step 5: Run clippy + fmt**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
```

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): verify_event rules for JoinCountersign

Actor must be Joined + power >= invite_threshold. Target event
existence is materialize-time, not verify-time — allows out-of-order
delivery on Zenoh state-root sync.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: materialize PendingJoin + JoinCountersign pairing + 30d expiry

**Files:**
- Modify: `src-tauri/src/community_membership.rs` (`materialize` function, around line 940+)

**Goal:** Extend `materialize` to (1) map PendingJoin → MemberStatus::PendingJoin, (2) pair JoinCountersign with a same-community PendingJoin → upgrade to Joined, (3) hide PendingJoins older than 30 days unless they have a JoinCountersign.

- [ ] **Step 1: Locate the existing `materialize` function**

```bash
grep -n "pub fn materialize\|fn materialize" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/community_membership.rs | head -5
```

The function takes a slice of events + admin_addr and returns `MaterializedMembership`. It iterates events in HLC order (verify the iteration order — `materialize` is typically called with events pre-sorted by HLC, but check the existing impl).

- [ ] **Step 2: Add `MATERIALIZE_PENDING_EXPIRY_MS` constant**

In `community_membership.rs` near the top (after the `POWER_THRESHOLDS` constant, search via `grep -n POWER_THRESHOLDS`):

```rust
/// ZEB-254: PendingJoin events older than this (community current HLC
/// minus event HLC, in wall-ms) are hidden from materialize unless a
/// matching JoinCountersign exists. 30 days.
pub const MATERIALIZE_PENDING_EXPIRY_MS: u64 = 30 * 86_400_000;
```

- [ ] **Step 3: Write failing tests**

Add to the test module (or new sibling `zeb_254_materialize_tests`):

```rust
#[cfg(test)]
mod zeb_254_materialize_tests {
    use super::*;
    use crate::community_invite::InviteToken;
    use crate::owner_state_types::{Hlc, OwnerAddr, SpaceId};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn synth_pending_join(actor: OwnerAddr, community_id: SpaceId, at_wall_ms: u64) -> SignedMembershipEvent {
        let sk = SigningKey::generate(&mut OsRng);
        let verifying = sk.verifying_key();
        let mut pub_bytes = [0u8; 64];
        pub_bytes[32..].copy_from_slice(verifying.as_bytes());
        let token = InviteToken {
            inviter: OwnerAddr([0u8; 16]),
            invitee_hint: Some(actor),
            minted_at: Hlc { wall_ms: at_wall_ms, logical: 0, device_id: actor },
            expires_at: None,
            sig: [0u8; 64],
        };
        let payload = EventPayload {
            id: [actor.0[0]; 16],
            community_id,
            kind: MembershipEventKind::PendingJoin {
                invite_token: token,
                joiner_identity_pub: pub_bytes,
            },
            actor,
            at: Hlc { wall_ms: at_wall_ms, logical: 0, device_id: actor },
        };
        sign_event(&payload, &sk).expect("sign")
    }

    fn synth_join_countersign(admin: OwnerAddr, community_id: SpaceId, target: EventId, at_wall_ms: u64) -> SignedMembershipEvent {
        let sk = SigningKey::generate(&mut OsRng);
        let payload = EventPayload {
            id: [admin.0[1]; 16],
            community_id,
            kind: MembershipEventKind::JoinCountersign { target_event_id: target },
            actor: admin,
            at: Hlc { wall_ms: at_wall_ms, logical: 0, device_id: admin },
        };
        sign_event(&payload, &sk).expect("sign")
    }

    fn synth_join(actor: OwnerAddr, community_id: SpaceId, at_wall_ms: u64) -> SignedMembershipEvent {
        let sk = SigningKey::generate(&mut OsRng);
        let payload = EventPayload {
            id: [actor.0[0] ^ 0xFF; 16],
            community_id,
            kind: MembershipEventKind::Join,
            actor,
            at: Hlc { wall_ms: at_wall_ms, logical: 0, device_id: actor },
        };
        sign_event(&payload, &sk).expect("sign")
    }

    #[test]
    fn materialize_pending_join_only_yields_pending_status() {
        let community = SpaceId([7u8; 16]);
        let joiner = OwnerAddr([2u8; 16]);
        let admin = OwnerAddr([1u8; 16]);
        let pending = synth_pending_join(joiner, community, 1_700_000_000_000);
        let mat = materialize(&[pending], admin);
        assert_eq!(mat.members.get(&joiner).map(|m| m.status), Some(MemberStatus::PendingJoin));
    }

    #[test]
    fn materialize_pending_join_with_countersign_yields_joined() {
        let community = SpaceId([7u8; 16]);
        let joiner = OwnerAddr([2u8; 16]);
        let admin = OwnerAddr([1u8; 16]);
        let pending = synth_pending_join(joiner, community, 1_700_000_000_000);
        let cs = synth_join_countersign(admin, community, pending.id, 1_700_000_001_000);
        let mat = materialize(&[pending, cs], admin);
        assert_eq!(mat.members.get(&joiner).map(|m| m.status), Some(MemberStatus::Joined));
    }

    #[test]
    fn materialize_pending_join_older_than_30d_hidden() {
        let community = SpaceId([7u8; 16]);
        let joiner = OwnerAddr([2u8; 16]);
        let admin = OwnerAddr([1u8; 16]);
        let pending = synth_pending_join(joiner, community, 1_700_000_000_000);
        // Add an event 31 days later to advance the community's current HLC.
        let later_event_actor = OwnerAddr([99u8; 16]);
        let later = synth_join(later_event_actor, community, 1_700_000_000_000 + 31 * 86_400_000);
        let mat = materialize(&[pending, later], admin);
        // Joiner is hidden — no entry in members map.
        assert!(mat.members.get(&joiner).is_none());
    }

    #[test]
    fn materialize_pending_join_countersign_resurrects_expired_pending() {
        let community = SpaceId([7u8; 16]);
        let joiner = OwnerAddr([2u8; 16]);
        let admin = OwnerAddr([1u8; 16]);
        let pending = synth_pending_join(joiner, community, 1_700_000_000_000);
        // Counter-sign 31 days later — past the expiry window.
        let cs = synth_join_countersign(admin, community, pending.id, 1_700_000_000_000 + 31 * 86_400_000);
        let mat = materialize(&[pending, cs], admin);
        assert_eq!(mat.members.get(&joiner).map(|m| m.status), Some(MemberStatus::Joined));
    }

    #[test]
    fn materialize_legacy_join_with_countersig_still_yields_joined() {
        let community = SpaceId([7u8; 16]);
        let joiner = OwnerAddr([2u8; 16]);
        let admin = OwnerAddr([1u8; 16]);
        // Pre-ZEB-254 wire shape: `j` event, countersig is present in
        // SignedMembershipEvent.countersig field. Materialize treats it
        // the same as before — countersig presence is not a materialize
        // concern; verify gates it.
        let join = synth_join(joiner, community, 1_700_000_000_000);
        let mat = materialize(&[join], admin);
        assert_eq!(mat.members.get(&joiner).map(|m| m.status), Some(MemberStatus::Joined));
    }

    #[test]
    fn materialize_pending_join_then_leave_yields_left() {
        let community = SpaceId([7u8; 16]);
        let joiner = OwnerAddr([2u8; 16]);
        let admin = OwnerAddr([1u8; 16]);
        let pending = synth_pending_join(joiner, community, 1_700_000_000_000);
        let sk = SigningKey::generate(&mut OsRng);
        let leave_payload = EventPayload {
            id: [0xABu8; 16],
            community_id: community,
            kind: MembershipEventKind::Leave,
            actor: joiner,
            at: Hlc { wall_ms: 1_700_000_001_000, logical: 0, device_id: joiner },
        };
        let leave = sign_event(&leave_payload, &sk).expect("sign leave");
        let mat = materialize(&[pending, leave], admin);
        assert_eq!(mat.members.get(&joiner).map(|m| m.status), Some(MemberStatus::Left));
    }

    #[test]
    fn materialize_pending_join_with_two_countersigns_yields_joined() {
        // Both admins counter-sign the same PendingJoin. Materialize accepts
        // both; result is Joined.
        let community = SpaceId([7u8; 16]);
        let joiner = OwnerAddr([2u8; 16]);
        let admin1 = OwnerAddr([1u8; 16]);
        let admin2 = OwnerAddr([3u8; 16]);
        let pending = synth_pending_join(joiner, community, 1_700_000_000_000);
        let cs1 = synth_join_countersign(admin1, community, pending.id, 1_700_000_001_000);
        let cs2 = synth_join_countersign(admin2, community, pending.id, 1_700_000_001_500);
        let mat = materialize(&[pending, cs1, cs2], admin1);
        assert_eq!(mat.members.get(&joiner).map(|m| m.status), Some(MemberStatus::Joined));
    }
}
```

- [ ] **Step 4: Verify the tests fail**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(zeb_254_materialize_tests)' 2>&1 | tail -40
```

Expected: most fail (PendingJoin / JoinCountersign not yet wired into materialize).

- [ ] **Step 5: Wire PendingJoin + JoinCountersign into materialize**

In `materialize` (around line 940+), the function currently has a `match event.kind { ... }` block iterating each event. Add:

Two-pass approach (simpler than mutating in-place during the match):

```rust
pub fn materialize(events: &[SignedMembershipEvent], admin_addr: OwnerAddr) -> MaterializedMembership {
    // Existing setup unchanged.
    let mut mat = MaterializedMembership::default();

    // ZEB-254 Pass 0: compute community's current HLC max (for expiry calc).
    // This is the max wall_ms across all events. PendingJoins use this
    // as the reference point for the 30d expiry check.
    let current_max_wall_ms: u64 = events.iter().map(|e| e.at.wall_ms).max().unwrap_or(0);

    // ZEB-254 Pass 1: collect target_event_ids of all JoinCountersigns.
    // Used in Pass 2 below to decide whether a PendingJoin pairs with a
    // valid JoinCountersign before applying expiry.
    let countersigned_pending_ids: std::collections::HashSet<EventId> = events
        .iter()
        .filter_map(|e| match &e.kind {
            MembershipEventKind::JoinCountersign { target_event_id } => Some(*target_event_id),
            _ => None,
        })
        .collect();

    for event in events {
        // Existing per-kind handling stays — Join / Leave / Invite / Kick / etc.
        match &event.kind {
            MembershipEventKind::Join => {
                // ...existing logic unchanged...
            }
            // ZEB-254: PendingJoin.
            MembershipEventKind::PendingJoin { .. } => {
                let countersigned = countersigned_pending_ids.contains(&event.id);
                let age_ms = current_max_wall_ms.saturating_sub(event.at.wall_ms);
                let expired = age_ms > MATERIALIZE_PENDING_EXPIRY_MS;

                // If pending was already superseded by Joined/Left/Banned via
                // existing rules (e.g. a later Leave from the joiner), respect that.
                let prior_status = mat.members.get(&event.actor).map(|m| m.status);
                match prior_status {
                    Some(MemberStatus::Joined) | Some(MemberStatus::Banned) | Some(MemberStatus::Left) => {
                        // Already in a terminal state; PendingJoin is shadowed.
                    }
                    _ => {
                        if countersigned {
                            // Pair found → Joined regardless of expiry.
                            mat.members.insert(event.actor, MemberState {
                                status: MemberStatus::Joined,
                                joined_at: Some(event.at.clone()),
                                left_at: None,
                            });
                        } else if !expired {
                            mat.members.insert(event.actor, MemberState {
                                status: MemberStatus::PendingJoin,
                                joined_at: None,
                                left_at: None,
                            });
                        }
                        // else: expired pending with no countersign → hidden from materialize.
                    }
                }
            }
            // ZEB-254: JoinCountersign — no direct mutation. Pairing is
            // handled when its target PendingJoin is materialized above.
            // If the JoinCountersign arrives BEFORE the PendingJoin in
            // the iteration order (possible with HLC-ordered replay if
            // the admin's wall clock raced), the Pass 1 lookup catches it.
            MembershipEventKind::JoinCountersign { .. } => {
                // No-op at materialize.
            }
            // ...remaining existing arms unchanged...
        }
    }

    mat
}
```

**Important:** preserve all existing arms (Leave, Invite, Kick, SetPower, Unban, ChannelCreate, ChannelModify, ChannelDelete, EpochRotation, EpochCatchup, Fork). The diff is purely ADDITIVE — two new arms inserted.

If the existing materialize iterates without explicitly sorting by HLC, verify the call sites sort before passing — search `grep -n "materialize(" src-tauri/src/community_state_sync.rs`. If they don't sort, the test may pass anyway because tests pass events in HLC order; production reads from `state.events.values()` which is BTreeMap-sorted-by-key (event_id, not HLC). The materialize tests need to keep working both ways: the Pass 1 collection of countersigned IDs is order-independent, so this is fine.

- [ ] **Step 6: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(zeb_254_materialize_tests)'
```

Expected: all 7 tests pass.

- [ ] **Step 7: Run full test suite to confirm no regression**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all existing tests still pass. Pay special attention to any tests under `community_membership_tests` that exercise materialize — they should be unaffected.

- [ ] **Step 8: Run clippy + fmt**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
```

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): materialize PendingJoin + JoinCountersign pairing + 30d expiry

Two-pass materialize: collect countersigned event_ids, then for each
PendingJoin either upgrade to Joined (if paired) or set PendingJoin
(if within 30d) or hide (if expired). Counter-sign overrides expiry —
admin's word is final.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Space.pending_join_at field

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs` (`Space` struct)
- Modify: any byte-fixture tests that pin Space CBOR.

**Goal:** Add the new optional field for joiner-side pending UI state.

- [ ] **Step 1: Locate the existing `Space` struct definition**

```bash
grep -n "pub struct Space\b\|pub struct Space " /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/owner_state_types.rs | head -3
```

Read 30-50 lines around the match.

- [ ] **Step 2: Write a failing round-trip test**

Add to `owner_state_types.rs`'s test module:

```rust
#[test]
fn space_with_pending_join_at_round_trip() {
    let space = Space {
        id: SpaceId([7u8; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "test community".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: OwnerAddr([1u8; 16]) },
        updated_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: OwnerAddr([1u8; 16]) },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(0),
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: Some(OwnerAddr([1u8; 16])),
        is_invite_only: Some(true),
        shared_in_profile: false,
        pending_join_at: Some(Hlc { wall_ms: 1_700_000_000_500, logical: 0, device_id: OwnerAddr([2u8; 16]) }),
    };
    let encoded = crate::owner_state_crypto::canonical_cbor_encode(&space).expect("encode");
    let decoded: Space = ciborium::from_reader(&mut encoded.as_slice()).expect("decode");
    assert_eq!(space, decoded);
}

#[test]
fn space_without_pending_join_at_omits_field() {
    // Pre-ZEB-254 Space (pending_join_at = None) must encode WITHOUT the
    // "pj" key — skip_serializing_if guarantees wire compat.
    let space = Space {
        id: SpaceId([7u8; 16]),
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "dm".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: OwnerAddr([1u8; 16]) },
        updated_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: OwnerAddr([1u8; 16]) },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: None,
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: None,
        is_invite_only: None,
        shared_in_profile: false,
        pending_join_at: None,
    };
    let encoded = crate::owner_state_crypto::canonical_cbor_encode(&space).expect("encode");
    // The "pj" key (3 bytes: text(2) marker + p + j) must NOT appear.
    assert!(!encoded.windows(3).any(|w| w == [0x62, b'p', b'j']),
        "Space with pending_join_at=None must omit the pj key from canonical CBOR");
}
```

- [ ] **Step 3: Verify the tests fail**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(space_with_pending_join_at_round_trip) | test(space_without_pending_join_at_omits_field)' 2>&1 | tail -10
```

Expected: compile error — field doesn't exist on `Space`.

- [ ] **Step 4: Add the field to `Space`**

In `owner_state_types.rs`, add the field at the END of the `Space` struct (after `shared_in_profile`):

```rust
    /// ZEB-254: set when the joiner has minted a PendingJoin for this
    /// community but no JoinCountersign has yet landed locally. None
    /// means the joiner is fully Joined (or this Space is non-Community,
    /// or pre-ZEB-254 Space). Transitions:
    ///   None → Some(hlc): set at redeem-invite commit when the 5s
    ///     fast-path timeout fires without a counter-sign.
    ///   Some(hlc) → None: cleared by the community engine's post-Inserted
    ///     hook when self's PendingJoin receives a JoinCountersign.
    ///
    /// CRDT merge: existing LWW-by-updated_at handles None ↔ Some
    /// transitions (Space.updated_at advances on each transition).
    #[serde(rename = "pj", skip_serializing_if = "Option::is_none", default)]
    pub pending_join_at: Option<Hlc>,
```

- [ ] **Step 5: Update any places that construct `Space` to set `pending_join_at: None`**

```bash
grep -rn "Space {" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/ | head -20
```

For each constructor (likely in `lib.rs:9301` `mint_redemption`, `community_invite.rs`, tests), add `pending_join_at: None,` to the struct literal. Build to verify nothing is missed:

```bash
cd src-tauri && cargo build --features test-fixtures 2>&1 | tail -30
```

Resolve any "missing field `pending_join_at`" errors.

- [ ] **Step 6: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(space_with_pending_join_at) | test(space_without_pending_join_at)'
```

Expected: 2 tests pass.

- [ ] **Step 7: Run full suite**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all existing tests pass.

- [ ] **Step 8: Clippy + fmt**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
```

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): add Space.pending_join_at: Option<Hlc>

CRDT merge: existing LWW-by-updated_at handles transitions.
skip_serializing_if = None keeps pre-ZEB-254 Space CBOR byte-stable.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Wire fixtures file

**Files:**
- Create: `src-tauri/tests/wire_format_zeb254_fixtures.rs`

**Goal:** Pin canonical CBOR bytes for the 4 new wire elements. Future regression catches if anyone accidentally reorders fields or changes a rename tag.

- [ ] **Step 1: Read the existing fixture pattern**

```bash
cat /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/wire_format_zeb285_fixtures.rs | head -80
```

Note the structure: deterministic seeds, fixed timestamps, hex-encoded expected bytes. Follow this shape exactly.

- [ ] **Step 2: Create the new fixture file**

```bash
cat > /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/wire_format_zeb254_fixtures.rs << 'EOF'
//! ZEB-254: Byte-pinned canonical CBOR fixtures for PendingJoin,
//! JoinCountersign, MemberStatus::PendingJoin, Space.pending_join_at.
//!
//! Future wire-format drift in any of these structures will fail the
//! corresponding test. To regenerate a fixture (only after a deliberate
//! wire-format change with a wire-bump version field):
//!
//!   cargo nextest run --features test-fixtures \
//!     -E 'test(wire_format_zeb254)' 2>&1 | grep "actual:"
//!
//! Then replace the EXPECTED constant with the printed hex.

use harmony_app::community_invite::InviteToken;
use harmony_app::community_membership::{
    EventPayload, MembershipEventKind, MemberStatus, sign_event,
};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::{Hlc, OwnerAddr, Space, SpaceId, SpaceKind};
use ed25519_dalek::SigningKey;

// Deterministic signing-key bytes for fixture stability.
const FIXTURE_ADMIN_SK_BYTES: [u8; 32] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
    0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10,
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
    0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20,
];

const FIXTURE_JOINER_SK_BYTES: [u8; 32] = [
    0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF, 0xB0, 0xB1,
    0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9,
    0xBA, 0xBB, 0xBC, 0xBD, 0xBE, 0xBF, 0xC0, 0xC1,
    0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9,
];

#[test]
fn wire_format_zeb254_pending_join_canonical_cbor_pinned() {
    let admin = OwnerAddr([0x11; 16]);
    let joiner = OwnerAddr([0x22; 16]);
    let community = SpaceId([0x33; 16]);
    let token = InviteToken {
        inviter: admin,
        invitee_hint: Some(joiner),
        minted_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: admin },
        expires_at: Some(1_700_000_604_800_000),
        sig: [0x44; 64],
    };
    let kind = MembershipEventKind::PendingJoin {
        invite_token: token,
        joiner_identity_pub: [0x55; 64],
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    println!("actual: {}", actual_hex);

    // The expected hex is determined by running this test ONCE in
    // generation mode (with the expected line stubbed out), reading the
    // "actual:" line, and pasting it here. The constant is then the
    // wire-format pin.
    //
    // To re-generate after intentional wire-bump:
    //   1. Replace the next two lines with `let expected_hex = actual_hex.as_str();`
    //   2. Run the test once
    //   3. Restore with the new hex
    let expected_hex = include_str!("./fixtures/zeb254_pending_join.hex").trim();
    assert_eq!(actual_hex, expected_hex);
}

#[test]
fn wire_format_zeb254_join_countersign_canonical_cbor_pinned() {
    let kind = MembershipEventKind::JoinCountersign {
        target_event_id: [0x66; 16],
    };
    let encoded = canonical_cbor_encode(&kind).expect("encode");
    let actual_hex = hex::encode(&encoded);
    println!("actual: {}", actual_hex);
    let expected_hex = include_str!("./fixtures/zeb254_join_countersign.hex").trim();
    assert_eq!(actual_hex, expected_hex);
}

#[test]
fn wire_format_zeb254_member_status_pending_join_canonical_cbor_pinned() {
    let status = MemberStatus::PendingJoin;
    let encoded = canonical_cbor_encode(&status).expect("encode");
    let actual_hex = hex::encode(&encoded);
    println!("actual: {}", actual_hex);
    let expected_hex = include_str!("./fixtures/zeb254_member_status_pending_join.hex").trim();
    assert_eq!(actual_hex, expected_hex);
}

#[test]
fn wire_format_zeb254_space_with_pending_join_at_canonical_cbor_pinned() {
    let admin = OwnerAddr([0x11; 16]);
    let space = Space {
        id: SpaceId([0x33; 16]),
        kind: SpaceKind::Community,
        parent: None,
        community_id: None,
        name: "fixture community".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: admin },
        updated_at: Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: admin },
        content_key: None,
        prior_content_keys: vec![],
        current_epoch: Some(0),
        current_epoch_key: None,
        old_epoch_keys: std::collections::BTreeMap::new(),
        admin_addr: Some(admin),
        is_invite_only: Some(true),
        shared_in_profile: false,
        pending_join_at: Some(Hlc { wall_ms: 1_700_000_000_500, logical: 0, device_id: admin }),
    };
    let encoded = canonical_cbor_encode(&space).expect("encode");
    let actual_hex = hex::encode(&encoded);
    println!("actual: {}", actual_hex);
    let expected_hex = include_str!("./fixtures/zeb254_space_pending_join.hex").trim();
    assert_eq!(actual_hex, expected_hex);
}
EOF
```

If any of the structs above have additional/different fields, adjust to match. The implementer should compile and re-check.

- [ ] **Step 3: Create the fixtures directory + first run to capture hex**

```bash
mkdir -p /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/fixtures
# Stub the hex files so the first compile + run captures actual values.
touch /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/fixtures/zeb254_pending_join.hex
touch /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/fixtures/zeb254_join_countersign.hex
touch /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/fixtures/zeb254_member_status_pending_join.hex
touch /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/fixtures/zeb254_space_pending_join.hex
```

Then run the tests with stdout capture:

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(wire_format_zeb254)' --no-capture 2>&1 | grep "^actual: " | tee /tmp/zeb254-fixtures.txt
```

Tests will fail (empty expected != non-empty actual). The `actual:` lines are the canonical hex strings.

- [ ] **Step 4: Populate the hex fixture files**

For each test, copy its `actual:` line into the matching `.hex` file. The order of the four `actual:` lines in stdout follows test name alphabetical order. Use `sed` / manual paste; the simplest is to capture each test individually:

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(wire_format_zeb254_pending_join_canonical_cbor_pinned)' --no-capture 2>&1 | grep "^actual: " | awk '{print $2}' > tests/fixtures/zeb254_pending_join.hex
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(wire_format_zeb254_join_countersign_canonical_cbor_pinned)' --no-capture 2>&1 | grep "^actual: " | awk '{print $2}' > tests/fixtures/zeb254_join_countersign.hex
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(wire_format_zeb254_member_status_pending_join_canonical_cbor_pinned)' --no-capture 2>&1 | grep "^actual: " | awk '{print $2}' > tests/fixtures/zeb254_member_status_pending_join.hex
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(wire_format_zeb254_space_with_pending_join_at_canonical_cbor_pinned)' --no-capture 2>&1 | grep "^actual: " | awk '{print $2}' > tests/fixtures/zeb254_space_pending_join.hex
```

- [ ] **Step 5: Run the tests and confirm they now pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(wire_format_zeb254)'
```

Expected: 4 tests pass.

- [ ] **Step 6: Clippy + fmt + commit**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
test(zeb-254): pin canonical CBOR fixtures for PendingJoin + JoinCountersign

Byte-stable wire format for the two new MembershipEventKind variants,
MemberStatus::PendingJoin, and Space with pending_join_at. Future drift
fails these tests; regen by replacing the .hex fixture files (only after
a deliberate wire-bump).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: mint_redemption produces PendingJoin for invite-only path

**Files:**
- Modify: `src-tauri/src/lib.rs:9227` (`mint_redemption` function)

**Goal:** When `payload.is_invite_only`, produce a `PendingJoin` event instead of a legacy `Join` event. Open-community path unchanged.

- [ ] **Step 1: Read the existing `mint_redemption`**

```bash
sed -n '9216,9335p' /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs
```

Confirm the current shape: signs a `Join` event, returns `MintedCommunity { ..., bootstrap_join: SignedMembershipEvent }`. The invite-only path is handled by the caller (`redeem_invite_inner`) which currently sends the same Join via Reticulum unicast for counter-signing.

- [ ] **Step 2: Find `CommunityInvitePayload.invite_token`**

```bash
grep -n "invite_token\|pub invite_token\|signed_invite_token" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/community_invite.rs | head -10
```

Verify that `CommunityInvitePayload` carries an `InviteToken` (typically the bearer credential signed by the admin at invite-issuance time).

- [ ] **Step 3: Write a failing test**

In `lib.rs`'s existing `redeem_invite_inner_tests` mod (search via `grep -n "mod redeem_invite_inner_tests" src-tauri/src/lib.rs`), add:

```rust
    #[test]
    fn mint_redemption_invite_only_produces_pending_join() {
        // Synthesize a minimal CommunityInvitePayload for invite-only.
        // The test focuses on the kind variant produced — not on the
        // engine spawn or unicast path.
        use crate::community_invite::{CommunityInvitePayload, InviteToken, EpochSnapshot};
        use crate::community_membership::MembershipEventKind;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let admin_sk = SigningKey::generate(&mut OsRng);
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let admin_addr = crate::owner_state_types::OwnerAddr([0x11; 16]);
        let community_id = crate::owner_state_types::SpaceId([0x33; 16]);
        let joiner_addr = crate::owner_state_types::OwnerAddr([0x22; 16]);

        let token = InviteToken {
            inviter: admin_addr,
            invitee_hint: Some(joiner_addr),
            minted_at: crate::owner_state_types::Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: admin_addr },
            expires_at: Some(1_700_000_604_800_000),
            sig: [0; 64],
        };

        // Minimal valid epoch_snapshot for invite-only (open epoch key
        // is 32 raw bytes; invite-only is 92-byte sealed envelope. For
        // test, we synthesize an open path so we don't need X25519 setup —
        // wait, this is invite-only. Use a valid 92-byte sealed envelope
        // synthesized via dm_signing::seal_to_owner with the joiner's
        // X25519 pub.).
        //
        // For the unit test, focus on the kind variant — if the actual
        // sealed-envelope decryption is too heavy for a unit test, swap
        // to is_invite_only=false to exercise the open path AND add a
        // SEPARATE integration test for invite-only in Task 15.

        let payload = CommunityInvitePayload {
            community_id,
            community_name: "T".into(),
            admin_addr,
            invite_token: token.clone(),
            epoch_snapshot: EpochSnapshot {
                epoch: 0,
                sealed_epoch_key: vec![0u8; 32], // synthesized; for is_invite_only=true we'd need the 92-byte envelope
            },
            is_invite_only: false, // simplifying for the unit test
            forked_from: None,
            pre_fork_snapshot: None,
            // ...any other fields...
        };

        let hlc = crate::owner_state_types::Hlc { wall_ms: 1_700_000_001_000, logical: 0, device_id: joiner_addr };
        let minted = mint_redemption(&payload, joiner_addr, &joiner_sk, hlc).expect("mint open");

        // is_invite_only=false → legacy Join kind retained.
        assert!(matches!(minted.bootstrap_join.kind, MembershipEventKind::Join));
    }

    // A SECOND test exercises the invite-only path. Uses a manually-
    // constructed sealed envelope so the X25519 decrypt branch in
    // mint_redemption succeeds.
    #[test]
    fn mint_redemption_invite_only_produces_pending_join_kind() {
        use crate::community_invite::{CommunityInvitePayload, InviteToken, EpochSnapshot};
        use crate::community_membership::MembershipEventKind;
        use crate::dm_signing::{ed25519_priv_to_x25519, seal_to_owner};
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;

        let admin_sk = SigningKey::generate(&mut OsRng);
        let joiner_sk = SigningKey::generate(&mut OsRng);
        let admin_addr = crate::owner_state_types::OwnerAddr([0x11; 16]);
        let community_id = crate::owner_state_types::SpaceId([0x33; 16]);
        let joiner_addr = crate::owner_state_types::OwnerAddr([0x22; 16]);

        let token = InviteToken {
            inviter: admin_addr,
            invitee_hint: Some(joiner_addr),
            minted_at: crate::owner_state_types::Hlc { wall_ms: 1_700_000_000_000, logical: 0, device_id: admin_addr },
            expires_at: Some(1_700_000_604_800_000),
            sig: [0; 64],
        };

        // Seal a 32-byte EpochKey to the joiner's X25519 pub.
        let raw_key = [0xEE; 32];
        let joiner_x25519 = ed25519_priv_to_x25519(&joiner_sk);
        // For seal_to_owner we need the joiner's X25519 PUB (not priv).
        // Use x25519_dalek public derivation:
        let joiner_x25519_pub = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(joiner_x25519));
        let sealed = seal_to_owner(joiner_x25519_pub.as_bytes(), &raw_key).expect("seal");
        assert_eq!(sealed.len(), 92, "sealed envelope must be 92 bytes");

        let payload = CommunityInvitePayload {
            community_id,
            community_name: "T".into(),
            admin_addr,
            invite_token: token,
            epoch_snapshot: EpochSnapshot {
                epoch: 0,
                sealed_epoch_key: sealed,
            },
            is_invite_only: true,
            forked_from: None,
            pre_fork_snapshot: None,
        };

        let hlc = crate::owner_state_types::Hlc { wall_ms: 1_700_000_001_000, logical: 0, device_id: joiner_addr };
        let minted = mint_redemption(&payload, joiner_addr, &joiner_sk, hlc).expect("mint pending");
        assert!(matches!(minted.bootstrap_join.kind, MembershipEventKind::PendingJoin { .. }),
            "invite-only mint must produce PendingJoin kind, got {:?}", minted.bootstrap_join.kind);
    }
```

The implementer should adjust struct-literal fields to match the actual `CommunityInvitePayload` and `EpochSnapshot` definitions. Use `grep -n "pub struct CommunityInvitePayload\|pub struct EpochSnapshot" src-tauri/src/community_invite.rs` to confirm.

- [ ] **Step 4: Verify the tests fail**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(mint_redemption_invite_only_produces_pending_join_kind) | test(mint_redemption_invite_only_produces_pending_join)' 2>&1 | tail -20
```

Expected: the `_pending_join_kind` test fails (current mint_redemption always produces Join, never PendingJoin). The `_pending_join` (open path) test should pass because the open path is unchanged.

- [ ] **Step 5: Modify `mint_redemption`**

Locate the `event_kind` decision in `mint_redemption` (currently it's hardcoded as `MembershipEventKind::Join`). Replace with:

```rust
    // ZEB-254: invite-only redemptions mint a PendingJoin event carrying
    // the InviteToken (admin-signed bearer credential) and the joiner's
    // identity_pub. Distributed via the community CRDT so admins who were
    // offline at redemption time can counter-sign asynchronously.
    let event_kind = if payload.is_invite_only {
        // Joiner's identity_pub = X25519_pub || Ed25519_pub.
        // ed25519_priv_to_x25519 returns the X25519 PRIVATE scalar; for the
        // pub side use x25519_dalek::PublicKey::from(StaticSecret::from(priv_scalar)).
        use crate::dm_signing::ed25519_priv_to_x25519;
        let x25519_priv = ed25519_priv_to_x25519(signing_key);
        let x25519_pub = x25519_dalek::PublicKey::from(
            &x25519_dalek::StaticSecret::from(x25519_priv)
        );
        let ed25519_pub_bytes = signing_key.verifying_key().to_bytes();
        let mut identity_pub = [0u8; 64];
        identity_pub[..32].copy_from_slice(x25519_pub.as_bytes());
        identity_pub[32..].copy_from_slice(&ed25519_pub_bytes);

        crate::community_membership::MembershipEventKind::PendingJoin {
            invite_token: payload.invite_token.clone(),
            joiner_identity_pub: identity_pub,
        }
    } else {
        crate::community_membership::MembershipEventKind::Join
    };

    let join_payload = crate::community_membership::EventPayload {
        id: event_id_bytes,
        community_id: payload.community_id,
        kind: event_kind,
        actor: self_owner,
        at: join_hlc.clone(),
    };
    let bootstrap_join = crate::community_membership::sign_event(&join_payload, signing_key)
        .map_err(|e| format!("sign bootstrap event: {e}"))?;
```

Verify the `MintedCommunity` field name (`bootstrap_join`) is still descriptive — it now carries a PendingJoin for the invite-only path. Consider renaming to `bootstrap_event` for clarity. **If the rename touches more than 3 files, defer the rename to a follow-up; keep `bootstrap_join` for now and add a comment noting the broader meaning.**

- [ ] **Step 6: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(mint_redemption_invite_only_produces_pending_join_kind) | test(mint_redemption_invite_only_produces_pending_join)'
```

Expected: both tests pass.

- [ ] **Step 7: Run full suite**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all tests pass. Some existing `redeem_invite_inner_tests` may fail because they assert on legacy `Join` kind — fix them by updating assertions to accept `PendingJoin` for invite-only payloads.

- [ ] **Step 8: Clippy + fmt + commit**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): mint_redemption produces PendingJoin for invite-only path

invite-only redeems now carry the admin-signed InviteToken inline in
the bootstrap event, plus the joiner's full 64-byte identity_pub. The
open-community path still produces a legacy Join — unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: redeem_invite_inner — 5s timeout + Ok pending path + Space commit

**Files:**
- Modify: `src-tauri/src/lib.rs:9370` (`redeem_invite_inner` function)
- Modify: `src-tauri/src/lib.rs:9192` (`RedeemInviteResultDto`)
- Modify: `src/lib/redeem-invite.ts` (frontend caller, if exists — see below)

**Goal:** Change the invite-only branch's behavior on timeout: instead of rollback+Err, proceed and return Ok with `pending: true`. Commit Space with `pending_join_at = Some(hlc)`.

- [ ] **Step 1: Add `pending` to `RedeemInviteResultDto`**

In `lib.rs:9192`:

```rust
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RedeemInviteResultDto {
    pub community_id: String,
    pub community_name: String,
    pub is_invite_only: bool,
    /// ZEB-254: true if redemption returned before a JoinCountersign
    /// landed locally (admin was offline; the 5s fast-path timeout
    /// fired). The community appears in nav greyed; ungrey happens
    /// later when JoinCountersign arrives via state-root sync.
    /// false if either (a) fast-path counter-sign came back within 5s,
    /// or (b) community is open (no countersign required).
    pub pending: bool,
}
```

- [ ] **Step 2: Read and locate the current 7d block in `redeem_invite_inner`**

Read `lib.rs:9720-9790` (the `7d. Await oneshot ≤ T` block).

- [ ] **Step 3: Reduce timeout default from 15s to 5s**

In the same block, change:

```rust
        let timeout_ms: u64 = std::env::var("HARMONY_REDEEM_INVITE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15_000);
```

to:

```rust
        // ZEB-254: 5s fast-path timeout (down from 15s in ZEB-262). If
        // the timeout fires, redeem_invite_inner does NOT roll back —
        // it proceeds to commit the Space with pending_join_at = Some
        // and returns Ok with `pending: true`. The PendingJoin event
        // is already on the wire via the engine's state-root publish.
        let timeout_ms: u64 = std::env::var("HARMONY_REDEEM_INVITE_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5_000);
```

- [ ] **Step 4: Replace the timeout-Err-rollback path with proceed-pending**

The current `Err(_elapsed) => { ... return Err(...) }` block at `lib.rs:9737+` returns Err on timeout. Replace with:

```rust
            Err(_elapsed) => {
                // ZEB-254: 5s fast-path timeout fired. Two sub-cases:
                //   (A) take_pending_redemption returns Some(tx) — we won
                //       the race; the notifier hadn't run; genuine timeout
                //       → set pending_redemption_timed_out = true and
                //       fall through to commit with pending = true.
                //   (B) take_pending_redemption returns None — notifier
                //       won the race and already ingested the counter-
                //       signed event → treat as success; commit with
                //       pending = false.
                match community_registry
                    .take_pending_redemption(&minted.bootstrap_join.id)
                    .await
                {
                    Some(_tx) => {
                        // Sub-case A. The Reticulum unicast fast path
                        // did not return within 5s. Do NOT roll back.
                        // The PendingJoin is already published via the
                        // engine's state-root publisher. Commit Space
                        // with pending_join_at = Some.
                        pending_redemption_timed_out = true;
                    }
                    None => {
                        // Sub-case B. Counter-sign already landed.
                        pending_redemption_timed_out = false;
                        tracing::debug!(
                            community_id = %hex::encode(minted.community_id.0),
                            event_id = %hex::encode(minted.bootstrap_join.id),
                            "ZEB-254: 5s timeout fired but counter-sign arrived just in time — joined cleanly"
                        );
                    }
                }
            }
```

Above this block, declare the local variable:

```rust
        let mut pending_redemption_timed_out: bool = false;
```

In the `Ok(Ok(()))` arm (fast-path success) and the `Ok(Err(_recv_err))` arm (sender dropped), keep `pending_redemption_timed_out = false`.

The `Ok(Err(...))` arm currently returns Err — ZEB-254 keeps that behavior (an unexpected oneshot close is still a hard fail; it's not the offline case).

- [ ] **Step 5: Wire pending_join_at into the Space commit (step 9)**

Locate `lib.rs:9807` (the `9. COMMIT owner-state Space` block). The block currently does `state_g.apply_space_with_canonicalization(minted.space.clone())`. The `minted.space` was constructed in `mint_redemption` without a `pending_join_at` value.

Two options:
- (a) Update `mint_redemption` to take a `pending_at: Option<Hlc>` argument and set it on the constructed Space.
- (b) Mutate `minted.space.pending_join_at` here at the commit site.

(b) is the smaller blast radius. Replace the block:

```rust
    {
        let mut state_g = crdt_state.lock().await;
        // ZEB-254: set pending_join_at if the invite-only fast-path
        // timed out (admin offline). For non-invite-only or
        // counter-signed-in-time paths, pending_join_at stays None.
        let mut space_to_commit = minted.space.clone();
        if pending_redemption_timed_out {
            space_to_commit.pending_join_at = Some(minted.space.created_at.clone());
        }
        let outcome = state_g.apply_space_with_canonicalization(space_to_commit);
        if matches!(outcome, crate::owner_state_crdt::ApplyOutcome::Rejected(_)) {
            // existing rejection-rollback logic unchanged
            return Err(format!(
                "apply_space rejected redemption Space: {outcome:?}"
            ));
        }
    }
```

`pending_redemption_timed_out` must be in scope from step 4. For non-invite-only or open-community paths, declare and leave it as `false`.

- [ ] **Step 6: Return Ok with pending field**

The function's existing return path constructs `RedeemInviteResultDto`. Locate the final return — typically near the end of the function. Add `pending: pending_redemption_timed_out` to the struct literal:

```rust
    Ok(RedeemInviteResultDto {
        community_id: hex::encode(minted.community_id.0),
        community_name: payload.community_name,
        is_invite_only: payload.is_invite_only,
        pending: pending_redemption_timed_out,
    })
```

For open-community paths (non-invite-only branch), `pending` is always `false`.

- [ ] **Step 7: Update redeem_invite outer wrapper to thread `pending` to caller**

The outer `redeem_invite` Tauri command at `lib.rs:9993` returns the DTO unchanged — already compatible.

- [ ] **Step 8: Write tests**

In `lib.rs` `redeem_invite_inner_tests`:

```rust
    #[tokio::test]
    async fn redeem_invite_inner_returns_ok_pending_when_no_admin_online() {
        // Set the timeout shorter for fast test.
        std::env::set_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS", "50");
        // Build a synthetic invite-only payload + minimal harness.
        // The harness already exists for non-pending tests; extend it.
        //
        // The test asserts:
        //   1. result.pending == true
        //   2. The Space written to owner-state has pending_join_at = Some
        //   3. No rollback happened (engine is still registered)
        //
        // ...
        // Note: the existing harness builds a fake unicast_send_tx + a
        // never-sending pending_redemption channel. Use that scaffolding;
        // the new behavior is the proceed-pending path on timeout.
        //
        // [Full test body — see redeem_invite_inner_tests existing patterns
        // for the harness layout. Specifically, use the test harness pattern
        // from `redeem_invite_inner_open_path_returns_ok` (if it exists).]
        std::env::remove_var("HARMONY_REDEEM_INVITE_TIMEOUT_MS");
    }
```

If the existing test harness for `redeem_invite_inner` is complex (it has many dependencies), implementer:
- (a) Read the existing tests to find one that exercises the unicast timeout path
- (b) Adapt that test to assert `pending == true` and `Space.pending_join_at == Some(...)` instead of `Err(...)`

If no such test exists, add a small unit test that verifies the DTO has `pending: true` when timeout fires. Full integration is covered by Task 15.

- [ ] **Step 9: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(redeem_invite_inner_returns_ok_pending) | test(redeem_invite_inner_)'
```

Expected: existing `redeem_invite_inner_tests` pass with updated assertions; new test passes.

- [ ] **Step 10: Clippy + fmt + commit**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): redeem_invite_inner 5s timeout + Ok pending path

Timeout drops from 15s to 5s (fast-path budget only). On timeout, do
NOT rollback — proceed to commit Space with pending_join_at = Some(hlc)
and return Ok { pending: true }. The PendingJoin is already on the wire
via the engine's state-root publish loop; admins counter-sign whenever
they next come online.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: handle_unicast — accept PendingJoin packet body + auto-emit JoinCountersign

**Files:**
- Modify: `src-tauri/src/community_invite.rs:1471` (`handle_unicast`)
- Modify: `src-tauri/src/community_invite.rs:230+` (`CommunityInviteSigned` — possibly the inner `signed_join` field shape changes)

**Goal:** When admin receives a `CommunityInviteSigned` packet over Reticulum unicast, the inner event is now `PendingJoin` (not legacy `Join`). The verify chain accepts both. After inserting the PendingJoin into the engine, emit a JoinCountersign event.

- [ ] **Step 1: Inspect `CommunityInviteSigned`**

Read `community_invite.rs:230-275`. The `signed_join` field type is `SignedMembershipEvent`. ZEB-254 doesn't need to change this — both legacy `Join` and new `PendingJoin` are valid `SignedMembershipEvent` kinds. Verify accordingly.

- [ ] **Step 2: Check the existing verify path in `handle_unicast`**

Currently `verify_packet_pure` calls `crate::community_membership::verify_signature(&signed.join_event, &signed.joiner_identity_pub)` — this verifies the joiner's actor signature regardless of kind. Good. The kind-specific verify (PendingJoin's token-binding gates) runs at engine `insert_local_event` time via `verify_event`.

- [ ] **Step 3: Update `handle_unicast` to auto-emit JoinCountersign after insert**

The current code at `community_invite.rs:1584+`:

```rust
    // 6. Attach countersig with our identity.
    let counter_signed = match crate::community_membership::attach_countersig_with_identity(
        &join_event,
        self_private_identity.as_ref(),
    ) {
        ...
    };
    // 7. Insert the counter-signed Join.
    let countersigner_pub = self_private_identity.identity.to_public_bytes();
    match engine_arc.insert_local_event_with_pubs(counter_signed, ...) {
```

ZEB-254 changes this: instead of attach_countersig + insert the legacy `Join` shape, the admin:
1. Inserts the joiner's `PendingJoin` event AS-IS (no countersig append).
2. Emits a separate `JoinCountersign(target=pending.id)` event signed by the admin.
3. Inserts the JoinCountersign.

Both events flow out via the next state-root publish.

The new block replaces steps 6-7:

```rust
    // ZEB-254: Two-event flow for invite-only counter-sign.
    // The joiner's signed PendingJoin enters the engine via
    // insert_local_event_with_pubs. The post-Inserted hook
    // (Task 10) detects PendingJoin + self-has-power and emits
    // a JoinCountersign automatically.
    //
    // For LEGACY clients still sending the old `j`+countersig=None
    // wire shape, fall back to the original attach_countersig path
    // so cross-version interop is preserved.
    let is_pending_join_shape = matches!(
        join_event.kind,
        crate::community_membership::MembershipEventKind::PendingJoin { .. }
    );

    if is_pending_join_shape {
        // ZEB-254 new shape. Insert PendingJoin; post-Inserted hook
        // emits JoinCountersign.
        //
        // Admin's identity_pub for the joiner's-side resolver cache:
        // resolve via community_registry's identity resolver.
        let joiner_pub = if let crate::community_membership::MembershipEventKind::PendingJoin {
            joiner_identity_pub,
            ..
        } = &join_event.kind {
            Some(*joiner_identity_pub)
        } else {
            None
        };
        let join_outcome = engine_arc.insert_local_event_with_pubs(
            join_event,
            joiner_pub,
            None, // no countersigner — PendingJoin doesn't have countersig field set
        ).await;
        if let Err(e) = join_outcome {
            tracing::warn!(error = ?e, "ZEB-254 handle_unicast: insert PendingJoin failed");
            // emit degraded and return as today
            return Err(CommunityInviteVerifyError::CounterSignAttachFailed);
        }
        // The auto-counter-sign hook fires inside the engine on InsertOutcome::Inserted.
        Ok(())
    } else {
        // LEGACY path: attach_countersig + insert as before.
        let counter_signed = match crate::community_membership::attach_countersig_with_identity(
            &join_event,
            self_private_identity.as_ref(),
        ) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "attach_countersig_with_identity failed");
                let e = CommunityInviteVerifyError::CounterSignAttachFailed;
                emit_degraded(app, &signed.community_id, e.reason_tag());
                return Err(e);
            }
        };
        let countersigner_pub = self_private_identity.identity.to_public_bytes();
        match engine_arc.insert_local_event_with_pubs(
            counter_signed,
            None,
            Some(countersigner_pub),
        ).await {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::warn!(error = ?e, "legacy invite-only insert failed");
                Err(CommunityInviteVerifyError::CounterSignAttachFailed)
            }
        }
    }
```

The implementer should verify the exact signature of `insert_local_event_with_pubs` against `community_state_sync.rs` and adjust pub-arg ordering as needed.

- [ ] **Step 4: Write a unit test in handle_unicast tests**

```bash
grep -n "mod tests\|mod handle_unicast_tests\|#\[test\]" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/community_invite.rs | head -10
```

If a handle_unicast test module exists, add:

```rust
    #[tokio::test]
    async fn handle_unicast_pending_join_inserts_and_triggers_countersign() {
        // Build a fake CommunityInvitePacket::Invite wrapping a
        // PendingJoin event. Run handle_unicast against a community
        // engine where self is a Joined admin. After the call returns
        // Ok, assert that:
        //   1. The engine's state contains the PendingJoin event.
        //   2. The engine's state contains a JoinCountersign(target=
        //      pending.id) event authored by self.
        //
        // [Full body — uses existing community_registry + engine fixture
        // harness from existing handle_unicast tests as a template.]
    }
```

The full integration test runs in Task 15 — this unit test is a sanity check on the handle_unicast branching.

- [ ] **Step 5: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(handle_unicast_pending_join_inserts) | test(handle_unicast)'
```

- [ ] **Step 6: Clippy + fmt + commit**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): handle_unicast accepts PendingJoin + triggers auto-counter-sign

Admin-side handle_unicast detects the new PendingJoin shape and inserts
it AS-IS — the post-Inserted hook (Task 10) emits the JoinCountersign.
Legacy j+countersig wire shape continues to use the attach_countersig
path for cross-version interop.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: Engine post-Inserted hook — admin auto-counter-sign

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (`insert_event` / `insert_local_event_with_pubs` post-Inserted hook)

**Goal:** When `insert_event` returns `InsertOutcome::Inserted` AND the event is a `PendingJoin` AND self has power ≥ invite_threshold AND no existing self-authored JoinCountersign for this target, spawn the JoinCountersign emit.

- [ ] **Step 1: Locate the existing post-Inserted hook**

```bash
grep -n "InsertOutcome::Inserted\|on_inserted\|notify_pending_redemption_in_map" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/community_state_sync.rs | head -15
```

The existing pattern: after `state.insert_event(event, &ctx) → Inserted`, call hooks. ZEB-254 adds two new hooks: admin auto-counter-sign (this task) and joiner Space update (Task 11).

- [ ] **Step 2: Identify what context the hook needs**

The hook needs:
- `self_owner: OwnerAddr` (which device am I?)
- `signing_key: Arc<SigningKey>` (to sign the JoinCountersign)
- `engine_arc` reference (to call insert_local_event from the spawned task)
- `community_id: SpaceId`
- `hlc_tracker: Arc<Mutex<HlcTracker>>` + `device_id` (to reserve HLC)
- Snapshot of `state.events` for idempotency check (already self-counter-signed?)
- Self's current Joined status + power level

Most of these already live on the `CommunitySyncEngine` struct. Add the auto-counter-sign function as a method on `CommunitySyncEngine`.

- [ ] **Step 3: Add the auto-counter-sign helper**

In `community_state_sync.rs` (in the `impl CommunitySyncEngine` block):

```rust
    /// ZEB-254: when a PendingJoin lands in this engine's state, check
    /// self-eligibility (Joined + power >= invite_threshold + no
    /// self-authored JoinCountersign for this target yet) and spawn a
    /// task to emit a JoinCountersign event.
    fn maybe_spawn_auto_counter_sign(
        &self,
        pending_event: &SignedMembershipEvent,
        state_snapshot: &CommunityState,
    ) {
        // Only act on PendingJoin kind.
        let MembershipEventKind::PendingJoin { .. } = &pending_event.kind else {
            return;
        };

        // Build materialized membership from the snapshot's events,
        // using the snapshot's full event log. Iteration order doesn't
        // matter for the self-eligibility check.
        let events: Vec<SignedMembershipEvent> = state_snapshot.events.values().cloned().collect();
        let mat = crate::community_membership::materialize(&events, self.admin_addr);

        let self_status = mat.members.get(&self.self_owner).map(|m| m.status);
        let self_power = mat.power_levels.get(&self.self_owner).copied().unwrap_or(0);

        if self_status != Some(crate::community_membership::MemberStatus::Joined) {
            return;
        }
        if self_power < crate::community_membership::POWER_THRESHOLDS.invite {
            return;
        }

        // Idempotency: skip if self-authored JoinCountersign for this
        // pending event already exists.
        let already_signed = state_snapshot.events.values().any(|e| {
            e.actor == self.self_owner
                && matches!(
                    &e.kind,
                    crate::community_membership::MembershipEventKind::JoinCountersign { target_event_id }
                    if *target_event_id == pending_event.id
                )
        });
        if already_signed {
            return;
        }

        // Spawn the emit. We can't reuse `self` directly across the
        // task boundary because of Send/Sync — clone the needed Arcs.
        let pending_id = pending_event.id;
        let community_id = self.community_id;
        let self_owner = self.self_owner;
        let signing_key = std::sync::Arc::clone(&self.signing_key);
        let engine_arc_weak = std::sync::Arc::downgrade(&self.self_arc);
        let hlc_tracker = std::sync::Arc::clone(&self.hlc_tracker);
        let device_id = self.device_id;

        tokio::spawn(async move {
            // Reserve HLC for the JoinCountersign event.
            let wall_now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let cs_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
                &hlc_tracker,
                &device_id,
                wall_now_ms,
            ).await;

            let cs_payload = crate::community_membership::EventPayload {
                id: {
                    use rand::RngCore;
                    let mut id = [0u8; 16];
                    rand::thread_rng().fill_bytes(&mut id);
                    id
                },
                community_id,
                kind: crate::community_membership::MembershipEventKind::JoinCountersign {
                    target_event_id: pending_id,
                },
                actor: self_owner,
                at: cs_hlc,
            };
            let signed_cs = match crate::community_membership::sign_event(&cs_payload, &signing_key) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "ZEB-254 auto-counter-sign: sign_event failed");
                    return;
                }
            };

            let engine_arc = match engine_arc_weak.upgrade() {
                Some(e) => e,
                None => {
                    tracing::debug!("ZEB-254 auto-counter-sign: engine dropped before emit");
                    return;
                }
            };
            if let Err(e) = engine_arc.insert_local_event(signed_cs).await {
                tracing::warn!(error = ?e, "ZEB-254 auto-counter-sign: insert failed");
            }
        });
    }
```

**Plumbing:** `self.self_arc` needs to be an `Arc<Self>` carried on the engine — the engine MUST already hold one for various Sub-C v1 reasons. Search `grep -n "self_arc\|Arc<Self>\|self_strong" src-tauri/src/community_state_sync.rs` to confirm. If it doesn't exist, the implementer needs to plumb it through `CommunitySyncEngine::new`. **If self_arc plumbing is complex,** an alternative is to pass the necessary `Arc<Mutex<CommunityState>>` and use a free-function `insert_event_via_state` — adjust accordingly.

- [ ] **Step 4: Wire the hook into insert paths**

The existing `insert_local_event` / `insert_local_event_with_pubs` calls in `community_state_sync.rs` have an `InsertOutcome::Inserted` arm. Add a call to `self.maybe_spawn_auto_counter_sign(&event, &state)` inside that arm (where `state` is the locked guard).

Also wire it into the merge path that runs when state-root publishes arrive (around `community_state_sync.rs:2556` `match state.insert_event(...)` — the `InsertOutcome::Inserted` arm). In that path, the event is `event_clone` and the state is the locked guard.

- [ ] **Step 5: Write tests**

In `community_state_sync.rs` tests:

```rust
#[cfg(test)]
mod zeb_254_auto_counter_sign_tests {
    use super::*;
    // ...

    #[tokio::test]
    async fn admin_engine_auto_counter_signs_on_pending_join_insert() {
        // Spin up a CommunitySyncEngine with self as a Joined admin.
        // Insert a synthetic PendingJoin from a non-member joiner.
        // Drain the engine's event log + assert that a JoinCountersign
        // authored by self with target=PendingJoin.id is present.
        //
        // [Use the existing CommunitySyncEngine test fixture for the
        // harness setup. Look at existing tests like
        // `engine_processes_kick_event` for the template.]
    }

    #[tokio::test]
    async fn admin_engine_idempotent_no_duplicate_counter_sign() {
        // Insert the same PendingJoin twice. Verify exactly one
        // JoinCountersign exists.
    }

    #[tokio::test]
    async fn non_admin_engine_does_not_auto_counter_sign() {
        // Self is not Joined (or has power = 0 with threshold > 0).
        // Insert PendingJoin. Verify NO JoinCountersign is emitted.
    }

    #[tokio::test]
    async fn kicked_admin_does_not_auto_counter_sign() {
        // Self was Joined+power, then Kicked. Verify hook does not fire.
    }
}
```

The implementer fills in the bodies using the existing fixture harness (search `grep -n "spawn_engine_for_test\|build_test_engine" src-tauri/src/community_state_sync.rs`).

- [ ] **Step 6: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(zeb_254_auto_counter_sign_tests)'
```

- [ ] **Step 7: Clippy + fmt + commit**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): admin engine auto-counter-signs PendingJoin on insert

CommunitySyncEngine post-Inserted hook: on PendingJoin event +
self.power >= invite_threshold + self.Joined + no existing self-
authored JoinCountersign for this target, spawn a task that signs
+ inserts JoinCountersign. Idempotent: duplicate PendingJoin inserts
don't double-emit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: Engine post-Inserted hook — joiner clears Space.pending_join_at on JoinCountersign

**Files:**
- Modify: `src-tauri/src/community_state_sync.rs` (post-Inserted hook for `JoinCountersign`)
- Modify: `src-tauri/src/lib.rs` (`nav-updated` emit helper, if needed)

**Goal:** When a `JoinCountersign(target=X)` lands in the joiner's engine AND X is a PendingJoin authored by self, the joiner-side hook:
1. Resolves the existing `pending_redemptions[X]` oneshot if registered (fast-path window still open).
2. Enqueues a Space update: `pending_join_at = None`, `updated_at = current_hlc`.
3. Emits `nav-updated { modified, pending: false }` Tauri event.

- [ ] **Step 1: Identify the existing pending_redemptions notifier**

```bash
grep -n "notify_pending_redemption_in_map\|pending_redemptions\|take_pending_redemption" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/community_state_sync.rs | head -15
```

The existing function resolves a oneshot keyed on a `Join` event_id (legacy path). ZEB-254 generalizes it to also resolve on JoinCountersign's `target_event_id`.

- [ ] **Step 2: Generalize the notifier**

Update `notify_pending_redemption_in_map` (or wherever the notification happens). For each inserted event:

```rust
    fn notify_pending_redemption_for_insert(
        &self,
        inserted_event: &SignedMembershipEvent,
    ) {
        // ZEB-254: pending_redemptions are keyed on event_id of the
        // PendingJoin. The notification can come from either:
        //   - Inserting a legacy Join with countersig=Some (handled by
        //     existing path — fires on the Join's event_id).
        //   - Inserting a JoinCountersign — fires on
        //     target_event_id.
        let key = match &inserted_event.kind {
            crate::community_membership::MembershipEventKind::JoinCountersign { target_event_id } => {
                Some(*target_event_id)
            }
            // Legacy invite-only Join with countersig — existing path.
            crate::community_membership::MembershipEventKind::Join => {
                if inserted_event.countersig.is_some() {
                    Some(inserted_event.id)
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(key) = key {
            // Take + send the oneshot for this key, if registered.
            // (Existing implementation — adjust the lookup to use `key`.)
        }
    }
```

- [ ] **Step 3: Add Space pending_join_at clear hook**

In the same insert path, after the notifier:

```rust
    fn maybe_clear_pending_join_at_on_self_countersign(
        &self,
        inserted_event: &SignedMembershipEvent,
        state_snapshot: &CommunityState,
    ) {
        // Fires when a JoinCountersign lands targeting a PendingJoin
        // authored by self (the joiner side).
        let crate::community_membership::MembershipEventKind::JoinCountersign { target_event_id } = &inserted_event.kind else {
            return;
        };

        // Look up the target PendingJoin in state.
        let target = state_snapshot.events.get(target_event_id);
        let target = match target {
            Some(t) => t,
            None => return, // out-of-order: target hasn't arrived yet; clear happens when it does
        };

        // Is the target a self-authored PendingJoin?
        if target.actor != self.self_owner {
            return;
        }
        if !matches!(
            &target.kind,
            crate::community_membership::MembershipEventKind::PendingJoin { .. }
        ) {
            return;
        }

        // We are the joiner; admin counter-signed our pending join.
        // Spawn the Space update.
        let community_id = self.community_id;
        let app_handle = self.app_handle.clone(); // assumes engine has Tauri AppHandle
        let crdt_state = std::sync::Arc::clone(&self.crdt_state); // assumes engine has owner-state arc
        let hlc_tracker = std::sync::Arc::clone(&self.hlc_tracker);
        let device_id = self.device_id;
        let space_name = self.space_name_snapshot.clone(); // see below

        tokio::spawn(async move {
            let wall_now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let new_hlc = crate::dm_outbox::reserve_next_hlc_for_device(
                &hlc_tracker,
                &device_id,
                wall_now_ms,
            ).await;

            // Apply Space update.
            let mut state_g = crdt_state.lock().await;
            if let Some(space) = state_g.spaces.get(&crate::owner_state_types::SpaceId(community_id.0)).cloned() {
                let mut updated = space;
                updated.pending_join_at = None;
                updated.updated_at = new_hlc.clone();
                let outcome = state_g.apply_space_with_canonicalization(updated);
                drop(state_g);
                tracing::info!(
                    community_id = %hex::encode(community_id.0),
                    outcome = ?outcome,
                    "ZEB-254 joiner-side: cleared Space.pending_join_at after JoinCountersign"
                );
            }

            // Emit nav-updated.
            if let Some(app) = app_handle {
                use tauri::Emitter;
                let payload = serde_json::json!({
                    "action": "modified",
                    "spaceId": hex::encode(community_id.0),
                    "kind": "community",
                    "name": space_name,
                    "pending": false,
                });
                let _ = app.emit("nav-updated", payload);
            }
        });
    }
```

The implementer needs to:
- Verify the engine carries an `app_handle` / `AppHandle` (search `grep -n "AppHandle\|app_handle" src-tauri/src/community_state_sync.rs`). If not, accept a `nav_updated_tx: mpsc::Sender<NavUpdatedPayload>` channel plumbed through `lib.rs`.
- Verify `crdt_state` is accessible from the engine. If not, plumb it through.

For `space_name_snapshot`, the engine should cache the community Space's name at spawn time (it's needed for the `nav-updated` payload). If not cached, look it up from `crdt_state.spaces` at hook time.

- [ ] **Step 4: Wire the new hook into both insert paths**

Add `self.maybe_clear_pending_join_at_on_self_countersign(&event, &state)` calls into both:
- `insert_local_event` / `insert_local_event_with_pubs` `InsertOutcome::Inserted` arm
- State-root merge path `InsertOutcome::Inserted` arm (around line 2556)

- [ ] **Step 5: Tests**

```rust
    #[tokio::test]
    async fn joiner_engine_clears_pending_join_at_on_countersign() {
        // Set up: joiner has a Space with pending_join_at = Some.
        // Insert a JoinCountersign(target = self's PendingJoin).
        // Assert: Space.pending_join_at flips to None.
    }

    #[tokio::test]
    async fn joiner_engine_emits_nav_updated_on_countersign() {
        // Set up: joiner engine with a recording AppHandle/event stream.
        // Insert JoinCountersign. Assert: a `nav-updated` event with
        // `pending: false` was emitted.
    }
```

- [ ] **Step 6: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(joiner_engine_clears_pending_join_at_on_countersign) | test(joiner_engine_emits_nav_updated)'
```

- [ ] **Step 7: Clippy + fmt + commit**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): joiner-side hook clears Space.pending_join_at on JoinCountersign

Post-Inserted hook for JoinCountersign: if target is a self-authored
PendingJoin, spawn a Space update setting pending_join_at = None and
emit nav-updated { pending: false }. Resolves the pending_redemptions
oneshot (fast-path) and ungreys the community in the joiner's nav.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 12: New IPCs — list_pending_joins + list_recent_counter_signs

**Files:**
- Modify: `src-tauri/src/lib.rs` (add two new `#[tauri::command]` functions + DTOs + register in invoke_handler)

**Goal:** Two new Tauri IPCs for the admin audit UI.

- [ ] **Step 1: Add DTOs**

In `lib.rs` (near other `RedeemInviteResultDto`-style structs):

```rust
/// ZEB-254: one entry in the admin's "Awaiting counter-sign" feed.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingJoinDto {
    pub event_id: String,
    pub joiner_addr: String,
    pub pending_at_hlc: crate::community_channel_log_engine::HlcDto,
    pub invitee_hint: Option<String>,
}

/// ZEB-254: one entry in the admin's "Recent joins" feed.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CounterSignDto {
    pub join_event_id: String,
    pub joiner_addr: String,
    pub countersigned_at_hlc: crate::community_channel_log_engine::HlcDto,
}
```

- [ ] **Step 2: Add `list_pending_joins`**

```rust
/// ZEB-254: admin audit feed — pending joins awaiting counter-sign.
/// Returns PendingJoin events that do NOT yet have a matching
/// JoinCountersign AND are within the 30-day expiry window. Sorted by
/// pending_at_hlc ascending (oldest first).
#[tauri::command]
async fn list_pending_joins(
    community_id: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<PendingJoinDto>, String> {
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid hex: {e}"))?
        .try_into()
        .map_err(|v: Vec<u8>| format!("expected 16 bytes, got {}", v.len()))?;
    let space_id = crate::owner_state_types::SpaceId(cid_bytes);

    let community_registry = {
        let g = state.lock().expect("lock");
        match &*g {
            NodeState::Running(r) => std::sync::Arc::clone(&r.community_registry),
            _ => return Err("node not running".into()),
        }
    };

    let community_state = match community_registry.state_for(&space_id).await {
        Some(s) => s,
        None => return Err(format!("community {} not found", community_id)),
    };

    let state_g = community_state.lock().await;
    let events: Vec<_> = state_g.events.values().cloned().collect();
    drop(state_g);

    let max_wall_ms: u64 = events.iter().map(|e| e.at.wall_ms).max().unwrap_or(0);
    let expiry_threshold = max_wall_ms.saturating_sub(crate::community_membership::MATERIALIZE_PENDING_EXPIRY_MS);

    let countersigned: std::collections::HashSet<crate::community_membership::EventId> = events
        .iter()
        .filter_map(|e| match &e.kind {
            crate::community_membership::MembershipEventKind::JoinCountersign { target_event_id } => Some(*target_event_id),
            _ => None,
        })
        .collect();

    let mut out: Vec<PendingJoinDto> = Vec::new();
    for event in &events {
        if let crate::community_membership::MembershipEventKind::PendingJoin { invite_token, .. } = &event.kind {
            if countersigned.contains(&event.id) {
                continue;
            }
            if event.at.wall_ms < expiry_threshold {
                continue;
            }
            out.push(PendingJoinDto {
                event_id: hex::encode(event.id),
                joiner_addr: hex::encode(event.actor.0),
                pending_at_hlc: crate::community_channel_log_engine::HlcDto::from(&event.at),
                invitee_hint: invite_token.invitee_hint.map(|h| hex::encode(h.0)),
            });
        }
    }
    out.sort_by_key(|p| (p.pending_at_hlc.wall_ms, p.pending_at_hlc.logical));
    Ok(out)
}
```

- [ ] **Step 3: Add `list_recent_counter_signs`**

```rust
/// ZEB-254: admin audit feed — recent counter-signs by self. Sorted by
/// countersigned_at_hlc descending (most recent first). Limit caps result
/// size; pass 0 to mean default 20.
#[tauri::command]
async fn list_recent_counter_signs(
    community_id: String,
    limit: u32,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<Vec<CounterSignDto>, String> {
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid hex: {e}"))?
        .try_into()
        .map_err(|v: Vec<u8>| format!("expected 16 bytes, got {}", v.len()))?;
    let space_id = crate::owner_state_types::SpaceId(cid_bytes);
    let cap = if limit == 0 { 20 } else { limit as usize };

    let (community_registry, self_owner) = {
        let g = state.lock().expect("lock");
        match &*g {
            NodeState::Running(r) => (std::sync::Arc::clone(&r.community_registry), r.self_owner),
            _ => return Err("node not running".into()),
        }
    };

    let community_state = match community_registry.state_for(&space_id).await {
        Some(s) => s,
        None => return Err(format!("community {} not found", community_id)),
    };

    let state_g = community_state.lock().await;
    let events: Vec<_> = state_g.events.values().cloned().collect();
    drop(state_g);

    // Map event_id → joiner_addr for JoinCountersign target lookups.
    let pending_actors: std::collections::HashMap<crate::community_membership::EventId, crate::owner_state_types::OwnerAddr> = events
        .iter()
        .filter_map(|e| match &e.kind {
            crate::community_membership::MembershipEventKind::PendingJoin { .. } => Some((e.id, e.actor)),
            _ => None,
        })
        .collect();

    let mut out: Vec<CounterSignDto> = events
        .iter()
        .filter(|e| e.actor == self_owner)
        .filter_map(|e| match &e.kind {
            crate::community_membership::MembershipEventKind::JoinCountersign { target_event_id } => {
                let joiner_addr = pending_actors.get(target_event_id)
                    .map(|a| hex::encode(a.0))
                    .unwrap_or_else(|| "(unknown — target missing)".into());
                Some(CounterSignDto {
                    join_event_id: hex::encode(target_event_id),
                    joiner_addr,
                    countersigned_at_hlc: crate::community_channel_log_engine::HlcDto::from(&e.at),
                })
            }
            _ => None,
        })
        .collect();

    out.sort_by(|a, b| b.countersigned_at_hlc.wall_ms.cmp(&a.countersigned_at_hlc.wall_ms));
    out.truncate(cap);
    Ok(out)
}
```

- [ ] **Step 4: Register the new IPCs**

Locate `tauri::generate_handler!` in `lib.rs` (search `grep -n "generate_handler" src-tauri/src/lib.rs`). Add `list_pending_joins` and `list_recent_counter_signs` to the handler list.

- [ ] **Step 5: Test**

```rust
#[tokio::test]
async fn list_pending_joins_returns_pending_only() {
    // Seed community state with: 1 PendingJoin, 1 PendingJoin+JoinCountersign,
    // 1 expired PendingJoin (no countersign, > 30d old), 1 Join.
    // Call list_pending_joins(community_id).
    // Assert: returns exactly 1 entry (the un-countersigned, non-expired PendingJoin).
}

#[tokio::test]
async fn list_recent_counter_signs_returns_self_authored() {
    // Seed: 2 JoinCountersigns by self, 1 by another admin.
    // Call list_recent_counter_signs(community_id, limit=10).
    // Assert: returns 2 entries (only self-authored).
}
```

- [ ] **Step 6: Verify tests pass**

```bash
cd src-tauri && cargo nextest run --features test-fixtures -E 'test(list_pending_joins) | test(list_recent_counter_signs)'
```

- [ ] **Step 7: Clippy + fmt + commit**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): list_pending_joins + list_recent_counter_signs IPCs

Admin audit feeds: pending joins awaiting counter-sign (filtered for
not-countersigned + within 30d) and self-authored counter-signs sorted
recent-first (default limit 20).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 13: Frontend — RedeemInviteWizard pending handling + NavService greyed render

**Files:**
- Modify: `src/lib/components/RedeemInviteWizard.svelte`
- Modify: `src/lib/nav-service.ts`

**Goal:** Frontend caller handles `pending: true` from redeem_invite; greys community in nav; listens for `nav-updated { pending: false }` to ungrey.

- [ ] **Step 1: Update RedeemInviteWizard**

Read the existing component:

```bash
cat /Users/zeblith/work/zeblithic/harmony-client/src/lib/components/RedeemInviteWizard.svelte
```

Locate the `invoke('redeem_invite', ...)` call. Update the success handler:

```svelte
<script lang="ts">
    // ...existing imports...
    import { toast } from '$lib/stores/toast'; // or whatever toast helper exists

    interface RedeemInviteResultDto {
        communityId: string;
        communityName: string;
        isInviteOnly: boolean;
        pending: boolean;
    }

    async function handleRedeem() {
        try {
            const result = await invoke<RedeemInviteResultDto>('redeem_invite', {
                inviteUrl: url,
                comment: commentValue || undefined,
            });
            if (result.pending) {
                toast.show(`Join request sent for "${result.communityName}". The community will unlock once an admin approves.`);
            } else {
                toast.show(`Joined "${result.communityName}"!`);
            }
            await navService.refresh();
            dismiss();
        } catch (e) {
            const msg = e instanceof Error ? e.message : String(e);
            errorMessage = msg;
        }
    }
</script>
```

- [ ] **Step 2: Update NavService — greyed render + nav-updated handler**

In `src/lib/nav-service.ts`:

```typescript
// ... existing code ...

interface NavNode {
    // ...existing fields...
    pending?: boolean; // ZEB-254: true if community Space.pending_join_at !== null
}

function spaceToNavNode(space: Space): NavNode {
    return {
        id: space.id,
        kind: space.kind,
        name: space.name,
        // ZEB-254: surface pending state from Space.pendingJoinAt (camelCase post-tauri).
        pending: space.pendingJoinAt != null,
        // ... other fields ...
    };
}

// In the nav-updated listener:
function onNavUpdated(payload: NavUpdatedPayload) {
    // ...existing logic...
    if (payload.action === 'modified' && payload.pending === false) {
        // ZEB-254: community ungreyed.
        const node = nodes.find((n) => n.id === payload.spaceId);
        if (node) {
            node.pending = false;
            toast.show(`You're in "${node.name}"!`);
        }
        refreshNavTree();
    }
}
```

- [ ] **Step 3: CSS / template — greyed state**

In the nav component (likely `src/lib/components/NavTree.svelte` or `Sidebar.svelte`), apply a CSS class when `node.pending`:

```svelte
<div class="nav-node" class:pending={node.pending}>
    {node.name}
    {#if node.pending}
        <span class="pending-badge" title="Waiting for admin to approve your join request">⏳</span>
    {/if}
</div>

<style>
    .nav-node.pending {
        opacity: 0.55;
        font-style: italic;
    }
    .pending-badge {
        font-size: 0.8em;
        margin-left: 0.4em;
    }
</style>
```

- [ ] **Step 4: Frontend tests**

In a test file `src/lib/components/RedeemInviteWizard.test.ts` (create if it doesn't exist):

```typescript
import { describe, test, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import RedeemInviteWizard from './RedeemInviteWizard.svelte';

vi.mock('@tauri-apps/api/core', () => ({
    invoke: vi.fn(),
}));

describe('RedeemInviteWizard', () => {
    beforeEach(() => {
        vi.clearAllMocks();
    });

    test('shows pending toast and dismisses when result.pending=true', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockResolvedValue({
            communityId: 'abc',
            communityName: 'Test',
            isInviteOnly: true,
            pending: true,
        });
        // ... test body — mount component, fire click, assert toast call ...
    });

    test('shows joined toast when result.pending=false', async () => {
        // similar but pending: false
    });
});
```

Add `src/lib/nav-service.test.ts`:

```typescript
test('nav-updated with pending=false ungreys the community node', () => {
    // ...
});
```

- [ ] **Step 5: Run tsc + vitest**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: all tests pass, no type errors.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): RedeemInviteWizard pending + NavService greyed render

Wizard branches on result.pending: true → "Join request sent" toast,
false → "You're in!" toast. NavService renders pending communities
with reduced opacity + ⏳ badge; listens for nav-updated {pending:false}
to ungrey.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 14: Frontend — PendingJoinsPanel component + CommunitySettingsPanel mount

**Files:**
- Create: `src/lib/components/PendingJoinsPanel.svelte`
- Create: `src/lib/components/PendingJoinsPanel.test.ts`
- Modify: `src/lib/components/CommunitySettingsPanel.svelte`

**Goal:** Admin-tier viewers see two collapsible sections in CommunitySettingsPanel: "Awaiting counter-sign" + "Recent joins".

- [ ] **Step 1: Create PendingJoinsPanel.svelte**

```svelte
<script lang="ts">
    import { onMount, onDestroy } from 'svelte';
    import { invoke } from '@tauri-apps/api/core';
    import { listen, type UnlistenFn } from '@tauri-apps/api/event';

    export let communityId: string;
    export let canModerate: boolean;

    interface PendingJoinDto {
        eventId: string;
        joinerAddr: string;
        pendingAtHlc: { wallMs: number; logical: number; deviceId: string };
        inviteeHint?: string;
    }

    interface CounterSignDto {
        joinEventId: string;
        joinerAddr: string;
        countersignedAtHlc: { wallMs: number; logical: number; deviceId: string };
    }

    let pending: PendingJoinDto[] = [];
    let recent: CounterSignDto[] = [];
    let convergedUnlisten: UnlistenFn | null = null;
    let errorMessage = '';

    async function refresh() {
        try {
            pending = await invoke<PendingJoinDto[]>('list_pending_joins', { communityId });
            recent = await invoke<CounterSignDto[]>('list_recent_counter_signs', { communityId, limit: 20 });
            errorMessage = '';
        } catch (e) {
            errorMessage = e instanceof Error ? e.message : String(e);
        }
    }

    async function kickJoiner(joinerAddr: string) {
        try {
            await invoke('kick', { communityId, targetAddr: joinerAddr, reason: 'Manually rejected pending join' });
            await refresh();
        } catch (e) {
            errorMessage = e instanceof Error ? e.message : String(e);
        }
    }

    function formatHlc(hlc: { wallMs: number }): string {
        return new Date(hlc.wallMs).toLocaleString();
    }

    onMount(async () => {
        await refresh();
        convergedUnlisten = await listen('community-state-sync-converged', async (evt) => {
            if ((evt.payload as any).communityId === communityId) {
                await refresh();
            }
        });
    });

    onDestroy(() => {
        convergedUnlisten?.();
    });
</script>

{#if canModerate}
    <section class="pending-joins-panel">
        {#if errorMessage}
            <p class="error">{errorMessage}</p>
        {/if}

        <details open={pending.length > 0}>
            <summary>Awaiting counter-sign ({pending.length})</summary>
            {#if pending.length === 0}
                <p class="muted">No pending join requests.</p>
            {:else}
                <ul>
                    {#each pending as p (p.eventId)}
                        <li>
                            <span class="joiner">{p.inviteeHint ?? p.joinerAddr.slice(0, 8)}</span>
                            <span class="time">since {formatHlc(p.pendingAtHlc)}</span>
                            <button on:click={() => kickJoiner(p.joinerAddr)}>Reject (kick)</button>
                        </li>
                    {/each}
                </ul>
            {/if}
        </details>

        <details>
            <summary>Recent joins ({recent.length})</summary>
            {#if recent.length === 0}
                <p class="muted">No recent counter-signs.</p>
            {:else}
                <ul>
                    {#each recent as r (r.joinEventId)}
                        <li>
                            <span class="joiner">{r.joinerAddr.slice(0, 8)}</span>
                            <span class="time">at {formatHlc(r.countersignedAtHlc)}</span>
                        </li>
                    {/each}
                </ul>
            {/if}
        </details>
    </section>
{/if}

<style>
    .pending-joins-panel { margin: 1em 0; }
    .pending-joins-panel ul { list-style: none; padding: 0; }
    .pending-joins-panel li { padding: 0.4em 0; display: flex; gap: 0.6em; align-items: center; }
    .joiner { font-weight: 600; }
    .time { color: #999; font-size: 0.9em; }
    .muted { color: #999; }
    .error { color: #c33; }
</style>
```

- [ ] **Step 2: Mount in CommunitySettingsPanel**

In `src/lib/components/CommunitySettingsPanel.svelte`, import + mount:

```svelte
<script lang="ts">
    import PendingJoinsPanel from './PendingJoinsPanel.svelte';
    // ...existing imports...
</script>

<!-- existing settings content -->

<PendingJoinsPanel {communityId} {canModerate} />
```

Where `canModerate` is derived from the user's power level in the community (existing logic).

- [ ] **Step 3: Tests**

In `src/lib/components/PendingJoinsPanel.test.ts`:

```typescript
import { describe, test, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, screen } from '@testing-library/svelte';
import PendingJoinsPanel from './PendingJoinsPanel.svelte';

vi.mock('@tauri-apps/api/core', () => ({
    invoke: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
    listen: vi.fn().mockResolvedValue(() => {}),
}));

describe('PendingJoinsPanel', () => {
    beforeEach(() => {
        vi.clearAllMocks();
    });

    test('renders 2 pending joins as 2 rows', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation((cmd: string) => {
            if (cmd === 'list_pending_joins') {
                return Promise.resolve([
                    { eventId: 'aaa', joinerAddr: '11223344', pendingAtHlc: { wallMs: 1700000000000, logical: 0, deviceId: '00' }, inviteeHint: 'alice' },
                    { eventId: 'bbb', joinerAddr: '55667788', pendingAtHlc: { wallMs: 1700000001000, logical: 0, deviceId: '00' } },
                ]);
            }
            if (cmd === 'list_recent_counter_signs') {
                return Promise.resolve([]);
            }
            return Promise.resolve(null);
        });
        const { container } = render(PendingJoinsPanel, { props: { communityId: 'abc', canModerate: true } });
        // Allow microtask for onMount + invoke promise.
        await new Promise((r) => setTimeout(r, 0));
        expect(container.querySelectorAll('li').length).toBeGreaterThanOrEqual(2);
    });

    test('Kick button calls kick IPC with correct args', async () => {
        const { invoke } = await import('@tauri-apps/api/core');
        (invoke as any).mockImplementation((cmd: string) => {
            if (cmd === 'list_pending_joins') {
                return Promise.resolve([
                    { eventId: 'aaa', joinerAddr: '11223344', pendingAtHlc: { wallMs: 1700000000000, logical: 0, deviceId: '00' } },
                ]);
            }
            if (cmd === 'list_recent_counter_signs') return Promise.resolve([]);
            return Promise.resolve(null);
        });
        const { getByText } = render(PendingJoinsPanel, { props: { communityId: 'abc', canModerate: true } });
        await new Promise((r) => setTimeout(r, 0));
        const btn = getByText(/reject/i);
        await fireEvent.click(btn);
        await new Promise((r) => setTimeout(r, 0));
        expect(invoke).toHaveBeenCalledWith('kick', expect.objectContaining({
            communityId: 'abc',
            targetAddr: '11223344',
        }));
    });

    test('Does not render when canModerate=false', () => {
        const { container } = render(PendingJoinsPanel, { props: { communityId: 'abc', canModerate: false } });
        expect(container.querySelector('.pending-joins-panel')).toBeNull();
    });
});
```

- [ ] **Step 4: Run tsc + vitest**

```bash
npx tsc --noEmit
npx vitest run
```

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-254): PendingJoinsPanel + CommunitySettingsPanel mount

Admin audit UI: "Awaiting counter-sign" + "Recent joins" sections,
both collapsible, both refreshed on community-state-sync-converged.
Kick button on each pending row routes to the existing kick IPC.
Component is gated on canModerate (admin-tier viewers only).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 15: Integration tests — two-engine end-to-end

**Files:**
- Create: `src-tauri/tests/community_pending_join_integration.rs`

**Goal:** Six tests covering the full pending → counter-sign → joined flow with two engines (joiner + admin) and realistic Zenoh state-root sync.

- [ ] **Step 1: Read an existing integration test as a template**

```bash
ls /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/ | grep -i community
head -100 /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/community_state_sync_integration.rs 2>/dev/null || ls /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/*.rs
```

Identify the two-engine test harness pattern.

- [ ] **Step 2: Create the file**

```bash
cat > /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/community_pending_join_integration.rs << 'EOF'
//! ZEB-254 integration tests: end-to-end pending-Join → counter-sign → joined
//! flow with two CommunitySyncEngines (joiner + admin) connected via a
//! shared in-memory Zenoh transport.

use harmony_app::community_membership::{
    MembershipEventKind, MemberStatus, materialize, sign_event, EventPayload,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};
// (additional imports per the existing community_state_sync test harness)

/// Helper: spin up two engines (joiner + admin) sharing a community.
/// Returns handles to drive events between them.
async fn two_engine_harness() -> TestHarness {
    // The implementer should READ the existing two-engine harness:
    //
    //   ls /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/ | grep -i community
    //   grep -nE "fn build_test_engine|fn spawn_engine_for_test|TestHarness|two_engine" \
    //       /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tests/community_*_integration.rs
    //
    // ZEB-249 (kick + epoch-rotation), ZEB-262 (invite-only flow), and
    // ZEB-285 (community-forking) all ship two-engine integration tests
    // — the most-recently-merged of those uses the canonical harness.
    // Port it into a helper function in this file or pull it into a
    // shared `tests/common/community_test_harness.rs` module if there's
    // a clear seam (no need to abstract unless duplication starts).
    //
    // The harness contract this file needs:
    //
    //   pub struct TestHarness {
    //       pub joiner_engine: Arc<CommunitySyncEngine>,
    //       pub joiner_state: Arc<Mutex<CommunityState>>,
    //       pub admin_engine: Arc<CommunitySyncEngine>,
    //       pub admin_state: Arc<Mutex<CommunityState>>,
    //       pub joiner_owner: OwnerAddr,
    //       pub admin_owner: OwnerAddr,
    //       pub community_id: SpaceId,
    //   }
    //
    //   impl TestHarness {
    //       /// Drive Zenoh state-root sync: pull all events from `from`
    //       /// and merge into `to`. Blocks until convergence (small loop
    //       /// over insert_event(...) for each new event).
    //       pub async fn sync(&self, from: &Arc<CommunitySyncEngine>, to: &Arc<CommunitySyncEngine>) { ... }
    //
    //       /// Drive both directions of sync until both engines have the
    //       /// same set of event_ids.
    //       pub async fn sync_both(&self) { ... }
    //   }
    unimplemented!("see comment block above; this is a doc placeholder for the implementer")
}

#[tokio::test]
async fn pending_join_resolves_when_admin_comes_online() {
    // 1. Spin up joiner engine; admin engine offline.
    // 2. Joiner mints PendingJoin, inserts into local engine.
    // 3. Joiner's state-root publishes via in-memory Zenoh.
    // 4. Spin up admin engine; admin receives PendingJoin via state-root sync.
    // 5. Admin auto-counter-signs (post-Inserted hook).
    // 6. Admin's state-root publishes JoinCountersign.
    // 7. Joiner receives JoinCountersign.
    // 8. Materialize on joiner: joiner_addr → MemberStatus::Joined.
    // 9. Joiner's Space.pending_join_at clears to None.
}

#[tokio::test]
async fn pending_join_survives_joiner_restart() {
    // 1. Joiner mints + publishes PendingJoin. Admin offline.
    // 2. Joiner shuts down engine + disk-persist.
    // 3. Joiner restarts; engine respawns from persisted state.
    // 4. Pending Join re-publishes via state-root.
    // 5. Admin starts; receives + counter-signs.
    // 6. Joiner observes; status flips to Joined.
}

#[tokio::test]
async fn pending_join_resolves_under_two_admin_race() {
    // 1. Two admin engines online, both Joined+power.
    // 2. Joiner mints + publishes PendingJoin.
    // 3. Both admins auto-counter-sign.
    // 4. Materialize: both JoinCountersign events accepted; joiner Joined.
}

#[tokio::test]
async fn legacy_invite_only_join_with_countersig_still_accepted() {
    // 1. Joiner mints LEGACY `j` event with countersig=None
    //    (simulating pre-ZEB-254 client).
    // 2. Sends via Reticulum unicast (simulated channel).
    // 3. Admin handle_unicast's legacy branch attaches countersig + inserts.
    // 4. Materialize: joiner Joined.
}

#[tokio::test]
async fn pending_join_cancellation_via_leave() {
    // 1. Joiner mints PendingJoin.
    // 2. Joiner mints Leave (cancel).
    // 3. Materialize: joiner Left (Leave supersedes PendingJoin).
    // 4. Admin's auto-counter-sign may still fire — still harmless
    //    because materialize keeps Left.
}

#[tokio::test]
async fn pending_join_30d_expiry_hides_joiner() {
    // 1. Joiner mints PendingJoin at wall_ms = T.
    // 2. Other events advance community HLC to T + 31 days.
    // 3. Materialize: joiner absent from members map.
    // 4. Later JoinCountersign arrives. Materialize: joiner Joined
    //    (countersign overrides expiry).
}
EOF
```

The implementer fills in test bodies using the existing two-engine harness from `community_state_sync_integration.rs` (or equivalent).

- [ ] **Step 3: Implement test bodies progressively**

For each test:
1. Read the existing two-engine harness setup.
2. Adapt: add a joiner engine + an admin engine sharing the same community state.
3. Drive the event flow using `insert_local_event` + a small "tick all engines" helper.
4. Assert on the final materialized state.

- [ ] **Step 4: Run the integration tests**

```bash
cd src-tauri && cargo nextest run --features test-fixtures --test community_pending_join_integration
```

Expected: 6 tests pass.

- [ ] **Step 5: Run the FULL workspace test suite**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: every test in the project passes. This is the regression gate before push.

- [ ] **Step 6: Clippy + fmt + commit**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo fmt --all
git add -A
git commit -m "$(cat <<'EOF'
test(zeb-254): two-engine integration tests for pending-Join flow

Six end-to-end tests covering: pending resolves when admin comes
online, joiner restart preserves pending state, two-admin race,
legacy j+countersig still accepted, Leave cancels pending,
30d expiry hides + JoinCountersign resurrects.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 16: Final 5-gate sweep + push + PR

**Files:** none changed (verification only).

**Goal:** Run all 5 CI gates locally. Push branch. Open PR with markdown-linked Linear refs.

- [ ] **Step 1: cargo fmt**

```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: clean. If not, run `cargo fmt --all` and commit the diff with `chore(zeb-254): cargo fmt`.

- [ ] **Step 2: cargo clippy**

```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tee /tmp/zeb254-clippy.log
echo "Exit: ${PIPESTATUS[0]}"
```

Expected: exit 0 (per `feedback_pipe_exit_codes_lie` — check `${PIPESTATUS[0]}`, not the last command).

- [ ] **Step 3: cargo nextest**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb254-nextest.log
echo "Exit: ${PIPESTATUS[0]}"
```

Expected: exit 0. Compare test count to Task 0 baseline + the new tests we added (~25 new tests; baseline ~840 → expect ~865+).

- [ ] **Step 4: tsc**

```bash
npx tsc --noEmit
```

Expected: exit 0. From repo root.

- [ ] **Step 5: vitest**

```bash
npx vitest run
```

Expected: exit 0.

- [ ] **Step 6: Push the branch**

```bash
git push -u origin zeb-254-pending-join-crdt
```

- [ ] **Step 7: Create the PR**

```bash
gh pr create --title "ZEB-254: persistent offline counter-signer queue for invite-only community redemption" --body "$(cat <<'EOF'
## Summary

Closes [ZEB-254](https://linear.app/zeblith/issue/ZEB-254).

Adds a pair pattern to the community CRDT: `PendingJoin` (joiner-signed) + `JoinCountersign` (admin-signed). When the joiner clicks an invite-only invite while no admin is online, the redeem call returns `Ok { pending: true }` within 5 seconds (down from 15s). The PendingJoin event is already on the wire via the engine's state-root publisher; whenever any admin returns, their device auto-counter-signs and the joiner's community ungreys in nav.

Related: [ZEB-217](https://linear.app/zeblith/issue/ZEB-217) Sub-C v1 (parent epic, shipped), [ZEB-251](https://linear.app/zeblith/issue/ZEB-251) (per-community thresholds — out of scope here).

### Highlights

- **No CRDT merge changes.** The pair pattern keeps `events.contains_key(id) → continue` short-circuit intact.
- **Wire-compat with pre-ZEB-254 clients.** Legacy `j`+countersig=Some events continue to verify + materialize as Joined. New clients emit `g` + `y`; old clients drop the unknown variants. Pre-Alpha small population — acceptable.
- **Idempotent auto-counter-sign.** Reticulum unicast + Zenoh state-root may both deliver the same PendingJoin; the hook's check for self-authored JoinCountersign suppresses duplicates.
- **Pure-function expiry.** Stale PendingJoins (>30 days) are hidden from materialize without tombstone events. Deterministic across peers. Counter-sign always overrides expiry.

### Test plan

- [x] Unit tests for verify_event PendingJoin gates (8 tests).
- [x] Unit tests for verify_event JoinCountersign gates (3+1 tests).
- [x] Unit tests for materialize: pending → status, pairing → Joined, expiry, legacy compat, Leave supersedes, multi-countersign (7 tests).
- [x] Wire format fixtures byte-pinned (4 fixtures).
- [x] Auto-counter-sign engine hook tests (4 tests).
- [x] Joiner-side hook tests for clearing pending_join_at + nav-updated emit (2 tests).
- [x] IPC tests for list_pending_joins + list_recent_counter_signs.
- [x] Frontend RedeemInviteWizard tests for pending=true/false branches.
- [x] Frontend PendingJoinsPanel tests for render + kick + canModerate gating.
- [x] Two-engine integration tests for offline-admin + restart + admin-race + legacy-compat + Leave-cancel + 30d-expiry (6 tests).
- [x] Full 5-gate CI sweep: cargo fmt, clippy (with -D warnings), nextest --all-targets, tsc, vitest.

### Spec + plan

- Spec: `docs/specs/2026-05-15-zeb-254-pending-join-crdt-design.md`
- Plan: `docs/plans/2026-05-15-zeb-254-pending-join-crdt-plan.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

The first cross-ref `[ZEB-254]` is the only one Linear auto-closes on merge (per `feedback_linear_pr_auto_close`). Other refs are markdown-linked for context only.

- [ ] **Step 8: Confirm PR URL**

The `gh pr create` output prints the PR URL. Capture it for the autonomous monitoring loop.

---

## Spec coverage verification

- §1 Problem statement: addressed by entire PR — joiner can now redeem invite-only with no admin online.
- §2 Architecture — Pair pattern: implemented in Tasks 1, 2, 3, 4.
- §2 State-flow diagram: Tasks 7 (mint), 8 (redeem), 9 (handle_unicast), 10 (auto-counter-sign), 11 (joiner hook).
- §2 Failure modes: addressed across verify_event (Tasks 2, 3), materialize (Task 4), and integration tests (Task 15).
- §3 Wire format — MembershipEventKind additions: Task 1.
- §3 MemberStatus addition: Task 1.
- §3 Space addition: Task 5.
- §3 Wire fixtures: Task 6.
- §4 redeem_invite_inner changes: Tasks 7, 8.
- §5 Admin-side flow: Tasks 9, 10.
- §6 New IPCs: Task 12.
- §7 Frontend: Tasks 13, 14.
- §8 Backward compatibility: addressed by Task 4 (materialize for legacy `j`+countersig), Task 9 (handle_unicast legacy branch), Task 5 (skip_serializing_if), Task 15 test 4.
- §9 Testing: distributed across all tasks; Task 15 + Task 6 + Task 13/14 cover the integration + fixture + frontend cases.
- §10 Scope — single bundled PR: Task 16.
- §11 Out of scope: respected (no per-community threshold work, no M-of-N, etc.).
- §12 Acceptance: all 5 gates green in Task 16.

---

## Notes for the implementer

- **Some Task-N step bodies reference helper functions or fixture harnesses by name** (e.g. `reserve_next_hlc_for_device`, `seal_to_owner`, the two-engine integration harness). If a name doesn't match what's in the codebase, search via `grep -rn <name> src-tauri/src/` to find the actual symbol and adjust the call.
- **`#[serde(deny_unknown_fields)]` on Space:** verify this is NOT set in `owner_state_types.rs`. If it IS set, removing it is the wire-compat-safe choice; flag it in a follow-up commit on the same branch.
- **`POWER_THRESHOLDS.invite` is 0 in v1.** The `JoinCountersignActorPowerInsufficient` error is structurally present but cannot fire under v1 thresholds. The test for it is a structural no-op (asserts the constant). When ZEB-251 ships, this gate becomes active.
- **`bootstrap_join` field name in `MintedCommunity`:** ZEB-254 carries a `PendingJoin` event there, not a `Join`. A rename to `bootstrap_event` is desirable but should NOT block this PR — note in a follow-up.
