# ZEB-280 Sub-D Phase 3 — Federated Republication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a second cryptographic attestation layer to `LibraryDirectoryEntry` (broadcasting library wraps the admin-signed entry with its own Ed25519 signature) so consumers can verify federation chains and surface an "unattested" badge when wrapping signatures fail.

**Architecture:** Wire-compatible extension of Phase 1's `LibraryDirectoryEntry` with two `Option<[u8; 64]>` fields (`library_identity_pub`, `library_signature`) using `skip_serializing_if = "Option::is_none"` so Phase 1 wire bytes stay byte-identical. New `AttestationStatus` enum (Unwrapped | Attested(addr) | Unattested(addr)) returned by `verify_entry`. Aggregation evolves to track two sets (`attested_by` + `unattested_by`) instead of Phase 1's single `listed_by` set. Frontend gains `DirectoryEntry.unattested: boolean` + amber badge inline next to community name. No new IPCs.

**Tech Stack:** Rust (Tauri backend), Svelte 5 + TypeScript (frontend), Ed25519 (ed25519-dalek), CBOR (ciborium), vitest (frontend tests), cargo-nextest (Rust tests).

**Spec:** `docs/specs/2026-05-12-zeb-280-sub-d-phase-3-federated-republication-design.md` (commit `87dcaca`, 475 lines, §1-§15).

**Branch:** `zeb-280-sub-d-phase-3-federated-republication` (already cut from `origin/main` `1b7c3be` post-PR-#109-merge).

---

## File structure

| File | Role | Action |
|---|---|---|
| `src-tauri/src/owner_state_types.rs` | Adds `serialize_optional_bytes_as_bstr` + `deserialize_optional_bytes_from_bstr` helpers | Modify |
| `src-tauri/src/library_directory.rs` | Adds Optional wire fields + `AttestationStatus` enum + new error variants + extends `verify_entry` + evolves `AggregatedEntry`/`Aggregation`/`DirectoryEntryDTO`/`process_sample` | Modify |
| `src-tauri/tests/common/library_fixtures.rs` | Adds `mock_library_entry_wrapped` + `mock_library_entry_republished_by` helpers | Modify |
| `src-tauri/tests/wire_format_library_directory_fixtures.rs` | Adds 3 Phase 3 pinning tests; existing Phase 1 tests must remain byte-identical | Modify |
| `src-tauri/tests/library_directory_integration.rs` | Adds 4 federation integration tests; existing tests updated for renamed `listed_by` → `attested_by` field | Modify |
| `src/lib/library-directory-service.ts` | Adds `unattested: boolean` to `DirectoryEntry` interface | Modify |
| `src/lib/components/LibraryDirectoryBrowser.svelte` | Adds inline `⚠ Unattested` badge | Modify |
| `src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts` | Adds 2 vitest cases for badge rendering | Modify |

---

## Task 0: Pre-flight + green-baseline confirm

**Goal:** Confirm all 5 CI gates green BEFORE any code changes. Capture baseline test counts so later regressions are obvious. No commit.

**Files:** None.

- [ ] **Step 1: Confirm branch + clean working tree**

Run:
```bash
git status
git rev-parse --abbrev-ref HEAD
git rev-parse HEAD
```

Expected output:
- Branch: `zeb-280-sub-d-phase-3-federated-republication`
- HEAD: `87dcaca` (spec commit) on top of `1b7c3be` (origin/main, PR #109 merge)
- Working tree clean

- [ ] **Step 2: Confirm origin/main is fully pulled**

Run:
```bash
git fetch origin
git log --oneline origin/main..HEAD | head -5
git log --oneline HEAD..origin/main | head -5
```

Expected: `origin/main..HEAD` lists only `87dcaca` (the spec commit). `HEAD..origin/main` is empty (no upstream commits we don't have).

- [ ] **Step 3: Run cargo fmt check**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: exit 0, no output. If non-zero, the baseline is already broken — STOP and surface to user.

- [ ] **Step 4: Run cargo clippy with --features test-fixtures**

Run:
```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
```

Expected: exit 0, ends with `Finished ... profile`. No warnings. If warnings exist, STOP.

- [ ] **Step 5: Run cargo nextest with --features test-fixtures**

Run:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
```

Expected: passes summary like `Summary [...] N tests run: N passed, 0 failed, M skipped` where N is around 1100 (Phase 2 baseline was 1103). Note the EXACT pass count for later regression detection.

If nextest fails on `community_channel_log_engine::tests::shutdown_completes_promptly` (the known flake, ZEB-282), re-run once:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures -E 'test(community_channel_log_engine::tests::shutdown_completes_promptly)'
```
Then re-run the full suite. If it persists across 2 runs, surface to user (don't auto-bump the budget — that's ZEB-282's job).

- [ ] **Step 6: Run cargo check (MSRV gate)**

Run:
```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -5
```

Expected: `Finished ... profile`. No errors.

- [ ] **Step 7: Run npx tsc --noEmit (frontend type check)**

Run:
```bash
npx tsc --noEmit 2>&1 | tail -5
```

Expected: no output, exit 0.

- [ ] **Step 8: Run npx vitest (frontend tests)**

Run:
```bash
npx vitest run 2>&1 | tail -10
```

Expected: `Test Files N passed (N)` + `Tests M passed (M)` where M is around 1600 (Phase 2 baseline was 1608). Note the EXACT pass count.

- [ ] **Step 9: Record baseline counts**

Document the baseline in a scratchpad (NOT committed — it's just an aide-memoire for later regression triage):

- Rust tests: <COUNT> passed
- Frontend tests: <COUNT> passed
- Last green commit: `87dcaca`

**No commit.** Task 0 ends with a green baseline confirmed; later tasks must preserve this.

---

## Task 1: Optional bstr helpers + wire format extension + `AttestationStatus` enum + extended error variants

**Goal:** Add the structural Phase 3 surface (new struct fields, new enum, new error variants, new helper fns) without touching `verify_entry`'s body. Phase 1 wire-format pinning fixtures MUST remain byte-identical (`skip_serializing_if = "Option::is_none"` invariant).

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs` (add 2 new helper fns)
- Modify: `src-tauri/src/library_directory.rs` (extend struct + add enum + add error variants)
- Test: `src-tauri/tests/wire_format_library_directory_fixtures.rs` (existing fixtures pass unchanged; Task 4 adds new ones)

- [ ] **Step 1: Add `serialize_optional_bytes_as_bstr` helper to `owner_state_types.rs`**

Locate the existing `serialize_bytes_as_bstr` (lines ~18-26) and INSERT the following new fn immediately after the `deserialize_bytes_from_bstr` block ends (after line 127).

Code to add:

```rust
/// Helper: serialize `Option<[u8; N]>` as CBOR bstr (major type 2)
/// when `Some`. Pair with `deserialize_optional_bytes_from_bstr` and
/// `#[serde(skip_serializing_if = "Option::is_none")]` on the field so
/// `None` cases omit the key entirely from canonical CBOR (preserving
/// wire-format byte-identity with earlier schema versions that didn't
/// have the field).
///
/// ZEB-280 (Sub-D Phase 3) adds Optional `library_identity_pub` and
/// `library_signature` fields to `LibraryDirectoryEntry`. Phase 1
/// entries (no wrapping sig) must serialize to byte-identical CBOR
/// when the new fields are `None` — see spec §4.1.
pub(crate) fn serialize_optional_bytes_as_bstr<const N: usize, S>(
    b: &Option<[u8; N]>,
    s: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    // `skip_serializing_if = "Option::is_none"` on the field guarantees
    // serializer is only called for `Some(...)` — but be defensive in
    // case a future caller forgets the attribute.
    match b {
        Some(arr) => s.serialize_bytes(arr),
        None => s.serialize_none(),
    }
}

/// Helper: deserialize CBOR bstr into `Option<[u8; N]>`. Returns
/// `Some(arr)` on a bstr, `None` on CBOR null OR absent field (the
/// absent-field case is handled by `#[serde(default)]` on the field).
/// Pair with `serialize_optional_bytes_as_bstr`.
pub(crate) fn deserialize_optional_bytes_from_bstr<'de, const N: usize, D>(
    d: D,
) -> Result<Option<[u8; N]>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Visitor;
    use std::fmt;

    struct OptBytesVisitor<const N: usize>;

    impl<'de, const N: usize> Visitor<'de> for OptBytesVisitor<N> {
        type Value = Option<[u8; N]>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            write!(formatter, "an optional byte string of length {}", N)
        }

        fn visit_none<E>(self) -> Result<Option<[u8; N]>, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Option<[u8; N]>, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D2>(self, d: D2) -> Result<Option<[u8; N]>, D2::Error>
        where
            D2: Deserializer<'de>,
        {
            let arr: [u8; N] = crate::owner_state_types::deserialize_bytes_from_bstr(d)?;
            Ok(Some(arr))
        }

        fn visit_bytes<E>(self, value: &[u8]) -> Result<Option<[u8; N]>, E>
        where
            E: serde::de::Error,
        {
            if value.len() != N {
                return Err(E::custom(format!(
                    "expected {} bytes, got {}",
                    N,
                    value.len()
                )));
            }
            let mut arr = [0u8; N];
            arr.copy_from_slice(value);
            Ok(Some(arr))
        }

        fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Option<[u8; N]>, E>
        where
            E: serde::de::Error,
        {
            self.visit_bytes(&v)
        }
    }

    d.deserialize_option(OptBytesVisitor::<N>)
}
```

- [ ] **Step 2: Verify owner_state_types.rs compiles**

Run:
```bash
cd src-tauri && cargo check --locked --features test-fixtures 2>&1 | tail -5
```

Expected: `Finished ... profile`. No errors.

- [ ] **Step 3: Extend `LibraryDirectoryEntry` with Optional fields in `library_directory.rs`**

Locate the existing struct declaration (lines 31-67). After the existing `community_signature` field and before the closing `}`, ADD the following fields:

```rust
    // === Sub-D Phase 3 (ZEB-280) wrapping signature fields ===
    //
    // Wire-compatible with Phase 1: `skip_serializing_if = "Option::is_none"`
    // omits the keys from canonical CBOR when None, so a Phase 1 entry's
    // bytes are byte-identical regardless of whether it's emitted by a
    // Phase 1 or Phase 3 client.
    //
    // 2-char field keys preserve `canonical_cbor_encode`'s same-length-keys
    // precondition (mirrors all other Sub-A/B/C wire types).
    //
    // See spec §4.1.

    /// 64-byte identity bundle (X25519_pub || Ed25519_pub) of the
    /// broadcasting library. None for unwrapped (Phase 1-style) entries.
    #[serde(
        rename = "li",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub library_identity_pub: Option<[u8; 64]>,

    /// Ed25519 wrapping signature from the broadcasting library over
    /// the canonical CBOR encoding of all fields with `library_signature`
    /// zeroed (analogous to Phase 1's `community_signature` pattern).
    /// None for unwrapped entries.
    #[serde(
        rename = "ls",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub library_signature: Option<[u8; 64]>,
```

The final struct looks like:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryDirectoryEntry {
    #[serde(rename = "cd")]
    pub community_id: SpaceId,

    #[serde(
        rename = "ai",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub community_admin_identity_pub: [u8; 64],

    #[serde(rename = "nm")]
    pub name: String,

    #[serde(rename = "ds")]
    pub description: String,

    #[serde(rename = "tp")]
    pub topics: Vec<String>,

    #[serde(rename = "iu")]
    pub invite_url: String,

    #[serde(rename = "lb")]
    pub listed_by: OwnerAddr,

    #[serde(rename = "la")]
    pub listed_at: Hlc,

    #[serde(
        rename = "cs",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub community_signature: [u8; 64],

    // === Sub-D Phase 3 (ZEB-280) wrapping signature fields ===
    // ... (fields described above) ...
    #[serde(
        rename = "li",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub library_identity_pub: Option<[u8; 64]>,

    #[serde(
        rename = "ls",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub library_signature: Option<[u8; 64]>,
}
```

- [ ] **Step 4: Add `AttestationStatus` enum after the `LibraryDirectoryEntry` struct block**

Insert immediately after `impl CanonicalPayload for LibraryDirectoryEntry {}` (around line 70):

```rust
/// Sub-D Phase 3 (ZEB-280) — outcome of `verify_entry`. Captures the
/// admin-sig-verified entry's wrapping-signature state, which feeds the
/// aggregation's broadcasting-library tracking and the frontend
/// "unattested" badge. Spec §4.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationStatus {
    /// Phase 1-style entry: no wrapping sig present (both
    /// `library_signature` and `library_identity_pub` are `None`).
    /// Implicit trust from subscription topic — entries arriving from
    /// library X's topic are treated as if X attested to them.
    Unwrapped,
    /// Phase 3: wrapping sig present and verified. `OwnerAddr` is the
    /// broadcasting library's derived address (from
    /// `library_identity_pub` via `Identity::from_public_bytes`).
    Attested(OwnerAddr),
    /// Phase 3: wrapping sig present but invalid. Entry is still
    /// surfaced — the community admin's signature is the trust anchor
    /// for content. `OwnerAddr` is the broadcasting library's CLAIMED
    /// address (the derived addr from `library_identity_pub`; we keep
    /// it for aggregation tracking even though we couldn't verify the
    /// claim).
    Unattested(OwnerAddr),
}
```

- [ ] **Step 5: Extend `EntryVerifyError` with two new variants**

Locate the existing `EntryVerifyError` enum (around lines 128-165 — has variants `InvalidIdentityPub`, `Encode`, `SignatureInvalid`, etc.).

Add the following two variants at the end of the enum (after the existing `PayloadAdminIdentityMismatch` variant, before the closing `}`):

```rust
    /// Sub-D Phase 3: exactly one of `library_signature` and
    /// `library_identity_pub` is `Some`. Cannot verify a wrapping sig
    /// without both fields; this is a malformed wire state and must
    /// be rejected (not silently treated as Unwrapped, which would
    /// mask a publisher bug). Spec §5.
    #[error("library_signature and library_identity_pub must both be Some or both be None")]
    LibrarySignatureFieldsInconsistent,

    /// Sub-D Phase 3: `library_identity_pub` bytes failed
    /// `Identity::from_public_bytes` validation. Spec §5.
    #[error("malformed library identity_pub: {0}")]
    InvalidLibraryIdentityPub(String),
```

- [ ] **Step 6: Update existing in-module tests that construct `LibraryDirectoryEntry` literals**

Search for `community_signature: [` in `src-tauri/src/library_directory.rs` (the in-module `#[cfg(test)] mod tests` block). Each direct literal constructor of `LibraryDirectoryEntry { ... }` must add the two new fields at the end:

```rust
        // ... existing fields ending in community_signature: [...] ,
        library_identity_pub: None,
        library_signature: None,
```

Use Edit replace_all for the simple cases. There are around 5-6 sites; each should pattern as:

```rust
LibraryDirectoryEntry {
    community_id: ...,
    // ...
    community_signature: [...],
    library_identity_pub: None,
    library_signature: None,
}
```

For Task 1, ALL existing tests get the trailing `library_identity_pub: None, library_signature: None` — Task 2 introduces the wrapping-aware tests separately.

- [ ] **Step 7: Update `mock_directory_entry` fixture (Phase 1 backward-compat path)**

Open `src-tauri/tests/common/library_fixtures.rs`. The existing `mock_directory_entry` constructs an unwrapped Phase 1 entry. Add the two new fields to its `LibraryDirectoryEntry { ... }` literal (the only change — Task 4 adds NEW helpers for wrapped entries):

Locate the literal at lines ~45-55:

```rust
    let mut entry = LibraryDirectoryEntry {
        community_id,
        community_admin_identity_pub: identity_pub,
        name: name.to_string(),
        description: description.to_string(),
        topics,
        invite_url,
        listed_by,
        listed_at,
        community_signature: [0u8; 64],
    };
```

Change to:

```rust
    let mut entry = LibraryDirectoryEntry {
        community_id,
        community_admin_identity_pub: identity_pub,
        name: name.to_string(),
        description: description.to_string(),
        topics,
        invite_url,
        listed_by,
        listed_at,
        community_signature: [0u8; 64],
        library_identity_pub: None,
        library_signature: None,
    };
```

The `for_sig` clone block immediately after is unchanged (it sets `community_signature = [0u8; 64]` before signing — both Optional fields are already None and `skip_serializing_if` omits them from CBOR, so the signed bytes match Phase 1's signed bytes exactly).

- [ ] **Step 8: Add a round-trip unit test for the new wire shape**

Add to the `tests` mod in `src-tauri/src/library_directory.rs` (locate `#[cfg(test)] mod tests {` block):

```rust
    /// ZEB-280 Phase 3: an entry with `library_identity_pub` and
    /// `library_signature` both populated (some `[0u8; 64]` sentinel
    /// here — Task 2 adds real-signer verifier tests) round-trips
    /// through canonical CBOR and the bstr serde helpers correctly.
    #[test]
    fn phase3_wrapped_entry_roundtrips_via_canonical_cbor() {
        let community_id = SpaceId([0x11; 16]);
        let admin_addr = OwnerAddr([0; 16]);
        let (admin_signing_key, admin_identity_pub) = {
            let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
            let id = harmony_identity::Identity::from_ed25519_verifying(&key.verifying_key())
                .expect("valid identity");
            let mut bundle = [0u8; 64];
            bundle[..32].copy_from_slice(&[0x11; 32]);
            bundle[32..].copy_from_slice(&key.verifying_key().to_bytes());
            (key, bundle)
        };
        let _ = admin_signing_key; // referenced for completeness; not signing here
        let _ = admin_addr;

        let entry = LibraryDirectoryEntry {
            community_id,
            community_admin_identity_pub: admin_identity_pub,
            name: "Phase 3 test".to_string(),
            description: "Round-trip test for wrapped entry".to_string(),
            topics: vec!["test".to_string()],
            invite_url: "harmony://invite/?p=AAAA".to_string(),
            listed_by: OwnerAddr([0xAA; 16]),
            listed_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "test".to_string(),
            },
            community_signature: [0u8; 64],
            library_identity_pub: Some([0xBB; 64]),
            library_signature: Some([0xCC; 64]),
        };
        let bytes = canonical_cbor_encode(&entry).expect("encode");
        let decoded: LibraryDirectoryEntry =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(entry, decoded, "wrapped entry round-trips");
        assert_eq!(decoded.library_identity_pub, Some([0xBB; 64]));
        assert_eq!(decoded.library_signature, Some([0xCC; 64]));
    }

    /// ZEB-280 Phase 3: a Phase 1-style entry (both Optional fields
    /// `None`) round-trips and the decoded fields are still `None`
    /// (the `#[serde(default)]` attribute lets ciborium decode missing
    /// fields as `None`).
    #[test]
    fn phase1_unwrapped_entry_roundtrips_with_optional_fields_absent() {
        let entry = LibraryDirectoryEntry {
            community_id: SpaceId([0x22; 16]),
            community_admin_identity_pub: [0x33; 64],
            name: "Phase 1 test".to_string(),
            description: "Backward compat check".to_string(),
            topics: vec![],
            invite_url: "harmony://invite/?p=AAAA".to_string(),
            listed_by: OwnerAddr([0x44; 16]),
            listed_at: Hlc {
                wall_ms: 0,
                logical: 0,
                device_id: String::new(),
            },
            community_signature: [0x55; 64],
            library_identity_pub: None,
            library_signature: None,
        };
        let bytes = canonical_cbor_encode(&entry).expect("encode");
        let decoded: LibraryDirectoryEntry =
            ciborium::de::from_reader(&bytes[..]).expect("decode");
        assert_eq!(entry, decoded, "Phase 1-shaped entry round-trips");
        assert_eq!(decoded.library_identity_pub, None);
        assert_eq!(decoded.library_signature, None);
    }
```

- [ ] **Step 9: Run the new + existing Phase 1 pinning tests; verify byte-identity**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(library_directory_entry_canonical_cbor_pinned) | test(phase3_wrapped_entry_roundtrips_via_canonical_cbor) | test(phase1_unwrapped_entry_roundtrips_with_optional_fields_absent) | test(library_directory_entry_field_keys_are_2char)' 2>&1 | tail -20
```

Expected: 4 tests passed.

**CRITICAL:** `library_directory_entry_canonical_cbor_pinned` must remain PASSING — the existing pinned hex string `a96263645011111111...` (starts with `a9` = map(9)) is the wire-format byte-identity assertion. If `skip_serializing_if` isn't omitting the new keys correctly, this test will fail with `a9` → `ab` (map(11)) and the pinned hex won't match.

If this test fails, STOP and surface — the `#[serde(default, skip_serializing_if = "Option::is_none")]` invariant is broken and the rest of Task 1 needs revisiting.

The `library_directory_entry_field_keys_are_2char` test continues to assert the expected key set is `["cd", "ai", "nm", "ds", "tp", "iu", "lb", "la", "cs"]` (9 keys, NOT including `li` or `ls` when both Optional fields are `None`).

- [ ] **Step 10: Run cargo fmt + clippy + check**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -5
```

Expected: all exit 0, no warnings.

- [ ] **Step 11: Run the full nextest suite to confirm Phase 1 invariants intact**

Run:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
```

Expected: same pass count as Task 0 baseline + 2 new tests passing (from Step 8). If any existing test fails, the Phase 1 wire-format invariant is broken — investigate before continuing.

- [ ] **Step 12: Commit Task 1**

```bash
git add src-tauri/src/owner_state_types.rs src-tauri/src/library_directory.rs src-tauri/tests/common/library_fixtures.rs
git status
```

Verify only these files are staged (no stray edits).

Commit:
```bash
git commit -m "$(cat <<'EOF'
feat(zeb-280): Phase 3 wire format — Optional li/ls fields + AttestationStatus

Adds the structural Phase 3 surface to library_directory.rs without
touching verify_entry's body (Task 2 handles that). Phase 1 wire-format
pinning fixtures remain byte-identical thanks to skip_serializing_if.

- owner_state_types.rs: serialize_optional_bytes_as_bstr +
  deserialize_optional_bytes_from_bstr helpers (Option<[u8; N]>
  variants of the existing bstr serde helpers)
- library_directory.rs:
  - LibraryDirectoryEntry: Optional library_identity_pub (rename "li") +
    library_signature (rename "ls") fields with skip_serializing_if so
    Phase 1 wire bytes are byte-identical
  - AttestationStatus enum: Unwrapped | Attested(addr) | Unattested(addr)
  - EntryVerifyError: LibrarySignatureFieldsInconsistent +
    InvalidLibraryIdentityPub variants
- tests/common/library_fixtures.rs: mock_directory_entry constructor
  threads the two new fields as None for Phase 1 compat
- 2 new in-module tests for round-trip of wrapped + unwrapped shapes

Phase 1 pinning test `library_directory_entry_canonical_cbor_pinned`
still passes byte-identically — confirms skip_serializing_if invariant.
EOF
)"
```

Run `git status` to confirm clean tree post-commit.

---

## Task 2: Extend `verify_entry` to return `Result<AttestationStatus, EntryVerifyError>`

**Goal:** Replace `verify_entry`'s `Result<(), EntryVerifyError>` return with `Result<AttestationStatus, EntryVerifyError>`. Add the wrapping-sig verification logic per spec §5. Update call sites that currently consume `Result<(), ...>`. Admin sig stays gatekeeper (verified first; entry dropped on failure). Tampered wrapping is `Ok(Unattested(addr))`, not `Err`.

**Files:**
- Modify: `src-tauri/src/library_directory.rs` (verify_entry body + call sites)

- [ ] **Step 1: Update `verify_entry` signature + add wrapping-sig logic**

Locate `verify_entry` (around lines 219-288). Change its signature from `Result<(), EntryVerifyError>` to `Result<AttestationStatus, EntryVerifyError>` AND extend the body with a new match block at the end (immediately before the final `Ok(())`).

The new body:

```rust
/// Verify a `LibraryDirectoryEntry` end-to-end and return the
/// wrapping-signature attestation outcome. Spec §5.
///
/// **Phase 1 invariants (unchanged):**
/// 1. Anti-spam bounds (name/description/topic lengths)
/// 2. Parse `community_admin_identity_pub` via
///    `harmony_identity::Identity::from_public_bytes`
/// 3. Verify the Ed25519 admin signature over canonical-CBOR-encoded
///    fields with `community_signature` zeroed (so verify == sign
///    exactly). The Optional `library_identity_pub` / `library_signature`
///    are also zeroed (via `None` + `skip_serializing_if`), so admin
///    sig bytes are portable across libraries.
/// 4. Parse `invite_url` and reject if `is_invite_only == true`
/// 5. Invite payload binding (community_id + admin_addr)
///
/// **Phase 3 addition:** if `library_signature` and
/// `library_identity_pub` are both `Some`, verify the wrapping sig
/// over canonical-CBOR-encoded fields with only `library_signature`
/// zeroed (keep `library_identity_pub` + `community_signature`
/// populated, so the wrapping sig commits to the admin-signed bundle).
///
/// **Returns:**
/// - `Ok(AttestationStatus::Unwrapped)` — Phase 1-style entry (both
///   Optional fields None)
/// - `Ok(AttestationStatus::Attested(library_addr))` — wrapping sig
///   verified, library_addr is derived from `library_identity_pub`
/// - `Ok(AttestationStatus::Unattested(library_addr))` — wrapping sig
///   present but invalid; entry NOT dropped (admin sig was valid).
///   library_addr is the CLAIMED broadcasting library.
/// - `Err(LibrarySignatureFieldsInconsistent)` — exactly one of
///   library_signature / library_identity_pub is Some (malformed).
/// - `Err(InvalidLibraryIdentityPub)` — library_identity_pub bytes
///   failed `Identity::from_public_bytes`.
/// - Other `Err(...)` — admin sig path failed; entry should be dropped.
pub fn verify_entry(
    entry: &LibraryDirectoryEntry,
) -> Result<AttestationStatus, EntryVerifyError> {
    // (1) Bounds — unchanged from Phase 1.
    if entry.name.len() > MAX_NAME_LEN {
        return Err(EntryVerifyError::NameTooLong);
    }
    if entry.description.len() > MAX_DESCRIPTION_LEN {
        return Err(EntryVerifyError::DescriptionTooLong);
    }
    if entry.topics.len() > MAX_TOPICS_PER_ENTRY {
        return Err(EntryVerifyError::TooManyTopics);
    }
    if entry.topics.iter().any(|t| t.len() > MAX_TOPIC_LEN) {
        return Err(EntryVerifyError::TopicTooLong);
    }

    // (2) Parse identity_pub.
    let identity =
        harmony_identity::Identity::from_public_bytes(&entry.community_admin_identity_pub)
            .map_err(|e| EntryVerifyError::InvalidIdentityPub(format!("{e:?}")))?;

    // (3) Verify admin sig over canonical CBOR with cs zeroed and
    //     li/ls forced to None. The Phase 1 invariant of "sig field
    //     zeroed during sign/verify" extends to "Optional fields
    //     forced absent" — skip_serializing_if omits them from CBOR.
    let mut for_sig = entry.clone();
    for_sig.community_signature = [0u8; 64];
    for_sig.library_identity_pub = None;
    for_sig.library_signature = None;
    let signed_bytes = canonical_cbor_encode(&for_sig)?;
    let sig = Signature::from_bytes(&entry.community_signature);
    identity
        .verifying_key
        .verify_strict(&signed_bytes, &sig)
        .map_err(|_| EntryVerifyError::SignatureInvalid)?;

    // (4) Invite-URL discipline — open-community only.
    let payload = crate::community_invite::decode_invite_url(&entry.invite_url)
        .map_err(|e| EntryVerifyError::InviteUrlParse(format!("{e}")))?;
    if payload.is_invite_only {
        return Err(EntryVerifyError::InviteOnlyUrl);
    }

    // (5) Invite-payload consistency with the signed directory entry.
    if payload.community_id != entry.community_id {
        return Err(EntryVerifyError::PayloadCommunityIdMismatch {
            entry: entry.community_id,
            payload: payload.community_id,
        });
    }
    let entry_admin_addr = OwnerAddr(identity.address_hash);
    if payload.admin_addr != entry_admin_addr {
        return Err(EntryVerifyError::PayloadAdminIdentityMismatch {
            entry_addr: entry_admin_addr,
            payload_addr: payload.admin_addr,
        });
    }

    // (6) Sub-D Phase 3 — wrapping signature check.
    match (&entry.library_signature, &entry.library_identity_pub) {
        (None, None) => Ok(AttestationStatus::Unwrapped),
        (Some(_), None) | (None, Some(_)) => {
            Err(EntryVerifyError::LibrarySignatureFieldsInconsistent)
        }
        (Some(lib_sig), Some(lib_pub)) => {
            let lib_identity = harmony_identity::Identity::from_public_bytes(lib_pub)
                .map_err(|e| EntryVerifyError::InvalidLibraryIdentityPub(format!("{e:?}")))?;
            let lib_addr = OwnerAddr(lib_identity.address_hash);

            // Reconstruct sign-time bytes: zero `library_signature`
            // only (keep `library_identity_pub` + `community_signature`
            // populated so the wrapping sig commits to the admin sig).
            let mut for_sig = entry.clone();
            for_sig.library_signature = None;
            let signed_bytes = canonical_cbor_encode(&for_sig)?;
            let sig = Signature::from_bytes(lib_sig);

            match lib_identity.verifying_key.verify_strict(&signed_bytes, &sig) {
                Ok(()) => Ok(AttestationStatus::Attested(lib_addr)),
                Err(_) => Ok(AttestationStatus::Unattested(lib_addr)),
            }
        }
    }
}
```

- [ ] **Step 2: Update existing in-module tests that call `verify_entry`**

Search for `verify_entry(` calls in `#[cfg(test)] mod tests` of `library_directory.rs`. The existing tests assert outcomes like:

```rust
assert!(matches!(verify_entry(&entry), Ok(())));
assert!(matches!(verify_entry(&entry), Err(EntryVerifyError::SignatureInvalid)));
```

Update happy-path assertions to expect `Ok(AttestationStatus::Unwrapped)`:

```rust
assert!(matches!(verify_entry(&entry), Ok(AttestationStatus::Unwrapped)));
```

Error-path assertions stay the same (still `Err(...)` patterns).

For each `verify_entry(&entry)` call, inspect the test name + body and pick:
- Happy path on Phase 1 entry → `Ok(AttestationStatus::Unwrapped)`
- Negative path (any kind of sig/payload/bounds error) → unchanged `Err(...)` pattern

- [ ] **Step 3: Run cargo check to surface call-site mismatches**

Run:
```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | grep -E '(error|warning)' | head -30
```

Expected: some errors about `Result<(), ...>` vs `Result<AttestationStatus, ...>` at call sites OUTSIDE the tests block. Fix them one at a time:

- `process_sample` (line ~804): change `verify_entry(&entry).map_err(...)?;` to capture the new status return and explicitly ignore it for Task 2 (Task 3 wires it into the aggregation):
  ```rust
  let status = verify_entry(&entry).map_err(ProcessSampleError::Verify)?;
  // Task 3 will pass `status` to `on_entry` + evolve the AttributionMismatch
  // check. For Task 2 we keep the change surgical — verify_entry's signature
  // shift is the whole delta. Aggregation behavior is unchanged.
  let _ = status;
  let mut agg = self.aggregation.lock().await;
  Ok(agg.on_entry(entry))
  ```

The `let _ = status;` line documents intent without affecting runtime behavior. No leftover sentinel strings in the source after Task 3 replaces it.

- [ ] **Step 4: Add unit tests for the new verify_entry outcomes**

Insert into the `#[cfg(test)] mod tests` block in `library_directory.rs`. These build on the existing test helpers (`build_test_admin_identity`, `build_open_invite_url_for`, etc. — search the file for these to find the section).

Add new helper inline (after `build_open_invite_url_for` or similar):

```rust
    /// ZEB-280 Phase 3: build a library signer + 64-byte identity bundle.
    fn build_test_library_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key().to_bytes();
        let mut bundle = [0u8; 64];
        bundle[..32].copy_from_slice(&[0x22; 32]);
        bundle[32..].copy_from_slice(&verifying);
        (signing, bundle)
    }

    /// ZEB-280 Phase 3: take an admin-signed entry, sign it with
    /// `library_signing_key` over canonical CBOR with `library_signature`
    /// zeroed (mirror of the verifier's reconstruction).
    fn wrap_entry(
        mut entry: LibraryDirectoryEntry,
        library_signing_key: &SigningKey,
        library_identity_bundle: [u8; 64],
    ) -> LibraryDirectoryEntry {
        entry.library_identity_pub = Some(library_identity_bundle);
        entry.library_signature = None;
        let signed_bytes = canonical_cbor_encode(&entry).expect("encode for lib sign");
        entry.library_signature = Some(library_signing_key.sign(&signed_bytes).to_bytes());
        entry
    }
```

Then add the 7 new unit tests (using the existing test helpers in the file). Find the helper `build_signed_open_entry` (or equivalent — search for `fn build_signed_*entry` in the tests module) and reuse it for happy-path admin signing:

```rust
    /// ZEB-280 Phase 3: Phase 1-style entry (Optional fields both None)
    /// verifies as `AttestationStatus::Unwrapped`.
    #[test]
    fn verify_entry_phase1_unwrapped_returns_unwrapped() {
        let community_id = SpaceId([0x11; 16]);
        let admin_seed = [7u8; 32];
        let entry = build_signed_open_entry_for(community_id, admin_seed);
        // (build_signed_open_entry_for returns an entry with li=None, ls=None)

        match verify_entry(&entry) {
            Ok(AttestationStatus::Unwrapped) => {}
            other => panic!("expected Unwrapped, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: a wrapped entry with a valid library signature
    /// verifies as `AttestationStatus::Attested(library_addr)`.
    #[test]
    fn verify_entry_phase3_wrapped_valid_returns_attested() {
        let community_id = SpaceId([0x22; 16]);
        let admin_seed = [8u8; 32];
        let admin_entry = build_signed_open_entry_for(community_id, admin_seed);

        let (lib_signing, lib_bundle) = build_test_library_identity([9u8; 32]);
        let wrapped = wrap_entry(admin_entry, &lib_signing, lib_bundle);

        let expected_lib_addr = {
            let id = harmony_identity::Identity::from_public_bytes(&lib_bundle)
                .expect("library identity");
            OwnerAddr(id.address_hash)
        };

        match verify_entry(&wrapped) {
            Ok(AttestationStatus::Attested(addr)) => {
                assert_eq!(addr, expected_lib_addr);
            }
            other => panic!("expected Attested, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: a wrapped entry with a TAMPERED library
    /// signature returns `Ok(AttestationStatus::Unattested(library_addr))`.
    /// The entry is NOT dropped — admin sig still valid.
    #[test]
    fn verify_entry_phase3_tampered_wrapping_sig_returns_unattested() {
        let community_id = SpaceId([0x33; 16]);
        let admin_seed = [10u8; 32];
        let admin_entry = build_signed_open_entry_for(community_id, admin_seed);

        let (lib_signing, lib_bundle) = build_test_library_identity([11u8; 32]);
        let mut wrapped = wrap_entry(admin_entry, &lib_signing, lib_bundle);

        // Tamper the library signature.
        let mut bad_sig = wrapped.library_signature.expect("wrapping sig present");
        bad_sig[0] ^= 0xFF;
        wrapped.library_signature = Some(bad_sig);

        let expected_lib_addr = {
            let id = harmony_identity::Identity::from_public_bytes(&lib_bundle)
                .expect("library identity");
            OwnerAddr(id.address_hash)
        };

        match verify_entry(&wrapped) {
            Ok(AttestationStatus::Unattested(addr)) => {
                assert_eq!(
                    addr, expected_lib_addr,
                    "Unattested still carries the CLAIMED library addr"
                );
            }
            other => panic!("expected Unattested, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: if the entry's payload is tampered (e.g., name
    /// field changed), the ADMIN sig fails FIRST, and the entry is
    /// dropped via `Err(SignatureInvalid)`. The wrapping sig is not
    /// even reached — admin sig is the gatekeeper.
    #[test]
    fn verify_entry_phase3_tampered_payload_invalidates_both_sigs() {
        let community_id = SpaceId([0x44; 16]);
        let admin_seed = [12u8; 32];
        let admin_entry = build_signed_open_entry_for(community_id, admin_seed);
        let (lib_signing, lib_bundle) = build_test_library_identity([13u8; 32]);
        let mut wrapped = wrap_entry(admin_entry, &lib_signing, lib_bundle);

        // Tamper the payload (name) AFTER both sigs were applied.
        wrapped.name = "TAMPERED".to_string();

        match verify_entry(&wrapped) {
            Err(EntryVerifyError::SignatureInvalid) => {}
            other => panic!("expected admin SignatureInvalid, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: an entry with `library_signature = Some` but
    /// `library_identity_pub = None` returns
    /// `Err(LibrarySignatureFieldsInconsistent)`.
    #[test]
    fn verify_entry_inconsistent_library_fields_rejected_lib_sig_only() {
        let community_id = SpaceId([0x55; 16]);
        let admin_seed = [14u8; 32];
        let mut entry = build_signed_open_entry_for(community_id, admin_seed);
        entry.library_signature = Some([0xAA; 64]);
        entry.library_identity_pub = None;

        // Admin sig is over (cs=0, li=None, ls=None) — but the entry
        // now has li=None, ls=Some. The admin sig verifier reconstructs
        // by setting cs=0, li=None, ls=None — so admin sig still
        // verifies (unchanged). Then we hit the inconsistency check.
        match verify_entry(&entry) {
            Err(EntryVerifyError::LibrarySignatureFieldsInconsistent) => {}
            other => panic!("expected LibrarySignatureFieldsInconsistent, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: an entry with `library_identity_pub = Some` but
    /// `library_signature = None` returns
    /// `Err(LibrarySignatureFieldsInconsistent)`.
    #[test]
    fn verify_entry_inconsistent_library_fields_rejected_lib_pub_only() {
        let community_id = SpaceId([0x66; 16]);
        let admin_seed = [15u8; 32];
        let mut entry = build_signed_open_entry_for(community_id, admin_seed);
        entry.library_identity_pub = Some([0xBB; 64]);
        entry.library_signature = None;

        match verify_entry(&entry) {
            Err(EntryVerifyError::LibrarySignatureFieldsInconsistent) => {}
            other => panic!("expected LibrarySignatureFieldsInconsistent, got {other:?}"),
        }
    }

    /// ZEB-280 Phase 3: an entry with a malformed `library_identity_pub`
    /// (bytes that fail `Identity::from_public_bytes`) returns
    /// `Err(InvalidLibraryIdentityPub)`.
    #[test]
    fn verify_entry_malformed_library_identity_pub_rejected() {
        let community_id = SpaceId([0x77; 16]);
        let admin_seed = [16u8; 32];
        let admin_entry = build_signed_open_entry_for(community_id, admin_seed);
        let (lib_signing, lib_bundle) = build_test_library_identity([17u8; 32]);

        // Wrap with the GOOD bundle (so the wrapping sig is valid for
        // this bundle), then SWAP IN a malformed bundle. The wrapping
        // sig won't verify against the malformed pub, but we never
        // get that far — the Identity::from_public_bytes check fires
        // first.
        let mut wrapped = wrap_entry(admin_entry, &lib_signing, lib_bundle);
        // 64 bytes of all-zero — Ed25519 zero pubkey is invalid.
        wrapped.library_identity_pub = Some([0u8; 64]);

        match verify_entry(&wrapped) {
            Err(EntryVerifyError::InvalidLibraryIdentityPub(_)) => {}
            other => panic!("expected InvalidLibraryIdentityPub, got {other:?}"),
        }
    }
```

**Important note on the test:** the `build_signed_open_entry_for(community_id, admin_seed)` helper may need to be created if it doesn't exist already. Search the file for `fn build_signed_open_entry` or similar. If only `build_signed_open_entry_default` exists (taking no args), add a wrapper:

```rust
    /// ZEB-280 Phase 3: variant of `build_signed_open_entry_default`
    /// that lets the caller bind a specific community_id and
    /// admin_seed. The invite URL is bound to the community_id and
    /// admin_addr derived from the seed so verify_entry's R2 F1
    /// payload-consistency check passes.
    fn build_signed_open_entry_for(
        community_id: SpaceId,
        admin_seed: [u8; 32],
    ) -> LibraryDirectoryEntry {
        let (signing_key, identity_pub) = build_test_admin_identity(admin_seed);
        let admin_addr = OwnerAddr(
            harmony_identity::Identity::from_public_bytes(&identity_pub)
                .expect("identity")
                .address_hash,
        );
        let invite_url = build_open_invite_url_for(community_id, admin_addr);
        let mut entry = LibraryDirectoryEntry {
            community_id,
            community_admin_identity_pub: identity_pub,
            name: "Test".to_string(),
            description: "Test desc".to_string(),
            topics: vec![],
            invite_url,
            listed_by: OwnerAddr([0xAA; 16]),
            listed_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "test".to_string(),
            },
            community_signature: [0u8; 64],
            library_identity_pub: None,
            library_signature: None,
        };
        let mut for_sig = entry.clone();
        for_sig.community_signature = [0u8; 64];
        let signed_bytes = canonical_cbor_encode(&for_sig).expect("encode");
        entry.community_signature = signing_key.sign(&signed_bytes).to_bytes();
        entry
    }

    fn build_test_admin_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key().to_bytes();
        let mut identity_pub = [0u8; 64];
        identity_pub[..32].copy_from_slice(&[0x11; 32]);
        identity_pub[32..].copy_from_slice(&verifying);
        (signing, identity_pub)
    }
```

If these helpers already exist (likely from Phase 1), DON'T re-add them — use the existing ones.

- [ ] **Step 5: Run the new + existing verify_entry tests**

Run:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(verify_entry)' 2>&1 | tail -20
```

Expected: 7 new tests passing + existing verify_entry tests still passing. If any existing test fails, inspect — most likely a call-site signature mismatch.

- [ ] **Step 6: Run cargo fmt + clippy**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: both exit 0.

- [ ] **Step 7: Run full nextest suite**

Run:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
```

Expected: same baseline + 9 new tests (Task 1's 2 + Task 2's 7). All passing.

- [ ] **Step 8: Commit Task 2**

```bash
git add src-tauri/src/library_directory.rs
git status
```

Verify only `library_directory.rs` changed.

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-280): verify_entry returns AttestationStatus

Extends verify_entry to verify the Phase 3 wrapping signature when
both library_identity_pub and library_signature are Some. Admin sig
remains the gatekeeper — entries with invalid admin sigs are still
dropped. Tampered wrapping sig surfaces as Ok(Unattested(addr)) so
the aggregation can flag the row in the UI without dropping it.

Return type changes from Result<(), EntryVerifyError> to
Result<AttestationStatus, EntryVerifyError>:
- Ok(Unwrapped) — Phase 1-style entry (both Optional fields None)
- Ok(Attested(addr)) — wrapping sig valid; addr derived from
  library_identity_pub
- Ok(Unattested(addr)) — wrapping sig present but invalid; entry
  still surfaced
- Err(LibrarySignatureFieldsInconsistent) — exactly one of
  (library_signature, library_identity_pub) is Some
- Err(InvalidLibraryIdentityPub) — library_identity_pub fails
  Identity::from_public_bytes

Wrapping sig bytes: canonical CBOR encode of entry with
library_signature=None (keep library_identity_pub + community_signature
populated). This commits the wrapping sig to the admin sig.

7 new unit tests cover all return paths. Existing tests updated to
expect Ok(Unwrapped) on Phase 1 happy paths.

process_sample threads the status through with `let _ = status;` for
now — Task 3 wires it into the aggregation + AttributionMismatch
check.
EOF
)"
```

---

## Task 3: Aggregation evolution + DTO update + AttributionMismatch evolution

**Goal:** Evolve `AggregatedEntry` to track `attested_by` + `unattested_by` sets (replacing Phase 1's single `listed_by` set). Update `Aggregation::on_entry` to accept `AttestationStatus`. Evolve the per-library cap and `drop_library` to use the broadcasting-library identity (from `AttestationStatus`) instead of the entry's signed `listed_by` field. Extend `DirectoryEntryDTO` with `unattested: bool`. Evolve `process_sample`'s `AttributionMismatch` check to require the broadcasting library equals the topic owner.

**Files:**
- Modify: `src-tauri/src/library_directory.rs` (Aggregation, AggregatedEntry, DirectoryEntryDTO, process_sample)
- Test: existing tests in `library_directory.rs::tests` + `tests/library_directory_integration.rs` — both need surgical updates for renamed/restructured fields.

- [ ] **Step 1: Rename `AggregatedEntry.listed_by` field to `attested_by` and add `unattested_by`**

Locate `AggregatedEntry` struct (around line 330):

```rust
#[derive(Debug, Clone)]
pub struct AggregatedEntry {
    pub entry: LibraryDirectoryEntry,
    pub listed_by: BTreeSet<OwnerAddr>,
}
```

Replace with:

```rust
#[derive(Debug, Clone)]
pub struct AggregatedEntry {
    /// Latest (highest-HLC) entry observed for this community.
    pub entry: LibraryDirectoryEntry,

    /// Sub-D Phase 3 (ZEB-280): libraries whose broadcast of this
    /// community we trust. Populated by:
    /// - `AttestationStatus::Attested(lib_addr)` → insert(lib_addr)
    /// - `AttestationStatus::Unwrapped` → insert(entry.listed_by)
    ///   (Phase 1 backward compat — implicit trust from subscription
    ///   topic)
    ///
    /// Replaces the Phase 1 `listed_by: BTreeSet<OwnerAddr>` field
    /// semantics. Eviction triggers when this set empties.
    pub attested_by: BTreeSet<OwnerAddr>,

    /// Sub-D Phase 3 (ZEB-280): libraries we received this entry from
    /// whose wrapping sig failed to verify. Drives the "unattested"
    /// UI badge (entry shown but flagged):
    ///   unattested = !unattested_by.is_empty()
    ///
    /// Tracks the broadcasting library's CLAIMED address (derived from
    /// the signed `library_identity_pub` — we know who claimed to
    /// broadcast even when their sig was bad).
    pub unattested_by: BTreeSet<OwnerAddr>,
}
```

- [ ] **Step 2: Update `Aggregation::on_entry` signature to take `AttestationStatus`**

Locate `Aggregation::on_entry` (around line 409). Change its signature from `pub fn on_entry(&mut self, entry: LibraryDirectoryEntry) -> ProcessResult` to:

```rust
    /// Process a verified entry. Caller MUST have run `verify_entry`
    /// first — this method does NOT re-verify the signature.
    ///
    /// `status` is the AttestationStatus returned by `verify_entry`,
    /// which determines whether the broadcasting library is the
    /// admin-signed `entry.listed_by` (Unwrapped) or the wrapping-sig
    /// derived `OwnerAddr` (Attested / Unattested). The
    /// broadcasting library drives:
    ///   - per-library cap accounting (Phase 1 used entry.listed_by;
    ///     Phase 3 uses the broadcasting library, which can differ
    ///     when library A republishes library B's entry verbatim)
    ///   - eviction by `drop_library` (must sweep both attested_by
    ///     and unattested_by)
    pub fn on_entry(
        &mut self,
        entry: LibraryDirectoryEntry,
        status: AttestationStatus,
    ) -> ProcessResult {
```

Inside the body, replace the existing `let library = entry.listed_by;` line with:

```rust
        let community_id = entry.community_id;
        // Sub-D Phase 3: the broadcasting library identity comes from
        // AttestationStatus. For Phase 1-shaped entries (Unwrapped),
        // it falls back to the admin-signed `listed_by` (which Phase 1
        // attribution-checking already constrained to equal the topic
        // owner).
        let library = match status {
            AttestationStatus::Attested(addr) | AttestationStatus::Unattested(addr) => addr,
            AttestationStatus::Unwrapped => entry.listed_by,
        };
```

Then replace the per-library cap block (the existing `let library_at_cap = ...` and following ~30 lines) to use the new field name `attested_by` instead of `listed_by` in the dedupe check:

```rust
        // Cap check BEFORE insert. If this library is already at cap
        // and we're about to add a NEW contribution (not an update),
        // evict the oldest entry from this library first.
        //
        // Phase 3: "is_new_contribution" checks BOTH attested_by AND
        // unattested_by sets — a library that previously broadcast
        // this community with a bad wrapping sig (unattested_by) is
        // still considered a contributor for cap purposes (counts
        // toward MAX_ENTRIES_PER_LIBRARY).
        let library_at_cap =
            self.entry_count_for_library(&library) >= MAX_ENTRIES_PER_LIBRARY;
        let is_new_contribution_for_library = !self
            .by_community
            .get(&community_id)
            .map(|agg| {
                agg.attested_by.contains(&library) || agg.unattested_by.contains(&library)
            })
            .unwrap_or(false);

        let mut evicted: Option<SpaceId> = None;
        if library_at_cap && is_new_contribution_for_library {
            if let Some(oldest_id) = self.find_oldest_for_library(&library) {
                self.evict_library_contribution(&library, oldest_id);
                evicted = Some(oldest_id);
            } else {
                tracing::warn!(
                    target: "library_directory",
                    library = ?library,
                    per_library_count = self.entry_count_for_library(&library),
                    max = MAX_ENTRIES_PER_LIBRARY,
                    "per_library_count says at-cap but find_oldest_for_library returned None — counter invariant violated",
                );
                debug_assert!(
                    false,
                    "per_library_count invariant violated: library {:?} count={} but no community lists it",
                    library,
                    self.entry_count_for_library(&library)
                );
            }
        }
```

Then replace the main `match self.by_community.get_mut(&community_id) { None => {...} Some(existing) => {...} }` block to handle two-set insertion based on `status`:

```rust
        let outcome = match self.by_community.get_mut(&community_id) {
            None => {
                // Brand-new community in the aggregation.
                let (attested_by, unattested_by) = match status {
                    AttestationStatus::Attested(lib_addr) => {
                        let mut set = BTreeSet::new();
                        set.insert(lib_addr);
                        (set, BTreeSet::new())
                    }
                    AttestationStatus::Unwrapped => {
                        let mut set = BTreeSet::new();
                        set.insert(entry.listed_by);
                        (set, BTreeSet::new())
                    }
                    AttestationStatus::Unattested(lib_addr) => {
                        let mut set = BTreeSet::new();
                        set.insert(lib_addr);
                        (BTreeSet::new(), set)
                    }
                };
                self.by_community.insert(
                    community_id,
                    AggregatedEntry {
                        entry,
                        attested_by,
                        unattested_by,
                    },
                );
                *self.per_library_count.entry(library).or_insert(0) += 1;
                OnEntryOutcome::Inserted(community_id)
            }
            Some(existing) => {
                let incoming_newer = entry
                    .listed_at
                    .is_strictly_newer_than(&existing.entry.listed_at);
                // Phase 3: insert into the correct set per AttestationStatus.
                let was_new_contribution = match status {
                    AttestationStatus::Attested(lib_addr) => {
                        existing.attested_by.insert(lib_addr)
                    }
                    AttestationStatus::Unwrapped => {
                        existing.attested_by.insert(entry.listed_by)
                    }
                    AttestationStatus::Unattested(lib_addr) => {
                        existing.unattested_by.insert(lib_addr)
                    }
                };
                if was_new_contribution {
                    *self.per_library_count.entry(library).or_insert(0) += 1;
                }
                if incoming_newer {
                    existing.entry = entry;
                    OnEntryOutcome::Replaced(community_id)
                } else if was_new_contribution {
                    OnEntryOutcome::AccretedListedBy(community_id)
                } else {
                    OnEntryOutcome::Idempotent
                }
            }
        };

        ProcessResult { outcome, evicted }
    }
```

(`OnEntryOutcome::AccretedListedBy` is the legacy variant name — keep it; renaming the enum variant is out of scope and would touch unrelated call sites.)

- [ ] **Step 3: Update `find_oldest_for_library` to scan both sets**

Locate `find_oldest_for_library` (around line 550). Change the filter to check membership in EITHER set:

```rust
    fn find_oldest_for_library(&self, library: &OwnerAddr) -> Option<SpaceId> {
        self.by_community
            .iter()
            .filter(|(_, agg)| {
                agg.attested_by.contains(library) || agg.unattested_by.contains(library)
            })
            .min_by(|a, b| {
                let ha = (
                    &a.1.entry.listed_at.wall_ms,
                    &a.1.entry.listed_at.logical,
                    a.1.entry.listed_at.device_id.as_str(),
                );
                let hb = (
                    &b.1.entry.listed_at.wall_ms,
                    &b.1.entry.listed_at.logical,
                    b.1.entry.listed_at.device_id.as_str(),
                );
                ha.cmp(&hb)
            })
            .map(|(id, _)| *id)
    }
```

- [ ] **Step 4: Update `evict_library_contribution` to handle both sets**

Locate `evict_library_contribution` (around line 581). The "source matches" rule (R1 F3) still applies: if the stored entry was sourced from `library`, evict the community entirely. Update the listed_by removal to remove from BOTH sets and capture "remaining" as the union of both:

```rust
    fn evict_library_contribution(&mut self, library: &OwnerAddr, community_id: SpaceId) {
        // Capture state in a scoped borrow, then apply mutations after.
        let mut surviving_attested: Option<BTreeSet<OwnerAddr>> = None;
        let mut surviving_unattested: Option<BTreeSet<OwnerAddr>> = None;
        if let Some(agg) = self.by_community.get_mut(&community_id) {
            let removed_from_attested = agg.attested_by.remove(library);
            let removed_from_unattested = agg.unattested_by.remove(library);
            if removed_from_attested || removed_from_unattested {
                if let Some(c) = self.per_library_count.get_mut(library) {
                    if *c > 0 {
                        *c -= 1;
                    }
                }
                // Phase 3 generalization of the Phase 1 "source matches"
                // rule: if the stored entry was sourced from this
                // library AND the broadcasting library identity for
                // that stored entry's last-write was this library,
                // evict the community entirely. We can't perfectly know
                // who broadcast the LATEST entry update without tracking
                // a separate `last_broadcast_by` field — but the entry's
                // signed `listed_by` is the closest proxy from the wire
                // (admin attested who's hosting). For the same Phase 1
                // R1 F3 rationale, we evict if either set is empty OR
                // if the stored entry's listed_by equals this library.
                let source_was_this_library = &agg.entry.listed_by == library;
                let both_sets_empty =
                    agg.attested_by.is_empty() && agg.unattested_by.is_empty();
                if source_was_this_library || both_sets_empty {
                    surviving_attested = Some(agg.attested_by.clone());
                    surviving_unattested = Some(agg.unattested_by.clone());
                }
            }
        }
        if let (Some(remaining_attested), Some(remaining_unattested)) =
            (surviving_attested, surviving_unattested)
        {
            self.by_community.remove(&community_id);
            // R2 F2: roll back per_library_count for OTHER libraries
            // whose contributions are also being dropped by this
            // eviction (both attested and unattested branches).
            for other in remaining_attested.into_iter().chain(remaining_unattested) {
                if &other != library {
                    if let Some(c) = self.per_library_count.get_mut(&other) {
                        if *c > 0 {
                            *c -= 1;
                        }
                    }
                }
            }
        }
    }
```

- [ ] **Step 5: Update `drop_library` to walk both sets**

Locate `drop_library` (around line 507). Apply the same dual-set logic:

```rust
    pub fn drop_library(&mut self, library: &OwnerAddr) -> Vec<SpaceId> {
        // Two-pass to satisfy the borrow checker (R2 F2 patterns
        // unchanged from Phase 1 — just generalized to both sets).
        let mut to_evict: Vec<(SpaceId, BTreeSet<OwnerAddr>, BTreeSet<OwnerAddr>)> = Vec::new();
        for (community_id, agg) in self.by_community.iter_mut() {
            let source_was_this_library = &agg.entry.listed_by == library;
            let _ = agg.attested_by.remove(library);
            let _ = agg.unattested_by.remove(library);
            let both_sets_empty =
                agg.attested_by.is_empty() && agg.unattested_by.is_empty();
            if source_was_this_library || both_sets_empty {
                to_evict.push((
                    *community_id,
                    agg.attested_by.clone(),
                    agg.unattested_by.clone(),
                ));
            }
        }
        let mut evicted_ids = Vec::with_capacity(to_evict.len());
        for (id, remaining_attested, remaining_unattested) in to_evict {
            self.by_community.remove(&id);
            for other in remaining_attested.into_iter().chain(remaining_unattested) {
                if &other != library {
                    if let Some(c) = self.per_library_count.get_mut(&other) {
                        if *c > 0 {
                            *c -= 1;
                        }
                    }
                }
            }
            evicted_ids.push(id);
        }
        self.per_library_count.remove(library);
        evicted_ids
    }
```

- [ ] **Step 6: Update `snapshot_filtered_by_library`**

Locate the existing `snapshot_filtered_by_library` (around line 389). It filters on `e.listed_by.contains(library)`. Change to check both sets:

```rust
    pub fn snapshot_filtered_by_library(&self, library: &OwnerAddr) -> Vec<AggregatedEntry> {
        self.by_community
            .values()
            .filter(|e| {
                e.attested_by.contains(library) || e.unattested_by.contains(library)
            })
            .cloned()
            .collect()
    }
```

- [ ] **Step 7: Update `DirectoryEntryDTO`**

Locate `DirectoryEntryDTO` (around line 893):

```rust
#[derive(Debug, Clone, Serialize)]
pub struct DirectoryEntryDTO {
    pub community_id: String,
    pub community_addr: String,
    pub name: String,
    pub description: String,
    pub topics: Vec<String>,
    pub invite_url: String,
    pub listed_by_count: usize,
    pub listed_at: Hlc,
}
```

Add `unattested: bool` field (snake_case wire key, matches existing convention on this DTO):

```rust
#[derive(Debug, Clone, Serialize)]
pub struct DirectoryEntryDTO {
    pub community_id: String,
    pub community_addr: String,
    pub name: String,
    pub description: String,
    pub topics: Vec<String>,
    pub invite_url: String,
    /// Sub-D Phase 3 (ZEB-280): count of libraries with valid attestation
    /// for this entry (i.e., `attested_by.len()`). Includes Phase 1
    /// unwrapped contributions (which fall back to entry.listed_by).
    pub listed_by_count: usize,
    /// Sub-D Phase 3 (ZEB-280): true if at least one broadcasting
    /// library's wrapping sig failed to verify (`!unattested_by.is_empty()`).
    /// Drives the inline "unattested" badge in the frontend browser.
    pub unattested: bool,
    pub listed_at: Hlc,
}
```

Update `DirectoryEntryDTO::from_aggregated`:

```rust
impl DirectoryEntryDTO {
    pub fn from_aggregated(agg: &AggregatedEntry) -> Self {
        let addr_bytes =
            harmony_identity::Identity::from_public_bytes(&agg.entry.community_admin_identity_pub)
                .map(|id| id.address_hash)
                .unwrap_or_default();
        Self {
            community_id: hex::encode(agg.entry.community_id.0),
            community_addr: hex::encode(addr_bytes),
            name: agg.entry.name.clone(),
            description: agg.entry.description.clone(),
            topics: agg.entry.topics.clone(),
            invite_url: agg.entry.invite_url.clone(),
            listed_by_count: agg.attested_by.len(),
            unattested: !agg.unattested_by.is_empty(),
            listed_at: agg.entry.listed_at.clone(),
        }
    }
}
```

- [ ] **Step 8: Update `process_sample`'s `AttributionMismatch` check**

Locate `process_sample` (around line 791). Update to:

```rust
    pub async fn process_sample(
        &self,
        library_addr: OwnerAddr,
        bytes: Vec<u8>,
    ) -> Result<ProcessResult, ProcessSampleError> {
        let entry: LibraryDirectoryEntry =
            ciborium::de::from_reader(&bytes[..]).map_err(ProcessSampleError::Decode)?;
        // Phase 1 attribution check: signed `listed_by` is the topic
        // owner's address. Phase 3 generalizes this — for wrapped
        // entries, the broadcasting library identity (from the
        // wrapping sig's library_identity_pub) is what must match the
        // topic owner. Library A republishing library B's entry has
        // listed_by=B but library_identity_pub=A. We require
        // library_identity_pub's derived addr == library_addr (the
        // topic owner). Unwrapped entries fall through to the Phase 1
        // listed_by == library_addr semantics.
        let status = verify_entry(&entry).map_err(ProcessSampleError::Verify)?;
        let broadcasting_lib = match status {
            AttestationStatus::Attested(addr) | AttestationStatus::Unattested(addr) => addr,
            AttestationStatus::Unwrapped => entry.listed_by,
        };
        if broadcasting_lib != library_addr {
            return Err(ProcessSampleError::AttributionMismatch {
                expected: library_addr,
                actual: broadcasting_lib,
            });
        }
        let mut agg = self.aggregation.lock().await;
        Ok(agg.on_entry(entry, status))
    }
```

- [ ] **Step 9: Update existing in-module unit tests for renamed field**

The existing in-module tests in `library_directory.rs::tests` access `agg.listed_by`, `snap[0].listed_by.len()`, etc. These need renaming to `agg.attested_by`.

Search:
```bash
cd src-tauri && grep -n "\.listed_by\." src/library_directory.rs | head -20
```

Locate each call site. Most are in the `tests` mod. Replace `.listed_by.` → `.attested_by.` for AggregatedEntry-context accesses. **Do NOT change `entry.listed_by` accesses** (that's the OwnerAddr field on LibraryDirectoryEntry, unchanged).

Specifically:
- Line 1376: `assert_eq!(snap[0].listed_by.len(), 2);` → `snap[0].attested_by.len()`
- Lines 1377-1378: `snap[0].listed_by.contains(&library_a)` → `snap[0].attested_by.contains(&library_a)` (both)
- Line 1438: `assert_eq!(snap[0].listed_by, [library_b].into_iter().collect());` → `snap[0].attested_by`
- Line 1508: `assert_eq!(snap[0].listed_by.len(), 2);` → `snap[0].attested_by.len()`

Each test also needs to thread `AttestationStatus::Unwrapped` into `on_entry` calls. Look for `agg.on_entry(entry)` patterns and replace with `agg.on_entry(entry, AttestationStatus::Unwrapped)`.

- [ ] **Step 10: Add 4 new aggregation unit tests**

Add to `#[cfg(test)] mod tests`:

```rust
    /// ZEB-280 Phase 3: an `AttestationStatus::Unwrapped` entry
    /// falls back to `entry.listed_by` when inserting into the
    /// `attested_by` set.
    #[test]
    fn aggregation_on_entry_unwrapped_inserts_into_attested_by_via_listed_by_fallback() {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x11; 16]);
        let library = OwnerAddr([0xAA; 16]);
        let entry = build_signed_open_entry_for_library(community_id, [7u8; 32], library);
        let _ = agg.on_entry(entry, AttestationStatus::Unwrapped);

        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert!(
            snap[0].attested_by.contains(&library),
            "Unwrapped path should insert listed_by into attested_by"
        );
        assert!(snap[0].unattested_by.is_empty(), "no unattested contributions");
    }

    /// ZEB-280 Phase 3: an `AttestationStatus::Attested(lib_addr)`
    /// entry inserts `lib_addr` (NOT `entry.listed_by`) into
    /// `attested_by`.
    #[test]
    fn aggregation_on_entry_attested_inserts_into_attested_by_via_lib_addr() {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x22; 16]);
        let listed_by = OwnerAddr([0xAA; 16]);
        let lib_addr = OwnerAddr([0xBB; 16]);
        // Federation case: admin signed listed_by=A but broadcaster is B.
        let entry = build_signed_open_entry_for_library(community_id, [8u8; 32], listed_by);
        let _ = agg.on_entry(entry, AttestationStatus::Attested(lib_addr));

        let snap = agg.snapshot_all();
        assert_eq!(snap.len(), 1);
        assert!(
            snap[0].attested_by.contains(&lib_addr),
            "Attested(lib_addr) inserts the broadcasting lib, NOT listed_by"
        );
        assert!(
            !snap[0].attested_by.contains(&listed_by),
            "listed_by is NOT inserted when status is Attested"
        );
    }

    /// ZEB-280 Phase 3: an `AttestationStatus::Unattested(lib_addr)`
    /// entry inserts `lib_addr` into `unattested_by`. The entry is
    /// NOT dropped from the aggregation — admin sig is still valid.
    #[test]
    fn aggregation_on_entry_unattested_inserts_into_unattested_by() {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x33; 16]);
        let listed_by = OwnerAddr([0xAA; 16]);
        let lib_addr = OwnerAddr([0xCC; 16]);
        let entry = build_signed_open_entry_for_library(community_id, [9u8; 32], listed_by);
        let _ = agg.on_entry(entry, AttestationStatus::Unattested(lib_addr));

        let snap = agg.snapshot_all();
        assert_eq!(
            snap.len(),
            1,
            "Unattested entries are NOT dropped from aggregation"
        );
        assert!(
            snap[0].unattested_by.contains(&lib_addr),
            "Unattested(lib_addr) inserts the lib into unattested_by"
        );
        assert!(snap[0].attested_by.is_empty(), "no attested contributions");

        // DTO surfaces unattested = true.
        let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
        assert!(dto.unattested, "DTO unattested = true when unattested_by non-empty");
    }

    /// ZEB-280 Phase 3: drop_library sweeps BOTH attested_by and
    /// unattested_by sets — the per_library_count decrements for the
    /// dropped library, and OTHER libraries' counts roll back when
    /// the source-matches eviction rule fires.
    #[test]
    fn aggregation_drop_library_sweeps_both_attestation_sets() {
        let mut agg = Aggregation::new();
        let community_id = SpaceId([0x44; 16]);
        let library_a = OwnerAddr([0xAA; 16]);
        let library_b = OwnerAddr([0xBB; 16]);

        // Library A attests via Unwrapped (listed_by=A); Library B
        // also broadcasts the same community but with a TAMPERED
        // wrapping sig, so they land in unattested_by.
        let entry_a = build_signed_open_entry_for_library(community_id, [10u8; 32], library_a);
        let _ = agg.on_entry(entry_a, AttestationStatus::Unwrapped);

        let entry_b = build_signed_open_entry_for_library(community_id, [10u8; 32], library_a);
        let _ = agg.on_entry(entry_b, AttestationStatus::Unattested(library_b));

        let snap_before = agg.snapshot_all();
        assert_eq!(snap_before.len(), 1);
        assert!(snap_before[0].attested_by.contains(&library_a));
        assert!(snap_before[0].unattested_by.contains(&library_b));

        // Drop library_b — should sweep it from unattested_by, NOT
        // evict the community (library_a still attests).
        let evicted = agg.drop_library(&library_b);
        assert!(
            evicted.is_empty(),
            "library_b drop should not evict community (library_a still attests)"
        );
        let snap_after_b = agg.snapshot_all();
        assert_eq!(snap_after_b.len(), 1);
        assert!(snap_after_b[0].attested_by.contains(&library_a));
        assert!(
            !snap_after_b[0].unattested_by.contains(&library_b),
            "library_b swept from unattested_by"
        );

        // Drop library_a — should evict (last remaining contributor).
        let evicted = agg.drop_library(&library_a);
        assert_eq!(
            evicted,
            vec![community_id],
            "library_a drop should evict the community"
        );
        let snap_after_a = agg.snapshot_all();
        assert!(snap_after_a.is_empty(), "community evicted after both drops");
    }
```

Helper `build_signed_open_entry_for_library` may need creating if absent — variant of `build_signed_open_entry_for` that takes a `listed_by: OwnerAddr` arg:

```rust
    fn build_signed_open_entry_for_library(
        community_id: SpaceId,
        admin_seed: [u8; 32],
        listed_by: OwnerAddr,
    ) -> LibraryDirectoryEntry {
        let (signing_key, identity_pub) = build_test_admin_identity(admin_seed);
        let admin_addr = OwnerAddr(
            harmony_identity::Identity::from_public_bytes(&identity_pub)
                .expect("identity")
                .address_hash,
        );
        let invite_url = build_open_invite_url_for(community_id, admin_addr);
        let mut entry = LibraryDirectoryEntry {
            community_id,
            community_admin_identity_pub: identity_pub,
            name: "Test".to_string(),
            description: "Test desc".to_string(),
            topics: vec![],
            invite_url,
            listed_by,
            listed_at: Hlc {
                wall_ms: 1_700_000_000_000,
                logical: 0,
                device_id: "test".to_string(),
            },
            community_signature: [0u8; 64],
            library_identity_pub: None,
            library_signature: None,
        };
        let mut for_sig = entry.clone();
        for_sig.community_signature = [0u8; 64];
        let signed_bytes = canonical_cbor_encode(&for_sig).expect("encode");
        entry.community_signature = signing_key.sign(&signed_bytes).to_bytes();
        entry
    }
```

- [ ] **Step 11: Update integration tests for renamed field**

Open `src-tauri/tests/library_directory_integration.rs`. Search for `.listed_by.` (the SET access):

```bash
cd src-tauri && grep -n "\.listed_by\." tests/library_directory_integration.rs
```

Replace each access (lines ~248-250):
- `snap[0].listed_by.len()` → `snap[0].attested_by.len()`
- `snap[0].listed_by.contains(&library_a)` → `snap[0].attested_by.contains(&library_a)`
- `snap[0].listed_by.contains(&library_b)` → `snap[0].attested_by.contains(&library_b)`

The `dto.listed_by_count` assertion stays — the DTO field name didn't change, only the source. The behavior is preserved: an Unwrapped path populates `attested_by` from `entry.listed_by`, so a single Phase 1 entry still produces `listed_by_count == 1`.

- [ ] **Step 12: Run cargo check + clippy + fmt**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -5
```

Expected: all exit 0. Any remaining compile errors are unfixed callsites or test updates — surface them.

- [ ] **Step 13: Run targeted aggregation tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(aggregation)' 2>&1 | tail -15
```

Expected: 4 new aggregation tests passing + existing aggregation tests passing (after Step 9 renames).

- [ ] **Step 14: Run full nextest suite**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -15
```

Expected: baseline + 2 (Task 1) + 7 (Task 2) + 4 (Task 3) = 13 new tests. All passing. No regression in count.

- [ ] **Step 15: Commit Task 3**

```bash
git add src-tauri/src/library_directory.rs src-tauri/tests/library_directory_integration.rs
git status
```

Confirm only these files changed (no stray edits).

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-280): aggregation evolution — attested_by + unattested_by sets

Replaces Phase 1's single AggregatedEntry.listed_by: BTreeSet<OwnerAddr>
field with two sets that track wrapping-sig attestation outcomes:
- attested_by: libraries with valid wrapping sigs (or Phase 1
  Unwrapped contributions falling back to entry.listed_by)
- unattested_by: libraries whose wrapping sig failed to verify

Aggregation::on_entry now takes AttestationStatus and routes the
broadcasting library into the appropriate set. Per-library cap
counting + drop_library + find_oldest_for_library + 
evict_library_contribution all generalize to scan/sweep BOTH sets.
A library can appear in both sets concurrently (e.g., one valid
broadcast + one tampered broadcast); union semantics are
commutative and idempotent.

DirectoryEntryDTO gains `unattested: bool` derived from
!unattested_by.is_empty(). `listed_by_count` now derives from
attested_by.len() — semantic shift: counts "N of my trusted libraries
attested" (Phase 1 entries still count via the Unwrapped fallback).

process_sample's AttributionMismatch check evolves: the broadcasting
library (from AttestationStatus) must equal the topic owner. For
Unwrapped entries this falls back to the Phase 1 listed_by check.
For wrapped entries, library_identity_pub's derived addr must equal
the topic owner. This is what allows library A to republish library
B's entry verbatim — admin signed listed_by=B, broadcaster is A,
attribution check is against A.

4 new aggregation unit tests cover all four AttestationStatus paths
including drop_library sweeping both sets. Existing tests + 
integration tests updated for field rename (listed_by → attested_by
on AggregatedEntry).
EOF
)"
```

---

## Task 4: Test fixture helpers + wire-format pinning

**Goal:** Add `mock_library_entry_wrapped` + `mock_library_entry_republished_by` fixture helpers in `tests/common/library_fixtures.rs`. Add 3 new wire-format pinning tests for Phase 3-wrapped entries. Existing Phase 1 pinning tests must remain byte-identical.

**Files:**
- Modify: `src-tauri/tests/common/library_fixtures.rs` (add 2 new helpers)
- Modify: `src-tauri/tests/wire_format_library_directory_fixtures.rs` (add 3 new tests; existing 3 unchanged)

- [ ] **Step 1: Add `mock_library_entry_wrapped` to `library_fixtures.rs`**

After the existing `mock_directory_entry` fn (around line 61), add:

```rust
/// ZEB-280 Phase 3: build a `LibraryDirectoryEntry` with both layers
/// of signatures. Pass `library_signer = None` to produce a Phase 1-
/// shaped (unwrapped) entry equivalent to `mock_directory_entry`.
/// Pass `Some((signing_key, identity_bundle))` to produce a fully
/// wrapped Phase 3 entry.
///
/// Admin sig signs over canonical CBOR with cs=0, li=None, ls=None
/// (skip_serializing_if omits the Optional fields). Library sig
/// signs over canonical CBOR with ls=None, li populated, cs populated.
///
/// Spec §5.
#[allow(clippy::too_many_arguments)]
pub fn mock_library_entry_wrapped(
    community_id: SpaceId,
    admin_seed: [u8; 32],
    listed_by: OwnerAddr,
    listed_at: Hlc,
    invite_url: String,
    name: &str,
    description: &str,
    topics: Vec<String>,
    library_signer: Option<(&SigningKey, [u8; 64])>,
) -> LibraryDirectoryEntry {
    let (admin_signing_key, admin_identity_pub) = build_test_admin_identity(admin_seed);

    // Phase 1 admin sig (Optional fields None — skip_serializing_if
    // omits them, so admin sig is identical to Phase 1).
    let mut entry = LibraryDirectoryEntry {
        community_id,
        community_admin_identity_pub: admin_identity_pub,
        name: name.to_string(),
        description: description.to_string(),
        topics,
        invite_url,
        listed_by,
        listed_at,
        community_signature: [0u8; 64],
        library_identity_pub: None,
        library_signature: None,
    };
    let mut for_sig = entry.clone();
    for_sig.community_signature = [0u8; 64];
    let admin_signed = canonical_cbor_encode(&for_sig).expect("encode admin sign");
    entry.community_signature = admin_signing_key.sign(&admin_signed).to_bytes();

    // Phase 3 wrapping sig (if library_signer provided).
    if let Some((library_signing_key, library_identity_bundle)) = library_signer {
        entry.library_identity_pub = Some(library_identity_bundle);
        entry.library_signature = None;
        let lib_signed = canonical_cbor_encode(&entry).expect("encode library sign");
        entry.library_signature = Some(library_signing_key.sign(&lib_signed).to_bytes());
    }

    entry
}

/// ZEB-280 Phase 3: take an already-admin-signed entry (presumed
/// produced by `mock_directory_entry` or `mock_library_entry_wrapped`)
/// and replace its wrapping sig with a new library's signature.
/// This is the verbatim re-syndication primitive: library A
/// republishes library B's entry by re-signing over the same
/// admin-signed bytes with A's own key, advertising A as the
/// broadcaster.
///
/// Spec §3 / §5.
pub fn mock_library_entry_republished_by(
    original: &LibraryDirectoryEntry,
    new_library_signing_key: &SigningKey,
    new_library_identity_bundle: [u8; 64],
) -> LibraryDirectoryEntry {
    let mut wrapped = original.clone();
    wrapped.library_identity_pub = Some(new_library_identity_bundle);
    wrapped.library_signature = None;
    let lib_signed = canonical_cbor_encode(&wrapped).expect("encode library sign");
    wrapped.library_signature = Some(new_library_signing_key.sign(&lib_signed).to_bytes());
    wrapped
}

/// ZEB-280 Phase 3: build a deterministic library identity bundle +
/// signing key. Mirrors `build_test_admin_identity` but with a stable
/// X25519 prefix of `0x22 × 32` (distinct from admin's `0x11 × 32`).
pub fn build_test_library_identity(seed: [u8; 32]) -> (SigningKey, [u8; 64]) {
    let signing = SigningKey::from_bytes(&seed);
    let verifying = signing.verifying_key().to_bytes();
    let mut bundle = [0u8; 64];
    bundle[..32].copy_from_slice(&[0x22; 32]);
    bundle[32..].copy_from_slice(&verifying);
    (signing, bundle)
}
```

- [ ] **Step 2: Add 3 new wire-format pinning tests**

Open `src-tauri/tests/wire_format_library_directory_fixtures.rs`. After the existing `library_directory_entry_field_keys_are_2char` test (line ~153), add:

```rust
/// ZEB-280 Phase 3: a wrapped entry (both Optional fields populated)
/// round-trips through canonical CBOR and the bstr serde helpers.
/// Sentinel byte patterns chosen to make wire-format diffs obvious.
#[test]
fn phase3_wrapped_entry_roundtrips() {
    let entry = LibraryDirectoryEntry {
        community_id: SpaceId([0x11; 16]),
        community_admin_identity_pub: fixture_admin_identity_pub(),
        name: "Wrapped Fixture".to_string(),
        description: "Phase 3 federation test.".to_string(),
        topics: vec!["federation".to_string()],
        invite_url: "harmony://invite/?p=AAAA".to_string(),
        listed_by: OwnerAddr([0x22; 16]),
        listed_at: fixture_hlc(),
        community_signature: [0x33; 64],
        library_identity_pub: Some([0x44; 64]),
        library_signature: Some([0x55; 64]),
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    let roundtrip: LibraryDirectoryEntry =
        ciborium::de::from_reader(&bytes[..]).expect("decode");
    assert_eq!(entry, roundtrip, "wrapped entry round-trips");
    assert_eq!(roundtrip.library_identity_pub, Some([0x44; 64]));
    assert_eq!(roundtrip.library_signature, Some([0x55; 64]));
}

/// ZEB-280 Phase 3: pin the canonical CBOR PREFIX bytes for a
/// wrapped entry. We pin the prefix (not the full hex string) because
/// the wrapped entry's bytes are 256+ bytes long — pinning the prefix
/// catches the map(11) marker + key ordering changes without requiring
/// us to maintain a 500-char hex string by hand.
///
/// Expected prefix:
/// - `ab` (CBOR map header, 11 keys: cd, ai, nm, ds, tp, iu, lb, la,
///   cs, li, ls)
/// - first key `cd` is text(2) = `62 63 64`
///
/// Wrapped entries MUST have exactly 11 keys (Phase 1's 9 + li + ls);
/// unwrapped entries have 9.
#[test]
fn phase3_wrapped_entry_pinned_bytes_prefix() {
    let entry = LibraryDirectoryEntry {
        community_id: SpaceId([0x11; 16]),
        community_admin_identity_pub: fixture_admin_identity_pub(),
        name: "Wrapped Fixture".to_string(),
        description: "Phase 3 federation test.".to_string(),
        topics: vec!["federation".to_string()],
        invite_url: "harmony://invite/?p=AAAA".to_string(),
        listed_by: OwnerAddr([0x22; 16]),
        listed_at: fixture_hlc(),
        community_signature: [0x33; 64],
        library_identity_pub: Some([0x44; 64]),
        library_signature: Some([0x55; 64]),
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");
    assert_eq!(
        bytes[0], 0xab,
        "wrapped entry must encode as map(11); got map({:#x}) prefix byte",
        bytes[0]
    );
    // First key after map header: text(2) "cd" = [0x62, 0x63, 0x64]
    assert_eq!(
        &bytes[1..4],
        b"\x62cd",
        "first map key must be text(2) \"cd\""
    );
}

/// ZEB-280 Phase 3: 2-char field-key invariant must hold for the
/// wrapped entry — all 11 keys (including new li + ls) are 2-char.
#[test]
fn phase3_wrapped_entry_two_char_keys_audit() {
    let entry = LibraryDirectoryEntry {
        community_id: SpaceId([0; 16]),
        community_admin_identity_pub: [0; 64],
        name: String::new(),
        description: String::new(),
        topics: vec![],
        invite_url: String::new(),
        listed_by: OwnerAddr([0; 16]),
        listed_at: Hlc {
            wall_ms: 0,
            logical: 0,
            device_id: String::new(),
        },
        community_signature: [0; 64],
        library_identity_pub: Some([0; 64]),
        library_signature: Some([0; 64]),
    };
    let bytes = canonical_cbor_encode(&entry).expect("encode");

    let value: ciborium::value::Value = ciborium::de::from_reader(&bytes[..]).expect("decode");
    let map = match value {
        ciborium::value::Value::Map(m) => m,
        other => panic!("expected CBOR map, got {other:?}"),
    };

    let mut keys = std::collections::BTreeSet::new();
    for (k, _) in map {
        match k {
            ciborium::value::Value::Text(s) => {
                assert_eq!(s.len(), 2, "field key must be 2 chars: {s:?}");
                keys.insert(s);
            }
            other => panic!("non-text map key in LibraryDirectoryEntry encoding: {other:?}"),
        }
    }

    let expected: std::collections::BTreeSet<String> =
        ["cd", "ai", "nm", "ds", "tp", "iu", "lb", "la", "cs", "li", "ls"]
            .into_iter()
            .map(str::to_string)
            .collect();
    assert_eq!(
        keys, expected,
        "Phase 3 wrapped entry must have exactly these 11 keys"
    );
}
```

- [ ] **Step 3: Run wire-format pinning tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_library_directory_fixtures 2>&1 | tail -15
```

Expected: 6 tests passing (3 existing Phase 1 + 3 new Phase 3). Existing Phase 1 pinned hex MUST still match exactly.

If `library_directory_entry_canonical_cbor_pinned` (the Phase 1 pinned-bytes test) FAILS, the `skip_serializing_if` invariant is broken. STOP and surface to user.

- [ ] **Step 4: Run cargo fmt + clippy**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: both exit 0.

- [ ] **Step 5: Run full nextest suite**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
```

Expected: baseline + 13 (Tasks 1-3) + 3 (Task 4) = 16 new tests. All passing.

- [ ] **Step 6: Commit Task 4**

```bash
git add src-tauri/tests/common/library_fixtures.rs src-tauri/tests/wire_format_library_directory_fixtures.rs
git status
```

```bash
git commit -m "$(cat <<'EOF'
test(zeb-280): wire-format pinning + mock fixtures for Phase 3 wrapping

- library_fixtures.rs:
  - mock_library_entry_wrapped: builds a fully Phase-3-signed entry
    when given a library_signer; falls back to Phase 1 shape when None
  - mock_library_entry_republished_by: replaces wrapping sig with a
    new library's sig over the same admin-signed bytes (verbatim
    re-syndication primitive)
  - build_test_library_identity: deterministic library signer +
    identity bundle (X25519 prefix 0x22 × 32, distinct from admin)
- wire_format_library_directory_fixtures.rs adds 3 Phase 3 tests:
  - phase3_wrapped_entry_roundtrips: full canonical-CBOR round-trip
  - phase3_wrapped_entry_pinned_bytes_prefix: map(11) header + first
    key prefix
  - phase3_wrapped_entry_two_char_keys_audit: ciborium::Value::Map
    iter confirms all 11 keys (cd, ai, nm, ds, tp, iu, lb, la, cs,
    li, ls) are 2-char text(2)

Existing Phase 1 pinning tests UNCHANGED — wire-format byte identity
preserved via skip_serializing_if.
EOF
)"
```

---

## Task 5: Integration tests in `library_directory_integration.rs`

**Goal:** Add 4 integration tests per spec §11.3 that exercise the full consumer-side pipeline (`process_sample` decode → verify → aggregate → snapshot/DTO) with Phase 3-wrapped entries. Each test uses the Task 4 fixture helpers.

**Files:**
- Modify: `src-tauri/tests/library_directory_integration.rs` (add 4 tests; existing tests already updated in Task 3)

- [ ] **Step 1: Add `federation_two_libraries_broadcast_same_community_aggregates`**

At the end of `library_directory_integration.rs`, before the closing brace of the test module (if any) or simply at the end of the file:

```rust
/// ZEB-280 Phase 3: library A and library B independently broadcast
/// the SAME admin-signed community entry, each with their own
/// wrapping sig. Aggregation should treat them as 2 distinct
/// broadcasting attestations of the same community. DTO surfaces
/// listed_by_count = 2, unattested = false.
#[tokio::test]
async fn federation_two_libraries_broadcast_same_community_aggregates() {
    use common::library_fixtures::{
        build_test_library_identity, mock_library_entry_wrapped,
    };

    let community_id = SpaceId([0x88; 16]);
    let admin_seed = [42u8; 32];

    let (lib_a_signer, lib_a_bundle) = build_test_library_identity([1u8; 32]);
    let (lib_b_signer, lib_b_bundle) = build_test_library_identity([2u8; 32]);
    let library_a = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&lib_a_bundle)
            .expect("identity a")
            .address_hash,
    );
    let library_b = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&lib_b_bundle)
            .expect("identity b")
            .address_hash,
    );

    // Both libraries broadcast the SAME admin-signed community.
    // The admin sig is over (cs=0, li=None, ls=None) — portable across
    // libraries. Each library wraps with its own sig. listed_by is the
    // ORIGINAL lister; since each library is the originator (not
    // republishing), listed_by = each library's own addr.
    let entry_a = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_a,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-a".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Federated Community",
        "Same community, two libraries.",
        vec!["federation".to_string()],
        Some((&lib_a_signer, lib_a_bundle)),
    );
    let entry_b = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_b,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 1,
            device_id: "test-b".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Federated Community",
        "Same community, two libraries.",
        vec!["federation".to_string()],
        Some((&lib_b_signer, lib_b_bundle)),
    );

    let (dir, _request_rx) = LibraryDirectory::new();
    let bytes_a = canonical_cbor_encode(&entry_a).expect("encode a");
    let bytes_b = canonical_cbor_encode(&entry_b).expect("encode b");
    dir.process_sample(library_a, bytes_a)
        .await
        .expect("process a");
    dir.process_sample(library_b, bytes_b)
        .await
        .expect("process b");

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1, "single community aggregated across libraries");
    assert_eq!(
        snap[0].attested_by.len(),
        2,
        "both broadcasting libraries attested"
    );
    assert!(snap[0].attested_by.contains(&library_a));
    assert!(snap[0].attested_by.contains(&library_b));
    assert!(snap[0].unattested_by.is_empty(), "no unattested broadcasts");

    let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
    assert_eq!(dto.listed_by_count, 2);
    assert!(!dto.unattested);
}
```

- [ ] **Step 2: Add `federation_one_library_tampered_wrapping_shows_unattested`**

```rust
/// ZEB-280 Phase 3: library A broadcasts a valid wrapped entry;
/// library B broadcasts the same community but with a TAMPERED
/// wrapping sig. Aggregation: attested_by = {A}, unattested_by = {B},
/// DTO: listed_by_count = 1, unattested = true (badge surfaces).
#[tokio::test]
async fn federation_one_library_tampered_wrapping_shows_unattested() {
    use common::library_fixtures::{
        build_test_library_identity, mock_library_entry_wrapped,
    };

    let community_id = SpaceId([0x99; 16]);
    let admin_seed = [43u8; 32];

    let (lib_a_signer, lib_a_bundle) = build_test_library_identity([3u8; 32]);
    let (lib_b_signer, lib_b_bundle) = build_test_library_identity([4u8; 32]);
    let library_a = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&lib_a_bundle)
            .expect("identity a")
            .address_hash,
    );
    let library_b = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&lib_b_bundle)
            .expect("identity b")
            .address_hash,
    );

    // Library A: valid wrapping.
    let entry_a = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_a,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-a".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Tampered Test",
        "One good wrap, one bad wrap.",
        vec![],
        Some((&lib_a_signer, lib_a_bundle)),
    );

    // Library B: produce a valid wrap first, then TAMPER the wrapping sig.
    let mut entry_b = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_b,
        Hlc {
            wall_ms: 1_700_000_000_001,
            logical: 0,
            device_id: "test-b".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Tampered Test",
        "One good wrap, one bad wrap.",
        vec![],
        Some((&lib_b_signer, lib_b_bundle)),
    );
    let mut tampered_sig = entry_b.library_signature.expect("sig present");
    tampered_sig[0] ^= 0xFF;
    entry_b.library_signature = Some(tampered_sig);

    let (dir, _request_rx) = LibraryDirectory::new();
    let bytes_a = canonical_cbor_encode(&entry_a).expect("encode a");
    let bytes_b = canonical_cbor_encode(&entry_b).expect("encode b");
    dir.process_sample(library_a, bytes_a)
        .await
        .expect("process a");
    dir.process_sample(library_b, bytes_b)
        .await
        .expect("process b (unattested but NOT dropped)");

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1, "tampered entry still surfaced");
    assert!(
        snap[0].attested_by.contains(&library_a),
        "library_a attested via valid wrap"
    );
    assert!(
        snap[0].unattested_by.contains(&library_b),
        "library_b in unattested_by (bad wrap)"
    );
    assert!(!snap[0].attested_by.contains(&library_b));

    let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
    assert_eq!(dto.listed_by_count, 1, "only library_a counted in listed_by_count");
    assert!(dto.unattested, "DTO unattested = true triggers UI badge");
}
```

- [ ] **Step 3: Add `federation_phase1_entry_aggregates_alongside_phase3_wrapped`**

```rust
/// ZEB-280 Phase 3: a Phase 1-style entry (no wrapping sig) and a
/// Phase 3 wrapped entry from different libraries aggregate to the
/// same community. Both contribute to attested_by. DTO unattested = false.
/// Tests cross-version wire compat.
#[tokio::test]
async fn federation_phase1_entry_aggregates_alongside_phase3_wrapped() {
    use common::library_fixtures::{
        build_test_library_identity, mock_directory_entry, mock_library_entry_wrapped,
    };

    let community_id = SpaceId([0xAA; 16]);
    let admin_seed = [44u8; 32];

    let library_a = OwnerAddr([0xA1; 16]); // Phase 1 library (no key pair needed)
    let (lib_b_signer, lib_b_bundle) = build_test_library_identity([5u8; 32]);
    let library_b = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&lib_b_bundle)
            .expect("identity b")
            .address_hash,
    );

    // Library A: Phase 1 unwrapped entry (no wrapping sig).
    let entry_a = mock_directory_entry(
        community_id,
        admin_seed,
        library_a,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-a".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Mixed Mode",
        "Phase 1 + Phase 3 in the same aggregation.",
        vec![],
    );

    // Library B: Phase 3 wrapped entry, same community.
    let entry_b = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_b,
        Hlc {
            wall_ms: 1_700_000_000_001,
            logical: 0,
            device_id: "test-b".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Mixed Mode",
        "Phase 1 + Phase 3 in the same aggregation.",
        vec![],
        Some((&lib_b_signer, lib_b_bundle)),
    );

    let (dir, _request_rx) = LibraryDirectory::new();
    let bytes_a = canonical_cbor_encode(&entry_a).expect("encode a");
    let bytes_b = canonical_cbor_encode(&entry_b).expect("encode b");
    dir.process_sample(library_a, bytes_a)
        .await
        .expect("process Phase 1 entry");
    dir.process_sample(library_b, bytes_b)
        .await
        .expect("process Phase 3 wrapped entry");

    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1);
    assert!(
        snap[0].attested_by.contains(&library_a),
        "Phase 1 entry: Unwrapped status falls back to entry.listed_by = library_a"
    );
    assert!(
        snap[0].attested_by.contains(&library_b),
        "Phase 3 entry: Attested(library_b)"
    );
    assert!(snap[0].unattested_by.is_empty());

    let dto = DirectoryEntryDTO::from_aggregated(&snap[0]);
    assert_eq!(dto.listed_by_count, 2);
    assert!(!dto.unattested, "no unattested contributions");
}
```

- [ ] **Step 4: Add `federation_remove_library_evicts_attested_and_unattested_contributions`**

```rust
/// ZEB-280 Phase 3: library_directory::drop_library walks BOTH the
/// attested_by and unattested_by sets and sweeps the dropped library
/// from each. A library that was only in unattested_by (no valid
/// attestation, only bad-sig attempts) is still cleanly removed.
#[tokio::test]
async fn federation_remove_library_evicts_attested_and_unattested_contributions() {
    use common::library_fixtures::{
        build_test_library_identity, mock_library_entry_wrapped,
    };

    let community_id = SpaceId([0xBB; 16]);
    let admin_seed = [45u8; 32];

    let (lib_a_signer, lib_a_bundle) = build_test_library_identity([6u8; 32]);
    let (lib_b_signer, lib_b_bundle) = build_test_library_identity([7u8; 32]);
    let library_a = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&lib_a_bundle)
            .expect("identity a")
            .address_hash,
    );
    let library_b = OwnerAddr(
        harmony_identity::Identity::from_public_bytes(&lib_b_bundle)
            .expect("identity b")
            .address_hash,
    );

    // Library A: valid wrapping.
    let entry_a = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_a,
        Hlc {
            wall_ms: 1_700_000_000_000,
            logical: 0,
            device_id: "test-a".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Remove Test",
        "A is attested, B is unattested (tampered).",
        vec![],
        Some((&lib_a_signer, lib_a_bundle)),
    );

    // Library B: tampered wrapping.
    let mut entry_b = mock_library_entry_wrapped(
        community_id,
        admin_seed,
        library_b,
        Hlc {
            wall_ms: 1_700_000_000_001,
            logical: 0,
            device_id: "test-b".to_string(),
        },
        open_invite_url_for(community_id, admin_seed),
        "Remove Test",
        "A is attested, B is unattested (tampered).",
        vec![],
        Some((&lib_b_signer, lib_b_bundle)),
    );
    let mut tampered = entry_b.library_signature.expect("sig present");
    tampered[0] ^= 0xFF;
    entry_b.library_signature = Some(tampered);

    let (dir, _request_rx) = LibraryDirectory::new();
    dir.process_sample(library_a, canonical_cbor_encode(&entry_a).expect("encode a"))
        .await
        .expect("process a");
    dir.process_sample(library_b, canonical_cbor_encode(&entry_b).expect("encode b"))
        .await
        .expect("process b unattested");

    // Confirm initial state.
    let snap = dir.snapshot_all().await;
    assert_eq!(snap.len(), 1);
    assert!(snap[0].attested_by.contains(&library_a));
    assert!(snap[0].unattested_by.contains(&library_b));

    // Drop library_b — should sweep it from unattested_by but NOT
    // evict the community (library_a still attests).
    let evicted = dir.drop_library(&library_b).await;
    assert!(
        evicted.is_empty(),
        "library_b drop should NOT evict (library_a still attests)"
    );
    let snap_after_b = dir.snapshot_all().await;
    assert_eq!(snap_after_b.len(), 1);
    assert!(snap_after_b[0].attested_by.contains(&library_a));
    assert!(snap_after_b[0].unattested_by.is_empty());

    // Drop library_a — should evict (no more attestations).
    let evicted = dir.drop_library(&library_a).await;
    assert_eq!(
        evicted,
        vec![community_id],
        "library_a drop evicts community"
    );
    let snap_final = dir.snapshot_all().await;
    assert!(snap_final.is_empty());
}
```

- [ ] **Step 5: Run integration tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test library_directory_integration 2>&1 | tail -20
```

Expected: existing Phase 1 integration tests still passing + 4 new federation tests passing.

If `federation_phase1_entry_aggregates_alongside_phase3_wrapped` fails because `open_invite_url_for` isn't available with that name, check the existing test file for the actual helper name (lines 40-50 of the file). The reference at the top of `library_directory_integration.rs` is:

```rust
fn open_invite_url_for(community_id: SpaceId, admin_seed: [u8; 32]) -> String { ... }
```

— it should already exist. If not, add it (the body builds an open-community invite URL using the test admin's keys).

- [ ] **Step 6: Run cargo fmt + clippy**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
```

Expected: both exit 0.

- [ ] **Step 7: Run full nextest**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
```

Expected: baseline + 16 (Tasks 1-4) + 4 (Task 5) = 20 new tests. All passing.

- [ ] **Step 8: Commit Task 5**

```bash
git add src-tauri/tests/library_directory_integration.rs
git status
```

```bash
git commit -m "$(cat <<'EOF'
test(zeb-280): integration tests for Phase 3 federation aggregation

4 new federation integration tests exercising process_sample's
end-to-end decode → verify → aggregate pipeline with Phase 3-wrapped
entries:

- federation_two_libraries_broadcast_same_community_aggregates:
  Both libraries independently broadcast the same admin-signed
  community with their own wrapping sigs. attested_by = {A, B},
  unattested_by = {}, DTO listed_by_count = 2, unattested = false.

- federation_one_library_tampered_wrapping_shows_unattested:
  Library A broadcasts valid; library B same community with tampered
  wrapping sig. attested_by = {A}, unattested_by = {B}, DTO
  listed_by_count = 1, unattested = true.

- federation_phase1_entry_aggregates_alongside_phase3_wrapped:
  Phase 1 unwrapped + Phase 3 wrapped entries from different libraries
  aggregate to the same community. Phase 1 path Unwrapped → fallback
  to entry.listed_by; Phase 3 path Attested(addr). DTO listed_by_count
  = 2, unattested = false. Tests cross-version wire compat.

- federation_remove_library_evicts_attested_and_unattested_contributions:
  drop_library walks BOTH sets. Dropping a library that only has
  unattested contributions sweeps it cleanly; dropping the last
  attestor evicts the community.

All tests use the Task 4 mock_library_entry_wrapped +
build_test_library_identity fixtures.
EOF
)"
```

---

## Task 6: Frontend — `unattested` boolean in DTO + inline badge

**Goal:** Update the frontend `DirectoryEntry` interface to include `unattested: boolean`. Render an inline `⚠ Unattested` badge in `LibraryDirectoryBrowser.svelte` when an entry has `unattested === true`. Add 2 vitest cases.

**Files:**
- Modify: `src/lib/library-directory-service.ts` (add field to `DirectoryEntry`)
- Modify: `src/lib/components/LibraryDirectoryBrowser.svelte` (add badge + amber styling)
- Modify: `src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts` (add 2 vitest cases)

- [ ] **Step 1: Add `unattested: boolean` to `DirectoryEntry` interface**

Open `src/lib/library-directory-service.ts`. Locate the existing `DirectoryEntry` interface (around line 40-51):

```typescript
export interface DirectoryEntry {
  community_id: string;
  community_addr: string;
  name: string;
  description: string;
  topics: string[];
  invite_url: string;
  listed_by_count: number;
  listed_at: Hlc;
}
```

Update to:

```typescript
/** Mirrors `library_directory::DirectoryEntryDTO` IPC return shape. */
export interface DirectoryEntry {
  /** Hex-encoded SpaceId (32 chars). */
  community_id: string;
  /** Hex-encoded OwnerAddr (32 chars) derived from the admin identity. */
  community_addr: string;
  name: string;
  description: string;
  topics: string[];
  invite_url: string;
  /**
   * Count of libraries with valid attestation for this entry
   * (`attested_by.len()` in the Rust aggregation). Includes Phase 1
   * unwrapped contributions via fallback to entry.listed_by.
   */
  listed_by_count: number;
  /**
   * Sub-D Phase 3 (ZEB-280): true if at least one broadcasting
   * library's wrapping sig failed to verify (Rust:
   * `!unattested_by.is_empty()`). Drives the inline "⚠ Unattested"
   * badge in `LibraryDirectoryBrowser.svelte`.
   */
  unattested: boolean;
  listed_at: Hlc;
}
```

- [ ] **Step 2: Add the inline badge to `LibraryDirectoryBrowser.svelte`**

Open `src/lib/components/LibraryDirectoryBrowser.svelte`. Locate the entry row markup (around line 310-320 — search for `entry.listed_by_count`). The badge should appear inline next to the community name.

Find the row's community-name display element. It typically looks like:

```svelte
<div class="row-header">
  <span class="community-name">{entry.name}</span>
  ...
</div>
```

Insert the badge right after the community-name span:

```svelte
<div class="row-header">
  <span class="community-name">{entry.name}</span>
  {#if entry.unattested}
    <span
      class="unattested-badge"
      role="img"
      aria-label="One or more libraries' wrapping signatures failed to verify for this entry"
      title="Unattested: at least one broadcasting library's signature failed to verify. The community admin's signature is still valid; the listing's content is trustworthy."
    >
      ⚠ Unattested
    </span>
  {/if}
  ...
</div>
```

Then add the CSS for the badge. Locate the `<style>` block at the bottom of the file and add:

```css
  .unattested-badge {
    display: inline-block;
    margin-left: 0.5rem;
    padding: 0.125rem 0.5rem;
    font-size: 0.75rem;
    font-weight: 500;
    background-color: #fef3c7; /* amber-100 */
    color: #92400e; /* amber-800 */
    border: 1px solid #fcd34d; /* amber-300 */
    border-radius: 0.25rem;
    vertical-align: middle;
    cursor: help; /* hover tooltip cue */
  }
```

The amber color set (NOT red) communicates "warning, but not critical" — admin sig is still valid; only the transport-integrity attestation is compromised. Spec §7.2.

- [ ] **Step 3: Add 2 vitest cases for badge rendering**

Open `src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts`. Locate the existing test block (typically uses `render(LibraryDirectoryBrowser, { props: { ... } })`).

Add 2 new tests at the end of the suite:

```typescript
  test('unattested_badge_renders_when_dto_unattested_true', async () => {
    const mockLibrary: LibraryInfo = {
      address: 'aa'.repeat(16),
      added_at: { wall_ms: 1_700_000_000_000, logical: 0, device_id: 'test' },
      entry_count: 1,
    };
    const mockEntry: DirectoryEntry = {
      community_id: 'cc'.repeat(16),
      community_addr: 'dd'.repeat(16),
      name: 'Test Community',
      description: 'A community for testing.',
      topics: ['test'],
      invite_url: 'harmony://invite/?p=AAAA',
      listed_by_count: 1,
      unattested: true, // ← drives badge
      listed_at: { wall_ms: 1_700_000_000_000, logical: 0, device_id: 'test' },
    };

    const adapter = {
      invoke: vi.fn(async (cmd: string) => {
        if (cmd === 'list_libraries') return [mockLibrary];
        if (cmd === 'list_discovered_libraries') return [];
        if (cmd === 'browse_library') return [mockEntry];
        return [];
      }),
    };

    const { container } = render(LibraryDirectoryBrowser, {
      props: { service: new LibraryDirectoryService(adapter as any) },
    });

    // Wait for the entries to load (refresh runs in onMount).
    await waitFor(() => {
      expect(container.querySelector('.unattested-badge')).toBeTruthy();
    });

    const badge = container.querySelector('.unattested-badge')!;
    expect(badge.textContent).toContain('Unattested');
    expect(badge.getAttribute('aria-label')).toContain('wrapping signature');
    expect(badge.getAttribute('title')).toContain('admin');
  });

  test('unattested_badge_absent_when_dto_unattested_false', async () => {
    const mockLibrary: LibraryInfo = {
      address: 'aa'.repeat(16),
      added_at: { wall_ms: 1_700_000_000_000, logical: 0, device_id: 'test' },
      entry_count: 1,
    };
    const mockEntry: DirectoryEntry = {
      community_id: 'cc'.repeat(16),
      community_addr: 'dd'.repeat(16),
      name: 'Test Community',
      description: 'A community for testing.',
      topics: ['test'],
      invite_url: 'harmony://invite/?p=AAAA',
      listed_by_count: 1,
      unattested: false,
      listed_at: { wall_ms: 1_700_000_000_000, logical: 0, device_id: 'test' },
    };

    const adapter = {
      invoke: vi.fn(async (cmd: string) => {
        if (cmd === 'list_libraries') return [mockLibrary];
        if (cmd === 'list_discovered_libraries') return [];
        if (cmd === 'browse_library') return [mockEntry];
        return [];
      }),
    };

    const { container } = render(LibraryDirectoryBrowser, {
      props: { service: new LibraryDirectoryService(adapter as any) },
    });

    // Wait for entries to load THEN assert badge is NOT present.
    await waitFor(() => {
      expect(container.textContent).toContain('Test Community');
    });

    expect(container.querySelector('.unattested-badge')).toBeNull();
  });
```

The vitest test file already imports `LibraryDirectoryBrowser`, `LibraryDirectoryService`, `LibraryInfo`, `DirectoryEntry`, `render`, `waitFor`, `vi` — confirm by reading the existing imports at the top of the file. If `DirectoryEntry` is not imported, add it to the existing import line.

- [ ] **Step 4: Run frontend type check**

```bash
npx tsc --noEmit 2>&1 | tail -5
```

Expected: no errors. The `DirectoryEntry` interface change is additive; existing call sites that don't read `unattested` still type-check.

- [ ] **Step 5: Run frontend vitest**

```bash
npx vitest run src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts 2>&1 | tail -15
```

Expected: existing LibraryDirectoryBrowser tests still passing + 2 new badge tests passing.

- [ ] **Step 6: Run full vitest suite**

```bash
npx vitest run 2>&1 | tail -10
```

Expected: baseline + 2 new vitest tests. All passing.

- [ ] **Step 7: Commit Task 6**

```bash
git add src/lib/library-directory-service.ts src/lib/components/LibraryDirectoryBrowser.svelte src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts
git status
```

```bash
git commit -m "$(cat <<'EOF'
feat(zeb-280): frontend ⚠ Unattested badge

Adds the `unattested: boolean` field to the DirectoryEntry interface
(mirrors Rust's DirectoryEntryDTO.unattested = !unattested_by.is_empty()).

LibraryDirectoryBrowser.svelte renders an inline amber badge next to
the community name when entry.unattested === true. The badge has:
- aria-label describing the meaning (for screen readers)
- title attribute with hover tooltip explaining "admin sig still valid"
- amber color scheme (NOT red) per spec §7.2 — admin sig is the
  trust anchor for content; wrapping sig is transport-integrity

2 vitest cases:
- unattested_badge_renders_when_dto_unattested_true
- unattested_badge_absent_when_dto_unattested_false

No new IPCs — the boolean rides through the existing browse_library
return shape (additive DTO field).
EOF
)"
```

---

## Task 7: Final verification + push + PR

**Goal:** Re-run all 5 CI gates to confirm green, push the branch to origin, open the PR with markdown-linked refs. NO Linear sub-tickets to file (ZEB-280 itself closes on merge; ZEB-218 stays In Progress; Phase 4 / Phase 6 follow-ups already exist as ZEB-281 / ZEB-252).

**Files:**
- No file edits.

- [ ] **Step 1: Re-run all 5 CI gates locally**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
cd src-tauri && cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -5
```

Expected: all 4 Rust gates green. Pass count = baseline + 20 (Tasks 1-5).

```bash
npx tsc --noEmit 2>&1 | tail -5
npx vitest run 2>&1 | tail -10
```

Expected: tsc clean, vitest = baseline + 2 (Task 6).

- [ ] **Step 2: Sanity-check commit history**

```bash
git log --oneline origin/main..HEAD
```

Expected output (8 commits — spec + 7 implementation):

```
<sha7> docs(zeb-280): plan added                  (optional — only if plan was committed; per project precedent it usually isn't until later)
<sha6> feat(zeb-280): frontend ⚠ Unattested badge
<sha5> test(zeb-280): integration tests for Phase 3 federation aggregation
<sha4> test(zeb-280): wire-format pinning + mock fixtures for Phase 3 wrapping
<sha3> feat(zeb-280): aggregation evolution — attested_by + unattested_by sets
<sha2> feat(zeb-280): verify_entry returns AttestationStatus
<sha1> feat(zeb-280): Phase 3 wire format — Optional li/ls fields + AttestationStatus
87dcaca docs(zeb-280): Sub-D Phase 3 federated republication design
```

(The plan file commit isn't shown above because per project precedent the plan is typically NOT committed to the feature branch — it lives in `docs/plans/` as a planning artifact. If the project precedent has been to commit plans, also commit it here before pushing.)

If the plan commit IS expected per recent precedent (PR #108, PR #109 both committed plans alongside specs), add it now:

```bash
git add docs/plans/2026-05-12-zeb-280-sub-d-phase-3-federated-republication-plan.md
git commit -m "docs(zeb-280): Phase 3 federated republication implementation plan"
```

- [ ] **Step 3: Push the branch to origin**

```bash
git push -u origin zeb-280-sub-d-phase-3-federated-republication 2>&1 | tail -5
```

Expected: `* [new branch]      zeb-280-... -> zeb-280-...` + tracking info. If push fails on hooks, investigate — DO NOT use --no-verify.

- [ ] **Step 4: Create the PR with markdown-linked refs**

```bash
gh pr create --title "ZEB-280 Sub-D Phase 3: federated republication of LibraryDirectoryEntry" --body "$(cat <<'EOF'
## Summary

Adds a second cryptographic attestation layer to [`LibraryDirectoryEntry`](src-tauri/src/library_directory.rs): broadcasting libraries can wrap admin-signed entries with their own Ed25519 signature, enabling verifiable federation while staying wire-compatible with Phase 1 entries (Optional fields + `skip_serializing_if`).

Implements [ZEB-280](https://linear.app/zeblith/issue/ZEB-280/) — Phase 3 of [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) (Sub-D library-federated discovery). Follows [PR #108](https://github.com/zeblithic/harmony-client/pull/108) (Phase 1 vertical slice) and [PR #109](https://github.com/zeblithic/harmony-client/pull/109) (Phase 2 auto-discovery). [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) stays In Progress after this merge — Phase 4 ([ZEB-281](https://linear.app/zeblith/issue/ZEB-281/) ProfileMembershipBroadcast) and Phase 6 ([ZEB-252](https://linear.app/zeblith/issue/ZEB-252/) direct-join IPC rewrite) remain.

## What changed

**Backend** (`src-tauri/`):
- `library_directory.rs`:
  - `LibraryDirectoryEntry` gains Optional `library_identity_pub` (`rename = "li"`) + `library_signature` (`rename = "ls"`) with `skip_serializing_if = "Option::is_none"` → Phase 1 wire bytes stay byte-identical.
  - `AttestationStatus` enum: `Unwrapped | Attested(addr) | Unattested(addr)`.
  - `verify_entry` returns `Result<AttestationStatus, EntryVerifyError>`. Admin sig still gatekeeper; tampered wrapping is `Ok(Unattested(addr))` (entry NOT dropped).
  - New error variants: `LibrarySignatureFieldsInconsistent` (exactly one of li/ls is `Some`), `InvalidLibraryIdentityPub`.
  - `AggregatedEntry.listed_by: BTreeSet` → split into `attested_by` + `unattested_by`. Per-library cap counts ALL contributions; `drop_library` sweeps both sets.
  - `Aggregation::on_entry` signature: `(LibraryDirectoryEntry, AttestationStatus) -> ProcessResult`.
  - `process_sample` AttributionMismatch check evolves: broadcasting library (from AttestationStatus) must equal topic owner. For Unwrapped entries, falls back to Phase 1 listed_by check. For wrapped entries, library_identity_pub's derived addr must equal topic owner.
  - `DirectoryEntryDTO`: gains `unattested: bool`; `listed_by_count = attested_by.len()`.
- `owner_state_types.rs`: adds `serialize_optional_bytes_as_bstr` + `deserialize_optional_bytes_from_bstr` helpers.

**Frontend** (`src/lib/`):
- `library-directory-service.ts`: `DirectoryEntry` interface gains `unattested: boolean`.
- `LibraryDirectoryBrowser.svelte`: inline `⚠ Unattested` badge with aria-label + title; amber color (NOT red — admin sig still valid).

**Tests** (~21 new):
- 11 unit tests (verify_entry × 7, aggregation × 4)
- 4 integration tests (federation × 4)
- 3 wire-format pinning tests (round-trip, prefix bytes, 2-char keys audit — Phase 1 pinning UNCHANGED)
- 2 frontend vitest tests (badge renders / absent)

**No new IPCs.** Federation is fully transparent — `browse_library` keeps its signature; the new boolean is additive to the DTO.

## Design references

- Spec: [`docs/specs/2026-05-12-zeb-280-sub-d-phase-3-federated-republication-design.md`](docs/specs/2026-05-12-zeb-280-sub-d-phase-3-federated-republication-design.md) — 475 lines, §1-§15
- Plan: [`docs/plans/2026-05-12-zeb-280-sub-d-phase-3-federated-republication-plan.md`](docs/plans/2026-05-12-zeb-280-sub-d-phase-3-federated-republication-plan.md)
- Parent epic spec: [`docs/specs/2026-04-30-zeb-206-nav-tree-design.md`](docs/specs/2026-04-30-zeb-206-nav-tree-design.md) §486-489 (signature verification contract)

## Cross-version compatibility

| Producer | Consumer | Behavior |
|---|---|---|
| Phase 1 (no wrapping fields) | Phase 1 | Works (current state) |
| Phase 1 (no wrapping fields) | Phase 3 | Works. `Unwrapped` status; fallback to entry.listed_by |
| Phase 3 (wrapping fields) | Phase 1 | Works. Ciborium tolerates unknown fields by default; admin sig still verifies (skip_serializing_if invariant) |
| Phase 3 (wrapping fields) | Phase 3 | Full Phase 3 path: admin sig verified, wrapping sig verified |

## Test plan

- [x] `cargo fmt --all -- --check` (run from `src-tauri/`)
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (from `src-tauri/`)
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (from `src-tauri/`) — baseline + 20 new
- [x] `cargo check --locked --all-targets --features test-fixtures` (MSRV gate, from `src-tauri/`)
- [x] `npx tsc --noEmit` (from repo root)
- [x] `npx vitest run` (from repo root) — baseline + 2 new
- [x] Phase 1 wire-format pinning fixtures (`library_directory_entry_canonical_cbor_pinned`) byte-identical (skip_serializing_if invariant)
- [x] Federation integration tests cover: two-library agg, tampered-wrapping unattested, mixed-mode wire compat, drop_library sweeps both sets

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)" 2>&1 | tail -5
```

Expected: the PR URL is returned. Capture it (the autonomous monitoring loop will reference it).

- [ ] **Step 5: Report PR URL + status**

After `gh pr create` returns, output the PR URL to the human/calling agent so the autonomous monitoring loop can pick it up.

The calling agent then enters the autonomous bot-review monitoring loop per `feedback_autonomous_pr_monitoring_loop` memory (270s wakeup cadence, batched fixups, race-prevention re-fetch immediately before each push, pushover-notify at convergence OR on exception requiring user input).

---

## Self-review

**1. Spec coverage:** Each spec section is implemented by a task:
- §1 Goal → Tasks 1-6 collectively
- §2 Why this shape → no implementation; design rationale
- §3 Architecture overview → Tasks 1-3 (backend) + Task 6 (frontend)
- §4.1 Wire format → Task 1
- §4.2 AttestationStatus → Task 1 (struct) + Task 2 (returned by verify_entry)
- §4.3 Aggregation evolution → Task 3
- §4.4 DirectoryEntryDTO → Task 3
- §5 Verification path → Task 2
- §6 IPC surface (unchanged) → No task; verified by Task 5 + Task 7 (existing IPCs still pass tests + tsc/vitest)
- §7 Frontend → Task 6
- §8 Cross-version compatibility → Task 1 + Task 5 (`federation_phase1_entry_aggregates_alongside_phase3_wrapped`)
- §9 Error handling → Task 2 (verify_entry) + Task 3 (process_sample AttributionMismatch evolution)
- §10 Performance/scale → No task; design rationale (additional Ed25519 verify ~50µs/entry)
- §11.1 Test fixtures → Task 4
- §11.2 Unit tests → Task 2 (7 verify_entry) + Task 3 (4 aggregation)
- §11.3 Integration tests → Task 5
- §11.4 Wire-format pinning → Task 4
- §11.5 Frontend vitest → Task 6
- §12 Deferred follow-ups → no tickets to file (speculative; ZEB-281/ZEB-252 already exist)
- §13 Out of scope → No task; honored by NOT implementing federation depth > 1, etc.
- §14 Acceptance criteria → All 4 criteria covered by Tasks 1-6 + final verification in Task 7

**2. Placeholder scan:** No "TBD", "TODO", "fill in details", etc. All code blocks are complete. Test command outputs are explicit.

**3. Type consistency:** 
- `AttestationStatus` defined in Task 1; consumed in Task 2 (`verify_entry`), Task 3 (`Aggregation::on_entry`), Task 5 (integration tests pass it via process_sample which extracts it from verify_entry).
- `AggregatedEntry.attested_by` (new field name) used consistently across Tasks 3, 5, and the integration test updates.
- `DirectoryEntryDTO.unattested` (bool) defined in Task 3; consumed in Task 6 (frontend reads `entry.unattested`).
- `mock_library_entry_wrapped` signature defined in Task 4; called consistently in Task 5 with `Option<(&SigningKey, [u8; 64])>` final arg.
- Helper `build_test_library_identity` defined in Task 4; called from Task 5 integration tests.
- `build_signed_open_entry_for` / `build_signed_open_entry_for_library` are test helpers either pre-existing in Phase 1 tests OR added in Task 2/3 — plan should not double-define.

**4. Tests-pass-after-each-commit invariant:**
- Task 1: round-trip tests + Phase 1 pinning pass.
- Task 2: 7 new verify_entry tests pass + existing Phase 1 verify_entry tests still pass (after the `_ = status;` no-op threading in process_sample).
- Task 3: 4 new aggregation tests pass + existing integration tests still pass (after rename + on_entry signature update).
- Task 4: 3 new wire-format pinning tests pass + existing Phase 1 pinning UNCHANGED.
- Task 5: 4 new integration tests pass.
- Task 6: 2 new vitest tests pass.

No fix-ups required between tasks; the plan is self-coherent.
