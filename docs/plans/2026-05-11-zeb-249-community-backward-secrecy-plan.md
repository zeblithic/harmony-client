# ZEB-249 Community Backward Secrecy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace v1's long-lived per-community `MembershipKey` with rotating `EpochKey`s so kicked or departed members lose decryption access to events published after their removal.

**Architecture:** Single 32-byte ChaCha20-Poly1305 key per community at any moment. Rotates on every `Kick` or `Leave` via a new `EpochRotation` CRDT event that ships an X25519-sealed copy of the new key to each remaining member. A separate non-advancing `EpochCatchup` variant handles the corner case where a kick lands between invite issuance and redemption (the new joiner would otherwise be stuck at the snapshot epoch). Atomic kick+rotation bundles use ZEB-274's `community_sync_tx` primitive. v1 `MembershipKey` is removed entirely (Harmony is pre-launch — no production communities exist).

**Tech Stack:** Rust (Tokio async, serde+ciborium for CBOR, ed25519-dalek for signatures, chacha20poly1305 for AEAD, x25519-dalek for ECDH, zeroize for key material). Tauri 2 IPC layer. CRDT layer is the existing community_state_sync + community_membership pair.

**Branch:** `zeb-249-community-backward-secrecy` (already cut from `origin/main` at `102aafc`). Spec committed at `2a41360`.

**Spec:** `docs/specs/2026-05-11-zeb-249-community-backward-secrecy-design.md`.

---

## File structure

Files modified or created across the plan:

| File | Role | Touched in |
|---|---|---|
| `src-tauri/src/owner_state_types.rs` | `EpochKey` (renamed from `MembershipKey`); `Space` epoch fields | Task 1 |
| `src-tauri/src/community_state_sync.rs` | `EncryptedEnvelope` + epoch-aware encrypt/decrypt; `EpochError` enum | Tasks 2, 6 |
| `src-tauri/src/community_membership.rs` | `EpochRotation` + `EpochCatchup` MembershipEventKind variants; materialization rules; `MaterializedMembership` epoch fields | Tasks 3, 4 |
| `src-tauri/src/community_invite.rs` | `InviteEpochSnapshot`, replace `membership_key` with `epoch_snapshot` | Task 5 |
| `src-tauri/src/lib.rs` | `create_community_inner`, `redeem_invite_inner` (Task 5); `admin_kick_member`, `leave_community` (Task 6) | Tasks 5, 6 |
| `src-tauri/src/event_loop.rs` | Self-healing observer (post-CRDT-apply check + synthesize) | Task 6 |
| `src-tauri/src/dm_signing.rs` | New `seal_to_owner` / `open_from_owner` helpers reusing X25519+ChaChaPoly hybrid for per-recipient sealed ciphertexts | Task 2 |
| `src-tauri/tests/wire_format_community_sync_fixtures.rs` | New canonical-CBOR pinning fixtures (5 total) | Tasks 2, 3, 4, 5 |
| `src-tauri/tests/community_backward_secrecy_integration.rs` | NEW — end-to-end integration tests | Tasks 5, 6 |
| All test/fixture files containing `MembershipKey` | Mechanical rename | Task 1 |

Counter to drop in Task 1: 163 occurrences of `MembershipKey` across 18 files. Counter to be added across Tasks 2–6: ~2500 LOC of net new code + ~22 new tests.

---

## Pattern reference (read before starting any task)

These existing patterns inform the plan; the plan mirrors them:

- **`DmContentKey`** at `src-tauri/src/owner_state_types.rs:265-318` — the 32-byte symmetric key newtype pattern (ZeroizeOnDrop, redacted Debug, bstr CBOR serde). `EpochKey` mirrors this precisely.
- **`MembershipEventKind`** at `src-tauri/src/community_membership.rs:22-90` — the adjacently-tagged enum pattern (`tg`/`vl` keys, 1-char variant codes). New variants `EpochRotation` (code `"r"`) and `EpochCatchup` (code `"f"`) join here.
- **`CommunitySyncRegistry::community_sync_tx`** at `src-tauri/src/community_state_sync.rs` — ZEB-274's transactional primitive (RAII guard, abort-on-drop, atomic commit). Reused for kick+rotation bundles.
- **`materialize`** at `src-tauri/src/community_membership.rs:765` — the canonical event-replay pattern. New variants get arms here.
- **`MaterializedMembership`** at `src-tauri/src/community_membership.rs:636-660` — the struct that gains `current_epoch` + `current_epoch_key` + `old_epoch_keys` + `pending_rotation_for` + `pending_catchup_for` fields.
- **Wire-format pinning** at `src-tauri/tests/wire_format_community_sync_fixtures.rs` — the canonical-byte fixture pattern. 5 new fixtures total.

---

## Task 0: Pre-flight + green-baseline confirm

**No commit.** Verify the just-cut branch is green on all 5 CI gates so any later red is unambiguously this work's doing.

**Files:** none.

- [ ] **Step 1: Confirm branch state**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git status                              # working tree clean
git rev-parse HEAD                       # should be 2a41360 (spec commit)
git rev-parse origin/main                # should be 102aafc
```

Expected: working tree clean, HEAD is `2a41360`, origin/main is `102aafc`, branch is `zeb-249-community-backward-secrecy`.

- [ ] **Step 2: cargo fmt gate**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
```

Expected: empty output, exit 0.

- [ ] **Step 3: cargo clippy gate**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: `Finished` line, no `error:` or `warning:` lines, exit 0.

- [ ] **Step 4: cargo nextest gate**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: `980 tests run: 980 passed (1 slow), 2 skipped` (or similar count), exit 0.

- [ ] **Step 5: cargo check (MSRV) gate**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo check --locked --all-targets --features test-fixtures
```

Expected: `Finished` line, exit 0.

- [ ] **Step 6: Frontend gates**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
```

Expected: empty output, exit 0.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx vitest run
```

Expected: `Test Files  127 passed (127), Tests 1580 passed (1580)` (or similar count), exit 0.

All 5 gates green → proceed to Task 1.

---

## Task 1: Rename `MembershipKey` → `EpochKey`; add Space epoch fields

**Spec §3.1, §3.2.** Mechanical rename across 18 files + 163 callsites. Add three new fields on `Space`. Update `Space::validate_invariants`. Update v1 test fixtures (the load-bearing CBOR change).

**Files (modify):**
- `src-tauri/src/owner_state_types.rs:265-318` (rename type + redacted Debug + `as_chacha_key`)
- `src-tauri/src/owner_state_types.rs:1414-1420` (replace `membership_key` field with three new fields)
- `src-tauri/src/owner_state_types.rs:1470-1660` (update `validate_invariants`)
- `src-tauri/src/owner_state_types.rs:2070-2950` (rename in test fixtures)
- `src-tauri/src/owner_state_crdt.rs` (callsite rename)
- `src-tauri/src/community_state_sync.rs` (callsite rename)
- `src-tauri/src/community_membership.rs` (no usage today, but if grep finds any, rename)
- `src-tauri/src/community_channel_log_engine.rs` (callsite rename)
- `src-tauri/src/community_channel_log.rs` (callsite rename)
- `src-tauri/src/community_invite.rs` (callsite rename — still using `MembershipKey` field; Task 5 will rip it out)
- `src-tauri/src/lib.rs` (callsite rename)
- All test files in `src-tauri/tests/` listed in the grep at the start of this plan (10 files)

- [ ] **Step 1: Mechanical rename `MembershipKey` → `EpochKey`**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
# Use perl with word-boundary to avoid renaming things like MembershipKey_v1 or comments containing the word incidentally.
perl -i -pe 's/\bMembershipKey\b/EpochKey/g' \
  src/owner_state_types.rs \
  src/community_state_sync.rs \
  src/community_channel_log_engine.rs \
  src/community_channel_log.rs \
  src/community_invite.rs \
  src/owner_state_crdt.rs \
  src/lib.rs \
  tests/community_channel_config_integration.rs \
  tests/community_invite_only_integration.rs \
  tests/community_channel_messages_integration.rs \
  tests/community_sync_registry_unit.rs \
  tests/community_sync_engine_unit.rs \
  tests/wire_format_channel_log_fixtures.rs \
  tests/community_root_hlc_tracker_unit.rs \
  tests/community_state_sync_crypto_unit.rs \
  tests/wire_format_community_fixtures.rs \
  tests/community_invite_unit.rs \
  tests/community_sync_integration.rs
```

Verify rename complete:

```bash
grep -rn "MembershipKey\b" src/ tests/
```

Expected: empty output (no remaining `MembershipKey` references).

- [ ] **Step 2: Verify rename compiles**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo check --locked --all-targets --features test-fixtures
```

Expected: `Finished`. The rename is type-system-checked end-to-end; if anything broke, this catches it.

- [ ] **Step 3: Update `EpochKey` doc comment**

Edit `src-tauri/src/owner_state_types.rs` at line 265-275 — replace the doc comment block above `pub struct EpochKey(` to reflect the rotation-on-kick semantics. Find:

```rust
/// 32-byte symmetric key for community membership-topic encryption
/// (ChaCha20-Poly1305). Wire format: bstr(32). In-memory: zeroized
/// on drop. Debug redacts bytes to avoid log leakage.
///
/// Mirrors DmContentKey precisely — same shape, different purpose.
/// Distributed via CommunityInvitePayload at invite-link generation;
/// stored in the community Space's `membership_key` field on every
/// member's owner-state CRDT (where it inherits encryption-at-rest
/// from ZEB-211).
///
/// See ZEB-217 spec §"Data model — Space struct additions".
```

Replace with:

```rust
/// 32-byte symmetric key for community membership-topic encryption
/// (ChaCha20-Poly1305) at a specific epoch. Wire format: bstr(32).
/// In-memory: zeroized on drop. Debug redacts bytes to avoid log
/// leakage.
///
/// Per-epoch — rotates on every Kick/Leave via the `EpochRotation`
/// CRDT event. The current key for new outbound events lives in
/// `Space.current_epoch_key`; historical keys (for decrypting old
/// events) live in `Space.old_epoch_keys`.
///
/// Mirrors DmContentKey precisely — same shape, different purpose.
/// Distributed via per-recipient X25519-sealed ciphertexts on every
/// rotation; the initial key ships in `CommunityInvitePayload.epoch_snapshot`.
///
/// See ZEB-249 spec §"Data model — EpochKey".
```

- [ ] **Step 4: Replace `Space.membership_key` field with three new fields**

Edit `src-tauri/src/owner_state_types.rs` at line 1414-1420. Find:

```rust
    /// Per-community symmetric key for membership topic encryption.
    /// MUST be Some for kind == Community; MUST be None otherwise.
    /// Wire: bstr(32) under "mk".  Zeroized on drop (via the
    /// MembershipKey newtype's ZeroizeOnDrop impl).
    /// See ZEB-217 spec §"Data model — Space struct additions".
    #[serde(rename = "mk", skip_serializing_if = "Option::is_none", default)]
    pub membership_key: Option<EpochKey>,
```

Replace with:

```rust
    /// Current epoch counter for this community. 0 at community creation;
    /// increments on every successful EpochRotation. MUST be Some for
    /// kind == Community; MUST be None otherwise. Wire: u64 under "ce".
    /// See ZEB-249 spec §3.2.
    #[serde(rename = "ce", skip_serializing_if = "Option::is_none", default)]
    pub current_epoch: Option<u64>,

    /// Active EpochKey for new outbound events at `current_epoch`.
    /// MUST be Some for kind == Community; MUST be None otherwise.
    /// Wire: bstr(32) under "ck". Zeroized on drop.
    /// See ZEB-249 spec §3.2.
    #[serde(rename = "ck", skip_serializing_if = "Option::is_none", default)]
    pub current_epoch_key: Option<EpochKey>,

    /// Historical EpochKeys for decrypting old events. Keyed by the
    /// epoch counter at which the key was current. MUST be empty for
    /// kind != Community. Wire: map<u64, bstr(32)> under "ok".
    /// See ZEB-249 spec §3.2 + §10.5 (storage growth bounds).
    #[serde(rename = "ok", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub old_epoch_keys: BTreeMap<u64, EpochKey>,
```

Add the `BTreeMap` import at the top of the struct's surrounding module if not already imported:

```bash
grep -n "use std::collections::BTreeMap" src-tauri/src/owner_state_types.rs | head -1
```

If not present, add `use std::collections::BTreeMap;` to the imports block at the top of the file.

- [ ] **Step 5: Update `validate_invariants` for new fields**

Edit `src-tauri/src/owner_state_types.rs` at line 1470-1660.

Find the non-Community block at line 1474-1493:

```rust
        if self.kind != SpaceKind::Community {
            if self.membership_key.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have membership_key=None (only Community carries it)",
                    self.kind
                )));
            }
            if self.admin_addr.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have admin_addr=None (only Community carries it)",
                    self.kind
                )));
            }
            if self.is_invite_only.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have is_invite_only=None (only Community carries it)",
                    self.kind
                )));
            }
        }
```

Replace `membership_key` check with three new field checks:

```rust
        if self.kind != SpaceKind::Community {
            if self.current_epoch.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have current_epoch=None (only Community carries epoch state)",
                    self.kind
                )));
            }
            if self.current_epoch_key.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have current_epoch_key=None (only Community carries epoch state)",
                    self.kind
                )));
            }
            if !self.old_epoch_keys.is_empty() {
                return Err(InvariantError(format!(
                    "{:?} must have old_epoch_keys=empty (only Community carries epoch state)",
                    self.kind
                )));
            }
            if self.admin_addr.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have admin_addr=None (only Community carries it)",
                    self.kind
                )));
            }
            if self.is_invite_only.is_some() {
                return Err(InvariantError(format!(
                    "{:?} must have is_invite_only=None (only Community carries it)",
                    self.kind
                )));
            }
        }
```

Find the Community block at line 1580-1623:

```rust
            SpaceKind::Community => {
                if self.membership_key.is_none() {
                    return Err(InvariantError(
                        "community must have membership_key (symmetric key for the membership topic)"
                            .into(),
                    ));
                }
```

Replace with:

```rust
            SpaceKind::Community => {
                if self.current_epoch.is_none() {
                    return Err(InvariantError(
                        "community must have current_epoch (epoch counter, 0 at creation)"
                            .into(),
                    ));
                }
                if self.current_epoch_key.is_none() {
                    return Err(InvariantError(
                        "community must have current_epoch_key (active symmetric key for the membership topic at current_epoch)"
                            .into(),
                    ));
                }
                // old_epoch_keys may be empty (epoch 0 has no history); no None check.
```

Then in the same Community arm find:

```rust
                if self.content_key.is_some() {
                    return Err(InvariantError(
                        "community must have content_key=None \
                         (membership_key is the community's symmetric key)"
                            .into(),
                    ));
                }
```

Replace with:

```rust
                if self.content_key.is_some() {
                    return Err(InvariantError(
                        "community must have content_key=None \
                         (current_epoch_key is the community's symmetric key)"
                            .into(),
                    ));
                }
```

Find the prior_content_keys check at line 1651-1657:

```rust
                if !self.prior_content_keys.is_empty() {
                    return Err(InvariantError(
                        "community must have prior_content_keys=[] \
                         (no historical content-key chain — membership_key \
                         is fixed in v1; rotation is ZEB-253)"
                            .into(),
                    ));
                }
```

Replace with:

```rust
                if !self.prior_content_keys.is_empty() {
                    return Err(InvariantError(
                        "community must have prior_content_keys=[] \
                         (historical epoch keys live in old_epoch_keys, \
                         not prior_content_keys)"
                            .into(),
                    ));
                }
```

- [ ] **Step 6: Update all test fixtures that construct Space with `membership_key`**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
grep -n "membership_key:" src/owner_state_types.rs | head -30
```

Each line with `membership_key: None,` (lines 2084, 2128, 2157, 2189, 2223, 2256, 2295, 2320, 2372, 2404, 2453, 2489, 2523, 2555, 2590, 2626, 2665, 2839, 2875) is a non-Community Space fixture. Replace each of those single lines:

```bash
perl -i -pe 's/^(\s*)membership_key: None,$/$1current_epoch: None,\n$1current_epoch_key: None,\n$1old_epoch_keys: ::std::collections::BTreeMap::new(),/g' src/owner_state_types.rs
```

For the one Community fixture at line 2902-2927:

```rust
        let key = EpochKey::new([3u8; 32]);
        // ... existing fixture construction ...
            membership_key: Some(key.clone()),
```

Find and replace the surrounding context. The exact diff at lines 2900-2935:

Find:

```rust
        let key = EpochKey::new([3u8; 32]);
```

Replace with (same line, no change — we keep the variable name `key` for backward compat):

```rust
        let key = EpochKey::new([3u8; 32]);
```

(No change to this line.) Then find:

```rust
            membership_key: Some(key.clone()),
```

Replace with:

```rust
            current_epoch: Some(0),
            current_epoch_key: Some(key.clone()),
            old_epoch_keys: ::std::collections::BTreeMap::new(),
```

Then update the round-trip assertion at line 2941:

Find:

```rust
            decoded.membership_key.as_ref().map(|k| *k.as_bytes()),
```

Replace with:

```rust
            decoded.current_epoch_key.as_ref().map(|k| *k.as_bytes()),
```

- [ ] **Step 7: Update community-side constructors that build Space with `membership_key: Some(...)` field**

The IPC layer in `lib.rs` and the test fixtures elsewhere construct community Spaces with `membership_key: Some(...)`. Grep for all such sites:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
grep -rn "membership_key:" src/ tests/ | grep -v "membership_key: None"
```

For each site, replace `membership_key: Some(x)` with the three new fields. The pattern:

```rust
        membership_key: Some(some_key),
```

Becomes:

```rust
        current_epoch: Some(0),
        current_epoch_key: Some(some_key),
        old_epoch_keys: ::std::collections::BTreeMap::new(),
```

(Use whatever variable name was passed; the substitution is `some_key` → the local variable.) Where the rename is mechanical, use:

```bash
perl -i -pe 's/membership_key: Some\(([^)]+)\),/current_epoch: Some(0),\n            current_epoch_key: Some($1),\n            old_epoch_keys: ::std::collections::BTreeMap::new(),/g' src/lib.rs tests/*.rs src/community_invite.rs src/community_state_sync.rs
```

Caveat: the perl single-line regex won't catch multi-line construction or nested parens. After running it, verify:

```bash
grep -rn "membership_key:" src/ tests/
```

Expected: empty (every `membership_key:` reference is replaced).

Any lines that still match → hand-edit them per the same pattern (likely cases: multi-line struct literal where `membership_key: Some(...)` spans lines, or it's inside a `// comment`).

- [ ] **Step 8: Update community Space's `membership_key.is_some()` and `.as_ref()` callsites**

Search for non-construction uses (read-side):

```bash
grep -rn "\.membership_key\." src/ tests/
grep -rn "membership_key\.is_some\|membership_key\.as_ref" src/ tests/
```

Each callsite is now `.current_epoch_key.` instead. Replace with perl:

```bash
perl -i -pe 's/\.membership_key\b/.current_epoch_key/g' src/community_state_sync.rs src/owner_state_crdt.rs src/lib.rs src/community_channel_log_engine.rs src/community_channel_log.rs src/community_invite.rs tests/*.rs
```

Verify:

```bash
grep -rn "membership_key" src/ tests/
```

Expected: empty.

- [ ] **Step 9: Cargo fmt + cargo check**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all
cargo check --locked --all-targets --features test-fixtures
```

Expected: `Finished`, no errors. If there are compile errors, they're usually from a missed callsite or a struct field that didn't get renamed; fix locally.

- [ ] **Step 10: Cargo nextest (let test failures inform what was missed)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: ALL tests pass. The rename should be type-preserving and behavior-preserving — this proves it.

Wire-format pinning tests are particularly load-bearing here. If a fixture pinned canonical bytes that included the field key `mk` (membership_key wire code), those tests will now fail because the wire keys changed (`mk` → `ce` + `ck` + `ok`). Update the pinned hex bytes in `tests/wire_format_community_fixtures.rs` and any other fixture using community-Space CBOR.

For each failed fixture test:
1. Read the test source to find the Space construction
2. Encode the new Space (with new fields) using `canonical_cbor_encode`
3. Replace the pinned hex in the test with the freshly encoded bytes

The pattern (from one such test):

```rust
let bytes = canonical_cbor_encode(&space).expect("encode");
println!("PINNED BYTES: {}", hex::encode(&bytes));  // temp diagnostic
```

Then run the failing test with `--nocapture` to print the bytes:

```bash
cargo nextest run --locked --features test-fixtures --no-capture -E 'test(=fixture_name_here)'
```

Copy the printed hex string into the test's `let expected = hex::decode("...")` line, remove the temp `println!`, re-run.

- [ ] **Step 11: Cargo clippy**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: no warnings, no errors.

- [ ] **Step 12: Frontend gates (should be unaffected but verify)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: clean. The rename is backend-only; frontend doesn't reference `MembershipKey`. But verify because some IPC payload type might be auto-generated.

- [ ] **Step 13: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A
git commit -m "$(cat <<'EOF'
refactor(zeb-249): rename MembershipKey → EpochKey, add Space epoch fields

Mechanical rename across 18 files (163 callsites). Adds three new
fields on Space (current_epoch, current_epoch_key, old_epoch_keys)
replacing the single membership_key field. Updates validate_invariants
to require Community to have current_epoch + current_epoch_key Some,
non-Community to have all three epoch fields None/empty.

Wire-format pinning fixtures updated for new Space CBOR shape:
field keys `mk` → `ce`+`ck`+`ok`.

Pure structural prep — no rotation logic, no new event variants yet.
All existing tests pass with renamed types.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add `EncryptedEnvelope` wire format + epoch-aware encrypt/decrypt + `EpochError`

**Spec §3.4, §7.2, §6.2.** Add the per-message epoch-tagged envelope. Add per-recipient sealed-key helper to `dm_signing.rs`. Make `community_state_sync.rs`'s encrypt/decrypt epoch-aware. Pin the canonical wire bytes.

**Files (modify):**
- `src-tauri/src/dm_signing.rs` (add `seal_to_owner` / `open_from_owner` helpers)
- `src-tauri/src/community_state_sync.rs` (add `EncryptedEnvelope` + epoch-aware helpers + `EpochError`)
- `src-tauri/tests/wire_format_community_sync_fixtures.rs` (add fixture)

- [ ] **Step 1: Write failing test for `seal_to_owner` round-trip**

Add to the bottom of `src-tauri/src/dm_signing.rs` (or its existing tests module):

```rust
#[cfg(test)]
mod epoch_seal_tests {
    use super::*;

    #[test]
    fn seal_and_open_round_trip() {
        // Recipient identity keypair (ed25519 → X25519 derived).
        // Reuse existing harmony identity keygen helper.
        let recipient_identity = generate_identity_for_test();
        let recipient_pub = recipient_identity.x25519_public_bytes();

        let plaintext = [0x42u8; 32]; // 32-byte payload (a fresh EpochKey)
        let sealed = seal_to_owner(&recipient_pub, &plaintext).expect("seal must succeed");

        // Sealed bytes layout: 32 ephemeral_pub + 12 nonce + 32 ciphertext + 16 tag = 92 bytes.
        assert_eq!(sealed.len(), 92, "sealed envelope must be exactly 92 bytes");

        let opened = open_from_owner(&recipient_identity.x25519_private_bytes(), &sealed)
            .expect("open must succeed");
        assert_eq!(opened, plaintext, "opened plaintext must match input");
    }

    #[test]
    fn open_with_wrong_key_fails() {
        let recipient_identity = generate_identity_for_test();
        let attacker_identity = generate_identity_for_test();
        let plaintext = [0x42u8; 32];

        let sealed = seal_to_owner(&recipient_identity.x25519_public_bytes(), &plaintext)
            .expect("seal must succeed");
        let result = open_from_owner(&attacker_identity.x25519_private_bytes(), &sealed);

        assert!(result.is_err(), "open with attacker key must fail with AEAD tag mismatch");
    }
}
```

If `generate_identity_for_test()` doesn't exist in `dm_signing.rs`'s test scope, use the existing test helper from elsewhere in the crate — search:

```bash
grep -rn "fn generate.*identity\|fn test_identity" src-tauri/src
```

Use the first hit; adapt the test to call it. If no such helper exists, generate inline:

```rust
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;

fn generate_identity_for_test() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}
```

And expose X25519-bytes accessors as needed (Ed25519 → X25519 conversion exists in `dm_signing.rs`).

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=seal_and_open_round_trip)'
```

Expected: FAIL — `seal_to_owner` / `open_from_owner` not yet defined.

- [ ] **Step 3: Implement `seal_to_owner` and `open_from_owner`**

Add to `src-tauri/src/dm_signing.rs` (find an appropriate location near other AEAD-related helpers; check existing patterns):

```rust
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use chacha20poly1305::aead::{Aead, OsRng as AeadOsRng};
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

/// Seal a payload to a recipient's X25519 public key using
/// X25519-ECDH-derived ChaCha20-Poly1305 (hybrid public-key encryption).
///
/// Output layout (92 bytes total for a 32-byte payload):
///   - 32 bytes: ephemeral X25519 public key (clears on every call)
///   - 12 bytes: AEAD random nonce
///   - 32 bytes: ciphertext
///   - 16 bytes: Poly1305 authentication tag
///
/// The shared secret is HKDF-derived from the ECDH output with empty
/// salt + a domain-separation `info` string. The ephemeral pubkey is
/// fresh per call — no nonce-reuse risk across multiple seals to the
/// same recipient.
///
/// Used by ZEB-249's EpochRotation/EpochCatchup events to deliver
/// fresh EpochKeys to specific recipients.
pub fn seal_to_owner(
    recipient_x25519_pub: &[u8; 32],
    plaintext: &[u8],
) -> Result<Vec<u8>, DmSignError> {
    let recipient_pub = PublicKey::from(*recipient_x25519_pub);
    let ephemeral = EphemeralSecret::random_from_rng(OsRng);
    let ephemeral_pub_bytes = *PublicKey::from(&ephemeral).as_bytes();

    let shared = ephemeral.diffie_hellman(&recipient_pub);
    let key_bytes = derive_seal_key(shared.as_bytes());
    let cipher = ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&key_bytes));

    let mut nonce_bytes = [0u8; 12];
    use rand::RngCore;
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| DmSignError::EncryptionFailed)?;

    let mut out = Vec::with_capacity(32 + 12 + ciphertext.len());
    out.extend_from_slice(&ephemeral_pub_bytes);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Open a sealed envelope using the recipient's X25519 private key.
/// Inverse of `seal_to_owner`. Returns `DmSignError::DecryptionFailed`
/// on AEAD tag mismatch (wrong recipient OR tampered ciphertext).
pub fn open_from_owner(
    recipient_x25519_priv: &[u8; 32],
    sealed: &[u8],
) -> Result<Vec<u8>, DmSignError> {
    if sealed.len() < 32 + 12 + 16 {
        return Err(DmSignError::MalformedSealedEnvelope);
    }
    let ephemeral_pub_bytes: [u8; 32] = sealed[0..32]
        .try_into()
        .map_err(|_| DmSignError::MalformedSealedEnvelope)?;
    let nonce_bytes: [u8; 12] = sealed[32..44]
        .try_into()
        .map_err(|_| DmSignError::MalformedSealedEnvelope)?;
    let ciphertext = &sealed[44..];

    let recipient_secret = StaticSecret::from(*recipient_x25519_priv);
    let ephemeral_pub = PublicKey::from(ephemeral_pub_bytes);
    let shared = recipient_secret.diffie_hellman(&ephemeral_pub);
    let key_bytes = derive_seal_key(shared.as_bytes());
    let cipher = ChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(&key_bytes));

    cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext)
        .map_err(|_| DmSignError::DecryptionFailed)
}

/// HKDF-derive a 32-byte ChaCha20-Poly1305 key from a 32-byte ECDH
/// shared secret. Empty salt, `b"harmony-zeb-249-epoch-key-seal"`
/// info string for domain separation.
fn derive_seal_key(shared_secret: &[u8; 32]) -> [u8; 32] {
    use hkdf::Hkdf;
    use sha2::Sha256;

    let hk = Hkdf::<Sha256>::new(None, shared_secret);
    let mut okm = [0u8; 32];
    hk.expand(b"harmony-zeb-249-epoch-key-seal", &mut okm)
        .expect("HKDF expand to 32 bytes always succeeds");
    okm
}
```

Add the two new `DmSignError` variants if not present. Find the `DmSignError` enum definition:

```bash
grep -n "enum DmSignError" src-tauri/src/dm_signing.rs
```

Add to that enum:

```rust
    #[error("AEAD encryption failed")]
    EncryptionFailed,
    #[error("AEAD decryption failed (tag mismatch or wrong key)")]
    DecryptionFailed,
    #[error("malformed sealed envelope (too short or bad framing)")]
    MalformedSealedEnvelope,
```

(If `EncryptionFailed` / `DecryptionFailed` already exist, skip duplicates.)

- [ ] **Step 4: Run test to verify it passes**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=seal_and_open_round_trip)' -E 'test(=open_with_wrong_key_fails)'
```

Expected: both PASS.

- [ ] **Step 5: Write failing test for `EncryptedEnvelope` CBOR round-trip**

Add to `src-tauri/src/community_state_sync.rs` test module (find the existing `#[cfg(test)]` block; if none, create one at end of file):

```rust
#[cfg(test)]
mod envelope_tests {
    use super::*;
    use crate::owner_state_crypto::{canonical_cbor_decode, canonical_cbor_encode};

    #[test]
    fn encrypted_envelope_round_trip() {
        let env = EncryptedEnvelope {
            epoch: 5,
            nonce: [0x10; 12],
            ciphertext: vec![0x20; 32],
            ratchet_generation: None,
        };

        let bytes = canonical_cbor_encode(&env).expect("encode");
        let decoded: EncryptedEnvelope = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(decoded, env, "EncryptedEnvelope round-trip must preserve all fields");
    }

    #[test]
    fn encrypted_envelope_with_ratchet_generation() {
        let env = EncryptedEnvelope {
            epoch: 5,
            nonce: [0x10; 12],
            ciphertext: vec![0x20; 32],
            ratchet_generation: Some(42),  // v3 forward-compat smoke test
        };
        let bytes = canonical_cbor_encode(&env).expect("encode");
        let decoded: EncryptedEnvelope = canonical_cbor_decode(&bytes).expect("decode");
        assert_eq!(decoded, env);
        assert_eq!(decoded.ratchet_generation, Some(42));
    }
}
```

- [ ] **Step 6: Run test to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=encrypted_envelope_round_trip)'
```

Expected: FAIL — `EncryptedEnvelope` not defined.

- [ ] **Step 7: Implement `EncryptedEnvelope` + `EpochError`**

Add to `src-tauri/src/community_state_sync.rs` (find a section near existing wire-format types):

```rust
use serde::{Deserialize, Serialize};

/// Per-event encrypted envelope. Replaces the bare ChaCha20-Poly1305
/// output of v1's membership-topic encryption with an epoch-tagged
/// container that lets receivers select the right historical key.
///
/// Wire format: 4-key CBOR map. All keys are 2 chars to satisfy the
/// same-length-keys invariant at this nesting level.
///
/// `ratchet_generation` is reserved for a future forward-secrecy
/// extension (ZEB-249 spec §9.2). v2 readers MUST tolerate `rg`
/// present-but-null; v2 writers MUST always set `rg = None`.
///
/// See ZEB-249 spec §3.4 + §7.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedEnvelope {
    #[serde(rename = "ep")]
    pub epoch: u64,

    #[serde(
        rename = "nc",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub nonce: [u8; 12],

    #[serde(
        rename = "ct",
        serialize_with = "serde_bytes::serialize",
        deserialize_with = "serde_bytes::deserialize"
    )]
    pub ciphertext: Vec<u8>,

    /// Reserved for ZEB-249 spec §9.2 forward-secrecy extension.
    /// Always `None` in v2 writers; `None` and `Some(_)` both decode in
    /// v2 readers (forward-compat).
    #[serde(rename = "rg", default, skip_serializing_if = "Option::is_none")]
    pub ratchet_generation: Option<u64>,
}

impl crate::owner_state_crypto::sealed::CanonicalPayloadSealed for EncryptedEnvelope {}
impl crate::owner_state_crypto::CanonicalPayload for EncryptedEnvelope {}

/// Failure modes for epoch-aware encryption/decryption.
/// See ZEB-249 spec §6.2.
#[derive(Debug, thiserror::Error)]
pub enum EpochError {
    #[error("key for epoch {0} not available locally")]
    KeyNotAvailable(u64),

    #[error("AEAD tag mismatch on event at epoch {0}")]
    DecryptionFailed(u64),

    #[error("rotation references stale prior_epoch {provided}, current is {current}")]
    StaleRotation { provided: u64, current: u64 },

    #[error("malformed rotation: target {target:?} included in recipient_ciphertexts")]
    MalformedRotation { target: crate::owner_state_types::OwnerAddr },

    #[error("rotation issuer {issuer:?} lacks authority (not admin and not target)")]
    InvalidIssuer { issuer: crate::owner_state_types::OwnerAddr },
}
```

Add `serde_bytes` dependency if not already present. Check `src-tauri/Cargo.toml`:

```bash
grep "serde_bytes\|serde-bytes" src-tauri/Cargo.toml
```

If missing, add to `[dependencies]`:

```toml
serde_bytes = "0.11"
```

If `serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` aren't exported from `owner_state_types`, search for them:

```bash
grep -n "fn serialize_bytes_as_bstr\|fn deserialize_bytes_from_bstr" src-tauri/src/owner_state_types.rs
```

They should exist (used by other newtypes). Use them as fully-qualified paths.

- [ ] **Step 8: Run tests to verify they pass**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(envelope)'
```

Expected: both `encrypted_envelope_round_trip` and `encrypted_envelope_with_ratchet_generation` PASS.

- [ ] **Step 9: Write failing wire-format pinning test**

Add to `src-tauri/tests/wire_format_community_sync_fixtures.rs`:

```rust
use harmony_app::community_state_sync::EncryptedEnvelope;

#[test]
fn encrypted_envelope_wire_bytes_pinned_v2_null_ratchet() {
    let env = EncryptedEnvelope {
        epoch: 5,
        nonce: [0x10; 12],
        ciphertext: vec![0x20; 32],
        ratchet_generation: None,
    };

    let bytes = canonical_cbor_encode(&env).expect("encode");
    // 3-key map (rg=None is skipped): ep, nc, ct. All keys 2-char.
    //
    // a3                                    map(3)
    //   62 6570                              text(2) "ep"
    //   05                                    uint(5)
    //   62 6e63                              text(2) "nc"
    //   4c 10101010101010101010101010101010  bstr(12) nonce
    //   62 6374                              text(2) "ct"
    //   5820 2020...20                       bstr(32) ciphertext
    let expected = hex::decode(
        "a36265700562 6e634c10101010101010101010101010 626374 58202020202020202020202020202020202020202020202020202020202020202020"
            .replace(' ', "")
    ).expect("hex");
    assert_eq!(
        bytes,
        expected,
        "EncryptedEnvelope wire bytes drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
}

#[test]
fn encrypted_envelope_wire_bytes_pinned_v3_with_ratchet() {
    let env = EncryptedEnvelope {
        epoch: 5,
        nonce: [0x10; 12],
        ciphertext: vec![0x20; 32],
        ratchet_generation: Some(42),
    };

    let bytes = canonical_cbor_encode(&env).expect("encode");
    // 4-key map (rg=Some present): ct, ep, nc, rg. Canonical CBOR =
    // shortest key first by lex; all 2-char so by alpha: ct < ep < nc < rg.
    //
    // a4
    //   62 6374 5820 2020...
    //   62 6570 05
    //   62 6e63 4c 1010...
    //   62 7267 182a   (uint 42)
    let expected = hex::decode(
        "a4626374582020202020202020202020202020202020202020202020202020202020202020626570056 26e634c1010101010101010101010101010626767 182a"
            .replace(' ', "")
    ).expect("hex");
    assert_eq!(
        bytes,
        expected,
        "EncryptedEnvelope (with rg) wire bytes drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
}
```

- [ ] **Step 10: Run pinning tests; capture actual bytes if mismatch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(encrypted_envelope_wire_bytes_pinned)' --no-capture
```

The expected hex strings above were estimated by hand; the canonical encoding may differ slightly (e.g., on the canonical lex order: ciborium may use length-then-lex, not pure-lex). When the test fails, capture the actual bytes from the diagnostic output and update the `expected` hex.

If the canonical CBOR library being used (ciborium) follows RFC 8949 §4.2.1 (canonical encoding by serialization length then lex), then for keys all of equal length the order is pure alphabetical. The pinned bytes will be in alpha order: `ct`, `ep`, `nc`, `rg`.

After capturing actual bytes, update the test's `let expected = hex::decode(...)` lines.

Re-run:

```bash
cargo nextest run --locked --features test-fixtures -E 'test(encrypted_envelope_wire_bytes_pinned)'
```

Expected: PASS.

- [ ] **Step 11: Add epoch-aware encrypt/decrypt helpers in `community_state_sync.rs`**

Find the existing `encrypt_*` / `decrypt_*` helpers for the membership topic. Search:

```bash
grep -n "fn encrypt_for\|fn decrypt_for\|fn encrypt_membership\|fn decrypt_membership" src-tauri/src/community_state_sync.rs
```

If they exist, modify them to accept an epoch parameter. If they don't (encryption was inline at callsites), add new helpers:

```rust
use crate::owner_state_types::Space;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use chacha20poly1305::aead::Aead;
use rand::RngCore;

/// Encrypt `plaintext` under the community's current epoch key,
/// wrapping the AEAD output in an `EncryptedEnvelope` that tags the
/// epoch for receiver-side key selection.
///
/// `space` MUST be a Community Space with `current_epoch` and
/// `current_epoch_key` both `Some`. Panics if invariant violated
/// (caller bug — validate_invariants would have rejected such a
/// Space before it reached this helper).
pub fn encrypt_for_topic(
    space: &Space,
    plaintext: &[u8],
) -> Result<EncryptedEnvelope, EpochError> {
    let epoch = space.current_epoch.expect("community must have current_epoch");
    let key = space
        .current_epoch_key
        .as_ref()
        .expect("community must have current_epoch_key");
    let cipher = ChaCha20Poly1305::new(key.as_chacha_key());

    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);

    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|_| EpochError::DecryptionFailed(epoch))?;

    Ok(EncryptedEnvelope {
        epoch,
        nonce: nonce_bytes,
        ciphertext,
        ratchet_generation: None,
    })
}

/// Decrypt an `EncryptedEnvelope` using the appropriate epoch key
/// from the community Space's current or old epoch keys.
///
/// Returns `EpochError::KeyNotAvailable(epoch)` if neither
/// `current_epoch_key` (when epoch matches `current_epoch`) nor
/// `old_epoch_keys[epoch]` contains the needed key. Caller classifies
/// the missing-key case per §6.2 (new member pre-join, kicked member
/// post-kick, pending-catchup transient, or bug).
pub fn decrypt_for_topic(
    space: &Space,
    envelope: &EncryptedEnvelope,
) -> Result<Vec<u8>, EpochError> {
    let current_epoch = space
        .current_epoch
        .ok_or(EpochError::KeyNotAvailable(envelope.epoch))?;
    let key = if envelope.epoch == current_epoch {
        space.current_epoch_key.as_ref()
    } else {
        space.old_epoch_keys.get(&envelope.epoch)
    }
    .ok_or(EpochError::KeyNotAvailable(envelope.epoch))?;

    let cipher = ChaCha20Poly1305::new(key.as_chacha_key());
    cipher
        .decrypt(
            Nonce::from_slice(&envelope.nonce),
            envelope.ciphertext.as_slice(),
        )
        .map_err(|_| EpochError::DecryptionFailed(envelope.epoch))
}
```

If older encrypt/decrypt helpers existed at call sites, replace those call sites with calls to the new helpers. Search:

```bash
grep -rn "ChaCha20Poly1305::new.*current_epoch_key\|ChaCha20Poly1305::new.*membership_key" src-tauri/src
```

For each match, replace inline encryption with `encrypt_for_topic(space, &plaintext)?` (or `decrypt_for_topic(space, &envelope)?` for the decode side).

- [ ] **Step 12: Write a round-trip test for the encrypt/decrypt helpers**

Add to the same `envelope_tests` module:

```rust
    #[test]
    fn encrypt_decrypt_round_trip_current_epoch() {
        let key = EpochKey::new([0xab; 32]);
        let space = build_test_community_space_with_key(0, key.clone());

        let plaintext = b"hello world from epoch 0";
        let envelope = encrypt_for_topic(&space, plaintext).expect("encrypt");
        assert_eq!(envelope.epoch, 0);

        let decrypted = decrypt_for_topic(&space, &envelope).expect("decrypt");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_with_old_epoch_key_succeeds() {
        let old_key = EpochKey::new([0xcc; 32]);
        let new_key = EpochKey::new([0xdd; 32]);
        let mut space = build_test_community_space_with_key(1, new_key);
        space.old_epoch_keys.insert(0, old_key.clone());

        // Build an envelope tagged for epoch 0, encrypted with old_key.
        let cipher = chacha20poly1305::ChaCha20Poly1305::new(old_key.as_chacha_key());
        let nonce = [0x11u8; 12];
        let plaintext = b"old epoch message";
        let ciphertext = cipher
            .encrypt(chacha20poly1305::Nonce::from_slice(&nonce), plaintext.as_ref())
            .expect("encrypt");
        let envelope = EncryptedEnvelope {
            epoch: 0,
            nonce,
            ciphertext,
            ratchet_generation: None,
        };

        let decrypted = decrypt_for_topic(&space, &envelope).expect("decrypt old epoch");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_missing_epoch_returns_key_not_available() {
        let key = EpochKey::new([0xab; 32]);
        let space = build_test_community_space_with_key(0, key);

        let envelope = EncryptedEnvelope {
            epoch: 999,  // not current, not in old_epoch_keys
            nonce: [0; 12],
            ciphertext: vec![0; 16],  // doesn't matter, lookup fails first
            ratchet_generation: None,
        };
        let err = decrypt_for_topic(&space, &envelope).expect_err("must fail");
        assert!(matches!(err, EpochError::KeyNotAvailable(999)),
            "expected KeyNotAvailable(999), got {err:?}");
    }

    fn build_test_community_space_with_key(epoch: u64, key: EpochKey) -> Space {
        use crate::owner_state_types::{Space, SpaceId, SpaceKind, Hlc, OwnerAddr};
        Space {
            id: SpaceId([0xaa; 16]),
            kind: SpaceKind::Community,
            display_name: "Test".into(),
            members: vec![],
            created_at: Hlc { wall_ms: 0, logical: 0, device_id: "test".into() },
            ordering_key: 0,
            current_epoch: Some(epoch),
            current_epoch_key: Some(key),
            old_epoch_keys: std::collections::BTreeMap::new(),
            admin_addr: Some(OwnerAddr([0xbb; 16])),
            is_invite_only: Some(false),
            transport: None,
            community_id: None,
            content_key: None,
            prior_content_keys: vec![],
            dedupe_key: crate::owner_state_types::DedupeKey::Id(SpaceId([0xaa; 16])),
            tombstoned_at: None,
        }
    }
```

Note: `build_test_community_space_with_key` is a local fixture. The exact `Space` field set depends on what's actually in the struct — check `src-tauri/src/owner_state_types.rs:1380-1430` for the current field list and adjust the fixture accordingly. Missing fields are common compile errors; fix by inspection.

- [ ] **Step 13: Run all envelope tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(envelope) | test(seal)'
```

Expected: all PASS.

- [ ] **Step 14: Run full gate suite**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

Expected: all clean.

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: clean.

- [ ] **Step 15: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-249): add EncryptedEnvelope wire format + epoch-aware encrypt/decrypt

New `EncryptedEnvelope` struct in community_state_sync.rs carries
the epoch tag alongside the AEAD ciphertext + nonce. Wire format:
4-key CBOR map with 2-char keys (ct, ep, nc, rg). `rg` (ratchet
generation) is reserved for forward-secrecy future extension —
always None in v2 writers, tolerated in v2 readers.

`encrypt_for_topic` / `decrypt_for_topic` in community_state_sync.rs
are epoch-aware: encrypt with current_epoch_key, decrypt by selecting
current_epoch_key vs old_epoch_keys[epoch] based on envelope's tag.

New `EpochError` thiserror enum with 5 variants per spec §6.2.

New `seal_to_owner` / `open_from_owner` X25519+ChaChaPoly hybrid
sealing helpers in dm_signing.rs for per-recipient key delivery in
EpochRotation/EpochCatchup events (Task 3+). 92-byte output:
32 ephemeral_pub + 12 nonce + 32 ct + 16 tag.

Two new wire-format pinning fixtures locking the canonical CBOR
of EncryptedEnvelope (both rg=None and rg=Some(_) shapes).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add `EpochRotation` MembershipEventKind variant + materialization

**Spec §3.3, §4.1–4.5.** New CRDT event variant; materialization rules; staleness gate; pending_rotation_for tracking; leaver-validity check. 8 unit tests. Wire-format fixture.

**Files (modify):**
- `src-tauri/src/community_membership.rs:22-90` (add EpochRotation variant)
- `src-tauri/src/community_membership.rs:636-660` (add pending_rotation_for + current_epoch + current_epoch_key + old_epoch_keys to MaterializedMembership)
- `src-tauri/src/community_membership.rs:765+` (materialize function: new arm for EpochRotation; Kick/Leave arms add to pending_rotation_for)
- `src-tauri/tests/wire_format_community_sync_fixtures.rs` (add fixture)

- [ ] **Step 1: Add `pending_rotation_for` + epoch fields to `MaterializedMembership`**

Edit `src-tauri/src/community_membership.rs:636-660`. Find:

```rust
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedMembership {
    pub members: BTreeMap<OwnerAddr, MemberState>,
    pub power_levels: BTreeMap<OwnerAddr, u8>,
    #[serde(default)]
    pub channels: BTreeMap<ChannelId, ChannelInfo>,
}
```

Replace with:

```rust
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedMembership {
    pub members: BTreeMap<OwnerAddr, MemberState>,
    /// Per-actor power level. Unset key = 0 = default. The community
    /// admin (Space.admin_addr) starts at 100 implicitly via the
    /// bootstrap rule — see `materialize`. SetPower events override.
    pub power_levels: BTreeMap<OwnerAddr, u8>,
    /// Per-channel materialized state.
    #[serde(default)]
    pub channels: BTreeMap<ChannelId, ChannelInfo>,

    /// ZEB-249: Current epoch counter; advances on each `EpochRotation`.
    /// `Some(_)` after the first Kick/Leave+rotation; `None` until then
    /// (pre-rotation state is materialized as None to keep this struct's
    /// Default impl valid).
    #[serde(default)]
    pub current_epoch: Option<u64>,

    /// ZEB-249: Tracks members whose Kick/Leave hasn't been followed
    /// by a successful matching EpochRotation. Self-healing path picks
    /// these up and synthesizes fresh rotations.
    /// See spec §4.3.
    #[serde(default)]
    pub pending_rotation_for: BTreeSet<OwnerAddr>,
}
```

If `BTreeSet` isn't imported at the top of `community_membership.rs`, add it. Find the existing `use std::collections` line:

```bash
grep -n "use std::collections" src-tauri/src/community_membership.rs
```

If it's `use std::collections::BTreeMap;`, change to `use std::collections::{BTreeMap, BTreeSet};`.

- [ ] **Step 2: Write failing test for EpochRotation materialization (test 1 from spec §6.5)**

Add to `src-tauri/src/community_membership.rs`'s `tests` module (find existing `#[cfg(test)] mod tests { ... }` block; if none, create one near the bottom):

```rust
    use std::collections::BTreeMap;

    fn make_signing_key_for(seed: u8) -> ed25519_dalek::SigningKey {
        // Deterministic test keys — same seed = same key (for repeatable wire bytes).
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        ed25519_dalek::SigningKey::from_bytes(&bytes)
    }

    fn make_kick_event(actor: OwnerAddr, target: OwnerAddr, at_wall_ms: u64) -> SignedMembershipEvent {
        let id: [u8; 16] = [0xfa; 16];  // unique-ish for tests; HLC tiebreaks anyway
        let community_id = SpaceId([0xc0; 16]);
        let at = Hlc { wall_ms: at_wall_ms, logical: 0, device_id: "test".into() };
        let payload = EventPayload {
            id,
            community_id,
            kind: MembershipEventKind::Kick { target, reason: None },
            actor,
            at: at.clone(),
        };
        let sig = sign_event(&make_signing_key_for(actor.0[0]), &payload).expect("sign");
        SignedMembershipEvent {
            id,
            community_id,
            kind: payload.kind,
            actor,
            at,
            sig,
            countersig: None,
        }
    }

    fn make_rotation_event(
        actor: OwnerAddr,
        triggered_by: [u8; 16],
        prior_epoch: u64,
        recipients: Vec<(OwnerAddr, Vec<u8>)>,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let id: [u8; 16] = [0xfb; 16];
        let community_id = SpaceId([0xc0; 16]);
        let at = Hlc { wall_ms: at_wall_ms, logical: 0, device_id: "test".into() };
        let payload = EventPayload {
            id,
            community_id,
            kind: MembershipEventKind::EpochRotation {
                prior_epoch,
                triggered_by,
                recipient_ciphertexts: recipients,
            },
            actor,
            at: at.clone(),
        };
        let sig = sign_event(&make_signing_key_for(actor.0[0]), &payload).expect("sign");
        SignedMembershipEvent {
            id,
            community_id,
            kind: payload.kind,
            actor,
            at,
            sig,
            countersig: None,
        }
    }

    #[test]
    fn epoch_rotation_advances_current_epoch() {
        // Admin A. Members A, B, C. A kicks C; rotation excludes C.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let carol = OwnerAddr([0xc1; 16]);

        let join_a = SignedMembershipEvent {
            id: [0x01; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: admin,
            at: Hlc { wall_ms: 100, logical: 0, device_id: "test".into() },
            sig: [0; 64], // signature not verified by materialize
            countersig: None,
        };
        let join_b = SignedMembershipEvent {
            id: [0x02; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: bob,
            at: Hlc { wall_ms: 200, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        let join_c = SignedMembershipEvent {
            id: [0x03; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: carol,
            at: Hlc { wall_ms: 300, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        let kick_c = make_kick_event(admin, carol, 400);
        let rotation = make_rotation_event(
            admin,
            kick_c.id,
            0,
            vec![(admin, vec![0xa1; 92]), (bob, vec![0xb1; 92])],  // ciphertexts for A and B, NOT C
            401,
        );

        let m = materialize(&[join_a, join_b, join_c, kick_c, rotation], admin);

        assert_eq!(m.current_epoch, Some(1), "rotation must advance epoch from 0 to 1");
        assert!(!m.pending_rotation_for.contains(&carol), "carol's pending rotation must be cleared");
        assert_eq!(m.pending_rotation_for.len(), 0, "no other pending rotations");
        assert_eq!(m.members[&carol].status, MemberStatus::Banned, "carol is banned");
    }
```

- [ ] **Step 3: Run test to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=epoch_rotation_advances_current_epoch)'
```

Expected: FAIL — `MembershipEventKind::EpochRotation` variant not defined.

- [ ] **Step 4: Add `EpochRotation` variant to `MembershipEventKind`**

Edit `src-tauri/src/community_membership.rs:22-90`. Find the existing enum:

```rust
pub enum MembershipEventKind {
    #[serde(rename = "j")]
    Join,
    // ... other variants ...
    #[serde(rename = "d")]
    ChannelDelete {
        #[serde(rename = "ch")]
        channel_id: ChannelId,
    },
}
```

Add a new variant just before the closing brace:

```rust
    /// ZEB-249: Advances current_epoch. Triggered by Kick/Leave
    /// (subtractive — excludes the kicked/leaving member from
    /// recipient_ciphertexts). Spec §4.1.
    ///
    /// Variant code "r". Inner field keys are 2-char (pe, ts, rc).
    #[serde(rename = "r")]
    EpochRotation {
        #[serde(rename = "pe")]
        prior_epoch: u64,

        #[serde(
            rename = "ts",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        triggered_by: EventId,

        #[serde(rename = "rc")]
        recipient_ciphertexts: Vec<(OwnerAddr, Vec<u8>)>,
    },
```

The `Vec<(OwnerAddr, Vec<u8>)>` needs to round-trip cleanly. Verify ciborium handles tuple-of-two as a 2-element array (it does); the `Vec<u8>` inside needs `serde_bytes` for bstr encoding rather than array-of-u8. Add wrapper:

Actually, the simpler shape is:

```rust
    #[serde(rename = "r")]
    EpochRotation {
        #[serde(rename = "pe")]
        prior_epoch: u64,

        #[serde(
            rename = "ts",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        triggered_by: EventId,

        #[serde(rename = "rc")]
        recipient_ciphertexts: Vec<RecipientCiphertext>,
    },
```

Add a helper struct nearby:

```rust
/// One per-recipient sealed ciphertext in an EpochRotation / EpochCatchup.
/// Wire format: 2-key CBOR map. Keys are 2-char (rc, ct) to satisfy the
/// same-length-keys invariant at this nesting level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientCiphertext {
    #[serde(rename = "rc")]
    pub recipient: OwnerAddr,

    /// X25519-sealed bytes (92 = 32 ephemeral + 12 nonce + 32 ct + 16 tag).
    /// See `dm_signing::seal_to_owner`.
    #[serde(
        rename = "ct",
        serialize_with = "serde_bytes::serialize",
        deserialize_with = "serde_bytes::deserialize"
    )]
    pub sealed: Vec<u8>,
}

impl CanonicalPayloadSealed for RecipientCiphertext {}
impl CanonicalPayload for RecipientCiphertext {}
```

Update the test helper (`make_rotation_event`) to construct `RecipientCiphertext` instead of raw tuples:

```rust
    fn make_rotation_event(
        actor: OwnerAddr,
        triggered_by: [u8; 16],
        prior_epoch: u64,
        recipients: Vec<(OwnerAddr, Vec<u8>)>,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let recipient_ciphertexts: Vec<RecipientCiphertext> = recipients
            .into_iter()
            .map(|(addr, sealed)| RecipientCiphertext { recipient: addr, sealed })
            .collect();
        // ... rest as before, using recipient_ciphertexts in EpochRotation variant
    }
```

- [ ] **Step 5: Add EpochRotation arm to `materialize`**

Edit `src-tauri/src/community_membership.rs:765+`. Find the existing match block in `materialize`:

```rust
        match &event.kind {
            MembershipEventKind::Join => { ... }
            MembershipEventKind::Leave => { ... }
            // ...
            MembershipEventKind::ChannelDelete { channel_id } => { ... }
        }
```

In the `Kick` arm, add tracking of pending_rotation_for. Find:

```rust
            MembershipEventKind::Kick { target, .. } => {
                if let Some(s) = m.members.get_mut(target) {
                    s.status = MemberStatus::Banned;
                    s.left_at = Some(event.at.clone());
                }
            }
```

Replace with:

```rust
            MembershipEventKind::Kick { target, .. } => {
                if let Some(s) = m.members.get_mut(target) {
                    s.status = MemberStatus::Banned;
                    s.left_at = Some(event.at.clone());
                }
                // ZEB-249: track that this kick needs a matching EpochRotation.
                // The self-healing observer (event_loop.rs) synthesizes one if
                // the bundled rotation didn't land.
                m.pending_rotation_for.insert(*target);
            }
```

Similarly in the `Leave` arm. Find:

```rust
            MembershipEventKind::Leave => {
                if let Some(s) = m.members.get_mut(&event.actor) {
                    if s.status != MemberStatus::Banned {
                        s.status = MemberStatus::Left;
                        s.left_at = Some(event.at.clone());
                    }
                }
            }
```

Add to the bottom of that arm (still inside the if-let-Some check is fine, but better outside so it tracks Leave even when actor is Banned — defense in depth):

```rust
            MembershipEventKind::Leave => {
                if let Some(s) = m.members.get_mut(&event.actor) {
                    if s.status != MemberStatus::Banned {
                        s.status = MemberStatus::Left;
                        s.left_at = Some(event.at.clone());
                    }
                }
                // ZEB-249: Leave needs a rotation; self-healing fills if not bundled.
                m.pending_rotation_for.insert(event.actor);
            }
```

Add the new `EpochRotation` arm before the closing brace:

```rust
            MembershipEventKind::EpochRotation {
                prior_epoch,
                triggered_by,
                recipient_ciphertexts,
            } => {
                // Staleness gate (spec §4.2): silently drop if not for current epoch.
                let current = m.current_epoch.unwrap_or(0);
                if *prior_epoch != current {
                    continue;
                }

                // Find the Kick/Leave event this rotation was generated for.
                // Look it up by EventId in the events slice. Linear scan ok —
                // event log is bounded.
                let triggered_event = events.iter().find(|e| e.id == *triggered_by);
                let kick_target = match triggered_event.map(|e| &e.kind) {
                    Some(MembershipEventKind::Kick { target, .. }) => Some(*target),
                    Some(MembershipEventKind::Leave) => Some(triggered_event.unwrap().actor),
                    _ => None,
                };
                // If we can't find the triggered_by event, the rotation is
                // malformed — drop silently. The self-healing path will fix.
                let Some(target) = kick_target else { continue; };

                // Malformed rotation check (spec §4.4): target must NOT be
                // in recipient_ciphertexts.
                if recipient_ciphertexts.iter().any(|rc| rc.recipient == target) {
                    continue;
                }

                // Validity check (spec §4.4): issuer must have admin power
                // OR be the target of a Leave (cooperative-leaver path).
                let issuer = event.actor;
                let issuer_power = m.power_levels.get(&issuer).copied().unwrap_or(0);
                let is_admin = issuer_power >= 50;  // POWER_THRESHOLDS.kick (placeholder)
                let is_self_leaver = matches!(
                    triggered_event.map(|e| &e.kind),
                    Some(MembershipEventKind::Leave)
                ) && issuer == target;
                if !is_admin && !is_self_leaver {
                    continue;
                }

                // Apply: advance epoch. We don't decrypt the ciphertext here —
                // materialize is pure replay, doesn't have access to local
                // identity privkey. The actual key insertion happens in
                // community_state_sync's apply layer (Task 5/6).
                m.current_epoch = Some(current + 1);
                m.pending_rotation_for.remove(&target);
            }
```

Find the `POWER_THRESHOLDS.kick` constant or equivalent:

```bash
grep -n "POWER_THRESHOLDS\|threshold_kick\|kick_power" src-tauri/src/community_membership.rs src-tauri/src/community_state_sync.rs | head -10
```

Use the actual constant name in place of the placeholder `50`. Common pattern: `POWER_THRESHOLDS.kick` accessed as `harmony_app::community_membership::POWER_THRESHOLDS.kick` or via `use`.

- [ ] **Step 6: Run test to verify it passes**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=epoch_rotation_advances_current_epoch)'
```

Expected: PASS.

- [ ] **Step 7: Write remaining 7 unit tests for EpochRotation**

Add to the same `tests` module:

```rust
    #[test]
    fn stale_rotation_dropped() {
        // Set up state at epoch 2; submit a rotation with prior_epoch=0.
        // Rotation should be dropped, epoch stays at 2.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let carol = OwnerAddr([0xc1; 16]);

        // Build a sequence that advances epoch to 2.
        // (Implementation: 2 kicks each with paired rotation.)
        let kick1 = make_kick_event(admin, bob, 100);
        let rot1 = make_rotation_event(admin, kick1.id, 0, vec![(admin, vec![1; 92]), (carol, vec![1; 92])], 101);
        let kick2 = make_kick_event(admin, carol, 200);
        let rot2 = make_rotation_event(admin, kick2.id, 1, vec![(admin, vec![2; 92])], 201);

        // Submit a stale rotation referencing prior_epoch=0 (when current is now 2).
        let stale_kick = make_kick_event(admin, OwnerAddr([0xd1; 16]), 300);
        let stale_rot = make_rotation_event(admin, stale_kick.id, 0, vec![(admin, vec![3; 92])], 301);

        let m = materialize(&[kick1, rot1, kick2, rot2, stale_kick, stale_rot], admin);

        assert_eq!(m.current_epoch, Some(2), "stale rotation must NOT advance from epoch 2");
        assert!(m.pending_rotation_for.contains(&OwnerAddr([0xd1; 16])),
            "kick at stale rotation must remain pending");
    }

    #[test]
    fn malformed_rotation_dropped() {
        // Rotation includes the kicked target in recipient_ciphertexts → dropped.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);

        let kick = make_kick_event(admin, bob, 100);
        // Malformed: recipient_ciphertexts includes bob.
        let malformed = make_rotation_event(
            admin,
            kick.id,
            0,
            vec![(admin, vec![1; 92]), (bob, vec![1; 92])],  // bob included → invalid
            101,
        );

        let m = materialize(&[kick, malformed], admin);
        assert_eq!(m.current_epoch, None, "malformed rotation must not advance epoch (None until first valid)");
        assert!(m.pending_rotation_for.contains(&bob), "bob's kick must remain pending");
    }

    #[test]
    fn leaver_issued_rotation_accepted_when_well_formed() {
        // Bob leaves; Bob's rotation excludes himself → accepted (cooperative leaver).
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);

        let leave = SignedMembershipEvent {
            id: [0xfc; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Leave,
            actor: bob,
            at: Hlc { wall_ms: 100, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        let rotation = make_rotation_event(
            bob,           // signer = leaver
            leave.id,
            0,
            vec![(admin, vec![1; 92])],  // bob NOT included
            101,
        );

        let m = materialize(&[leave, rotation], admin);
        assert_eq!(m.current_epoch, Some(1), "leaver-issued rotation must apply");
        assert!(!m.pending_rotation_for.contains(&bob), "bob's leave must be cleared");
    }

    #[test]
    fn leaver_issued_rotation_rejected_when_self_included() {
        // Bob leaves; Bob's rotation INCLUDES himself → rejected.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);

        let leave = SignedMembershipEvent {
            id: [0xfc; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Leave,
            actor: bob,
            at: Hlc { wall_ms: 100, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        let rotation = make_rotation_event(
            bob,
            leave.id,
            0,
            vec![(admin, vec![1; 92]), (bob, vec![1; 92])],  // bob INCLUDED → malformed
            101,
        );

        let m = materialize(&[leave, rotation], admin);
        assert_eq!(m.current_epoch, None, "self-including leaver rotation must NOT apply");
        assert!(m.pending_rotation_for.contains(&bob), "bob's leave must remain pending");
    }

    #[test]
    fn pending_rotation_tracking_clears_after_matching_rotation_lands() {
        // Sanity: kick → pending_rotation_for has target; then rotation → empty.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);

        let kick = make_kick_event(admin, bob, 100);
        let m_partial = materialize(&[kick.clone()], admin);
        assert!(m_partial.pending_rotation_for.contains(&bob), "post-kick: bob is pending");

        let rotation = make_rotation_event(admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let m_full = materialize(&[kick, rotation], admin);
        assert_eq!(m_full.pending_rotation_for.len(), 0, "post-rotation: no pending");
    }

    #[test]
    fn kick_then_rotation_same_hlc_tick_materializes_atomically() {
        // Both events at HLC (100, 0, "test"); the sort tiebreaks on EventId
        // which deterministically orders kick before rotation (kick has
        // [0xfa; 16] id < rotation's [0xfb; 16]).
        // Verify the rotation applies correctly even at same wall_ms.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);

        let kick = make_kick_event(admin, bob, 100);  // id = [0xfa; 16]
        let rotation = make_rotation_event(admin, kick.id, 0, vec![(admin, vec![1; 92])], 100);  // same wall_ms, id = [0xfb; 16]

        let m = materialize(&[kick, rotation], admin);
        assert_eq!(m.current_epoch, Some(1));
        assert_eq!(m.pending_rotation_for.len(), 0);
    }

    #[test]
    fn concurrent_kicks_self_heal() {
        // Two admins concurrently kick different targets.
        // Both rotations reference prior_epoch=0 (concurrent generation).
        // After materialization in HLC order: first rotation lands, second
        // is stale → dropped. The second kick's pending_rotation_for entry
        // remains, ready for self-healing.
        let admin1 = OwnerAddr([0xa1; 16]);
        let admin2 = OwnerAddr([0xa2; 16]);
        let alice = OwnerAddr([0xb1; 16]);
        let bob = OwnerAddr([0xb2; 16]);

        // Both kicks + rotations at distinct HLCs so order is deterministic.
        let kick_a = make_kick_event(admin1, alice, 100);
        let rot_a = make_rotation_event(admin1, kick_a.id, 0, vec![(admin2, vec![1; 92]), (bob, vec![1; 92])], 101);
        let kick_b = make_kick_event(admin2, bob, 200);
        let rot_b = make_rotation_event(admin2, kick_b.id, 0, vec![(admin1, vec![2; 92]), (alice, vec![2; 92])], 201);
        // rot_b references prior_epoch=0 BUT by time it materializes, epoch is 1.

        // Need admin1 AND admin2 to be admins. Inject SetPower events.
        let setpwr_admin2 = SignedMembershipEvent {
            id: [0x05; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::SetPower { target: admin2, level: 100 },
            actor: admin1,
            at: Hlc { wall_ms: 50, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };

        let m = materialize(&[setpwr_admin2, kick_a, rot_a, kick_b, rot_b], admin1);

        assert_eq!(m.current_epoch, Some(1), "only one rotation advanced (the first); second was stale");
        assert!(m.pending_rotation_for.contains(&bob), "bob's kick remains pending for self-healing");
        assert!(!m.pending_rotation_for.contains(&alice), "alice's kick was cleared");
    }
```

- [ ] **Step 8: Run all 8 unit tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(epoch_rotation) | test(stale_rotation) | test(malformed_rotation) | test(leaver_issued) | test(pending_rotation) | test(kick_then_rotation) | test(concurrent_kicks_self_heal)'
```

Expected: all 8 PASS. If any fail, the materialization logic is wrong — debug.

- [ ] **Step 9: Add wire-format pinning fixture**

Add to `src-tauri/tests/wire_format_community_sync_fixtures.rs`:

```rust
use harmony_app::community_membership::{
    EventPayload, MembershipEventKind, RecipientCiphertext, sign_event,
};
use harmony_app::owner_state_types::{Hlc, OwnerAddr, SpaceId};

#[test]
fn epoch_rotation_event_wire_bytes_pinned() {
    // 3-recipient rotation. Wire-pin both the EventPayload (signed bytes)
    // AND the inner EpochRotation variant.
    let triggered_by: [u8; 16] = [0xfa; 16];
    let kind = MembershipEventKind::EpochRotation {
        prior_epoch: 5,
        triggered_by,
        recipient_ciphertexts: vec![
            RecipientCiphertext { recipient: OwnerAddr([0xa1; 16]), sealed: vec![0xab; 92] },
            RecipientCiphertext { recipient: OwnerAddr([0xb1; 16]), sealed: vec![0xcd; 92] },
            RecipientCiphertext { recipient: OwnerAddr([0xc1; 16]), sealed: vec![0xef; 92] },
        ],
    };
    let bytes = canonical_cbor_encode(&kind).expect("encode");

    // Replace with actual hex after first run via --nocapture diagnostic.
    let expected = hex::decode("PLACEHOLDER").unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for pinning: {}\n", hex::encode(&bytes));
        panic!("update PLACEHOLDER with the bytes above");
    });
    assert_eq!(
        bytes,
        expected,
        "EpochRotation wire bytes drifted: {} vs {}",
        hex::encode(&bytes),
        hex::encode(&expected)
    );
}
```

- [ ] **Step 10: Capture the pinned bytes**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=epoch_rotation_event_wire_bytes_pinned)' --no-capture
```

Expected: the diagnostic prints the actual hex. Copy that hex into the test's `expected` declaration, removing the placeholder logic. Re-run; should PASS.

- [ ] **Step 11: Run all gate checks**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit && npx vitest run
```

All expected: clean.

- [ ] **Step 12: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-249): EpochRotation MembershipEventKind variant + materialization

New EpochRotation variant on MembershipEventKind with prior_epoch
staleness gate, triggered_by EventId pointing to the Kick/Leave that
motivated the rotation, and recipient_ciphertexts: Vec<RecipientCiphertext>.

RecipientCiphertext is a 2-key CBOR helper (rc/ct) carrying the
recipient OwnerAddr + their X25519-sealed copy of the new EpochKey.

MaterializedMembership gains current_epoch + pending_rotation_for
fields. Kick/Leave arms in materialize() now add target to
pending_rotation_for; the EpochRotation arm validates (staleness,
malformed-recipient-list, leaver-validity) and advances epoch.

8 new unit tests covering the protocol invariants:
- epoch_rotation_advances_current_epoch
- stale_rotation_dropped
- malformed_rotation_dropped
- leaver_issued_rotation_accepted_when_well_formed
- leaver_issued_rotation_rejected_when_self_included
- pending_rotation_tracking_clears_after_matching_rotation_lands
- kick_then_rotation_same_hlc_tick_materializes_atomically
- concurrent_kicks_self_heal

Wire-format pinning fixture locks canonical CBOR bytes for the new
variant.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Add `EpochCatchup` MembershipEventKind variant + materialization

**Spec §3.3, §4.6.** Non-advancing variant for stale-invite-bootstrap case. 6 unit tests. Wire-format fixture.

**Files (modify):**
- `src-tauri/src/community_membership.rs:22-90` (add EpochCatchup variant)
- `src-tauri/src/community_membership.rs:636-660` (add pending_catchup_for to MaterializedMembership)
- `src-tauri/src/community_membership.rs:765+` (materialize: Join with stale snapshot → pending_catchup_for; EpochCatchup arm)
- `src-tauri/tests/wire_format_community_sync_fixtures.rs` (add fixture)

- [ ] **Step 1: Add `pending_catchup_for` field to `MaterializedMembership`**

Edit `src-tauri/src/community_membership.rs` — find the `MaterializedMembership` struct (updated in Task 3). Add a new field after `pending_rotation_for`:

```rust
    /// ZEB-249: Tracks new members whose Bootstrap-Join landed with a
    /// stale snapshot_epoch < current_epoch (kick happened between
    /// invite issuance and redemption). Self-healing observer
    /// synthesizes EpochCatchup events to deliver current_epoch_key
    /// to these members. Spec §4.6.
    #[serde(default)]
    pub pending_catchup_for: BTreeSet<OwnerAddr>,
```

- [ ] **Step 2: Write failing test for EpochCatchup (spec test #10)**

Add to `src-tauri/src/community_membership.rs` tests module:

```rust
    fn make_catchup_event(
        actor: OwnerAddr,
        triggered_by: [u8; 16],
        epoch: u64,
        recipients: Vec<(OwnerAddr, Vec<u8>)>,
        at_wall_ms: u64,
    ) -> SignedMembershipEvent {
        let id: [u8; 16] = [0xfd; 16];
        let community_id = SpaceId([0xc0; 16]);
        let at = Hlc { wall_ms: at_wall_ms, logical: 0, device_id: "test".into() };
        let recipient_ciphertexts: Vec<RecipientCiphertext> = recipients
            .into_iter()
            .map(|(addr, sealed)| RecipientCiphertext { recipient: addr, sealed })
            .collect();
        let payload = EventPayload {
            id,
            community_id,
            kind: MembershipEventKind::EpochCatchup {
                epoch,
                triggered_by,
                recipient_ciphertexts,
            },
            actor,
            at: at.clone(),
        };
        let sig = sign_event(&make_signing_key_for(actor.0[0]), &payload).expect("sign");
        SignedMembershipEvent {
            id,
            community_id,
            kind: payload.kind,
            actor,
            at,
            sig,
            countersig: None,
        }
    }

    #[test]
    fn epoch_catchup_delivers_current_key_without_advancing_epoch() {
        // Admin kicks Bob (advances to epoch 1). Then Dave joins with snapshot=0.
        // Dave enters pending_catchup_for. Admin issues EpochCatchup at epoch 1.
        // After catchup, Dave is cleared from pending_catchup_for; epoch is STILL 1.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(admin, bob, 100);
        let rot = make_rotation_event(admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        // Dave's Bootstrap-Join carries a "join_snapshot_epoch" hint via the
        // event kind. For test purposes we assume Join always uses the
        // current epoch at join time; the test injects a Join after rotation
        // (Dave joined when current=1 but his snapshot was issued at 0).
        // The plan's materialize will inspect the Join + a snapshot field
        // (added below as JoinV2 with epoch hint, OR — alternative — Join
        // remains structurally unchanged and the snapshot_epoch hint lives
        // in CommunityInvitePayload, materialized into MemberState).
        //
        // For this test, simulate the gap by:
        //   1. Kick → rotation → epoch=1.
        //   2. Add Dave to members with snapshot_epoch=0 via post-Join Hook.
        //   3. Verify Dave is in pending_catchup_for.
        //   4. Submit EpochCatchup at epoch=1; verify cleared.
        // (Hookup detail: MemberState gains an optional snapshot_epoch field.)

        let join_d = SignedMembershipEvent {
            id: [0xd0; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: dave,
            at: Hlc { wall_ms: 200, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };

        // First check: after kick+rotation+join, Dave is in pending_catchup_for.
        let m_pre = materialize(&[kick.clone(), rot.clone(), join_d.clone()], admin);
        assert_eq!(m_pre.current_epoch, Some(1));
        assert!(m_pre.pending_catchup_for.contains(&dave),
            "post-Join with stale snapshot: dave is pending");

        let catchup = make_catchup_event(admin, join_d.id, 1, vec![(dave, vec![5; 92])], 300);
        let m_post = materialize(&[kick, rot, join_d, catchup], admin);

        assert_eq!(m_post.current_epoch, Some(1), "catchup must NOT advance epoch");
        assert!(!m_post.pending_catchup_for.contains(&dave), "dave cleared");
    }
```

Note: this test embeds an assumption that materialize() detects "stale snapshot" by comparing Join HLC vs current_epoch transition HLCs. The simpler heuristic: if a Join lands at a wall_ms after an EpochRotation already advanced epoch beyond 0 AND the actor is new (status was None), they joined at stale snapshot.

The clean way (without modifying Join's wire format): in the Join arm, if `m.current_epoch > 0` (any rotation has happened) AND this is a first-time-Join for the actor, add to pending_catchup_for. This is a conservative over-trigger (catches some non-stale joins too) but the self-healing observer just issues an extra catchup, which is harmless.

- [ ] **Step 3: Run test to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=epoch_catchup_delivers_current_key_without_advancing_epoch)'
```

Expected: FAIL — `MembershipEventKind::EpochCatchup` not defined.

- [ ] **Step 4: Add `EpochCatchup` variant**

Edit `src-tauri/src/community_membership.rs`. Find the `EpochRotation` variant added in Task 3; add the new variant directly after:

```rust
    /// ZEB-249: Delivers `current_epoch_key` to specified members WITHOUT
    /// advancing the epoch. Triggered by a Join whose snapshot was stale
    /// at redemption time. Spec §4.6.
    ///
    /// Variant code "f" (for "fill"). Inner field keys are 2-char.
    #[serde(rename = "f")]
    EpochCatchup {
        #[serde(rename = "ep")]
        epoch: u64,

        #[serde(
            rename = "ts",
            serialize_with = "serialize_bytes_as_bstr",
            deserialize_with = "deserialize_bytes_from_bstr"
        )]
        triggered_by: EventId,

        #[serde(rename = "rc")]
        recipient_ciphertexts: Vec<RecipientCiphertext>,
    },
```

- [ ] **Step 5: Update materialize for stale-Join detection + EpochCatchup arm**

Find the existing Join arm in materialize. Modify to add pending_catchup_for tracking:

```rust
            MembershipEventKind::Join => {
                let prior_status = m.members.get(&event.actor).map(|s| s.status);
                let should_refresh = match prior_status {
                    None | Some(MemberStatus::Invited) | Some(MemberStatus::Left) => true,
                    Some(MemberStatus::Joined) | Some(MemberStatus::Banned) => false,
                };
                if should_refresh {
                    m.members.insert(
                        event.actor,
                        MemberState {
                            status: MemberStatus::Joined,
                            joined_at: event.at.clone(),
                            left_at: None,
                        },
                    );
                    // ZEB-249: if any rotation has already happened
                    // (current_epoch > 0), this new member's snapshot
                    // may be stale — mark for catchup. The self-healing
                    // observer issues a catchup; if the join was actually
                    // current-epoch (snapshot wasn't stale), the catchup
                    // is a no-op (recipient already has the key) but
                    // we issue conservatively.
                    if prior_status.is_none() && m.current_epoch.unwrap_or(0) > 0 {
                        m.pending_catchup_for.insert(event.actor);
                    }
                }
            }
```

Add the EpochCatchup arm in the match:

```rust
            MembershipEventKind::EpochCatchup {
                epoch,
                triggered_by,
                recipient_ciphertexts,
            } => {
                // Epoch must match current (spec §4.6).
                let current = m.current_epoch.unwrap_or(0);
                if *epoch != current {
                    continue;
                }

                // triggered_by must reference a Join event.
                let triggered_event = events.iter().find(|e| e.id == *triggered_by);
                let join_actor = match triggered_event.map(|e| &e.kind) {
                    Some(MembershipEventKind::Join) => Some(triggered_event.unwrap().actor),
                    _ => None,
                };
                let Some(target) = join_actor else { continue; };

                // target must be in recipient_ciphertexts.
                if !recipient_ciphertexts.iter().any(|rc| rc.recipient == target) {
                    continue;
                }

                // Issuer must have admin power (spec §4.6 — no cooperative-joiner).
                let issuer_power = m.power_levels.get(&event.actor).copied().unwrap_or(0);
                let is_admin = issuer_power >= 50;  // POWER_THRESHOLDS.kick — see note in Task 3 Step 5
                if !is_admin {
                    continue;
                }

                // Apply: clear target from pending_catchup_for.
                // (Actual key delivery to receiver's Space happens in
                // community_state_sync apply layer — Task 6.)
                m.pending_catchup_for.remove(&target);
            }
```

Use the same actual `POWER_THRESHOLDS.kick` constant from Task 3 Step 5 (substitute the placeholder `50`).

- [ ] **Step 6: Run the catchup test**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=epoch_catchup_delivers_current_key_without_advancing_epoch)'
```

Expected: PASS.

- [ ] **Step 7: Write remaining catchup tests (tests 9, 11, 12, 13, 14 from spec §6.5)**

Add to tests module:

```rust
    #[test]
    fn stale_invite_join_marks_pending_catchup_for() {
        // First: kick+rotation advances epoch. Then Dave joins → pending_catchup_for has dave.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(admin, bob, 100);
        let rot = make_rotation_event(admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let join_d = SignedMembershipEvent {
            id: [0xd0; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: dave,
            at: Hlc { wall_ms: 200, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };

        let m = materialize(&[kick, rot, join_d], admin);
        assert_eq!(m.current_epoch, Some(1));
        assert!(m.pending_catchup_for.contains(&dave),
            "stale-snapshot Join must enter pending_catchup_for");
    }

    #[test]
    fn epoch_catchup_with_stale_epoch_dropped() {
        // Catchup targeting epoch=0 when current_epoch=1 → dropped.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(admin, bob, 100);
        let rot = make_rotation_event(admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let join_d = SignedMembershipEvent {
            id: [0xd0; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: dave,
            at: Hlc { wall_ms: 200, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        // Catchup says epoch=0 but current is now 1 → must be dropped.
        let stale_catchup = make_catchup_event(admin, join_d.id, 0, vec![(dave, vec![1; 92])], 300);

        let m = materialize(&[kick, rot, join_d, stale_catchup], admin);
        assert!(m.pending_catchup_for.contains(&dave),
            "stale-epoch catchup must NOT clear pending_catchup_for");
    }

    #[test]
    fn epoch_catchup_referencing_non_join_event_dropped() {
        // Catchup whose triggered_by points to a Kick → dropped.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(admin, bob, 100);
        let rot = make_rotation_event(admin, kick.id, 0, vec![(admin, vec![1; 92])], 101);
        let join_d = SignedMembershipEvent {
            id: [0xd0; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: dave,
            at: Hlc { wall_ms: 200, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        // Malformed catchup: triggered_by points to kick (not a Join).
        let bad_catchup = make_catchup_event(admin, kick.id, 1, vec![(dave, vec![1; 92])], 300);

        let m = materialize(&[kick, rot, join_d, bad_catchup], admin);
        assert!(m.pending_catchup_for.contains(&dave),
            "catchup with non-Join triggered_by must NOT clear pending_catchup_for");
    }

    #[test]
    fn non_admin_issued_catchup_dropped() {
        // Carol (non-admin, power=0) tries to issue a catchup → dropped.
        let admin = OwnerAddr([0xa1; 16]);
        let bob = OwnerAddr([0xb1; 16]);
        let carol = OwnerAddr([0xc1; 16]);  // not promoted to admin
        let dave = OwnerAddr([0xd1; 16]);

        let kick = make_kick_event(admin, bob, 100);
        let rot = make_rotation_event(admin, kick.id, 0, vec![(admin, vec![1; 92]), (carol, vec![1; 92])], 101);
        let join_d = SignedMembershipEvent {
            id: [0xd0; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: dave,
            at: Hlc { wall_ms: 200, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        // Carol tries to catchup-fill dave's gap, but she's not admin.
        let bad_catchup = make_catchup_event(carol, join_d.id, 1, vec![(dave, vec![1; 92])], 300);

        let m = materialize(&[kick, rot, join_d, bad_catchup], admin);
        assert!(m.pending_catchup_for.contains(&dave),
            "non-admin catchup must NOT clear pending_catchup_for");
    }

    #[test]
    fn epoch_catchup_for_already_caught_up_member_dropped() {
        // Two admin-issued catchups for the same Join → the second is no-op.
        // (Validates idempotency under duplicate self-heal.)
        let admin = OwnerAddr([0xa1; 16]);
        let admin2 = OwnerAddr([0xa2; 16]);  // co-admin
        let bob = OwnerAddr([0xb1; 16]);
        let dave = OwnerAddr([0xd1; 16]);

        let setpwr_admin2 = SignedMembershipEvent {
            id: [0x05; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::SetPower { target: admin2, level: 100 },
            actor: admin,
            at: Hlc { wall_ms: 50, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        let kick = make_kick_event(admin, bob, 100);
        let rot = make_rotation_event(admin, kick.id, 0, vec![(admin, vec![1; 92]), (admin2, vec![1; 92])], 101);
        let join_d = SignedMembershipEvent {
            id: [0xd0; 16],
            community_id: SpaceId([0xc0; 16]),
            kind: MembershipEventKind::Join,
            actor: dave,
            at: Hlc { wall_ms: 200, logical: 0, device_id: "test".into() },
            sig: [0; 64],
            countersig: None,
        };
        let catchup1 = make_catchup_event(admin, join_d.id, 1, vec![(dave, vec![1; 92])], 300);
        let catchup2 = make_catchup_event(admin2, join_d.id, 1, vec![(dave, vec![2; 92])], 301);

        let m = materialize(&[setpwr_admin2, kick, rot, join_d, catchup1, catchup2], admin);
        assert!(!m.pending_catchup_for.contains(&dave), "dave was caught up by first catchup");
        assert_eq!(m.current_epoch, Some(1), "epoch unchanged by catchups");
    }
```

- [ ] **Step 8: Run all 6 EpochCatchup tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(catchup) | test(stale_invite_join)'
```

Expected: all PASS.

- [ ] **Step 9: Add wire-format pinning fixture**

Add to `src-tauri/tests/wire_format_community_sync_fixtures.rs`:

```rust
#[test]
fn epoch_catchup_event_wire_bytes_pinned() {
    let triggered_by: [u8; 16] = [0xfa; 16];
    let kind = MembershipEventKind::EpochCatchup {
        epoch: 7,
        triggered_by,
        recipient_ciphertexts: vec![
            RecipientCiphertext { recipient: OwnerAddr([0xd1; 16]), sealed: vec![0xab; 92] },
        ],
    };
    let bytes = canonical_cbor_encode(&kind).expect("encode");
    let expected = hex::decode("PLACEHOLDER").unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for pinning: {}\n", hex::encode(&bytes));
        panic!("update PLACEHOLDER with the bytes above");
    });
    assert_eq!(bytes, expected,
        "EpochCatchup wire bytes drifted: {} vs {}",
        hex::encode(&bytes), hex::encode(&expected));
}
```

- [ ] **Step 10: Capture pinned bytes for catchup**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=epoch_catchup_event_wire_bytes_pinned)' --no-capture
```

Copy actual hex into test, remove placeholder logic, re-run; PASS.

- [ ] **Step 11: Run full gate suite**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit && npx vitest run
```

All expected: clean.

- [ ] **Step 12: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-249): EpochCatchup MembershipEventKind variant for stale-invite gap

New EpochCatchup variant: non-advancing, admin-only, delivers
current_epoch_key to a new member whose Bootstrap-Join landed
with snapshot_epoch < current_epoch (kick happened between
invite issuance and redemption).

Validation rules (spec §4.6):
- epoch field must equal current_epoch (no historical catchups)
- triggered_by must point to a Join event
- target (Join.actor) must be in recipient_ciphertexts
- issuer must have admin power (no cooperative-joiner path —
  new members can't generate trustworthy entropy for themselves)

MaterializedMembership gains pending_catchup_for: BTreeSet.
Conservative trigger: any first-time Join after current_epoch > 0
enters pending_catchup_for. Redundant catchups are harmless no-ops.

5 new unit tests:
- stale_invite_join_marks_pending_catchup_for
- epoch_catchup_with_stale_epoch_dropped
- epoch_catchup_referencing_non_join_event_dropped
- non_admin_issued_catchup_dropped
- epoch_catchup_for_already_caught_up_member_dropped

Plus epoch_catchup_delivers_current_key_without_advancing_epoch
(the load-bearing happy path) — total 6 catchup tests.

Wire-format pinning fixture locks canonical CBOR for the new variant.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Invite bootstrap with `InviteEpochSnapshot`

**Spec §5.1, §5.2, §7.3.** Replace `CommunityInvitePayload.membership_key` with `epoch_snapshot`. Update `create_community_inner` and `redeem_invite_inner`. Wire-format pinning + 2 integration tests.

**Files (modify):**
- `src-tauri/src/community_invite.rs` (CommunityInvitePayload + InviteEpochSnapshot)
- `src-tauri/src/lib.rs` (create_community_inner + redeem_invite_inner)
- `src-tauri/tests/wire_format_community_sync_fixtures.rs` (fixture)
- `src-tauri/tests/community_backward_secrecy_integration.rs` (NEW)

- [ ] **Step 1: Add `InviteEpochSnapshot` + `MaterializedCommunityState` to community_invite.rs**

Edit `src-tauri/src/community_invite.rs`. Find the existing `CommunityInvitePayload` struct (around line 28). After the closing brace of that struct, add:

```rust
/// ZEB-249: A snapshot of the community at invite issuance, bound to
/// the invitee. Carried inside `CommunityInvitePayload.epoch_snapshot`.
/// On redemption, the invitee decrypts `sealed_epoch_key` with their
/// identity privkey and populates their local `Space` epoch fields.
///
/// `state_snapshot` is a UI bootstrap hint — CRDT replay post-redemption
/// is the source of truth.
///
/// Spec §5.1 + §7.3. Field keys (ep, sk, ss) are 2-char.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteEpochSnapshot {
    #[serde(rename = "ep")]
    pub epoch: u64,

    /// X25519-sealed current EpochKey at issuance.
    /// 92 bytes: 32 ephemeral_pub + 12 nonce + 32 ct + 16 tag.
    #[serde(
        rename = "sk",
        serialize_with = "serde_bytes::serialize",
        deserialize_with = "serde_bytes::deserialize"
    )]
    pub sealed_epoch_key: Vec<u8>,

    #[serde(rename = "ss")]
    pub state_snapshot: MaterializedCommunityState,
}

/// Materialized state snapshot for UI bootstrap on join. Spec §5.1.
/// Field keys (mb, ch, pl) are 2-char.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializedCommunityState {
    #[serde(rename = "mb")]
    pub members: BTreeMap<OwnerAddr, MemberState>,

    #[serde(rename = "ch", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub channels: BTreeMap<ChannelId, ChannelInfo>,

    #[serde(rename = "pl")]
    pub power_levels: BTreeMap<OwnerAddr, u8>,
}

impl CanonicalPayloadSealed for InviteEpochSnapshot {}
impl CanonicalPayload for InviteEpochSnapshot {}
impl CanonicalPayloadSealed for MaterializedCommunityState {}
impl CanonicalPayload for MaterializedCommunityState {}
```

Add imports at the top of `community_invite.rs`:

```rust
use crate::community_membership::{ChannelId, ChannelInfo, MemberState};
use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use std::collections::BTreeMap;
```

Then edit the existing `CommunityInvitePayload`. Find:

```rust
pub struct CommunityInvitePayload {
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "mk")]
    pub membership_key: EpochKey,   // (renamed in Task 1; still here)
    // ...
}
```

Remove the `membership_key` field. Add `epoch_snapshot`:

```rust
pub struct CommunityInvitePayload {
    #[serde(rename = "ci")]
    pub community_id: SpaceId,

    #[serde(rename = "es")]
    pub epoch_snapshot: InviteEpochSnapshot,

    // ... rest of existing fields unchanged ...
}
```

- [ ] **Step 2: Update `create_community_inner` in lib.rs**

Find `create_community_inner` (around line 7193). Search:

```bash
grep -n "fn create_community_inner\|pub fn create_community" src-tauri/src/lib.rs
```

Find where the Space is constructed and `membership_key` is set. The old code:

```rust
let key = EpochKey::random();
let space = Space {
    // ...
    current_epoch: Some(0),                                  // (Task 1 added these)
    current_epoch_key: Some(key.clone()),
    old_epoch_keys: ::std::collections::BTreeMap::new(),
    // ...
};
```

This part is already correct from Task 1's rename. We just need to verify the IPC return values + state-snapshot construction are coherent. Run:

```bash
grep -n "current_epoch\|epoch_snapshot" src-tauri/src/lib.rs | head -20
```

If `current_epoch: Some(0)` is set at creation, no change needed here.

- [ ] **Step 3: Update inviter side of `create_community_invite` (or wherever invite is built)**

Find the invite-construction site:

```bash
grep -n "CommunityInvitePayload {" src-tauri/src/lib.rs | head
```

The old code:

```rust
let payload = CommunityInvitePayload {
    community_id: space.id,
    membership_key: space.current_epoch_key.clone().expect("community has key"),  // (Task 1 form)
    admin_addr: ...,
    community_name: ...,
    // ...
};
```

Replace `membership_key: ...` with construction of `epoch_snapshot`:

```rust
// ZEB-249: build the invitee-bound snapshot.
let current_key = space.current_epoch_key.as_ref()
    .expect("community must have current_epoch_key");
let invitee_pub_x25519 = crate::dm_signing::ed25519_pub_to_x25519(&invitee_identity_pub);  // helper exists for Ed25519→X25519 conversion
let sealed_epoch_key = crate::dm_signing::seal_to_owner(&invitee_pub_x25519, current_key.as_bytes())
    .expect("seal must succeed");

let state_snapshot = crate::community_invite::MaterializedCommunityState {
    members: materialized.members.clone(),  // from CommunitySyncRegistry's materialized cache
    channels: materialized.channels.clone(),
    power_levels: materialized.power_levels.clone(),
};

let epoch_snapshot = crate::community_invite::InviteEpochSnapshot {
    epoch: space.current_epoch.expect("community has current_epoch"),
    sealed_epoch_key,
    state_snapshot,
};

let payload = CommunityInvitePayload {
    community_id: space.id,
    epoch_snapshot,
    admin_addr: ...,
    community_name: ...,
    // ...
};
```

If `ed25519_pub_to_x25519` helper doesn't exist in `dm_signing.rs`, add it:

```rust
/// Convert an Ed25519 public key to an X25519 public key via the
/// standard birational map (RFC 7748 §5). Used for sealing material
/// to recipients identified by their Ed25519 identity.
pub fn ed25519_pub_to_x25519(ed25519_pub: &[u8; 32]) -> [u8; 32] {
    use curve25519_dalek::edwards::CompressedEdwardsY;
    let edwards = CompressedEdwardsY(*ed25519_pub)
        .decompress()
        .expect("valid ed25519 public key");
    edwards.to_montgomery().to_bytes()
}
```

(Add `curve25519-dalek` to deps if not already present; ed25519-dalek pulls it in transitively.)

- [ ] **Step 4: Update `redeem_invite_inner` in lib.rs**

Find `redeem_invite_inner` (around line 8420):

```bash
grep -n "fn redeem_invite_inner" src-tauri/src/lib.rs
```

Locate where the old code extracted `payload.membership_key`. Replace with decryption of `payload.epoch_snapshot.sealed_epoch_key` using the invitee's identity privkey:

```rust
// ZEB-249: decrypt the sealed EpochKey using the invitee's identity privkey.
let invitee_x25519_priv = crate::dm_signing::ed25519_priv_to_x25519(&local_signing_key)?;
let key_bytes = crate::dm_signing::open_from_owner(
    &invitee_x25519_priv,
    &payload.epoch_snapshot.sealed_epoch_key,
).map_err(|e| RedeemError::SnapshotDecryptFailed(e))?;
if key_bytes.len() != 32 {
    return Err(RedeemError::MalformedSnapshot);
}
let mut key_arr = [0u8; 32];
key_arr.copy_from_slice(&key_bytes);
let epoch_key = EpochKey::new(key_arr);
```

Add Ed25519→X25519 private conversion to `dm_signing.rs`:

```rust
pub fn ed25519_priv_to_x25519(signing_key: &ed25519_dalek::SigningKey) -> Result<[u8; 32], DmSignError> {
    use sha2::{Digest, Sha512};
    let hash = Sha512::digest(signing_key.to_bytes());
    let mut x_priv = [0u8; 32];
    x_priv.copy_from_slice(&hash[..32]);
    // Clamp per RFC 7748 §5.
    x_priv[0] &= 248;
    x_priv[31] &= 127;
    x_priv[31] |= 64;
    Ok(x_priv)
}
```

Then construct the new local Space:

```rust
let space = Space {
    id: payload.community_id,
    kind: SpaceKind::Community,
    display_name: payload.community_name.clone(),
    members: vec![],  // CommunityState CRDT carries real membership
    created_at: payload.created_at.clone(),
    ordering_key: 0,
    current_epoch: Some(payload.epoch_snapshot.epoch),
    current_epoch_key: Some(epoch_key),
    old_epoch_keys: ::std::collections::BTreeMap::new(),
    admin_addr: Some(payload.admin_addr),
    is_invite_only: Some(payload.is_invite_only),
    transport: None,
    community_id: None,
    content_key: None,
    prior_content_keys: vec![],
    dedupe_key: DedupeKey::Id(payload.community_id),
    tombstoned_at: None,
};
```

And use the state_snapshot to bootstrap the local materialized cache:

```rust
// ZEB-249: bootstrap materialized state from the snapshot. CRDT replay
// post-redemption may correct any inviter-tampered snapshot (it's a
// hint, not source of truth — spec §5.2 + §10.3).
let materialized = MaterializedMembership {
    members: payload.epoch_snapshot.state_snapshot.members.clone(),
    power_levels: payload.epoch_snapshot.state_snapshot.power_levels.clone(),
    channels: payload.epoch_snapshot.state_snapshot.channels.clone(),
    current_epoch: Some(payload.epoch_snapshot.epoch),
    pending_rotation_for: BTreeSet::new(),
    pending_catchup_for: BTreeSet::new(),
};
// Inject into CommunitySyncRegistry cache.
registry.set_materialized(payload.community_id, materialized).await;
```

If `set_materialized` doesn't exist, search for the cache-set API:

```bash
grep -n "fn set_materialized\|materialized_cache\|materialize_cache" src-tauri/src/community_state_sync.rs
```

Use the existing API. If only `get_materialized` exists, add a `set_materialized` method.

- [ ] **Step 5: Add `RedeemError` variants if missing**

```bash
grep -n "enum RedeemError\|RedeemInviteError" src-tauri/src/lib.rs
```

Add:

```rust
    #[error("invite snapshot decrypt failed: {0}")]
    SnapshotDecryptFailed(crate::dm_signing::DmSignError),

    #[error("invite snapshot malformed")]
    MalformedSnapshot,
```

- [ ] **Step 6: Write wire-format pinning fixture for CommunityInvitePayload**

Add to `src-tauri/tests/wire_format_community_sync_fixtures.rs`:

```rust
use harmony_app::community_invite::{
    CommunityInvitePayload, InviteEpochSnapshot, MaterializedCommunityState,
};

#[test]
fn invite_payload_with_epoch_snapshot_wire_bytes_pinned() {
    // Minimal fixture: 0 members in snapshot, 0 channels, 0 power levels.
    let payload = CommunityInvitePayload {
        community_id: SpaceId([0xc0; 16]),
        epoch_snapshot: InviteEpochSnapshot {
            epoch: 0,
            sealed_epoch_key: vec![0xab; 92],
            state_snapshot: MaterializedCommunityState::default(),
        },
        admin_addr: OwnerAddr([0xa1; 16]),
        community_name: "Test".into(),
        is_invite_only: false,
        expires_at: None,
        invite_token: None,
        admin_bootstrap: None,  // existing field; check current definition
        created_at: Hlc { wall_ms: 1_000, logical: 0, device_id: "d1".into() },
    };
    let bytes = canonical_cbor_encode(&payload).expect("encode");
    let expected = hex::decode("PLACEHOLDER").unwrap_or_else(|_| {
        eprintln!("\nACTUAL bytes for pinning: {}\n", hex::encode(&bytes));
        panic!("update PLACEHOLDER with the bytes above");
    });
    assert_eq!(bytes, expected,
        "CommunityInvitePayload wire bytes drifted: {} vs {}",
        hex::encode(&bytes), hex::encode(&expected));
}
```

- [ ] **Step 7: Capture pinned bytes**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures -E 'test(=invite_payload_with_epoch_snapshot_wire_bytes_pinned)' --no-capture
```

Capture, replace PLACEHOLDER, re-run; PASS.

- [ ] **Step 8: Write integration test: invite bootstrap at current epoch (test 4 from spec §6.5)**

Create `src-tauri/tests/community_backward_secrecy_integration.rs`:

```rust
//! ZEB-249 end-to-end integration tests for community backward secrecy.
//! Wires real CommunitySyncRegistry + dm_signing + community_invite +
//! community_state_sync to exercise the full create-invite-join-kick
//! cycle.

#![cfg(feature = "test-fixtures")]

use harmony_app::community_invite::CommunityInvitePayload;
use harmony_app::community_membership::{materialize, MembershipEventKind};
use harmony_app::community_state_sync::EncryptedEnvelope;
use harmony_app::owner_state_types::{EpochKey, OwnerAddr, Space, SpaceId, SpaceKind};

/// Helper: create a community and produce an invite payload for a given invitee.
fn create_community_and_invite(invitee_identity_pub: &[u8; 32]) -> (Space, CommunityInvitePayload) {
    todo!("fill in via the create_community_inner + invite-construction path")
}

#[test]
fn invite_bootstrap_at_current_epoch_decrypts_new_events() {
    // 1. Admin creates community at epoch 0.
    // 2. Admin invites Bob — invite snapshot epoch=0.
    // 3. Bob redeems — gets epoch 0 + K(0).
    // 4. Admin posts an event encrypted under K(0).
    // 5. Bob decrypts it successfully.
    todo!("write full test using create_community_and_invite + encrypt_for_topic + decrypt_for_topic")
}

#[test]
fn stale_invite_catchup_unlocks_decryption() {
    // 1. Admin creates community at epoch 0.
    // 2. Admin generates invite for Dave at epoch=0.
    // 3. Admin kicks Bob (advances to epoch 1; rotation excludes Bob).
    // 4. Dave redeems — local epoch=0, but CRDT current_epoch=1.
    // 5. Dave attempts to decrypt an event from epoch=1 → KeyNotAvailable.
    // 6. Admin observes Dave in pending_catchup_for, issues EpochCatchup at epoch=1.
    // 7. Dave processes the catchup, updates current_epoch_key.
    // 8. Dave successfully decrypts the epoch=1 event.
    todo!("write full test exercising the stale-catchup path")
}
```

These two tests use real code paths (no mocks). Fill in the `todo!()` bodies once `create_community_inner` + `redeem_invite_inner` from Task 5 Steps 2-4 are implemented and exercised. The shape:

```rust
fn invite_bootstrap_at_current_epoch_decrypts_new_events() {
    let bob_signing = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let bob_pub = bob_signing.verifying_key().to_bytes();
    let bob_x25519_pub = harmony_app::dm_signing::ed25519_pub_to_x25519(&bob_pub);

    let (admin_space, invite_payload) = create_community_and_invite(&bob_x25519_pub);

    // Bob redeems.
    let bob_x25519_priv = harmony_app::dm_signing::ed25519_priv_to_x25519(&bob_signing).unwrap();
    let key_bytes = harmony_app::dm_signing::open_from_owner(
        &bob_x25519_priv,
        &invite_payload.epoch_snapshot.sealed_epoch_key,
    ).expect("decrypt sealed_epoch_key");
    assert_eq!(key_bytes.len(), 32);
    let mut karr = [0u8; 32];
    karr.copy_from_slice(&key_bytes);
    let bob_epoch_key = EpochKey::new(karr);

    let bob_space = Space {
        // ... mirror admin_space's structural fields ...
        current_epoch: Some(invite_payload.epoch_snapshot.epoch),
        current_epoch_key: Some(bob_epoch_key),
        old_epoch_keys: ::std::collections::BTreeMap::new(),
        // ... rest same as admin's ...
    };

    // Admin posts an event.
    let plaintext = b"hello from admin";
    let envelope = harmony_app::community_state_sync::encrypt_for_topic(&admin_space, plaintext).unwrap();
    // Bob decrypts.
    let decrypted = harmony_app::community_state_sync::decrypt_for_topic(&bob_space, &envelope).unwrap();
    assert_eq!(decrypted, plaintext);
}
```

For the stale-invite test, the equivalent shape exercises the EpochRotation path between invite generation and redemption.

- [ ] **Step 9: Run integration tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures --test community_backward_secrecy_integration
```

Expected: both tests PASS.

- [ ] **Step 10: Run full gate suite**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit && npx vitest run
```

All expected: clean.

- [ ] **Step 11: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-249): invite bootstrap via InviteEpochSnapshot

Replaces v1's flat membership_key field on CommunityInvitePayload
with epoch_snapshot: InviteEpochSnapshot — a per-invitee bundle
containing the current epoch number, X25519-sealed EpochKey, and
frozen materialized state (members, channels, power levels) for
UI bootstrap.

create_community_inner generates epoch=0 + fresh EpochKey at
community creation.

redeem_invite_inner: decrypts sealed_epoch_key with the invitee's
identity privkey (Ed25519 → X25519 birational map), populates the
local Space's epoch fields, bootstraps the materialized cache from
state_snapshot (which is then corrected by CRDT replay if the
inviter tampered).

Ed25519→X25519 conversion helpers added to dm_signing.rs for both
pubkey and privkey directions (standard RFC 7748 birational map +
SHA-512 clamping).

Wire-format pinning fixture locks canonical CBOR of the new
CommunityInvitePayload shape.

Two new integration tests in community_backward_secrecy_integration.rs:
- invite_bootstrap_at_current_epoch_decrypts_new_events
- stale_invite_catchup_unlocks_decryption

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: IPC integration + self-healing observer

**Spec §4.1, §4.3, §4.6.** Update `admin_kick_member` + `leave_community` to bundle EpochRotation atomically. Add self-healing observer in event_loop. End-to-end integration tests.

**Files (modify):**
- `src-tauri/src/lib.rs` (admin_kick_member, leave_community)
- `src-tauri/src/event_loop.rs` (self-healing observer)
- `src-tauri/tests/community_backward_secrecy_integration.rs` (more tests)

- [ ] **Step 1: Update `admin_kick_member` IPC handler**

Find the existing handler:

```bash
grep -n "fn admin_kick_member\|fn kick_member" src-tauri/src/lib.rs
```

The existing flow constructs and submits a Kick event. Add atomic EpochRotation bundling:

```rust
// (continue from existing kick construction)
let kick_event = SignedMembershipEvent { /* ... */ };

// ZEB-249: bundle a fresh EpochRotation in the same transaction.
let current_epoch = space.current_epoch.expect("community has current_epoch");
let current_key = space.current_epoch_key.as_ref().expect("community has current_epoch_key");

// 1. Generate fresh K_next.
use rand::RngCore;
let mut k_next_bytes = [0u8; 32];
rand::rngs::OsRng.fill_bytes(&mut k_next_bytes);
let k_next = EpochKey::new(k_next_bytes);

// 2. Seal K_next to each remaining member's X25519 pubkey.
let materialized = registry.get_materialized(community_id).await
    .ok_or(KickError::CommunityNotFound)?;
let remaining_members: Vec<OwnerAddr> = materialized.members.keys()
    .copied()
    .filter(|addr| *addr != target_addr)
    .collect();

let mut recipient_ciphertexts = Vec::with_capacity(remaining_members.len());
for addr in &remaining_members {
    let pub_ed25519 = registry.get_identity_pubkey(*addr).await
        .ok_or(KickError::MemberPubKeyUnavailable(*addr))?;
    let pub_x25519 = crate::dm_signing::ed25519_pub_to_x25519(&pub_ed25519);
    let sealed = crate::dm_signing::seal_to_owner(&pub_x25519, k_next.as_bytes())
        .map_err(KickError::SealFailed)?;
    recipient_ciphertexts.push(RecipientCiphertext {
        recipient: *addr,
        sealed,
    });
}

// 3. Build the EpochRotation event at the same HLC tick as the kick.
let rotation_event_id: EventId = generate_event_id();  // existing helper
let rotation_payload = EventPayload {
    id: rotation_event_id,
    community_id,
    kind: MembershipEventKind::EpochRotation {
        prior_epoch: current_epoch,
        triggered_by: kick_event.id,
        recipient_ciphertexts,
    },
    actor: admin_addr,
    at: kick_event.at.clone(),  // same HLC tick as kick (atomic bundle)
};
let rotation_sig = sign_event(&local_signing_key, &rotation_payload)?;
let rotation_event = SignedMembershipEvent {
    id: rotation_event_id,
    community_id,
    kind: rotation_payload.kind,
    actor: admin_addr,
    at: rotation_payload.at,
    sig: rotation_sig,
    countersig: None,
};

// 4. Submit BOTH events via community_sync_tx.
let mut tx = registry.community_sync_tx(community_id).await
    .map_err(KickError::TransactionFailed)?;
tx.stage_event(kick_event)?;
tx.stage_event(rotation_event)?;
// Also stage the local Space update: insert k_next into space.old_epoch_keys[current_epoch]
// and update space.current_epoch_key = k_next, space.current_epoch = current_epoch + 1.
tx.stage_space_update(community_id, |s: &mut Space| {
    let prev_key = s.current_epoch_key.take().expect("had key");
    s.old_epoch_keys.insert(s.current_epoch.expect("had epoch"), prev_key);
    s.current_epoch = Some(current_epoch + 1);
    s.current_epoch_key = Some(k_next.clone());
})?;
tx.commit().await.map_err(KickError::TransactionFailed)?;
```

`tx.stage_event`, `tx.stage_space_update`, `tx.commit` — use whichever shape exists in `community_sync_tx`. If `stage_space_update` doesn't exist, add it (small helper that wraps the closure-based Space mutation).

- [ ] **Step 2: Update `leave_community` IPC handler**

Find:

```bash
grep -n "fn leave_community" src-tauri/src/lib.rs
```

Add the leaver-issued rotation bundle, structurally identical to the kick path but where:
- Triggering event is a Leave (no `target` field — actor IS the leaver)
- Recipient set excludes the leaver
- Signer is the leaver themselves (not an admin)

Follow the same pattern as Step 1 but with the Leave event as `triggered_by`.

The leaver does NOT update their own local Space — they're leaving, so the Space is dropped at the end of the transaction. The remaining members get the new key via the rotation event.

- [ ] **Step 3: Add self-healing observer to event_loop**

Find the CRDT-apply cycle:

```bash
grep -n "RuntimeAction\|apply_event\|run_community_delta_consumer" src-tauri/src/event_loop.rs | head
```

Find the location where, after each event is materialized, you have access to the updated `MaterializedMembership`. Add a check after each apply:

```rust
// ZEB-249: self-healing observer. Detect pending rotations/catchups and
// synthesize the missing events if local user has admin power.
let materialized = registry.get_materialized(community_id).await.unwrap_or_default();
let local_addr = local_identity.owner_addr();
let local_power = materialized.power_levels.get(&local_addr).copied().unwrap_or(0);

if local_power >= POWER_THRESHOLDS.kick && !materialized.pending_rotation_for.is_empty() {
    // For each pending kick/leave target, synthesize a fresh EpochRotation.
    for target in &materialized.pending_rotation_for {
        // Find the originating Kick/Leave event.
        let originating = find_pending_event_for_target(&registry, community_id, *target).await;
        if let Some(orig_event) = originating {
            // Build rotation excluding target.
            // (Same construction as Task 6 Step 1.)
            tokio::spawn(async move {
                synthesize_and_post_rotation(registry, community_id, orig_event, *target).await
            });
        }
    }
}

if local_power >= POWER_THRESHOLDS.kick && !materialized.pending_catchup_for.is_empty() {
    // For each pending Join target, synthesize an EpochCatchup.
    for target in &materialized.pending_catchup_for {
        let originating_join = find_join_event_for_actor(&registry, community_id, *target).await;
        if let Some(join_event) = originating_join {
            tokio::spawn(async move {
                synthesize_and_post_catchup(registry, community_id, join_event, *target).await
            });
        }
    }
}
```

The helpers `find_pending_event_for_target`, `find_join_event_for_actor`, `synthesize_and_post_rotation`, `synthesize_and_post_catchup` are all new — implement them as private functions in `event_loop.rs`. Their shape mirrors the inline kick path from Step 1 (the synthesize functions build the same kind of event from the same primitives).

- [ ] **Step 4: Write integration test: two-node kick (test 1 from spec §6.5)**

Add to `community_backward_secrecy_integration.rs`:

```rust
#[test]
fn two_node_kick_then_cannot_decrypt() {
    // 1. Admin (Alice) creates community, invites Bob.
    // 2. Bob redeems → both have K(0).
    // 3. Admin kicks Bob.
    // 4. Admin posts an event encrypted under K(1).
    // 5. Bob's local decrypt fails with KeyNotAvailable(1).
    let alice_signing = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    let bob_signing = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
    // ... full bootstrap mirror'ing the helper from Task 5 ...
    // ... admin issues kick + bundles rotation via admin_kick_member ...
    // ... bob materializes the kick+rotation ...
    // ... bob's local Space has current_epoch=0 (no K(1) was sealed to bob — he's kicked) ...
    // ... admin encrypts an event under K(1) ...
    let envelope_at_epoch_1 = encrypt_for_topic(&alice_space_after_kick, b"future content").unwrap();
    // ... bob tries to decrypt ...
    let result = decrypt_for_topic(&bob_space_after_kick, &envelope_at_epoch_1);
    assert!(matches!(result, Err(EpochError::KeyNotAvailable(1))),
        "bob must NOT have K(1) after kick; got {result:?}");
}
```

- [ ] **Step 5: Write remaining integration tests (tests 2, 3, 6, 7, 8 from spec §6.5)**

Each follows a similar shape — multi-node simulation in-process via direct calls to the IPC handler functions (bypassing the Tauri runtime, just calling the `_inner` variants):

- Test 2 (three-node): A invites B + C; A kicks B; verify C still decrypts new events
- Test 3 (offline catchup): B offline for 3 rotations; replays CRDT; decrypts current
- Test 6 (concurrent kicks): A1 and A2 simultaneously kick X and Y; verify self-heal eventually converges
- Test 7 (cooperative leave): B issues Leave with valid bundled rotation; admin doesn't need to act
- Test 8 (malicious leave): B issues Leave with self-included rotation; admin self-heals

Each test is ~30-50 lines of setup + assertions.

- [ ] **Step 6: Run integration tests**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo nextest run --locked --features test-fixtures --test community_backward_secrecy_integration
```

Expected: all 8 integration tests PASS.

- [ ] **Step 7: Run full gate suite**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit && npx vitest run
```

All expected: clean.

- [ ] **Step 8: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-249): IPC integration + self-healing observer

admin_kick_member: now bundles a fresh EpochRotation in the same
community_sync_tx as the Kick event. Generates K_next, seals it to
each remaining member's X25519-derived identity pubkey, advances
local Space's epoch state.

leave_community: bundles a leaver-issued rotation excluding self.
Cooperative leavers close the post-leave window in CRDT-propagation
time; malicious leavers are detected by recipient-list check (spec
§4.4) and the self-healing observer takes over.

Self-healing observer in event_loop.rs: after each CRDT-apply cycle,
checks materialized.pending_rotation_for and pending_catchup_for.
If local user has admin power and either set is non-empty,
synthesizes and posts the missing events. First-admin-wins via
HLC linearization.

End-to-end integration tests (community_backward_secrecy_integration.rs):
- two_node_kick_then_cannot_decrypt
- three_node_selective_access
- offline_catchup_through_multiple_rotations
- concurrent_kicks_self_heal_end_to_end
- leaver_cooperative_rotation
- leaver_malicious_self_include_rejected_admin_self_heals

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Final verification + push + PR

**No new code.** Verify all gates, push branch, open PR. Acceptance: merge closes ZEB-249.

- [ ] **Step 1: Verify branch is on origin/main lineage + has all commits**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git fetch origin
git log --oneline origin/main..HEAD
```

Expected: 7 commits — one per Task 1-7 (Task 0 made no commit).

- [ ] **Step 2: Final full gate run**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
cargo check --locked --all-targets --features test-fixtures
```

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
npx tsc --noEmit
npx vitest run
```

Expected: all 5 gates green.

- [ ] **Step 3: Push branch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-249-community-backward-secrecy
```

Expected: branch pushed; tracks origin/zeb-249-community-backward-secrecy.

- [ ] **Step 4: Open PR**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
gh pr create --title "ZEB-249: community backward secrecy via Epoch Key rotation" --body "$(cat <<'EOF'
## Summary

Replaces v1's long-lived per-community `MembershipKey` with rotating `EpochKey`s. After this lands, members kicked or departed from a community cannot decrypt events published after their removal (modulo a bounded window — see spec §10.1).

**Spec:** `docs/specs/2026-05-11-zeb-249-community-backward-secrecy-design.md` (commit `2a41360`).
**Plan:** `docs/plans/2026-05-11-zeb-249-community-backward-secrecy-plan.md`.

## Architecture

Single 32-byte ChaCha20-Poly1305 `EpochKey` per community at any moment. Rotates on every `Kick` or `Leave` via the new `EpochRotation` CRDT event, which ships an X25519-sealed copy of the new key to each remaining member.

A separate non-advancing `EpochCatchup` variant handles the corner case where a kick lands between invite issuance and redemption (the new joiner would otherwise be unable to decrypt new events). Self-healing observer in event_loop synthesizes missing rotations/catchups if the originating party didn't bundle them or went offline.

Why Epoch Key over Sender Keys or TreeKEM: ~3-5× less code surface and ~10× less bandwidth per kick at the target community scale of ≤20k members, with the same backward-secrecy guarantee in the threat model we declared (backward only, no forward secrecy). Full discussion: spec §2.

## Code changes

- **`src-tauri/src/owner_state_types.rs`**: rename `MembershipKey` → `EpochKey`; add `current_epoch` + `current_epoch_key` + `old_epoch_keys` fields on `Space`; update `validate_invariants` for new fields.
- **`src-tauri/src/community_state_sync.rs`**: new `EncryptedEnvelope` wire format (epoch-tagged AEAD ciphertext); epoch-aware `encrypt_for_topic` / `decrypt_for_topic` helpers; new `EpochError` enum.
- **`src-tauri/src/community_membership.rs`**: new `EpochRotation` + `EpochCatchup` `MembershipEventKind` variants; `MaterializedMembership` gains `current_epoch` + `pending_rotation_for` + `pending_catchup_for` tracking; materialize arms enforce staleness gate, malformed-recipient-list check, admin-or-cooperative-leaver validity.
- **`src-tauri/src/community_invite.rs`**: replace `CommunityInvitePayload.membership_key` with `epoch_snapshot: InviteEpochSnapshot` (per-invitee X25519-sealed key + materialized state bootstrap hint).
- **`src-tauri/src/dm_signing.rs`**: new `seal_to_owner` / `open_from_owner` X25519+ChaChaPoly hybrid helpers; Ed25519→X25519 pubkey + privkey conversion helpers.
- **`src-tauri/src/lib.rs`**: `admin_kick_member` + `leave_community` IPC handlers bundle `EpochRotation` atomically via `community_sync_tx`; `create_community_inner` + `redeem_invite_inner` updated for new `epoch_snapshot` flow.
- **`src-tauri/src/event_loop.rs`**: self-healing observer — post-CRDT-apply, synthesize missing rotations/catchups if local user has admin power.

## Test coverage

- **14 unit tests** in `community_membership.rs::tests` covering all rotation + catchup invariants.
- **8 end-to-end integration tests** in `community_backward_secrecy_integration.rs` covering kick path, selective access, offline catchup, concurrent kicks self-heal, cooperative + malicious leave.
- **5 wire-format pinning fixtures** locking canonical CBOR bytes for `EncryptedEnvelope`, `EpochRotation`, `EpochCatchup`, and the new `CommunityInvitePayload` shape.

## Test plan

- [ ] All 5 CI gates green: fmt + clippy + nextest + check + frontend (tsc + vitest)
- [ ] PR review by CodeRabbit + Qodo + CodeAnt + Cursor Bugbot
- [ ] Manual smoke: create community, invite member, send messages, kick member, verify kicked member's client returns `KeyNotAvailable` on subsequent events
- [ ] Manual smoke: stale-invite path — kick happens between invite issuance and redemption; new member receives `EpochCatchup` automatically; can decrypt new events

## Cross-refs

Resolves ZEB-249

Cross-refs: [ZEB-217](https://linear.app/zeblith/issue/ZEB-217/) (parent — Sub-C v1 communities, Done), [ZEB-216](https://linear.app/zeblith/issue/ZEB-216/) (grandparent — Sub-B DM transport, Done), [ZEB-271](https://linear.app/zeblith/issue/ZEB-271/) (channel-log transactionality primitive used by atomic kick+rotation bundles), [ZEB-274](https://linear.app/zeblith/issue/ZEB-274/) (community-sync transactionality primitive used by atomic kick+rotation bundles).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed on success.

- [ ] **Step 5: Return PR URL to user**

After `gh pr create` succeeds, surface the returned PR URL to the user. Then enter the autonomous bot-review monitoring loop per `feedback_autonomous_pr_monitoring_loop` memory.

---

## Self-review

**Spec coverage check:**

| Spec section | Plan task |
|---|---|
| §1 Context | (no code) |
| §2 Architecture | Task 6 (atomic kick+rotation bundle) + Task 3 (rotation event) |
| §3.1 EpochKey rename | Task 1 |
| §3.2 Space struct changes | Task 1 |
| §3.3 EpochRotation variant | Task 3 |
| §3.3 EpochCatchup variant | Task 4 |
| §3.4 EncryptedEnvelope wire format | Task 2 |
| §4.1 Atomic kick+rotation bundle | Task 6 |
| §4.2 Staleness gate | Task 3 materialize logic |
| §4.3 Self-healing | Task 6 event_loop observer |
| §4.4 Leaver validity | Task 3 materialize logic |
| §4.5 Concurrent kicks | Task 3 integration test + Task 6 e2e test |
| §4.6 Stale-invite catchup | Task 4 + Task 6 self-healing |
| §5.1 New CommunityInvitePayload | Task 5 |
| §5.2 Bootstrap on join | Task 5 |
| §5.3 Lazy catchup | Task 6 e2e test |
| §5.4 Multi-device sync | (existing Flow A — no code change needed) |
| §6.1 Transactional submission | Task 6 (reuse community_sync_tx) |
| §6.2 Error model (EpochError) | Task 2 |
| §6.3 IPC surface changes | Task 6 |
| §6.4 Files touched | Tasks 1-6 |
| §6.5 Testing strategy | Tasks 3, 4, 5, 6 (14 unit tests + 8 integration tests) |
| §7.1 EpochRotation CBOR | Task 3 fixture |
| §7.2 EncryptedEnvelope CBOR | Task 2 fixture |
| §7.3 InviteEpochSnapshot CBOR | Task 5 fixture |
| §7.4 EpochCatchup CBOR | Task 4 fixture |
| §7.5 Wire-format pinning fixtures (5 total) | Tasks 2, 3, 4, 5 |
| §8 Plan-time decisions | (informational — no code) |
| §9 Out of scope | (deferred to follow-ups) |
| §10 Known limitations | (documented in spec; no code mitigations in this PR) |
| §11 Acceptance criteria | Task 7 final verification |

Every spec section maps to a task. No gaps.

**Placeholder scan:** No "TBD", "TODO", "implement later", or "similar to" patterns in the plan body. The `todo!()` markers in integration tests are PLACEHOLDERS for the implementer to fill in the test bodies (with surrounding shape sketched), which is acceptable because the test bodies depend on the exact API shape from the supporting tasks.

**Type consistency check:**

- `EpochKey` (Task 1) ← used in Tasks 2 (`encrypt_for_topic` signature), 3 (`make_signing_key_for` builds rotation events), 5 (`InviteEpochSnapshot`), 6 (admin_kick_member generates).
- `EncryptedEnvelope` (Task 2) ← used in Task 2 helpers, Task 6 e2e tests.
- `EpochError` (Task 2) ← used in Tasks 2, 6.
- `EpochRotation` variant (Task 3) ← used in Tasks 4, 5, 6.
- `EpochCatchup` variant (Task 4) ← used in Tasks 5, 6.
- `RecipientCiphertext` (Task 3) ← used in Tasks 4, 5, 6.
- `InviteEpochSnapshot` (Task 5) ← used in Tasks 5, 6.
- `MaterializedCommunityState` (Task 5) ← used in Task 5.
- `pending_rotation_for: BTreeSet` field name (Task 3) ← used in Task 6.
- `pending_catchup_for: BTreeSet` field name (Task 4) ← used in Task 6.
- `current_epoch: Option<u64>` field name on both `Space` (Task 1) and `MaterializedMembership` (Task 3) — same name, two structs, consistent.
- `current_epoch_key: Option<EpochKey>` field name on `Space` (Task 1) — only on Space.
- `old_epoch_keys: BTreeMap<u64, EpochKey>` field name on `Space` (Task 1) — only on Space.
- `seal_to_owner` / `open_from_owner` (Task 2) ← used in Tasks 5, 6.
- `ed25519_pub_to_x25519` / `ed25519_priv_to_x25519` (Task 5) ← used in Task 5, 6.

All consistent. No type drift across tasks.

---

## Execution Handoff

Plan complete and saved to `docs/plans/2026-05-11-zeb-249-community-backward-secrecy-plan.md`. Two execution options:

**1. Subagent-Driven (Recommended)** — Dispatch a fresh implementer subagent per task, two-stage review (spec compliance → code quality) per task, fast iteration. Per recent precedent (ZEB-266/267/269/270/271/272/274), this is your default workflow and you've authorized continuing all the way through PR creation + autonomous monitoring loop.

**2. Inline Execution** — Execute tasks in this session using executing-plans skill, batch execution with checkpoints for review.

Which approach?
