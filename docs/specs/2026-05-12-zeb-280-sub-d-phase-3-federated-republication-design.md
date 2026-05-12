# ZEB-280 Sub-D Phase 3 — Federated Republication of Directory Entries

**Status:** Design approved 2026-05-12.

**Parent:** [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) Sub-D library-federated discovery directory.

**Predecessors:**
- Phase 1 vertical slice: PR [#108](https://github.com/zeblithic/harmony-client/pull/108), spec `docs/specs/2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md`.
- Phase 2 auto-discovery: PR [#109](https://github.com/zeblithic/harmony-client/pull/109), spec `docs/specs/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-design.md`.

## 1. Goal

Add a second cryptographic attestation layer to `LibraryDirectoryEntry` so consumers can verify which library *broadcast* a given entry, independently of the community admin's signature that authenticates the listing's *content*. This unlocks verifiable cross-library re-syndication: library A can rebroadcast library B's catalog entries by adding its own wrapping signature, and consumers can verify both layers cryptographically.

Entries whose wrapping signature is present but fails verification are surfaced with an "unattested" badge per the Phase 1 spec §486-489 contract — the community admin's signature is the trust anchor for content, so a bad wrapping sig degrades transport-integrity confidence without invalidating the listing itself.

## 2. Why this shape (not the full Sub-D scope)

Phase 1 shipped a manual paste-an-address library trust model with single-signature entries (community admin only). Phase 2 added auto-discovery via the `harmony/discovery/library/announce` topic. Phase 3 is the third of the four Sub-D phases, deferring:

- ProfileMembershipBroadcast → Phase 4 ([ZEB-281](https://linear.app/zeblith/issue/ZEB-281/))
- Direct-join IPC bypassing redeem_invite → Phase 6 ([ZEB-252](https://linear.app/zeblith/issue/ZEB-252/) rewrite)

The Phase 3 design is intentionally wire-compatible with Phase 1: both new wrapping fields are Optional and use `skip_serializing_if = "Option::is_none"`, so Phase 1 entries' canonical CBOR encoding stays byte-identical. Phase 1 wire-format pinning fixtures must continue to match without regeneration.

## 3. Architecture overview

```
┌────────────────────────────────────────────────────────────────┐
│ Library X (publisher)                                          │
│                                                                │
│  1. Receive admin-signed entry from community admin K          │
│     (community_signature = K's sig over Phase-1 fields with    │
│      cs=[0;64], li=None, ls=None)                              │
│                                                                │
│  2. Set library_identity_pub = Some(X's 64-byte bundle)        │
│                                                                │
│  3. Wrapping sign over canonical_cbor_encode(entry with        │
│     ls=None, li=Some(X's pub), cs=Some(K's sig))               │
│     → library_signature = Some(X's sig)                        │
│                                                                │
│  4. Publish on harmony/discovery/library/{X_addr}/communities  │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼  (Zenoh)
┌────────────────────────────────────────────────────────────────┐
│ Consumer (harmony-client)                                      │
│                                                                │
│  library_directory::verify_entry(entry) →                      │
│    1. Verify admin sig (zero cs, li, ls); drop on failure      │
│    2. Verify wrapping sig (zero ls only); flag unattested      │
│       if present-but-invalid                                   │
│    3. Return AttestationStatus::{Unwrapped, Attested, Unattested}│
│                                                                │
│  library_directory::Aggregation::on_entry(entry, status) →     │
│    Insert into attested_by or unattested_by set per status     │
│                                                                │
│  list_libraries / browse_library IPCs → DirectoryEntryDTO      │
│    {unattested: bool, listed_by_count: usize, ...}             │
│                                                                │
│  LibraryDirectoryBrowser.svelte                                │
│    Inline ⚠ Unattested badge on rows where unattested = true   │
└────────────────────────────────────────────────────────────────┘
```

The protocol is consumer-side only. No library-to-library traffic: each library independently obtains admin-signed entries (Phase 1 path), wraps with its own signature, broadcasts on its own topic. "Federation" is in the consumer-side aggregation, where the same admin-signed entry can arrive from multiple library topics, each carrying that library's wrapping signature.

## 4. Data model

### 4.1 Wire format — `LibraryDirectoryEntry` extension

The Phase 1 fields are unchanged. Two new Optional fields are appended:

```rust
pub struct LibraryDirectoryEntry {
    // ... Phase 1 fields (cd, ai, nm, ds, tp, iu, lb, la, cs) ...

    /// 64-byte identity bundle (X25519_pub || Ed25519_pub) of the
    /// broadcasting library. None for unwrapped (Phase 1-style) entries.
    /// Same bundle shape as `community_admin_identity_pub`, so the
    /// `Identity::from_public_bytes` validation path is shared.
    #[serde(
        rename = "li",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_bytes_as_bstr",
        deserialize_with = "deserialize_optional_bytes_from_bstr",
    )]
    pub library_identity_pub: Option<[u8; 64]>,

    /// Ed25519 wrapping signature from the broadcasting library over
    /// the canonical CBOR encoding of all fields with `library_signature`
    /// zeroed (analogous to Phase 1's `community_signature`). None for
    /// unwrapped entries.
    #[serde(
        rename = "ls",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_bytes_as_bstr",
        deserialize_with = "deserialize_optional_bytes_from_bstr",
    )]
    pub library_signature: Option<[u8; 64]>,
}
```

**Field-key choices:** `li` and `ls` are 2-char (preserves `canonical_cbor_encode`'s same-length-keys precondition) and distinct from existing keys `{cd, ai, nm, ds, tp, iu, lb, la, cs}`.

**Wire compatibility:** `skip_serializing_if = "Option::is_none"` omits the keys from the canonical CBOR map when None. A Phase 1 entry's canonical encoding is byte-identical regardless of which client version produced it. Existing Phase 1 wire-format pinning fixtures **MUST continue to match without regeneration**.

**Helper additions:** Two new helpers in `owner_state_types.rs`:

```rust
pub fn serialize_optional_bytes_as_bstr<S>(
    value: &Option<[u8; 64]>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer;

pub fn deserialize_optional_bytes_from_bstr<'de, D>(
    deserializer: D,
) -> Result<Option<[u8; 64]>, D::Error>
where
    D: serde::Deserializer<'de>;
```

These follow the existing `serialize_bytes_as_bstr` / `deserialize_bytes_from_bstr` pattern but for `Option<[u8; 64]>`. The `None` case is handled by `skip_serializing_if` on the field — these helpers only handle the `Some` case (analogous to the non-Option variants).

### 4.2 Attestation status — `AttestationStatus`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationStatus {
    /// Phase 1-style entry: no wrapping sig present. Implicit trust
    /// from subscription topic — entries arriving from library X's
    /// topic are treated as if X attested to them.
    Unwrapped,
    /// Phase 3: wrapping sig present and verified. `OwnerAddr` is the
    /// broadcasting library's derived address (from
    /// `library_identity_pub`).
    Attested(OwnerAddr),
    /// Phase 3: wrapping sig present but invalid. Entry is still
    /// surfaced — the community admin's signature is the trust anchor
    /// for content. UI flags the row with the "unattested" badge.
    Unattested(OwnerAddr),
}
```

The `Unattested` variant retains the broadcasting library's claimed `OwnerAddr` (derived from the *signed* `library_identity_pub` value before verification was attempted). This lets the aggregation tag *which* library's broadcast failed verification, which feeds the per-library cap counting and the eventual remove-library sweep.

### 4.3 Aggregation evolution — `AggregatedEntry`

The Phase 1 `listed_by: BTreeSet<OwnerAddr>` field is replaced by two sets that track attestation outcomes:

```rust
pub struct AggregatedEntry {
    pub entry: LibraryDirectoryEntry,

    /// Libraries whose broadcast of this community verified (or whose
    /// unwrapped broadcast we trust implicitly via subscription topic).
    /// Per-receive insertion rule:
    ///   AttestationStatus::Attested(lib_addr) → insert(lib_addr)
    ///   AttestationStatus::Unwrapped          → insert(entry.listed_by)
    pub attested_by: BTreeSet<OwnerAddr>,

    /// Libraries whose broadcast of this community had a wrapping sig
    /// that failed verification. Per-receive insertion rule:
    ///   AttestationStatus::Unattested(lib_addr) → insert(lib_addr)
    /// Drives the "unattested" UI badge:
    ///   unattested = !unattested_by.is_empty()
    pub unattested_by: BTreeSet<OwnerAddr>,
}
```

**Set-merge invariants:**
- Both sets use the standard `BTreeSet::insert` — idempotent and commutative across multiple receives.
- A library can appear in **both** sets concurrently (e.g., library A broadcasts the same community twice — once with a valid wrapping sig, once with a tampered one). Both contributions are tracked.
- Set unions across receives never need conflict resolution: if A's first broadcast attests and A's second tampers, both are surfaced (attested AND unattested for A) — the UI will show the badge because `unattested_by` is non-empty, which is the conservative correct answer.

**`Aggregation::on_entry` signature change:**

```rust
// Phase 1:
pub fn on_entry(&mut self, entry: LibraryDirectoryEntry) -> ProcessResult;

// Phase 3:
pub fn on_entry(
    &mut self,
    entry: LibraryDirectoryEntry,
    status: AttestationStatus,
) -> ProcessResult;
```

Callers pass the `AttestationStatus` returned by `verify_entry` so the aggregation knows which set to populate.

**Per-library cap (`MAX_ENTRIES_PER_LIBRARY = 10_000`):** counts ALL contributions from a library (attested + unattested + Unwrapped's fallback `listed_by`). A misbehaving library cannot bypass the cap by tampering its own sigs.

**`drop_library(library)` evolution:** sweeps the library from both `attested_by` and `unattested_by` sets. Per-library count decrements normally. Existing R1/R2 fixes to `drop_library` (source-matches eviction + counter rollback) apply unchanged to both sets.

### 4.4 Frontend DTO — `DirectoryEntryDTO` extension

```rust
pub struct DirectoryEntryDTO {
    // ... existing Phase 1 fields ...
    pub listed_by_count: usize,       // = attested_by.len()
    pub unattested: bool,             // = !unattested_by.is_empty()
}
```

**Semantic shift for `listed_by_count`:** in Phase 1, this counted `listed_by` set entries (admin-signed lister field). In Phase 3, it counts `attested_by` set entries (broadcasting libraries whose sig verified, plus Phase 1 fallback). This change is intentional — the user-visible "Listed by N libraries" now reflects "N of my trusted libraries actively vouched for this community", which is the more useful UX in a federated world.

**`unattested: bool`** is a simple derived boolean — the frontend doesn't need per-library attestation breakdown for v1 (deferred follow-up).

Wire-key style on this DTO is snake_case (matches Phase 1's `DirectoryEntryDTO`; differs from Phase 2's camelCase `DiscoveredLibraryInfo` which was a deliberate choice for the discovery surface).

## 5. Verification path

`verify_entry` extends to return `Result<AttestationStatus, EntryVerifyError>`:

```rust
pub fn verify_entry(
    entry: &LibraryDirectoryEntry,
) -> Result<AttestationStatus, EntryVerifyError> {
    // 1. Bounds (unchanged from Phase 1)
    // 2. Parse community_admin_identity_pub (unchanged)
    // 3. Verify community admin sig:
    //    clone, set cs=[0;64], li=None, ls=None, encode, verify
    //    → drop entry on failure (returns EntryVerifyError::SignatureInvalid)
    // 4. Invite-URL discipline + payload binding (unchanged from Phase 1)
    //
    // 5. NEW — wrapping sig check:
    match (&entry.library_signature, &entry.library_identity_pub) {
        (None, None) => Ok(AttestationStatus::Unwrapped),
        (Some(_), None) | (None, Some(_)) => {
            Err(EntryVerifyError::LibrarySignatureFieldsInconsistent)
        }
        (Some(lib_sig), Some(lib_pub)) => {
            let lib_identity = harmony_identity::Identity::from_public_bytes(lib_pub)
                .map_err(|e| {
                    EntryVerifyError::InvalidLibraryIdentityPub(format!("{e:?}"))
                })?;
            let lib_addr = OwnerAddr(lib_identity.address_hash);

            // Reconstruct sign-time bytes: zero ls, keep cs + li populated.
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

**Key properties:**

1. **Admin sig is the gatekeeper.** If the admin sig fails, `verify_entry` returns `Err`, the entry is dropped. The wrapping-sig check only runs on entries whose content has already been authenticated.

2. **Admin sig portability.** Admin signs over Phase 1 fields with cs/li/ls zeroed/absent. The `skip_serializing_if` invariant means the canonical CBOR is byte-identical whether the library later sets li/ls or not. Any library can wrap any admin-signed entry.

3. **Wrapping sig commits to the admin sig.** Library signs over (ls=None, li=Some, cs=Some(admin_sig), Phase 1 fields). A library cannot wrap a different admin's signature without invalidating its own wrapping.

4. **Inconsistent-field reject.** `(Some, None)` or `(None, Some)` returns an error — cannot verify wrapping without both fields, cannot have a pubkey without a sig. This is a malformed wire and should not be silently treated as Unwrapped (which would hide a publisher bug).

5. **Tampered-wrapping is `Ok(Unattested)`, not `Err`.** The entry is still surfaced to the UI; only the badge tells the user.

**New `EntryVerifyError` variants:**

```rust
#[error("library_signature and library_identity_pub must both be Some or both be None")]
LibrarySignatureFieldsInconsistent,

#[error("malformed library identity_pub: {0}")]
InvalidLibraryIdentityPub(String),
```

## 6. IPC surface

**No new IPCs.** Federation is fully transparent to the IPC layer:

- `list_libraries` — unchanged.
- `add_library` / `remove_library` — unchanged.
- `browse_library` — unchanged signature, but returned `DirectoryEntryDTO[]` now carries `unattested: bool` (additive — frontend can ignore until the badge is wired).
- `library-directory-updated` event — unchanged firing semantics. Phase 3 broadcasting events fire on receive identical to Phase 1.

## 7. Frontend

### 7.1 Service-layer change

`src/lib/library-directory-service.ts` — `DirectoryEntry` interface gains:

```typescript
export interface DirectoryEntry {
  // ... existing fields ...
  listed_by_count: number;
  unattested: boolean;
}
```

### 7.2 Browser component — `LibraryDirectoryBrowser.svelte`

Add an inline badge on each entry row when `entry.unattested === true`:

```svelte
{#if entry.unattested}
  <span
    class="unattested-badge"
    aria-label="One or more libraries' wrapping signatures failed to verify for this entry"
    title="Unattested: at least one broadcasting library's signature failed to verify. The community admin's signature is still valid; the listing's content is trustworthy."
  >
    ⚠ Unattested
  </span>
{/if}
```

**Styling:**
- Subtle amber/yellow background; **NOT** red (this isn't a critical error — admin sig is valid).
- Inline with the community name in the entry row header, not in a separate row.
- `aria-label` describes the meaning; `title` provides hover detail.

**No `--features test-fixtures` impact on frontend tests:** vitest doesn't touch Cargo features. Only Rust-side tests pin wire formats.

## 8. Cross-version compatibility

| Producer | Consumer | Behavior |
|---|---|---|
| Phase 1 (no wrapping fields) | Phase 1 | Works (current state). |
| Phase 1 (no wrapping fields) | Phase 3 | Works. `verify_entry` returns `Unwrapped`; aggregation uses `attested_by.insert(entry.listed_by)` fallback. No badge. |
| Phase 3 (with wrapping) | Phase 1 | Works. Ciborium ignores unknown fields by default (verified — no `deny_unknown_fields` on Phase 1 struct). Phase 1 admin-sig verification still passes because `skip_serializing_if` means li/ls aren't in the canonical CBOR when None — Phase 1 admin signed over identical bytes. |
| Phase 3 (with wrapping) | Phase 3 | Full Phase 3 path: admin sig verified, wrapping sig verified, attested. |
| Phase 3 (with tampered wrapping) | Phase 3 | `Unattested` path: entry surfaced, badge visible. |

**Critical wire invariant:** Phase 1 fixtures' pinned bytes in `tests/wire_format_library_directory_fixtures.rs` **MUST not change**. If they do, it's a regression in the `skip_serializing_if` invariant and should fail CI.

## 9. Error handling

| Scenario | Verifier outcome | Aggregation outcome | UI |
|---|---|---|---|
| Admin sig invalid | `Err(SignatureInvalid)` | Entry dropped (not inserted) | Entry not shown |
| Wrapping sig invalid | `Ok(Unattested(addr))` | `unattested_by.insert(addr)` | Entry shown + badge |
| Wrapping sig valid | `Ok(Attested(addr))` | `attested_by.insert(addr)` | Entry shown, no badge |
| No wrapping fields | `Ok(Unwrapped)` | `attested_by.insert(entry.listed_by)` | Entry shown, no badge |
| Only one of (lib_sig, lib_pub) Some | `Err(LibrarySignatureFieldsInconsistent)` | Entry dropped | Entry not shown |
| Malformed lib_identity_pub | `Err(InvalidLibraryIdentityPub(...))` | Entry dropped | Entry not shown |
| Encode failure on sign-time bytes | `Err(Encode)` (existing variant) | Entry dropped | Entry not shown |

The verifier-vs-aggregation split is preserved from Phase 1: `verify_entry` makes only crypto/content decisions; the aggregation handles insertion outcomes (Inserted / Replaced / AccretedListedBy / Idempotent) orthogonal to attestation.

## 10. Performance / scale

- **Verification cost:** one additional Ed25519 verify per entry when wrapped. Ed25519 verify ≈ 50 µs on commodity hardware. At the 100 k-entry cold-startup worst case (10 libraries × `MAX_ENTRIES_PER_LIBRARY` = 10 k each), this adds ≈ 5 s of CPU work above Phase 1's 5 s — total cold-startup verification still well under perceptible budget.
- **Aggregation state size:** two sets (`attested_by`, `unattested_by`) per `AggregatedEntry` instead of one. `BTreeSet<OwnerAddr>` is 16 bytes per entry plus tree overhead — negligible at our scale.
- **No change to `MAX_ENTRIES_PER_LIBRARY = 10_000`** or any other cap.
- **Wire-size increase:** Phase 3-wrapped entries are ~134 bytes larger than Phase 1 (64 bytes lib_sig + 64 bytes lib_pub + ~6 bytes CBOR framing). At 10 k entries × 10 libraries = 100 k entries × 134 bytes = ~13 MB of additional cold-startup bandwidth worst-case. Acceptable.

## 11. Testing

### 11.1 Test fixture additions — `tests/common/library_fixtures.rs`

```rust
/// Phase 3 helper: sign an entry with both layers.
/// Pass `library_signer = None` for Phase 1-shaped (unwrapped) output.
pub fn mock_library_entry_wrapped(
    community_admin_signer: &SigningKey,
    community_admin_identity_bundle: [u8; 64],
    community_id: SpaceId,
    name: &str,
    description: &str,
    topics: Vec<String>,
    listed_by: OwnerAddr,
    listed_at: Hlc,
    library_signer: Option<(&SigningKey, [u8; 64])>,
) -> LibraryDirectoryEntry;

/// Helper: produce a "republished" entry — same admin-signed bytes,
/// different broadcasting library's wrapping sig. Tests verbatim
/// cross-library federation in the consumer-side aggregation.
pub fn mock_library_entry_republished_by(
    original: &LibraryDirectoryEntry,
    new_library_signer: &SigningKey,
    new_library_identity_bundle: [u8; 64],
) -> LibraryDirectoryEntry;
```

### 11.2 Unit tests — `library_directory.rs::tests`

| Test | What it pins |
|---|---|
| `verify_entry_phase1_unwrapped_returns_unwrapped` | `AttestationStatus::Unwrapped` for `(None, None)` entry |
| `verify_entry_phase3_wrapped_valid_returns_attested` | `AttestationStatus::Attested(addr)` for valid wrapping; `addr` matches library_identity_pub's derived address |
| `verify_entry_phase3_tampered_wrapping_sig_returns_unattested` | `Ok(Unattested(addr))` for tampered ls; entry is NOT dropped |
| `verify_entry_phase3_tampered_payload_invalidates_both_sigs` | Tampered name field → admin sig fails first → `Err(SignatureInvalid)`; entry dropped (admin sig is gatekeeper) |
| `verify_entry_inconsistent_library_fields_rejected_lib_sig_only` | `(Some(sig), None)` → `Err(LibrarySignatureFieldsInconsistent)` |
| `verify_entry_inconsistent_library_fields_rejected_lib_pub_only` | `(None, Some(pub))` → `Err(LibrarySignatureFieldsInconsistent)` |
| `verify_entry_malformed_library_identity_pub_rejected` | Bad library_identity_pub bytes → `Err(InvalidLibraryIdentityPub)` |
| `aggregation_on_entry_unwrapped_inserts_into_attested_by_via_listed_by_fallback` | `Unwrapped` status → `attested_by.contains(entry.listed_by)` |
| `aggregation_on_entry_attested_inserts_into_attested_by_via_lib_addr` | `Attested(A)` → `attested_by.contains(A)` |
| `aggregation_on_entry_unattested_inserts_into_unattested_by` | `Unattested(B)` → `unattested_by.contains(B)`; entry IS inserted (not dropped) |
| `aggregation_drop_library_sweeps_both_attestation_sets` | `drop_library(X)` removes X from both `attested_by` and `unattested_by` |

### 11.3 Integration tests — `tests/library_directory_integration.rs`

| Test | What it pins |
|---|---|
| `federation_two_libraries_broadcast_same_community_aggregates` | Library A + library B independently broadcast the same admin-signed community with their own wrapping sigs; aggregation: `attested_by = {A, B}`, `unattested_by = {}`, DTO `listed_by_count = 2`, `unattested = false` |
| `federation_one_library_tampered_wrapping_shows_unattested` | Library A broadcasts valid; library B broadcasts same community with tampered ls; aggregation: `attested_by = {A}`, `unattested_by = {B}`, DTO `unattested = true`, badge visible in browse output |
| `federation_phase1_entry_aggregates_alongside_phase3_wrapped` | Library A broadcasts Phase 1-style (no wrapping fields on wire); library B broadcasts Phase 3-wrapped; both contribute to `attested_by`; DTO `unattested = false`. Mixed-mode wire compat. |
| `federation_remove_library_evicts_attested_and_unattested_contributions` | After `remove_library(B)`, B is gone from both `attested_by` and `unattested_by` for every community |

### 11.4 Wire-format pinning — `tests/wire_format_library_directory_fixtures.rs`

| Test | What it pins |
|---|---|
| (existing Phase 1 pinning tests) | **MUST remain byte-identical** — skip_serializing_if invariant |
| `phase3_wrapped_entry_roundtrips` | Wrapped entry encodes + decodes round-trip via canonical CBOR |
| `phase3_wrapped_entry_pinned_bytes_prefix` | Pinned CBOR prefix bytes for a wrapped entry: map(11) marker + `li` key + `ls` key in correct sorted order |
| `phase3_wrapped_entry_two_char_keys_audit` | `ciborium::Value::Map` iter confirms ALL keys (including new li, ls) are 2-char |

### 11.5 Frontend vitest — `__tests__/LibraryDirectoryBrowser.test.ts`

| Test | What it pins |
|---|---|
| `unattested_badge_renders_when_dto_unattested_true` | DTO with `unattested: true` → badge visible, with aria-label and title attributes correctly set |
| `unattested_badge_absent_when_dto_unattested_false` | DTO with `unattested: false` → badge not rendered |

## 12. Deferred follow-ups

| Tag | Title | Description |
|---|---|---|
| 3.5 | Federation depth > 1 / republication chain | Single `library_signature` field limits federation depth to 1 hop. Future: `republished_via: Vec<{lib_pub, lib_sig}>` for transitive trust. YAGNI for v1 of federation. |
| 3.5 | Per-broadcast attestation breakdown UI | "Attested by 2 of 3 libraries — click for which" — defer until real-world unattested rates make the surface area worth it. |
| 3.5 | Library republish policy controls | Let users disable federation from specific trusted libraries (treat their broadcasts as Phase 1-Unwrapped). Defer until evidence of need. |

(No new Linear sub-tickets need to be filed for the 3.5 row — these are speculative future work, not committed Phase 3 scope. The existing Phase 4 ([ZEB-281](https://linear.app/zeblith/issue/ZEB-281/)) and Phase 6 ([ZEB-252](https://linear.app/zeblith/issue/ZEB-252/)) tickets are unchanged.)

## 13. Out of scope this round

- **Federation depth > 1.** Single library_signature field; no republish chain. A library republishing a republished entry simply replaces the wrapping sig (drops the previous library's attestation). YAGNI.
- **Per-broadcast attestation UI.** v1 ships the simple `unattested: boolean` badge. Detailed per-library breakdown deferred.
- **Library republish policy.** No client-side enforcement of "library A is allowed to republish library B's content." Federation policy is per-library (each library chooses what to syndicate); we just verify the wrapping sig.
- **Wrapping sig revocation.** No mechanism to revoke a library's wrapping sig. Trust is per-library, revocable by `remove_library(addr)`.
- **Anti-amplification rules.** A library could republish an entry many times. Same `MAX_ENTRIES_PER_LIBRARY` cap from Phase 1 still applies.

## 14. Acceptance criteria

1. **Federated republish verifies correctly.** A library that broadcasts an admin-signed entry with its own wrapping sig is `AttestationStatus::Attested(library_addr)`; the entry appears in `attested_by`; DTO `unattested = false`; no badge in UI.

2. **Tampered wrapping surfaces "unattested" badge.** A library that broadcasts an entry with a tampered wrapping sig is `AttestationStatus::Unattested(library_addr)`; entry is NOT dropped (admin sig still valid); `unattested_by` includes the library; DTO `unattested = true`; badge visible inline next to community name.

3. **Phase 1 entries continue to work.** An entry with no wrapping fields (`library_signature = None`, `library_identity_pub = None`) returns `AttestationStatus::Unwrapped`; aggregation inserts into `attested_by` via the `entry.listed_by` fallback; DTO `unattested = false`; no badge; wire-format pinning fixtures from Phase 1 remain byte-identical.

4. **All 5 CI gates green:**
   - `cargo fmt --all -- --check`
   - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
   - `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
   - `cargo check --locked --all-targets --features test-fixtures` (MSRV)
   - `npx tsc --noEmit` + `npx vitest run`

## 15. References

- Parent: [ZEB-218](https://linear.app/zeblith/issue/ZEB-218/) — Sub-D library-federated discovery directory (still In Progress; Phases 4 + 6 remain).
- This phase: [ZEB-280](https://linear.app/zeblith/issue/ZEB-280/) — Phase 3 federated republication.
- Phase 1 spec: `docs/specs/2026-05-11-zeb-218-sub-d-library-directory-vertical-slice-design.md`.
- Phase 2 spec: `docs/specs/2026-05-11-zeb-279-sub-d-phase-2-library-auto-discovery-design.md`.
- Parent epic spec — Sub-D signature verification contract: `docs/specs/2026-04-30-zeb-206-nav-tree-design.md` §486-489.
- Phase 1 PR: [#108](https://github.com/zeblithic/harmony-client/pull/108).
- Phase 2 PR: [#109](https://github.com/zeblithic/harmony-client/pull/109).
- Sibling implementation patterns:
  - `src-tauri/src/library_directory.rs::verify_entry` — Phase 1 admin-sig verification pattern this extends.
  - `src-tauri/src/library_directory.rs::verify_announce` — Phase 2 single-layer sig verification (similar shape to wrapping sig).
  - `src-tauri/src/owner_state_types.rs::serialize_bytes_as_bstr` — bstr serde helper this generalizes to Option.
