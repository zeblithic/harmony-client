# ZEB-279 Sub-D Phase 2 — library auto-discovery implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the Sub-D Phase 2 vertical slice — `harmony/discovery/library/announce` subscription, library-signed `LibraryAnnounce` wire records, in-memory discovered set capped at 1,000, inline collapsible "Discovered libraries" section in `LibraryDirectoryBrowser`. Add-from-discovered reuses Phase 1 `addLibrary` IPC.

**Architecture:** Extend the existing `library_directory.rs` consumer module with a parallel `Announces` map alongside the Phase 1 `Aggregation`. Single permanent Zenoh subscriber at startup (no add/remove plumbing — fixed key). One new IPC `list_discovered_libraries`; filter excludes already-added libraries. One UI section added to `LibraryDirectoryBrowser.svelte`.

**Tech stack:** Rust (Tauri backend, Zenoh, Ed25519, ciborium), Svelte 5, TypeScript, vitest, cargo nextest.

**Spec:** [`docs/specs/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-design.md`](../specs/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-design.md) (commit `9e109ad`)

**Branch:** `zeb-279-sub-d-phase-2-library-auto-discovery` (already cut from `origin/main` `239c146`).

**Phase 1 predecessor:** PR [#108](https://github.com/zeblithic/harmony-client/pull/108) merged at `239c146`. Phase 1's wire types, IPC handlers, frontend service, and Zenoh wiring patterns are the canonical references for everything in this plan.

---

## Task 0: Pre-flight + green-baseline confirm

**Files:** none (verification only).

**No commit.** Goal: confirm all 5 CI gates green on the freshly-cut branch before any implementation work, so later regressions are unambiguous.

- [ ] **Step 1: Confirm branch state**

Run:
```bash
git status
git log --oneline -3
```

Expected:
```
On branch zeb-279-sub-d-phase-2-library-auto-discovery
nothing to commit, working tree clean
```
```
9e109ad docs(zeb-279): Sub-D Phase 2 library auto-discovery design spec
239c146 Merge pull request #108 from zeblithic/zeb-218-sub-d-library-directory-vertical-slice
...
```

If branch is wrong or working tree is dirty: STOP. Report to controller.

- [ ] **Step 2: `cargo fmt` gate**

Run:
```bash
cd src-tauri && cargo fmt --all -- --check
```

Expected: exit code 0, no output.

- [ ] **Step 3: `cargo clippy` gate**

Run:
```bash
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: exit code 0. Compile may take 2-3 min.

- [ ] **Step 4: `cargo nextest` gate**

Run:
```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: exit code 0, `X passed / 0 failed`. Capture the baseline test count for later comparison.

- [ ] **Step 5: `cargo check` (msrv) gate**

Run:
```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

Expected: exit code 0.

- [ ] **Step 6: Frontend `tsc` gate**

Run from repo root:
```bash
npx tsc --noEmit
```

Expected: exit code 0, no errors.

- [ ] **Step 7: Frontend `vitest` gate**

Run from repo root:
```bash
npx vitest run
```

Expected: exit code 0, `X passed / 0 failed`. Capture baseline.

- [ ] **Step 8: Report baselines**

Report to controller:
- Rust test count: `<N>`
- Frontend test count: `<M>`
- All 5 gates: GREEN

No commit. Status: DONE.

---

## Task 1: Wire format `LibraryAnnounce` + verify + pinning fixtures

**Files:**
- Modify: `src-tauri/src/library_directory.rs` (add `LibraryAnnounce` struct, `AnnounceVerifyError`, `verify_announce`, new bounds constants are not needed — reuse `MAX_NAME_LEN` / `MAX_DESCRIPTION_LEN`)
- Create: `src-tauri/tests/wire_format_library_announce_fixtures.rs`

**TDD shape:** write the pinning fixture test first (it'll fail-to-compile because struct doesn't exist), then add the struct + verify + bounds, then green the test.

- [ ] **Step 1: Add `LibraryAnnounce` struct + impls to `library_directory.rs`**

Open `src-tauri/src/library_directory.rs`. After the existing `LibraryDirectoryEntry` block ending around line 70 (the `impl CanonicalPayload for LibraryDirectoryEntry {}` line), insert:

```rust
/// Sub-D Phase 2 auto-discovery announce record. Spec §4.1.
///
/// Published by libraries to `harmony/discovery/library/announce` to
/// advertise their existence. Each device subscribes the topic once at
/// startup; valid announces populate the in-memory `Announces` map and
/// surface in the `LibraryDirectoryBrowser` "Discovered libraries"
/// section.
///
/// Signing model: the library signs its own announce with the Ed25519
/// half of its 64-byte identity bundle. The OwnerAddr derives from the
/// identity bundle (via `Identity::from_public_bytes`), so no separate
/// `library_addr` field is on the wire — it cannot disagree with the
/// signed identity.
///
/// 2-char field keys (`ai`, `nm`, `ds`, `la`, `ls`) satisfy
/// `canonical_cbor_encode`'s same-length-keys precondition (mirrors
/// `LibraryDirectoryEntry` and all other Sub-A/B/C wire types).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryAnnounce {
    /// 64-byte identity bundle (X25519_pub(32) || Ed25519_pub(32)).
    /// The OwnerAddr derives from this; the Ed25519 half verifies
    /// `library_signature`.
    #[serde(
        rename = "ai",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub library_identity_pub: [u8; 64],

    #[serde(rename = "nm")]
    pub name: String,

    #[serde(rename = "ds")]
    pub description: String,

    #[serde(rename = "la")]
    pub listed_at: Hlc,

    /// Ed25519 sig over canonical CBOR with `ls` zeroed.
    #[serde(
        rename = "ls",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub library_signature: [u8; 64],
}

impl CanonicalPayloadSealed for LibraryAnnounce {}
impl CanonicalPayload for LibraryAnnounce {}
```

- [ ] **Step 2: Add `AnnounceVerifyError` enum**

In the same file, after the existing `EntryVerifyError` enum (ending around line 115), insert:

```rust
/// Verification error categories for `LibraryAnnounce`. Mirrors
/// `EntryVerifyError` but simpler — no invite URL, no
/// community/admin payload binding. Each variant surfaces as a
/// warn-level log; the announce is silently dropped from the
/// caller's perspective.
#[derive(Debug, thiserror::Error)]
pub enum AnnounceVerifyError {
    #[error("malformed library identity_pub: {0}")]
    InvalidIdentityPub(String),
    #[error("canonical CBOR encode failed: {0}")]
    Encode(#[from] crate::owner_state_crypto::CryptoError),
    #[error("Ed25519 signature verification failed")]
    SignatureInvalid,
    #[error("name exceeds {MAX_NAME_LEN} bytes")]
    NameTooLong,
    #[error("description exceeds {MAX_DESCRIPTION_LEN} bytes")]
    DescriptionTooLong,
}
```

(The `{MAX_NAME_LEN}` and `{MAX_DESCRIPTION_LEN}` placeholders work because Rust's `thiserror` interpolates const names in scope.)

- [ ] **Step 3: Add `verify_announce` function**

After `verify_entry` (ending around line 200), insert:

```rust
/// Verify a `LibraryAnnounce` end-to-end:
/// 1. Anti-spam bounds (name/description lengths)
/// 2. Parse `library_identity_pub` via
///    `harmony_identity::Identity::from_public_bytes` (validates both
///    halves of the X25519||Ed25519 bundle)
/// 3. Verify the Ed25519 signature over canonical-CBOR-encoded fields
///    with `library_signature` zeroed (so verify == sign exactly)
///
/// Returns the derived `OwnerAddr` (library_addr) on success — callers
/// need this to insert into the Announces map.
pub fn verify_announce(
    announce: &LibraryAnnounce,
) -> Result<OwnerAddr, AnnounceVerifyError> {
    // (1) Bounds
    if announce.name.len() > MAX_NAME_LEN {
        return Err(AnnounceVerifyError::NameTooLong);
    }
    if announce.description.len() > MAX_DESCRIPTION_LEN {
        return Err(AnnounceVerifyError::DescriptionTooLong);
    }

    // (2) Parse identity_pub — also rejects malformed point bytes.
    let identity =
        harmony_identity::Identity::from_public_bytes(&announce.library_identity_pub)
            .map_err(|e| AnnounceVerifyError::InvalidIdentityPub(format!("{e:?}")))?;

    // (3) Verify sig over canonical CBOR with signature field zeroed.
    let mut for_sig = announce.clone();
    for_sig.library_signature = [0u8; 64];
    let signed_bytes = canonical_cbor_encode(&for_sig)?;
    let sig = Signature::from_bytes(&announce.library_signature);
    identity
        .verifying_key
        .verify_strict(&signed_bytes, &sig)
        .map_err(|_| AnnounceVerifyError::SignatureInvalid)?;

    Ok(OwnerAddr(identity.address_hash))
}
```

- [ ] **Step 4: Verify it compiles**

Run from repo root:
```bash
cd src-tauri && cargo check --features test-fixtures
```

Expected: clean compile. If errors, fix imports / typos.

- [ ] **Step 5: Create wire-format pinning fixture test**

Create new file `src-tauri/tests/wire_format_library_announce_fixtures.rs`:

```rust
//! Wire-format pinning fixtures for `LibraryAnnounce`.
//!
//! These tests pin the exact canonical-CBOR bytes for a known-good
//! announce record. Pinning catches accidental wire-format changes
//! (field renames, key additions, type substitutions) BEFORE they
//! break cross-device compat.
//!
//! Companion to `wire_format_library_directory_fixtures.rs` (Phase 1).

use ciborium::value::Value as CborValue;
use harmony_app::library_directory::{verify_announce, LibraryAnnounce};
use harmony_app::owner_state_crypto::canonical_cbor_encode;
use harmony_app::owner_state_types::Hlc;
use std::collections::BTreeSet;

/// Build a canonical test `LibraryAnnounce` with deterministic keys.
/// Returns (announce, derived library_addr_hex).
#[cfg(feature = "test-fixtures")]
fn canonical_test_announce() -> (LibraryAnnounce, String) {
    use harmony_app::test_fixtures::deterministic_identity_for_test;

    // Deterministic Ed25519 keypair from seed [7u8; 32] (chosen to differ
    // from any Phase 1 fixture seed).
    let (signing_key, identity) = deterministic_identity_for_test([7u8; 32]);
    let mut announce = LibraryAnnounce {
        library_identity_pub: identity.public_bundle(),
        name: "Indie Games Library".to_string(),
        description: "Curated indie game communities".to_string(),
        listed_at: Hlc {
            wall_ms: 1_715_000_000_000,
            logical: 1,
            device_id: 1,
        },
        library_signature: [0u8; 64],
    };
    // Sign canonical CBOR with sig zeroed.
    let signed_bytes = canonical_cbor_encode(&announce).expect("encode");
    let sig = ed25519_dalek::Signer::sign(&signing_key, &signed_bytes);
    announce.library_signature = sig.to_bytes();

    let addr_hex = hex::encode(identity.address_hash);
    (announce, addr_hex)
}

#[test]
#[cfg(feature = "test-fixtures")]
fn announce_canonical_cbor_roundtrip() {
    let (announce, _) = canonical_test_announce();
    let bytes = canonical_cbor_encode(&announce).expect("encode");
    let decoded: LibraryAnnounce =
        ciborium::from_reader(&bytes[..]).expect("decode");
    assert_eq!(decoded, announce);
}

#[test]
#[cfg(feature = "test-fixtures")]
fn announce_verifies_after_signing() {
    let (announce, expected_addr_hex) = canonical_test_announce();
    let addr = verify_announce(&announce).expect("verify");
    assert_eq!(hex::encode(addr.0), expected_addr_hex);
}

#[test]
#[cfg(feature = "test-fixtures")]
fn announce_field_keys_are_2char() {
    let (announce, _) = canonical_test_announce();
    let bytes = canonical_cbor_encode(&announce).expect("encode");
    let value: CborValue = ciborium::from_reader(&bytes[..]).expect("decode as value");
    let map = match value {
        CborValue::Map(m) => m,
        other => panic!("expected map, got {:?}", other),
    };
    let keys: BTreeSet<String> = map
        .iter()
        .filter_map(|(k, _)| match k {
            CborValue::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let expected: BTreeSet<String> =
        ["ai", "nm", "ds", "la", "ls"].iter().map(|s| s.to_string()).collect();
    assert_eq!(keys, expected, "field keys must be exactly {{ai,nm,ds,la,ls}}");
}

#[test]
#[cfg(feature = "test-fixtures")]
fn announce_pinned_bytes_prefix_stable() {
    // Pin the canonical bytes' length and a structural prefix so any
    // accidental change to field order or types fails loudly. We don't
    // pin the full byte string because deterministic_identity_for_test
    // returns a public key derived from the seed — the bytes are
    // reproducible but verbose to assert against here.
    let (announce, _) = canonical_test_announce();
    let bytes = canonical_cbor_encode(&announce).expect("encode");

    // Map with 5 entries → CBOR major type 5, count 5 → first byte 0xA5.
    assert_eq!(bytes[0], 0xA5, "canonical CBOR must start with map(5)");

    // First key is "ai" — 2-char text string, 0x62 prefix.
    assert_eq!(bytes[1], 0x62, "first key prefix must be 2-char text-string");
    assert_eq!(&bytes[2..4], b"ai", "first key must be 'ai'");
}
```

- [ ] **Step 6: Run the pinning test**

Run from repo root:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test wire_format_library_announce_fixtures
```

Expected: 4 passed.

If `deterministic_identity_for_test` doesn't exist or has a different signature, look at the existing Phase 1 fixture (`tests/wire_format_library_directory_fixtures.rs`) for the actual import path and adapt accordingly. Same for `Identity::public_bundle()` — adapt to whatever the Phase 1 fixture uses.

- [ ] **Step 7: Add 2 unit tests for verify_announce in module `tests`**

Inside `src-tauri/src/library_directory.rs`, find the existing `#[cfg(test)] mod tests` block (or add one near the bottom). Add:

```rust
#[cfg(test)]
mod announce_verify_tests {
    use super::*;
    use crate::owner_state_crypto::canonical_cbor_encode;
    use crate::owner_state_types::Hlc;

    fn unsigned_announce_with_identity(identity_pub: [u8; 64]) -> LibraryAnnounce {
        LibraryAnnounce {
            library_identity_pub: identity_pub,
            name: "Test".to_string(),
            description: "Test desc".to_string(),
            listed_at: Hlc { wall_ms: 1, logical: 0, device_id: 1 },
            library_signature: [0u8; 64],
        }
    }

    #[test]
    fn rejects_invalid_identity_pub() {
        // All-0x7F identity_pub is invalid as an X25519 point.
        let announce = unsigned_announce_with_identity([0x7F; 64]);
        let err = verify_announce(&announce).unwrap_err();
        assert!(matches!(err, AnnounceVerifyError::InvalidIdentityPub(_)));
    }

    #[test]
    fn rejects_name_too_long() {
        // Use a known-valid identity (zero bytes are invalid; reuse the
        // Phase 1 fixture pattern of just constructing then expecting
        // identity-parse failure if we set fake bytes — so for the
        // name-too-long test we want identity parse to PASS, then bounds
        // to fail. The simplest way: borrow Phase 1's fixture pattern
        // here by accepting that identity parse comes before bounds
        // means: actually the bounds checks come FIRST in verify_announce
        // (see step 3) — so any identity_pub is fine for this test.
        let mut announce = unsigned_announce_with_identity([0x7F; 64]);
        announce.name = "x".repeat(MAX_NAME_LEN + 1);
        let err = verify_announce(&announce).unwrap_err();
        assert!(matches!(err, AnnounceVerifyError::NameTooLong));
    }
}
```

Run from repo root:
```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --lib library_directory::announce_verify_tests
```

Expected: 2 passed.

- [ ] **Step 8: `cargo fmt` + `cargo clippy`**

Run from repo root:
```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: clippy clean.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/library_directory.rs src-tauri/tests/wire_format_library_announce_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(zeb-279): Task 1 — LibraryAnnounce wire type + verify + pinning

Adds the Phase 2 wire format mirroring Phase 1's LibraryDirectoryEntry
shape: 2-char CBOR field keys (ai/nm/ds/la/ls), bstr-serialized 64-byte
identity_pub + signature, Ed25519 sig over canonical CBOR with sig
field zeroed. verify_announce returns the derived OwnerAddr so callers
don't repeat the Identity::from_public_bytes derivation.

AnnounceVerifyError mirrors EntryVerifyError minus the invite-URL and
payload-binding variants (announce records carry no invite_url and no
community/admin payload to bind to).

Wire-format pinning fixture asserts (a) round-trip, (b) verify after
sign, (c) field keys are exactly {ai,nm,ds,la,ls}, (d) canonical bytes
start with CBOR map(5) and first key is "ai" — catches accidental
key-rename or field-add slip-through.

EOF
)"
```

Status: DONE.

---

## Task 2: `Announces` map + `process_announce` + cap eviction

**Files:**
- Modify: `src-tauri/src/library_directory.rs` (add `Announces` struct, `AnnounceOutcome`, `AnnounceProcessResult`, `process_announce` method on `LibraryDirectory`, `MAX_DISCOVERED_LIBRARIES` const)

**TDD shape:** add module-internal unit tests first that pin behavior, then implement.

- [ ] **Step 1: Add `MAX_DISCOVERED_LIBRARIES` constant**

In `library_directory.rs`, near the other capacity constants (around line 121 where `MAX_ENTRIES_PER_LIBRARY` lives), add:

```rust
/// Cap on the in-memory `Announces` map. Smaller than
/// `MAX_ENTRIES_PER_LIBRARY` because this is a global count of known
/// libraries, not per-library entries. Spec §4.2 / §10.
pub const MAX_DISCOVERED_LIBRARIES: usize = 1_000;
```

- [ ] **Step 2: Add `Announces` struct + `AnnounceOutcome` + `AnnounceProcessResult`**

After the existing `Aggregation` block (look for `pub struct Aggregation`), insert:

```rust
/// In-memory discovered-libraries map populated by
/// `process_announce`. NOT persisted — rebuilt on startup from the
/// announce-topic subscription. Spec §4.2.
#[derive(Debug, Default)]
pub struct Announces {
    by_addr: BTreeMap<OwnerAddr, LibraryAnnounce>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnounceOutcome {
    /// New library address — first time seen.
    Inserted(OwnerAddr),
    /// Existing library, replaced by newer-HLC announce.
    Updated(OwnerAddr),
    /// Existing library, incoming has older/equal HLC.
    Idempotent,
}

/// Result of `process_announce`. The outer outcome and any
/// orthogonal cap-eviction are independent — both fields can be
/// populated when an at-cap insert evicts an unrelated library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceProcessResult {
    pub outcome: AnnounceOutcome,
    /// Some(addr) if the cap was hit and `addr` was evicted to make
    /// room for the incoming announce.
    pub evicted: Option<OwnerAddr>,
}

impl Announces {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_addr.len()
    }

    /// Snapshot for IPC return. Sorted by listed_at descending (newest
    /// first) so the UI surfaces fresh announces at the top.
    pub fn snapshot(&self) -> Vec<LibraryAnnounce> {
        let mut out: Vec<_> = self.by_addr.values().cloned().collect();
        out.sort_by(|a, b| b.listed_at.cmp(&a.listed_at));
        out
    }

    /// Process a verified announce. Caller MUST have run
    /// `verify_announce` first (which returns the derived `library_addr`).
    /// This method does NOT re-verify.
    pub fn on_announce(
        &mut self,
        library_addr: OwnerAddr,
        announce: LibraryAnnounce,
    ) -> AnnounceProcessResult {
        // Dedupe: latest-listed_at-wins.
        if let Some(existing) = self.by_addr.get(&library_addr) {
            if announce.listed_at <= existing.listed_at {
                return AnnounceProcessResult {
                    outcome: AnnounceOutcome::Idempotent,
                    evicted: None,
                };
            }
            // Strictly newer — replace.
            self.by_addr.insert(library_addr, announce);
            return AnnounceProcessResult {
                outcome: AnnounceOutcome::Updated(library_addr),
                evicted: None,
            };
        }

        // Brand-new library — apply cap.
        let mut evicted: Option<OwnerAddr> = None;
        if self.by_addr.len() >= MAX_DISCOVERED_LIBRARIES {
            // Evict oldest-by-listed_at. Stable tie-break by addr byte order.
            if let Some(oldest_addr) = self
                .by_addr
                .iter()
                .min_by(|(addr_a, a), (addr_b, b)| {
                    a.listed_at.cmp(&b.listed_at).then_with(|| addr_a.cmp(addr_b))
                })
                .map(|(addr, _)| *addr)
            {
                self.by_addr.remove(&oldest_addr);
                evicted = Some(oldest_addr);
            }
        }
        self.by_addr.insert(library_addr, announce);
        AnnounceProcessResult {
            outcome: AnnounceOutcome::Inserted(library_addr),
            evicted,
        }
    }
}
```

- [ ] **Step 3: Add `announces` field to `LibraryDirectory` struct**

Find the `pub struct LibraryDirectory` block (it has `aggregation: tokio::sync::Mutex<Aggregation>`). Add a sibling field:

```rust
pub struct LibraryDirectory {
    pub aggregation: tokio::sync::Mutex<Aggregation>,
    /// Sub-D Phase 2: discovered-libraries map populated by the
    /// announce-topic subscriber. Spec §4.2.
    pub announces: tokio::sync::Mutex<Announces>,
    request_tx: mpsc::UnboundedSender<LibraryDirectoryRequest>,
}
```

In the `impl LibraryDirectory { pub fn new(...) -> ... }` constructor, initialize the new field:

```rust
pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<LibraryDirectoryRequest>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let dir = Arc::new(Self {
        aggregation: tokio::sync::Mutex::new(Aggregation::new()),
        announces: tokio::sync::Mutex::new(Announces::new()),
        request_tx: tx,
    });
    (dir, rx)
}
```

- [ ] **Step 4: Add `process_announce` async method on `LibraryDirectory`**

Add a new method alongside `process_sample`:

```rust
impl LibraryDirectory {
    /// Sub-D Phase 2: ingest one announce-topic sample. Verifies, then
    /// inserts/updates the announces map. Returns the outcome so the
    /// caller can emit `library-directory-updated` on non-Idempotent
    /// changes (or on orthogonal cap-eviction).
    pub async fn process_announce(
        self: &Arc<Self>,
        bytes: Vec<u8>,
    ) -> Result<AnnounceProcessResult, AnnounceVerifyError> {
        let announce: LibraryAnnounce = ciborium::from_reader(&bytes[..])
            .map_err(|e| AnnounceVerifyError::Encode(
                crate::owner_state_crypto::CryptoError::CborDecode(format!("{e}"))
            ))?;
        let library_addr = verify_announce(&announce)?;
        let mut announces = self.announces.lock().await;
        Ok(announces.on_announce(library_addr, announce))
    }
}
```

**Verify** `CryptoError::CborDecode(String)` is the actual variant. If it doesn't exist, find the closest variant in `crate::owner_state_crypto::CryptoError` and adapt; or add a new `AnnounceVerifyError::DecodeFailed(String)` variant and use it.

- [ ] **Step 5: Add unit tests for `Announces::on_announce`**

In the existing `#[cfg(test)] mod tests` block in `library_directory.rs`, add:

```rust
#[cfg(test)]
mod announce_tests {
    use super::*;
    use crate::owner_state_types::Hlc;

    fn test_announce(name: &str, wall_ms: u64) -> LibraryAnnounce {
        LibraryAnnounce {
            library_identity_pub: [0u8; 64], // not verified at this layer
            name: name.to_string(),
            description: String::new(),
            listed_at: Hlc { wall_ms, logical: 0, device_id: 1 },
            library_signature: [0u8; 64],
        }
    }

    fn addr(b: u8) -> OwnerAddr {
        OwnerAddr([b; 16])
    }

    #[test]
    fn on_announce_inserts_new_library() {
        let mut announces = Announces::new();
        let result = announces.on_announce(addr(1), test_announce("LibA", 100));
        assert_eq!(result.outcome, AnnounceOutcome::Inserted(addr(1)));
        assert_eq!(result.evicted, None);
        assert_eq!(announces.len(), 1);
    }

    #[test]
    fn on_announce_dedupes_latest_listed_at_wins() {
        let mut announces = Announces::new();
        announces.on_announce(addr(1), test_announce("Old", 100));
        let result = announces.on_announce(addr(1), test_announce("New", 200));
        assert_eq!(result.outcome, AnnounceOutcome::Updated(addr(1)));
        let snap = announces.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "New");
    }

    #[test]
    fn on_announce_older_is_idempotent() {
        let mut announces = Announces::new();
        announces.on_announce(addr(1), test_announce("New", 200));
        let result = announces.on_announce(addr(1), test_announce("Older", 100));
        assert_eq!(result.outcome, AnnounceOutcome::Idempotent);
        let snap = announces.snapshot();
        assert_eq!(snap[0].name, "New");
    }

    #[test]
    fn on_announce_cap_eviction_drops_oldest_listed_at() {
        let mut announces = Announces::new();
        // Fill to cap with distinct addrs and ascending listed_at.
        for i in 0..MAX_DISCOVERED_LIBRARIES as u8 {
            // Use distinct addrs by writing i to first byte; rest stays 0.
            let mut a = [0u8; 16];
            a[0] = i;
            announces.on_announce(
                OwnerAddr(a),
                test_announce(&format!("Lib{}", i), 100 + i as u64),
            );
        }
        assert_eq!(announces.len(), MAX_DISCOVERED_LIBRARIES);

        // Insert one more — should evict the oldest (i=0, listed_at=100).
        let mut new_addr = [0u8; 16];
        new_addr[0] = 0xFF;
        let result =
            announces.on_announce(OwnerAddr(new_addr), test_announce("New", 9999));
        assert_eq!(
            result.outcome,
            AnnounceOutcome::Inserted(OwnerAddr(new_addr))
        );
        let evicted_addr = result.evicted.expect("must have evicted oldest");
        let mut expected_evicted = [0u8; 16];
        expected_evicted[0] = 0;
        assert_eq!(evicted_addr.0, expected_evicted);
        assert_eq!(announces.len(), MAX_DISCOVERED_LIBRARIES);
    }

    #[test]
    fn snapshot_sorted_newest_first() {
        let mut announces = Announces::new();
        announces.on_announce(addr(1), test_announce("Old", 100));
        announces.on_announce(addr(2), test_announce("Mid", 200));
        announces.on_announce(addr(3), test_announce("New", 300));
        let snap = announces.snapshot();
        assert_eq!(snap[0].name, "New");
        assert_eq!(snap[1].name, "Mid");
        assert_eq!(snap[2].name, "Old");
    }
}
```

- [ ] **Step 6: Run unit tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --lib library_directory::announce_tests
```

Expected: 5 passed.

- [ ] **Step 7: `cargo fmt` + `cargo clippy`**

```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
```

Expected: clippy clean.

- [ ] **Step 8: Run full nextest to verify no regressions**

```bash
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: baseline + new tests passed; 0 failures.

- [ ] **Step 9: Commit**

```bash
git add src-tauri/src/library_directory.rs
git commit -m "$(cat <<'EOF'
feat(zeb-279): Task 2 — Announces map + process_announce + cap eviction

In-memory discovered-libraries map alongside Phase 1's Aggregation,
sharing the same LibraryDirectory struct. Latest-listed_at-wins
dedupe with HLC's ordering. MAX_DISCOVERED_LIBRARIES = 1_000;
on-cap insert evicts oldest-by-listed_at (stable tie-break by
addr byte order).

snapshot() returns sorted newest-first so the UI surfaces fresh
announces at the top.

process_announce decodes + verifies + dispatches to on_announce
in one call. Verify failure → AnnounceVerifyError propagated to
the caller (event_loop's subscriber task), which warn-logs and
silently drops.

EOF
)"
```

Status: DONE.

---

## Task 3: Zenoh subscription wiring + mock fixture + non-IPC integration tests

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (spawn permanent announce subscriber)
- Modify: `src-tauri/tests/common/library_fixtures.rs` (add `mock_library_announce` helper)
- Create: `src-tauri/tests/library_announce_integration.rs`

**TDD shape:** write the integration tests first (driving `process_announce` directly with mock fixtures), implement the wiring, watch them go green. Wiring isn't directly testable without an actual Zenoh session, so coverage is via the unit-shape integration tests + the explicit `cargo check` that the wiring compiles.

- [ ] **Step 1: Add `mock_library_announce` to test fixtures**

Open `src-tauri/tests/common/library_fixtures.rs`. Find the existing `mock_library_entry` helper. Add a new helper:

```rust
/// Build a signed `LibraryAnnounce` ready to publish on
/// `harmony/discovery/library/announce`. Returns the bytes + the
/// derived library_addr (caller checks process_announce returned
/// the expected addr).
#[cfg(feature = "test-fixtures")]
pub fn mock_library_announce(
    seed: [u8; 32],
    name: &str,
    description: &str,
    listed_at_wall_ms: u64,
) -> (Vec<u8>, harmony_app::owner_state_types::OwnerAddr) {
    use harmony_app::library_directory::LibraryAnnounce;
    use harmony_app::owner_state_crypto::canonical_cbor_encode;
    use harmony_app::owner_state_types::Hlc;
    use harmony_app::test_fixtures::deterministic_identity_for_test;

    let (signing_key, identity) = deterministic_identity_for_test(seed);
    let mut announce = LibraryAnnounce {
        library_identity_pub: identity.public_bundle(),
        name: name.to_string(),
        description: description.to_string(),
        listed_at: Hlc {
            wall_ms: listed_at_wall_ms,
            logical: 0,
            device_id: 1,
        },
        library_signature: [0u8; 64],
    };
    let signed_bytes = canonical_cbor_encode(&announce).expect("encode");
    let sig = ed25519_dalek::Signer::sign(&signing_key, &signed_bytes);
    announce.library_signature = sig.to_bytes();

    let bytes = canonical_cbor_encode(&announce).expect("encode for publish");
    let addr = harmony_app::owner_state_types::OwnerAddr(identity.address_hash);
    (bytes, addr)
}
```

**Verify** the actual signatures of `deterministic_identity_for_test` and `identity.public_bundle()` match what's in scope. If not, copy the shape from `mock_library_entry` in the same file and adapt.

- [ ] **Step 2: Create integration test file**

Create `src-tauri/tests/library_announce_integration.rs`:

```rust
//! Sub-D Phase 2 announce integration tests.
//!
//! Drives `LibraryDirectory::process_announce` directly (mirroring
//! the pattern in `library_directory_integration.rs`). The Zenoh
//! subscription wiring is exercised indirectly — these tests prove
//! the ingest path is correct given valid byte input.

mod common;

use harmony_app::library_directory::{
    AnnounceOutcome, AnnounceVerifyError, LibraryDirectory, MAX_DISCOVERED_LIBRARIES,
};

#[tokio::test]
#[cfg(feature = "test-fixtures")]
async fn announce_ingests_and_appears_in_snapshot() {
    let (dir, _rx) = LibraryDirectory::new();
    let (bytes, addr) = common::library_fixtures::mock_library_announce(
        [1u8; 32],
        "Indie Games",
        "Curated indie games",
        100,
    );
    let result = dir.process_announce(bytes).await.expect("process_announce ok");
    assert_eq!(result.outcome, AnnounceOutcome::Inserted(addr));

    let announces = dir.announces.lock().await;
    let snap = announces.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].name, "Indie Games");
}

#[tokio::test]
#[cfg(feature = "test-fixtures")]
async fn announce_dedupes_latest_listed_at_wins() {
    let (dir, _rx) = LibraryDirectory::new();
    // Use the SAME seed so both fixtures produce the same library_addr.
    let (bytes_old, addr) = common::library_fixtures::mock_library_announce(
        [2u8; 32],
        "Old name",
        "Old desc",
        100,
    );
    let (bytes_new, addr_new) = common::library_fixtures::mock_library_announce(
        [2u8; 32],
        "New name",
        "New desc",
        200,
    );
    assert_eq!(addr, addr_new, "same seed → same library_addr");

    dir.process_announce(bytes_old).await.expect("first ok");
    let result = dir.process_announce(bytes_new).await.expect("second ok");
    assert_eq!(result.outcome, AnnounceOutcome::Updated(addr));

    let snap = dir.announces.lock().await.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].name, "New name");
}

#[tokio::test]
#[cfg(feature = "test-fixtures")]
async fn announce_older_listed_at_dropped_silently() {
    let (dir, _rx) = LibraryDirectory::new();
    let (bytes_new, _addr) = common::library_fixtures::mock_library_announce(
        [3u8; 32],
        "New",
        "",
        200,
    );
    let (bytes_old, _) = common::library_fixtures::mock_library_announce(
        [3u8; 32],
        "Older",
        "",
        100,
    );
    dir.process_announce(bytes_new).await.expect("new ok");
    let result = dir.process_announce(bytes_old).await.expect("older ok (no sig fail)");
    assert_eq!(result.outcome, AnnounceOutcome::Idempotent);

    let snap = dir.announces.lock().await.snapshot();
    assert_eq!(snap[0].name, "New");
}

#[tokio::test]
#[cfg(feature = "test-fixtures")]
async fn announce_invalid_sig_rejected() {
    let (dir, _rx) = LibraryDirectory::new();
    let (mut bytes, _addr) = common::library_fixtures::mock_library_announce(
        [4u8; 32],
        "Tampered",
        "",
        100,
    );
    // Flip a bit in the middle of the payload to corrupt the signed bytes.
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;

    let err = dir.process_announce(bytes).await.unwrap_err();
    assert!(
        matches!(err, AnnounceVerifyError::SignatureInvalid | AnnounceVerifyError::Encode(_)),
        "expected SignatureInvalid (or Encode if the bit flip broke CBOR structure), got {:?}",
        err,
    );
}

#[tokio::test]
#[cfg(feature = "test-fixtures")]
async fn announce_name_too_long_rejected() {
    let (dir, _rx) = LibraryDirectory::new();
    // 201-byte name exceeds MAX_NAME_LEN=200.
    let huge_name = "x".repeat(201);
    let (bytes, _addr) = common::library_fixtures::mock_library_announce(
        [5u8; 32],
        &huge_name,
        "",
        100,
    );
    let err = dir.process_announce(bytes).await.unwrap_err();
    assert!(matches!(err, AnnounceVerifyError::NameTooLong));
}

#[tokio::test]
#[cfg(feature = "test-fixtures")]
async fn announce_cap_eviction_drops_oldest() {
    let (dir, _rx) = LibraryDirectory::new();

    // Fill exactly to cap with distinct seeds + ascending listed_at.
    for i in 0..MAX_DISCOVERED_LIBRARIES {
        let mut seed = [0u8; 32];
        seed[0] = (i & 0xFF) as u8;
        seed[1] = ((i >> 8) & 0xFF) as u8;
        let (bytes, _addr) = common::library_fixtures::mock_library_announce(
            seed,
            "filler",
            "",
            1_000 + i as u64,
        );
        dir.process_announce(bytes).await.expect("fill ok");
    }
    assert_eq!(dir.announces.lock().await.snapshot().len(), MAX_DISCOVERED_LIBRARIES);

    // Insert one more with the highest listed_at. The earliest filler
    // (listed_at=1000, seed[0]=0,seed[1]=0) should be evicted.
    let mut new_seed = [0xFEu8; 32];
    new_seed[0] = 0xFE;
    let (bytes_new, new_addr) = common::library_fixtures::mock_library_announce(
        new_seed,
        "newest",
        "",
        99_999,
    );
    let result = dir.process_announce(bytes_new).await.expect("over-cap ok");
    assert_eq!(result.outcome, AnnounceOutcome::Inserted(new_addr));
    assert!(result.evicted.is_some(), "must have evicted oldest");

    let snap = dir.announces.lock().await.snapshot();
    assert_eq!(snap.len(), MAX_DISCOVERED_LIBRARIES);
    // Newest is at index 0 (snapshot sorts by listed_at desc).
    assert_eq!(snap[0].name, "newest");
}
```

- [ ] **Step 3: Run integration tests**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test library_announce_integration
```

Expected: 6 passed.

If tests fail because `process_announce` doesn't decode CBOR-wrong bytes (e.g., a corruption test where CBOR layout breaks), adjust the assertion — the test asserts EITHER `SignatureInvalid` or `Encode` per the comment, accommodating both failure paths.

- [ ] **Step 4: Wire the permanent announce subscriber in `event_loop.rs`**

Open `src-tauri/src/event_loop.rs`. Find the closing `}` of the `if let (Some(library_directory), Some(library_request_rx)) = ...` block (around line 715 — right before the comment `// ── Process startup actions (declare queryables + subscribers) ────`).

INSIDE that conditional block (so it only runs when library_directory exists), AFTER the existing per-library subscriber spawn (after the closing `});` of the existing tokio::spawn at ~line 714), add the announce subscriber:

```rust
        // Sub-D Phase 2 (ZEB-279): permanent announce-topic subscriber.
        // Single fixed-key subscription, lifetime = app lifetime — no
        // add/remove plumbing. Mirrors the per-library subscriber shape
        // above but without the request-channel.
        {
            let dir = Arc::clone(&library_directory_handle);
            let session_for_announce = Arc::clone(&session_arc);
            let app_for_announce = app.clone();
            let closing_announce = Arc::clone(&closing);
            tokio::spawn(async move {
                let key_expr = "harmony/discovery/library/announce";
                let sub = match session_for_announce.declare_subscriber(key_expr).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "declare_subscriber failed for library announce — auto-discovery disabled this session"
                        );
                        return;
                    }
                };
                loop {
                    match sub.recv_async().await {
                        Ok(sample) => {
                            let bytes = sample.payload().to_bytes().to_vec();
                            match dir.process_announce(bytes).await {
                                Ok(result) => {
                                    let changed = matches!(
                                        result.outcome,
                                        crate::library_directory::AnnounceOutcome::Inserted(_)
                                            | crate::library_directory::AnnounceOutcome::Updated(_)
                                    );
                                    if changed || result.evicted.is_some() {
                                        let _ = app_for_announce.emit(
                                            "library-directory-updated",
                                            serde_json::json!({ "communityId": null }),
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = ?e,
                                        "library announce rejected"
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            if !closing_announce.load(Ordering::SeqCst) {
                                tracing::warn!(
                                    "library announce subscriber closed unexpectedly"
                                );
                            }
                            break;
                        }
                    }
                }
            });
        }
```

**Verify** that `library_directory_handle` is the variable name in scope (it is, per the existing block at line 602). Also verify that `closing` and `app` are in scope from the outer event_loop function (they are, since the per-library spawn at line 642 uses them).

- [ ] **Step 5: Run `cargo check` to confirm wiring compiles**

```bash
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
```

Expected: clean.

If any clippy warnings appear (e.g., `Arc::clone(&app)` style), follow the pattern of the existing per-library spawn.

- [ ] **Step 6: `cargo fmt` + `cargo clippy` + full `cargo nextest`**

```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all gates green; baseline + 6 new tests all passing.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/event_loop.rs src-tauri/tests/common/library_fixtures.rs src-tauri/tests/library_announce_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-279): Task 3 — announce-topic subscriber + integration tests

Permanent Zenoh subscriber spawned at startup alongside Phase 1's
per-library subscriber, subscribing to harmony/discovery/library/announce
exact-key. Sample loop decodes → process_announce → emit
library-directory-updated on Inserted/Updated/cap-eviction.

mock_library_announce extends tests/common/library_fixtures.rs with
the deterministic-keys helper builder. 6 integration tests cover
ingest, dedupe (latest-listed_at-wins), older-listed_at-dropped,
invalid-sig-rejected, name-too-long-rejected, cap-eviction.

The wiring location mirrors Phase 1's per-library subscriber shape
exactly, just without the LibraryDirectoryRequest channel (announce
key is fixed for app lifetime).

EOF
)"
```

Status: DONE.

---

## Task 4: `list_discovered_libraries` IPC + filter + IPC integration tests

**Files:**
- Modify: `src-tauri/src/lib.rs` (add DTO + handler + register in `generate_handler!`)
- Modify: `src-tauri/tests/library_announce_integration.rs` (add IPC-shaped tests)

- [ ] **Step 1: Add `DiscoveredLibraryInfo` DTO**

Open `src-tauri/src/lib.rs`. Find the existing `LibraryInfo` struct (near `pub struct LibraryInfo` or where Phase 1 DTOs live — search for `LibraryInfo`). Add alongside:

```rust
/// IPC DTO returned by `list_discovered_libraries`. Spec §6.1.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredLibraryInfo {
    /// Hex-encoded 16-byte library OwnerAddr (32 hex chars).
    pub library_addr: String,
    pub name: String,
    pub description: String,
    /// ISO-8601 string from `listed_at.wall_ms`. UI display only —
    /// callers MUST NOT use this for HLC ordering decisions.
    pub listed_at: String,
}
```

If `LibraryInfo` doesn't live in lib.rs but in `library_directory.rs`, put `DiscoveredLibraryInfo` next to it. Make it `pub use`'d from lib.rs if needed.

- [ ] **Step 2: Add `list_discovered_libraries` handler**

In `lib.rs`, find the existing `list_libraries` handler (around line 9687). Add immediately after it:

```rust
/// Sub-D Phase 2 (ZEB-279): list libraries the user has discovered via
/// the `harmony/discovery/library/announce` topic but has NOT yet
/// added. Filtered against `OwnerState.libraries` non-tombstoned
/// entries — once the user adds a discovered library, it disappears
/// from this list and appears in `list_libraries`.
#[tauri::command]
async fn list_discovered_libraries(
    state: tauri::State<'_, NodeState>,
) -> Result<Vec<DiscoveredLibraryInfo>, String> {
    let (crdt_state, library_directory) = {
        let g = state
            .lock()
            .map_err(|e| format!("node state lock poisoned: {e}"))?;
        let crdt_state = g
            .owner_state
            .as_ref()
            .ok_or("owner_state missing — node not running?")?
            .clone();
        let dir = g
            .library_directory
            .clone()
            .ok_or("library_directory missing — node not running?")?;
        (crdt_state, dir)
    };

    let already_added: std::collections::BTreeSet<crate::owner_state_types::OwnerAddr> = {
        let crdt_g = crdt_state.lock().await;
        crdt_g
            .libraries
            .iter()
            .filter(|(_, entry)| entry.is_effective())
            .map(|(addr, _)| *addr)
            .collect()
    };

    let announces_g = library_directory.announces.lock().await;
    let snapshot = announces_g.snapshot();
    drop(announces_g);

    let mut out: Vec<DiscoveredLibraryInfo> = Vec::with_capacity(snapshot.len());
    for ann in snapshot {
        // Derive library_addr from the signed identity bundle. This
        // re-runs the Identity::from_public_bytes parse, but only on
        // already-verified records (`process_announce` ran them through
        // verify_announce first). The cost is one Blake3 hash per
        // discovered library at IPC call time — negligible at ≤1000
        // entries.
        let identity = match harmony_identity::Identity::from_public_bytes(
            &ann.library_identity_pub,
        ) {
            Ok(i) => i,
            Err(_) => continue, // shouldn't happen — verify_announce already validated
        };
        let addr = crate::owner_state_types::OwnerAddr(identity.address_hash);
        if already_added.contains(&addr) {
            continue;
        }
        out.push(DiscoveredLibraryInfo {
            library_addr: hex::encode(addr.0),
            name: ann.name,
            description: ann.description,
            listed_at: format_hlc_wall_ms_iso8601(ann.listed_at.wall_ms),
        });
    }
    Ok(out)
}
```

- [ ] **Step 3: Verify `format_hlc_wall_ms_iso8601` helper exists**

Search:
```bash
grep -n 'fn format_hlc_wall_ms_iso8601\|format_wall_ms_iso\|wall_ms_to_iso' src-tauri/src/lib.rs src-tauri/src/owner_state_types.rs
```

If a similarly-named helper exists, use it. If not, add this near the top of `lib.rs`:

```rust
fn format_hlc_wall_ms_iso8601(wall_ms: u64) -> String {
    use chrono::{DateTime, Utc};
    DateTime::<Utc>::from_timestamp_millis(wall_ms as i64)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("wall_ms={wall_ms}"))
}
```

Confirm `chrono` is in `Cargo.toml`:
```bash
grep '^chrono' src-tauri/Cargo.toml
```

If not, the simpler fallback is to emit the wall_ms as a string and let the frontend format it. In that case:

```rust
listed_at: ann.listed_at.wall_ms.to_string(),
```

…and the frontend renders it. Pick whichever path keeps the dependency surface unchanged.

- [ ] **Step 4: Register `list_discovered_libraries` in `tauri::generate_handler!`**

In `lib.rs`, find the `tauri::generate_handler![` block (around line 12028). Locate `list_libraries,` in the list and add a sibling line below:

```rust
            list_libraries,
            list_discovered_libraries,  // ZEB-279 Sub-D Phase 2
            add_library,
            remove_library,
            browse_library,
```

ALSO register in the SECOND generate_handler block if there is one (around line 12124 — the test-only or alternate-entry builder). Search:
```bash
grep -n 'generate_handler' src-tauri/src/lib.rs
```

Mirror the registration in any other handler block that lists `list_libraries`.

- [ ] **Step 5: Add IPC-shaped integration tests**

Append to `src-tauri/tests/library_announce_integration.rs`:

```rust
#[tokio::test]
#[cfg(feature = "test-fixtures")]
async fn announce_filter_omits_already_added_libraries() {
    use harmony_app::owner_state_crdt::OwnerState;
    use harmony_app::owner_state_types::{Hlc, LibraryEntry, OwnerAddr};

    let (dir, _rx) = LibraryDirectory::new();
    let (bytes, addr) = common::library_fixtures::mock_library_announce(
        [10u8; 32],
        "Already added",
        "",
        100,
    );
    dir.process_announce(bytes).await.expect("ingest ok");

    // Simulate the owner having already added this library via Phase 1.
    let mut crdt = OwnerState::default();
    crdt.libraries.insert(
        addr,
        LibraryEntry {
            address: addr,
            added_at: Hlc { wall_ms: 50, logical: 0, device_id: 1 },
            removed_at: None,
        },
    );

    // Replicate the filter logic from the IPC handler:
    let already_added: std::collections::BTreeSet<OwnerAddr> = crdt
        .libraries
        .iter()
        .filter(|(_, e)| e.is_effective())
        .map(|(a, _)| *a)
        .collect();
    let snap = dir.announces.lock().await.snapshot();
    let filtered: Vec<_> = snap
        .iter()
        .filter(|ann| {
            let id = harmony_identity::Identity::from_public_bytes(&ann.library_identity_pub)
                .expect("ident");
            !already_added.contains(&OwnerAddr(id.address_hash))
        })
        .collect();
    assert!(filtered.is_empty(), "already-added library must be filtered out");
}
```

- [ ] **Step 6: Run the new test**

```bash
cd src-tauri && cargo nextest run --locked --features test-fixtures --test library_announce_integration announce_filter_omits_already_added
```

Expected: 1 passed.

- [ ] **Step 7: `cargo fmt` + `cargo clippy` + full `cargo nextest`**

```bash
cd src-tauri && cargo fmt --all
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: all gates green.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/tests/library_announce_integration.rs
git commit -m "$(cat <<'EOF'
feat(zeb-279): Task 4 — list_discovered_libraries IPC + filter

New Tauri command returns Vec<DiscoveredLibraryInfo> (camelCase JSON
serde for JS consumption). Filter at IPC layer excludes any library
already in OwnerState.libraries as non-tombstoned — so newly-added
libraries seamlessly migrate from the "Discovered" panel to the
"Your libraries" panel on the next refetch.

DiscoveredLibraryInfo DTO derives library_addr from the signed
library_identity_pub at IPC call time (process_announce already
validated; re-derive is a single Blake3 hash per entry, negligible
at ≤1000 entries).

Test asserts the filter: ingest one announce, simulate adding it
to OwnerState.libraries, replicate the filter logic, confirm the
filtered list is empty.

EOF
)"
```

Status: DONE.

---

## Task 5: Frontend — service wrapper + LibraryDirectoryBrowser collapsible section

**Files:**
- Modify: `src/lib/library-directory-service.ts` (add `listDiscoveredLibraries`)
- Modify: `src/lib/components/LibraryDirectoryBrowser.svelte` (add collapsible Discovered section)
- Modify: `src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts` (add vitest cases)

- [ ] **Step 1: Add `listDiscoveredLibraries` to the service wrapper**

Open `src/lib/library-directory-service.ts`. Find the existing `listLibraries` export. Add alongside:

```typescript
export interface DiscoveredLibraryInfo {
  libraryAddr: string;
  name: string;
  description: string;
  listedAt: string;
}

export async function listDiscoveredLibraries(): Promise<DiscoveredLibraryInfo[]> {
  return await invoke('list_discovered_libraries');
}
```

Make sure `invoke` is already imported at the top of the file (it should be — used by `listLibraries`).

- [ ] **Step 2: Extend `LibraryDirectoryBrowser.svelte`**

Open `src/lib/components/LibraryDirectoryBrowser.svelte`.

In the `<script lang="ts">` section, ADD:

```typescript
import { listDiscoveredLibraries, type DiscoveredLibraryInfo } from '../library-directory-service';

let discovered: DiscoveredLibraryInfo[] = $state([]);
let discoveredOpen: boolean = $state(false);
let addingDiscovered: Record<string, boolean> = $state({});
let discoveredError: Record<string, string> = $state({});
```

Find the existing `refetch` function (which calls `listLibraries`). Extend it to ALSO call `listDiscoveredLibraries`:

```typescript
async function refetch() {
  // existing: libraries = await listLibraries();
  // existing: errors etc.
  // ADD:
  try {
    discovered = await listDiscoveredLibraries();
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    console.warn('listDiscoveredLibraries failed:', msg);
    discovered = [];
  }
  // Auto-expand section when non-empty.
  if (discovered.length > 0 && !discoveredOpen) {
    discoveredOpen = true;
  }
}
```

Add an `addDiscovered` handler:

```typescript
async function addDiscovered(libraryAddr: string) {
  addingDiscovered[libraryAddr] = true;
  discoveredError[libraryAddr] = '';
  try {
    await addLibrary(libraryAddr);
    // refetch is triggered by the library-directory-updated event listener,
    // but call explicitly here as a belt-and-suspenders for cases where the
    // event hasn't propagated yet.
    await refetch();
  } catch (e) {
    discoveredError[libraryAddr] = e instanceof Error ? e.message : String(e);
  } finally {
    addingDiscovered[libraryAddr] = false;
  }
}
```

In the template, find the section just before the "Add library manually" button (or wherever fits the §7.1 wireframe). INSERT:

```svelte
{#if discovered.length > 0 || libraries.length > 0}
  <section class="discovered-section">
    <button
      type="button"
      class="discovered-toggle"
      onclick={() => (discoveredOpen = !discoveredOpen)}
      aria-expanded={discoveredOpen}
    >
      {discoveredOpen ? '▼' : '▶'} Discovered libraries ({discovered.length})
    </button>
    {#if discoveredOpen}
      <ul class="discovered-list">
        {#each discovered as d (d.libraryAddr)}
          <li class="discovered-row">
            <div class="discovered-meta">
              <strong>{d.name}</strong>
              <span class="discovered-desc">{d.description}</span>
              <span class="discovered-addr">({d.libraryAddr.slice(0, 8)}…)</span>
            </div>
            <button
              type="button"
              onclick={() => addDiscovered(d.libraryAddr)}
              disabled={addingDiscovered[d.libraryAddr]}
            >
              {addingDiscovered[d.libraryAddr] ? 'Adding…' : 'Add'}
            </button>
            {#if discoveredError[d.libraryAddr]}
              <span class="discovered-error">{discoveredError[d.libraryAddr]}</span>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>
{/if}
```

Add minimal CSS at the bottom of the `<style>` block:

```svelte
.discovered-section { margin: 0.5rem 0; }
.discovered-toggle {
  background: transparent;
  border: none;
  cursor: pointer;
  font-weight: 600;
  padding: 0.25rem 0;
  width: 100%;
  text-align: left;
}
.discovered-list { list-style: none; padding: 0; margin: 0.25rem 0; }
.discovered-row {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  padding: 0.25rem 0;
  flex-wrap: wrap;
}
.discovered-meta { flex: 1; display: flex; flex-direction: column; }
.discovered-desc { font-size: 0.85em; opacity: 0.8; }
.discovered-addr { font-size: 0.75em; opacity: 0.6; font-family: monospace; }
.discovered-error { color: var(--color-error, #d33); font-size: 0.85em; width: 100%; }
```

- [ ] **Step 3: Run `tsc` and `vitest` to confirm compile**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: tsc clean; existing vitest tests still pass.

- [ ] **Step 4: Add vitest cases**

Open `src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts`. Find the imports + mock setup. Mock `listDiscoveredLibraries` similarly to `listLibraries`:

```typescript
vi.mock('../../library-directory-service', () => ({
  listLibraries: vi.fn(),
  listDiscoveredLibraries: vi.fn(),  // ADD
  addLibrary: vi.fn(),
  removeLibrary: vi.fn(),
  browseLibrary: vi.fn(),
}));
```

(Adapt to whatever the existing mock shape is — keep all existing exports + add `listDiscoveredLibraries`.)

Add new test cases at the bottom of the describe block:

```typescript
import { listDiscoveredLibraries, addLibrary } from '../../library-directory-service';

describe('Discovered libraries panel', () => {
  beforeEach(() => {
    vi.mocked(listDiscoveredLibraries).mockResolvedValue([]);
  });

  it('renders 0 discovered when none exist', async () => {
    vi.mocked(listLibraries).mockResolvedValue([
      { libraryAddr: 'aabbcc'.padEnd(32, '0'), addedAt: '2026-01-01' },
    ]);
    vi.mocked(listDiscoveredLibraries).mockResolvedValue([]);
    const { findByText } = render(LibraryDirectoryBrowser);
    expect(await findByText(/Discovered libraries \(0\)/)).toBeInTheDocument();
  });

  it('renders 3 discovered with their names + descriptions', async () => {
    vi.mocked(listLibraries).mockResolvedValue([]);
    vi.mocked(listDiscoveredLibraries).mockResolvedValue([
      { libraryAddr: '11'.padEnd(32, '0'), name: 'LibA', description: 'desc A', listedAt: '' },
      { libraryAddr: '22'.padEnd(32, '0'), name: 'LibB', description: 'desc B', listedAt: '' },
      { libraryAddr: '33'.padEnd(32, '0'), name: 'LibC', description: 'desc C', listedAt: '' },
    ]);
    const { findByText } = render(LibraryDirectoryBrowser);
    expect(await findByText(/Discovered libraries \(3\)/)).toBeInTheDocument();
    expect(await findByText('LibA')).toBeInTheDocument();
    expect(await findByText('LibB')).toBeInTheDocument();
    expect(await findByText('LibC')).toBeInTheDocument();
  });

  it('clicking Add invokes addLibrary with the correct libraryAddr', async () => {
    vi.mocked(listLibraries).mockResolvedValue([]);
    vi.mocked(listDiscoveredLibraries).mockResolvedValue([
      { libraryAddr: 'abcd'.padEnd(32, '0'), name: 'LibX', description: 'd', listedAt: '' },
    ]);
    vi.mocked(addLibrary).mockResolvedValue(undefined);

    const { findAllByText } = render(LibraryDirectoryBrowser);
    const addButtons = await findAllByText(/^Add$/);
    // Find the Add button inside the discovered row (not the manual-add).
    const discoveredAddBtn = addButtons.find((b) =>
      b.closest('.discovered-row') !== null,
    );
    expect(discoveredAddBtn).toBeDefined();
    await fireEvent.click(discoveredAddBtn!);
    expect(addLibrary).toHaveBeenCalledWith('abcd'.padEnd(32, '0'));
  });

  it('add failure surfaces inline error next to the row', async () => {
    vi.mocked(listLibraries).mockResolvedValue([]);
    vi.mocked(listDiscoveredLibraries).mockResolvedValue([
      { libraryAddr: 'fail'.padEnd(32, '0'), name: 'BadLib', description: '', listedAt: '' },
    ]);
    vi.mocked(addLibrary).mockRejectedValue(new Error('library add failed'));

    const { findByText } = render(LibraryDirectoryBrowser);
    const addBtn = await findByText(/^Add$/);
    await fireEvent.click(addBtn);
    expect(await findByText(/library add failed/)).toBeInTheDocument();
  });
});
```

**Adapt the imports + render helpers** to whatever shape the existing test file uses (`@testing-library/svelte`, `vitest`, etc.). Use the existing test patterns as the canonical reference.

- [ ] **Step 5: Run vitest**

```bash
npx vitest run --reporter=verbose src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts
```

Expected: existing + 4 new tests all pass.

- [ ] **Step 6: Run all frontend gates**

```bash
npx tsc --noEmit
npx vitest run
```

Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add src/lib/library-directory-service.ts src/lib/components/LibraryDirectoryBrowser.svelte src/lib/components/__tests__/LibraryDirectoryBrowser.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-279): Task 5 — frontend Discovered libraries panel

LibraryDirectoryBrowser gains a collapsible "Discovered libraries (N)"
section between "Your libraries" and the manual-add affordance.
Auto-expanded when non-empty, click-toggleable. Each row: name (bold),
description (subdued), short addr suffix, Add button. Add invokes
existing Phase 1 addLibrary IPC; on success, refetch removes the row
from the discovered list via the IPC-layer filter (Task 4).

library-directory-service.ts adds listDiscoveredLibraries() wrapper
+ DiscoveredLibraryInfo type definition.

Add failures surface inline next to the row (mirrors Phase 1
removeError placement). Per-row spinner state prevents double-click.

Vitest extensions: empty state shows "(0)", populated state renders
names/descs, click-Add invokes addLibrary with correct addr, add
failure surfaces inline error.

EOF
)"
```

Status: DONE.

---

## Task 6: Final verification + push + PR creation

**Files:** none (verification + push only).

- [ ] **Step 1: Run all 5 gates from a clean state**

```bash
cd src-tauri && cargo fmt --all -- --check
cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures
cd src-tauri && cargo check --locked --all-targets --features test-fixtures
cd ..
npx tsc --noEmit
npx vitest run
```

Expected: ALL 5 gates green. Report test counts.

- [ ] **Step 2: Re-fetch latest origin/main and confirm no race**

```bash
git fetch origin
git log origin/main..HEAD --oneline
git log HEAD..origin/main --oneline
```

The first should list the 5 task commits (plus the spec commit). The second should be EMPTY (no upstream commits we'd be missing). If the second is non-empty, the user has merged something else since branch-cut — STOP, report to controller, do not push.

- [ ] **Step 3: Push the branch**

```bash
git push -u origin zeb-279-sub-d-phase-2-library-auto-discovery
```

Expected: push successful, branch tracking origin.

- [ ] **Step 4: Create the PR**

```bash
gh pr create --title "Resolves ZEB-279 — Sub-D Phase 2 library auto-discovery" --body "$(cat <<'EOF'
## Summary

Ships the Sub-D Phase 2 vertical slice: clients subscribe to a single global `harmony/discovery/library/announce` Zenoh topic, libraries self-announce via signed `LibraryAnnounce` records, and the `LibraryDirectoryBrowser` gains an inline collapsible "Discovered libraries" section. Add-from-discovered reuses Phase 1's `addLibrary` IPC — no new join path.

Resolves ZEB-279.

Cross-refs:
- Parent epic: [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) — Sub-D library-federated discovery (Phase 1 shipped via #108)
- Sibling phases (NOT in scope this round): [ZEB-280](https://linear.app/zeblith/issue/ZEB-280/) (Phase 3 federated republication), [ZEB-281](https://linear.app/zeblith/issue/ZEB-281/) (Phase 4 ProfileMembershipBroadcast), [ZEB-252](https://linear.app/zeblith/issue/ZEB-252/) (Phase 6 direct-join IPC)
- Phase 1 predecessor: [#108](https://github.com/zeblithic/harmony-client/pull/108)

## What's in this PR

**Backend (Rust)**

- `library_directory.rs`: new `LibraryAnnounce` wire type (2-char CBOR keys: `ai`/`nm`/`ds`/`la`/`ls`), `AnnounceVerifyError`, `verify_announce` (bounds + identity parse + Ed25519 sig over canonical CBOR with `ls` zeroed), `Announces` map (`BTreeMap<OwnerAddr, LibraryAnnounce>`), `process_announce` async method.
- `event_loop.rs`: permanent Zenoh subscriber spawned at startup alongside Phase 1's per-library subscriber, subscribing to `harmony/discovery/library/announce` exact-key. Emits `library-directory-updated` on Inserted/Updated/cap-eviction.
- `lib.rs`: new `list_discovered_libraries` IPC + `DiscoveredLibraryInfo` DTO. Filter excludes any library already in `OwnerState.libraries` non-tombstoned — added libraries seamlessly migrate from "Discovered" to "Your libraries" on the next refetch.

**Frontend (Svelte 5)**

- `library-directory-service.ts`: new `listDiscoveredLibraries()` wrapper + `DiscoveredLibraryInfo` type.
- `LibraryDirectoryBrowser.svelte`: collapsible "Discovered libraries (N)" section auto-expanded when N>0. Each row: name + description + short addr + Add button. Click Add → existing Phase 1 `addLibrary` → refetch removes the row via the IPC filter.

**Tests**

- `tests/wire_format_library_announce_fixtures.rs` (new): 4 tests — round-trip, verify-after-sign, 2-char-key audit via `ciborium::Value::Map`, pinned bytes prefix.
- `tests/library_announce_integration.rs` (new): 7 integration tests — ingest, dedupe latest-listed_at-wins, older-listed_at-dropped, invalid-sig-rejected, name-too-long-rejected, cap-eviction, already-added filter.
- `library_directory.rs::announce_tests` (in-module): 5 unit tests covering `Announces::on_announce` semantics.
- `LibraryDirectoryBrowser.test.ts`: 4 new vitest cases covering the discovered panel.

## Design

Spec: [`docs/specs/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-design.md`](./docs/specs/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-design.md) (`9e109ad`).

Plan: [`docs/plans/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-plan.md`](./docs/plans/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-plan.md).

**Key invariants:**
- Library-signed announce records (Ed25519 over canonical CBOR with `ls` zeroed). Prevents identity hijack at announce time; sets up Phase 3 to reuse the same signing key.
- Identity bundle is the source of truth: `library_addr` derives from `library_identity_pub`, not carried on the wire. No way for the addr to disagree with the signed identity.
- User explicit-add gate preserved: discovered libraries do nothing until the user clicks Add. Auto-add would violate Phase 1's paste-an-address-only trust model.
- In-memory only: discovered set rebuilt on every startup from the subscription firehose. Cross-device replication via every device subscribing the same global topic ("loose" — converges as fresh announces arrive). Strong CRDT replication deferred (see spec §12 row 4).
- Cap: `MAX_DISCOVERED_LIBRARIES = 1_000`, evict oldest-by-`listed_at` on overflow.

## Deferred follow-ups (not filed as Linear tickets this round)

Per spec §12:

1. Persistent dismiss-list (LWW CRDT replicated through Phase 1 owner-state sync).
2. TTL / re-announce-or-evict.
3. Per-source-identity anti-spam quotas.
4. Strong CRDT replication of the discovered set itself.

File later only on demand.

## Test plan

- [ ] CI: rust-check (fmt + clippy) green
- [ ] CI: rust-test (nextest workspace) green
- [ ] CI: msrv green
- [ ] CI: frontend (tsc + vitest) green
- [ ] Manual: launch app, confirm `LibraryDirectoryBrowser` renders "Discovered libraries (0)" header when no announces in flight
- [ ] Manual: future, after a mock library starts publishing announces — confirm the panel populates and Add migrates the entry to "Your libraries"

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Capture the PR URL.

- [ ] **Step 5: Verify PR created cleanly**

```bash
gh pr view --json url,number,title,state
```

Expected: PR is OPEN, title matches, number is assigned.

- [ ] **Step 6: Report to controller**

Report:
- PR URL: `<url>`
- PR number: `#<N>`
- Tip commit SHA: `<sha>`
- All 5 gates green pre-push
- Branch pushed; ready for CI + bot review

Status: DONE. STOP here. The calling agent enters the autonomous PR monitoring loop.

---

## Self-review checklist

Before dispatching this plan to implementer subagents, the controller MUST verify:

- [ ] Spec coverage: every spec section (§1–§14) is addressed by at least one task above.
- [ ] No placeholders: search this file for "TBD", "TODO", "fill in later" — should find none.
- [ ] Type consistency: `LibraryAnnounce` field types match across Task 1 (definition), Task 2 (`Announces::on_announce` signature), Task 3 (mock fixture builder), Task 4 (IPC DTO conversion).
- [ ] Code shape consistency with Phase 1: `verify_announce` mirrors `verify_entry`, `process_announce` mirrors `process_sample`, `AnnounceOutcome` mirrors `OnEntryOutcome`, the event_loop wiring mirrors the per-library subscriber spawn.
- [ ] CI gate parity: every implementation task ends with `cargo fmt + cargo clippy + cargo nextest` (Rust changes) or `tsc + vitest` (frontend changes); Task 6 re-runs all 5 gates.
- [ ] Linear ID correctness: PR body uses `Resolves ZEB-279` (bare ref = auto-close) and markdown-linked refs for ZEB-218 / ZEB-280 / ZEB-281 / ZEB-252 (no auto-close).
- [ ] No worktree mentions: per HARD RULE memory, all task steps use `git checkout` in the main repo.

If any check fails, fix this plan before dispatching.
