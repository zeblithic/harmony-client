# ZEB-216 Sub-B Phase 1: DM encryption primitives — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the ZEB-219 / ZEB-216-Section-2-3 encryption-primitives contract in code: new wire newtypes (`DmContentKey`, `DeviceIdentityHash`), Space struct fields (`content_key`, `prior_content_keys`), `OwnerDeviceCache`, dm_envelope.rs and dm_crypto.rs modules, extended `validate_invariants`, and the `merge_prior_content_keys` cap rule wired into `apply_space`'s dedupe canonicalization. No transport, no IPC, no UI — pure data + crypto + CRDT primitives.

**Architecture:** Five new types in `owner_state_types.rs` (`DmContentKey`, `DeviceIdentityHash`, `OwnerDeviceCache`, `OwnerDeviceEntry`, plus extended `Space`). Two new modules: `dm_envelope.rs` (MessagePayload + DmInvite + DmCidNotify + DmAck + DmPacket discriminant codec) and `dm_crypto.rs` (encrypt/decrypt/AAD/sender-binding). One extended file: `owner_state_crdt.rs` (extended `Space::validate_invariants`, extended `lww_merge_space`, new `apply_owner_device_update`, new `merge_prior_content_keys` helper integrated into the dedupe canonicalization path). One verified file: `owner_state_persist.rs` (round-trip with new Space fields + OwnerDeviceCache).

**Tech Stack:** Rust 2021, ciborium for CBOR, chacha20poly1305 = "0.10" (already in Cargo.toml), zeroize = "1" (already in Cargo.toml), serde with custom bstr serializers (existing pattern in `owner_state_types.rs:17-23`).

**Branch:** `zeb-216-sub-b-phase1-encryption-primitives` (already checked out, branched from `origin/main` at `55f30cd`).

---

## File structure

| File | Action | Approx LoC delta |
|---|---|---|
| `src-tauri/src/owner_state_types.rs` | Modify | +~190 (new newtypes, constants, OwnerDeviceCache, Space fields, regression tests) |
| `src-tauri/src/owner_state_crdt.rs` | Modify | +~150 (extended validate_invariants, lww_merge_space prior_content_keys handling, merge_prior_content_keys helper, apply_owner_device_update, OwnerState.owner_device_cache field, dedupe-merge tests including 5-Space convergence) |
| `src-tauri/src/owner_state_persist.rs` | Modify | +~20 (just verifies new fields persist via existing serde round-trip) |
| `src-tauri/src/dm_envelope.rs` | Create | ~280 (MessagePayload + 3 wire structs + DmPacket enum + encode_packet/decode_packet + tests) |
| `src-tauri/src/dm_crypto.rs` | Create | ~240 (encrypt_dm_message + decrypt_dm_message + compute_aad + verify_sender_binding + error enums + tests) |
| `src-tauri/src/lib.rs` | Modify | +2 (`mod dm_envelope; mod dm_crypto;`) |

The post-Task-9 PR diff: roughly +880 LoC of code + tests across 5 files modified, 2 created. Comparable to Sub-A Phase 2's surface area.

---

## Task 1: Wire newtypes — DmContentKey + DeviceIdentityHash + constants

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs` (after the existing `OutboxEntryId` definition around line 198, before `SpaceKind` enum)

**Context:** Per ZEB-216 spec §"Wire-format newtypes (Phase 1)" and §"OwnerDeviceCache (Phase 1)". `DmContentKey` is the bstr(32) wire type for the per-DM-Space symmetric key; in-memory it must zeroize on drop and redact in Debug output. `DeviceIdentityHash` is bstr(16). Two constants: `MAX_PRIOR_CONTENT_KEYS = 16` and `MAX_DEVICES_PER_OWNER = 32`.

- [ ] **Step 1: Write failing tests for the four properties (bstr-32 wire, bstr-16 wire, zeroize-on-drop, redacted Debug)**

Add to `#[cfg(test)] mod newtype_tests` (the existing module starting around line 344):

```rust
#[test]
fn dm_content_key_serializes_as_bstr_32() {
    use ciborium::into_writer;
    let k = DmContentKey::new([0u8; 32]);
    let mut bytes = Vec::new();
    into_writer(&k, &mut bytes).unwrap();
    // bstr(32): 0x58 0x20 || <32 bytes> = 34 bytes total.
    assert_eq!(bytes.len(), 34);
    assert_eq!(bytes[0], 0x58);
    assert_eq!(bytes[1], 0x20);
}

#[test]
fn dm_content_key_round_trip() {
    use ciborium::{from_reader, into_writer};
    let k = DmContentKey::new([0xab; 32]);
    let mut bytes = Vec::new();
    into_writer(&k, &mut bytes).unwrap();
    let recovered: DmContentKey = from_reader(&bytes[..]).unwrap();
    assert_eq!(k.as_bytes(), recovered.as_bytes());
}

#[test]
fn dm_content_key_debug_redacts_bytes() {
    let k = DmContentKey::new([0xab; 32]);
    let s = format!("{:?}", k);
    // No raw byte values, no hex, no decimal — must be a fixed redacted form.
    assert!(!s.contains("0xab"));
    assert!(!s.contains("171"));  // 0xab as decimal
    assert!(s.contains("redacted") || s.contains("REDACTED") || s.contains("***"));
}

#[test]
fn dm_content_key_zeroized_on_drop() {
    // Use ZeroizeOnDrop's invariant: dropping the wrapper zeros the
    // underlying [u8; 32]. We can't easily observe the freed memory,
    // but we can verify the trait is implemented by constraining a
    // generic function.
    fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
    assert_zeroize_on_drop::<DmContentKey>();
}

#[test]
fn device_identity_hash_serializes_as_bstr_16() {
    use ciborium::into_writer;
    let d = DeviceIdentityHash([0u8; 16]);
    let mut bytes = Vec::new();
    into_writer(&d, &mut bytes).unwrap();
    // bstr(16): 0x50 || <16 bytes> = 17 bytes total.
    assert_eq!(bytes.len(), 17);
    assert_eq!(bytes[0], 0x50);
}

#[test]
fn device_identity_hash_round_trip() {
    use ciborium::{from_reader, into_writer};
    let d = DeviceIdentityHash([0xcd; 16]);
    let mut bytes = Vec::new();
    into_writer(&d, &mut bytes).unwrap();
    let recovered: DeviceIdentityHash = from_reader(&bytes[..]).unwrap();
    assert_eq!(d, recovered);
}
```

- [ ] **Step 2: Run the tests to verify they fail (types don't exist)**

```bash
cargo test --manifest-path src-tauri/Cargo.toml owner_state_types::newtype_tests::dm_content_key 2>&1 | tail -20
```

Expected: compile error — `DmContentKey` and `DeviceIdentityHash` not defined.

- [ ] **Step 3: Add the newtype definitions and constants**

After the existing `OutboxEntryId` definition (around line 198 of `owner_state_types.rs`), add:

```rust
/// Maximum number of historical content keys retained per Space.
/// See ZEB-219 §"Cap policy" and ZEB-216 §"Dedupe-merge cap rule".
pub const MAX_PRIOR_CONTENT_KEYS: usize = 16;

/// Maximum number of device identities retained per OwnerAddr in
/// OwnerDeviceCache. Bounds the cache's memory footprint AND the
/// Reticulum-MTU cost of any piggybacked sender_devices lists.
/// See ZEB-216 §"OwnerDeviceCache".
pub const MAX_DEVICES_PER_OWNER: usize = 32;

/// 32-byte symmetric content key for DM/group-DM ChaCha20-Poly1305
/// encryption. Wire format: bstr(32). In-memory: zeroized on drop
/// (custom Drop via ZeroizeOnDrop derive). Debug redacts the bytes
/// to avoid accidental leakage to logs.
///
/// See ZEB-216 §"Wire-format newtypes (Phase 1)".
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, zeroize::ZeroizeOnDrop)]
pub struct DmContentKey(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    [u8; 32],
);

impl DmContentKey {
    pub fn new(key: [u8; 32]) -> Self {
        Self(key)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Generate a fresh random key from OS entropy. Used when creating a
    /// new DM/group-DM Space.
    pub fn random() -> Self {
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        Self(k)
    }
}

impl std::fmt::Debug for DmContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DmContentKey(<32 bytes redacted>)")
    }
}

/// 16-byte Reticulum device identity hash. Wire format: bstr(16).
/// See ZEB-216 §"OwnerDeviceCache".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceIdentityHash(
    #[serde(
        serialize_with = "serialize_bytes_as_bstr",
        deserialize_with = "deserialize_bytes_from_bstr"
    )]
    pub [u8; 16],
);
```

Verify `rand` is already in `Cargo.toml`:

```bash
grep -n "^rand" src-tauri/Cargo.toml
```

If absent, add `rand = "0.8"` to `[dependencies]` (it's already a transitive of chacha20poly1305 = 0.10 so it's likely there).

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --manifest-path src-tauri/Cargo.toml owner_state_types::newtype_tests 2>&1 | tail -30
```

Expected: all six new tests pass plus the existing newtype tests still pass.

- [ ] **Step 5: Verification gates**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

All three must pass clean. Use `${PIPESTATUS[0]}` if you pipe any of these to a tail/head/grep — pipe exit codes lie.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/owner_state_types.rs src-tauri/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(zeb-216-phase1): wire newtypes — DmContentKey + DeviceIdentityHash

Per ZEB-216 §"Wire-format newtypes (Phase 1)". DmContentKey is the
bstr(32) wire type for per-DM-Space symmetric keys, with ZeroizeOnDrop
and redacted Debug to avoid leaking key material via logs. DeviceIdentityHash
is the bstr(16) Reticulum identity-hash newtype. Constants MAX_PRIOR_CONTENT_KEYS
and MAX_DEVICES_PER_OWNER bound the cap rules elsewhere in the design.
EOF
)"
```

---

## Task 2: Add Space.content_key + prior_content_keys fields

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs:521` (locate the `pub struct Space { ... }` definition; add the two new fields at the bottom of the struct)

**Context:** Per ZEB-216 spec §"Space struct additions (Phase 1)". Both fields use `serde(default)` so non-DM Spaces (which omit them) deserialize cleanly. `lww_merge_space` in `owner_state_crdt.rs` will need to be updated in the same task — the merge function currently doesn't know about these fields, so a fresh same-SpaceId merge would silently drop them. For Task 2 the merge is "winner takes both" (trivial passthrough). The actual cap rule lives in Task 7.

- [ ] **Step 1: Write failing test for round-trip with content_key, and Folder rejecting content_key**

Add to `owner_state_types.rs` test module (find an appropriate `mod` for Space tests, around the existing `dedupe_key_dm_sorts_members` test):

```rust
#[test]
fn space_dm_with_content_key_round_trip() {
    use ciborium::{from_reader, into_writer};
    let s = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "alice-bob".to_string(),
        transport: None,
        members: vec![OwnerAddr([1u8; 16]), OwnerAddr([2u8; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "dev".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "dev".into() },
        content_key: Some(DmContentKey::new([0xaa; 32])),
        prior_content_keys: vec![DmContentKey::new([0xbb; 32])],
    };
    let mut bytes = Vec::new();
    into_writer(&s, &mut bytes).unwrap();
    let recovered: Space = from_reader(&bytes[..]).unwrap();
    assert_eq!(s.content_key.as_ref().map(|k| *k.as_bytes()),
               recovered.content_key.as_ref().map(|k| *k.as_bytes()));
    assert_eq!(s.prior_content_keys.len(), recovered.prior_content_keys.len());
    assert_eq!(s.prior_content_keys[0].as_bytes(),
               recovered.prior_content_keys[0].as_bytes());
}

#[test]
fn space_folder_omits_content_key_keys_in_cbor() {
    use ciborium::into_writer;
    let s = Space {
        id: SpaceId([1u8; 16]),
        kind: SpaceKind::Folder,
        parent: None,
        community_id: None,
        name: "Work".to_string(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "dev".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "dev".into() },
        content_key: None,
        prior_content_keys: vec![],
    };
    let mut bytes = Vec::new();
    into_writer(&s, &mut bytes).unwrap();
    // Folder serialization MUST NOT contain the "ck" or "pk" map keys —
    // the skip_serializing_if attributes elide them. Crude check: the
    // text strings "ck" and "pk" should not appear in the encoded bytes.
    let needle_ck = b"ck";
    let needle_pk = b"pk";
    assert!(!bytes.windows(2).any(|w| w == needle_ck), "Folder serialization unexpectedly contains 'ck' key");
    assert!(!bytes.windows(2).any(|w| w == needle_pk), "Folder serialization unexpectedly contains 'pk' key");
}
```

- [ ] **Step 2: Run the tests to verify they fail (Space struct missing fields)**

```bash
cargo test --manifest-path src-tauri/Cargo.toml space_dm_with_content_key 2>&1 | tail -20
```

Expected: compile error — `Space` has no field `content_key` / `prior_content_keys`.

- [ ] **Step 3: Add the Space fields**

In `owner_state_types.rs` `pub struct Space { ... }` definition (around line 521), add at the bottom:

```rust
    /// Per-DM-Space symmetric content key (ChaCha20-Poly1305).
    /// MUST be Some for kind ∈ {dm, group-dm}; MUST be None otherwise.
    /// Wire format: bstr(32) inside the Space CBOR map under key "ck".
    /// In-memory: zeroized on drop via DmContentKey's ZeroizeOnDrop impl.
    /// See ZEB-216 §"Space struct additions (Phase 1)".
    #[serde(rename = "ck", skip_serializing_if = "Option::is_none", default)]
    pub content_key: Option<DmContentKey>,

    /// Historical content keys retained from past dedupe-collision merges.
    /// Used as fallback decryption for messages encrypted under a now-
    /// superseded key. Bounded by MAX_PRIOR_CONTENT_KEYS = 16 (enforced
    /// in validate_invariants and merge_prior_content_keys).
    /// MUST NOT contain the current `content_key`.
    /// MUST be empty for non-DM kinds.
    /// Wire format: array of bstr(32) under key "pk".
    #[serde(rename = "pk", skip_serializing_if = "Vec::is_empty", default)]
    pub prior_content_keys: Vec<DmContentKey>,
```

- [ ] **Step 4: Update `lww_merge_space` to pass through the new fields**

Find `lww_merge_space` in `owner_state_crdt.rs` (search: `fn lww_merge_space`). Add the two new fields to the merged Space output. For Task 2, the merge logic is "winner provides them" — actual cap-rule merge lands in Task 7. Use the side with the newer `updated_at` HLC (matches existing LWW pattern). Example:

```rust
content_key: if newer_is_incoming {
    incoming.content_key.clone()
} else {
    existing.content_key.clone()
},
prior_content_keys: if newer_is_incoming {
    incoming.prior_content_keys.clone()
} else {
    existing.prior_content_keys.clone()
},
```

(The exact pattern depends on `lww_merge_space`'s existing structure — read it first and add the fields in the same idiomatic style.)

- [ ] **Step 5: Update any test fixtures that construct `Space` literals**

`grep -rn "Space {" src-tauri/src/ src-tauri/tests/` to find all sites. Each construction needs `content_key: None, prior_content_keys: vec![]` for non-DM kinds (which is the existing default — none of them are DM/group-dm yet). Compile errors will surface every site that needs updating.

- [ ] **Step 6: Run the tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml 2>&1 | tail -30
```

Expected: the two new tests pass; all existing tests still pass.

- [ ] **Step 7: Verification gates**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

- [ ] **Step 8: Commit**

```bash
git add -u
git commit -m "$(cat <<'EOF'
feat(zeb-216-phase1): add Space.content_key + prior_content_keys fields

Per ZEB-216 §"Space struct additions (Phase 1)". Both fields use
serde(default) so non-DM Spaces deserialize cleanly when the keys
are absent. lww_merge_space passes both fields through; the actual
prior_content_keys cap merge lands in Task 7.
EOF
)"
```

---

## Task 3: Extend Space::validate_invariants for content_key MUST/MUST-NOT rules

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs:566-662` (the `validate_invariants` impl on `Space`)

**Context:** Per ZEB-216 §"Validate invariants extension (Phase 1)". Four new invariants:
1. DM/group-DM MUST have `content_key: Some(...)`
2. Non-DM kinds MUST have `content_key: None` AND `prior_content_keys: vec![]`
3. `content_key.as_bytes()` MUST NOT appear in any `prior_content_keys` entry
4. `prior_content_keys.len()` ≤ `MAX_PRIOR_CONTENT_KEYS`

- [ ] **Step 1: Write failing tests for each invariant**

Add to the existing test module that contains `dm_must_have_exactly_two_members`:

```rust
#[test]
fn dm_must_have_content_key() {
    let mut d = Space {
        id: SpaceId([1; 16]),
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "x".into(),
        transport: None,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: None,  // ← invariant violation
        prior_content_keys: vec![],
    };
    assert!(d.validate_invariants().is_err());
    d.content_key = Some(DmContentKey::new([0xaa; 32]));
    assert!(d.validate_invariants().is_ok());
}

#[test]
fn group_dm_must_have_content_key() {
    let mut d = Space {
        id: SpaceId([1; 16]),
        kind: SpaceKind::GroupDm,
        parent: None,
        community_id: None,
        name: "x".into(),
        transport: None,
        members: (0u8..3).map(|i| OwnerAddr([i; 16])).collect(),
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: None,
        prior_content_keys: vec![],
    };
    assert!(d.validate_invariants().is_err());
    d.content_key = Some(DmContentKey::new([0xaa; 32]));
    assert!(d.validate_invariants().is_ok());
}

#[test]
fn folder_rejects_content_key() {
    let f = Space {
        id: SpaceId([1; 16]),
        kind: SpaceKind::Folder,
        parent: None,
        community_id: None,
        name: "Work".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: Some(DmContentKey::new([0xaa; 32])),  // ← invariant violation
        prior_content_keys: vec![],
    };
    assert!(f.validate_invariants().is_err());
}

#[test]
fn folder_rejects_prior_content_keys() {
    let f = Space {
        id: SpaceId([1; 16]),
        kind: SpaceKind::Folder,
        parent: None,
        community_id: None,
        name: "Work".into(),
        transport: None,
        members: vec![],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: None,
        prior_content_keys: vec![DmContentKey::new([0xbb; 32])],  // ← invariant violation
    };
    assert!(f.validate_invariants().is_err());
}

#[test]
fn dm_content_key_in_prior_list_rejects() {
    let dup = DmContentKey::new([0xaa; 32]);
    let d = Space {
        id: SpaceId([1; 16]),
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "x".into(),
        transport: None,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: Some(dup.clone()),
        prior_content_keys: vec![dup],  // ← same as content_key — violation
    };
    assert!(d.validate_invariants().is_err());
}

#[test]
fn dm_prior_content_keys_cap_exceeded_rejects() {
    let d = Space {
        id: SpaceId([1; 16]),
        kind: SpaceKind::Dm,
        parent: None,
        community_id: None,
        name: "x".into(),
        transport: None,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
        custom_name: None,
        notification_pref: None,
        left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: Some(DmContentKey::new([0xaa; 32])),
        prior_content_keys: (0u8..(MAX_PRIOR_CONTENT_KEYS as u8 + 1))
            .map(|i| DmContentKey::new([i; 32]))
            .collect(),
    };
    assert!(d.validate_invariants().is_err());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml dm_must_have_content_key folder_rejects_content_key 2>&1 | tail -20
```

Expected: each test fails because `validate_invariants` doesn't check the new fields yet.

- [ ] **Step 3: Extend `validate_invariants`**

In `owner_state_types.rs`, the existing `pub fn validate_invariants(&self) -> Result<(), InvariantError>` (around line 566-662). Add at the END of the function (after the kind-specific checks and before `Ok(())`):

```rust
        // Content-key invariants per ZEB-216 §"Validate invariants extension".
        match self.kind {
            SpaceKind::Dm | SpaceKind::GroupDm => {
                if self.content_key.is_none() {
                    return Err(InvariantError(format!(
                        "{:?} kind requires content_key",
                        self.kind
                    )));
                }
            }
            _ => {
                if self.content_key.is_some() {
                    return Err(InvariantError(format!(
                        "{:?} kind must not have content_key",
                        self.kind
                    )));
                }
                if !self.prior_content_keys.is_empty() {
                    return Err(InvariantError(format!(
                        "{:?} kind must not have prior_content_keys",
                        self.kind
                    )));
                }
            }
        }

        if self.prior_content_keys.len() > MAX_PRIOR_CONTENT_KEYS {
            return Err(InvariantError(format!(
                "prior_content_keys.len()={} exceeds MAX_PRIOR_CONTENT_KEYS={}",
                self.prior_content_keys.len(),
                MAX_PRIOR_CONTENT_KEYS
            )));
        }

        if let Some(ck) = &self.content_key {
            if self.prior_content_keys.iter().any(|p| p.as_bytes() == ck.as_bytes()) {
                return Err(InvariantError(
                    "content_key must not appear in prior_content_keys".into(),
                ));
            }
        }
```

- [ ] **Step 4: Update existing test fixtures that construct DM/group-DM Spaces**

`grep -rn "kind: SpaceKind::Dm\|kind: SpaceKind::GroupDm" src-tauri/src/ src-tauri/tests/` and ensure each gets a `content_key: Some(DmContentKey::new([_; 32]))`. Compile errors and validate-invariant failures will surface every site.

- [ ] **Step 5: Run the tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml 2>&1 | tail -30
```

Expected: all six new invariant tests pass; existing tests still pass.

- [ ] **Step 6: Verification gates + commit**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
git add -u
git commit -m "$(cat <<'EOF'
feat(zeb-216-phase1): extend Space::validate_invariants for content_key

Per ZEB-216 §"Validate invariants extension (Phase 1)". DM and group-DM
kinds MUST have content_key Some(_); other kinds MUST have None and
empty prior_content_keys. content_key MUST NOT appear in
prior_content_keys (would be redundant decryption attempt).
prior_content_keys length capped at MAX_PRIOR_CONTENT_KEYS=16.
EOF
)"
```

---

## Task 4: OwnerDeviceCache + apply_owner_device_update with dedupe + cap

**Files:**
- Modify: `src-tauri/src/owner_state_types.rs` (add `OwnerDeviceCache`, `OwnerDeviceEntry` types after the new newtypes from Task 1)
- Modify: `src-tauri/src/owner_state_crdt.rs` (add `owner_device_cache: OwnerDeviceCache` field to `OwnerState` struct around line 22; add `apply_owner_device_update` function with LWW + dedupe + cap)

**Context:** Per ZEB-216 §"OwnerDeviceCache (Phase 1)" and the round 2 review fix that added dedupe + cap. The cache replicates via Flow A so the existing `OwnerState` CBOR map gets a new field.

- [ ] **Step 1: Write failing tests for LWW, dedupe, cap, resolve-via-binary-search**

Add to `owner_state_crdt.rs` test module (or create a new mod owner_device_cache_tests):

```rust
#[cfg(test)]
mod owner_device_cache_tests {
    use super::*;
    use crate::owner_state_types::{
        DeviceIdentityHash, MAX_DEVICES_PER_OWNER, OwnerAddr, OwnerDeviceCache,
    };

    fn hlc(ms: u64) -> Hlc {
        Hlc { wall_ms: ms, logical: 0, device_id: "d".into() }
    }

    #[test]
    fn lww_newer_replaces() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let d1 = vec![DeviceIdentityHash([1; 16])];
        let d2 = vec![DeviceIdentityHash([2; 16])];
        assert_eq!(apply_owner_device_update(&mut c, addr, d1.clone(), hlc(1)), ApplyOutcome::Inserted);
        assert_eq!(apply_owner_device_update(&mut c, addr, d2.clone(), hlc(2)), ApplyOutcome::Merged { old_id: None });
        assert_eq!(c.devices.get(&addr).unwrap().devices, d2);
    }

    #[test]
    fn lww_older_is_noop() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let d1 = vec![DeviceIdentityHash([1; 16])];
        let d2 = vec![DeviceIdentityHash([2; 16])];
        apply_owner_device_update(&mut c, addr, d2.clone(), hlc(2));
        let outcome = apply_owner_device_update(&mut c, addr, d1, hlc(1));
        assert!(matches!(outcome, ApplyOutcome::Rejected(RejectionReason::StaleHlc { .. })));
        assert_eq!(c.devices.get(&addr).unwrap().devices, d2);
    }

    #[test]
    fn dedupes_input() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let d1 = DeviceIdentityHash([1; 16]);
        let d2 = DeviceIdentityHash([2; 16]);
        apply_owner_device_update(&mut c, addr, vec![d1, d2, d1], hlc(1));
        // Stored vec must be deduped + sorted.
        assert_eq!(c.devices.get(&addr).unwrap().devices, vec![d1, d2]);
    }

    #[test]
    fn caps_at_max_devices_per_owner() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let big: Vec<DeviceIdentityHash> = (0..100).map(|i| DeviceIdentityHash([i; 16])).collect();
        apply_owner_device_update(&mut c, addr, big, hlc(1));
        let stored = &c.devices.get(&addr).unwrap().devices;
        assert_eq!(stored.len(), MAX_DEVICES_PER_OWNER);
        // Lex-smallest entries survive — first 32 of [0..100].
        assert_eq!(stored[0], DeviceIdentityHash([0; 16]));
        assert_eq!(stored[31], DeviceIdentityHash([31; 16]));
    }

    #[test]
    fn binary_search_works_after_apply() {
        let mut c = OwnerDeviceCache::default();
        let addr = OwnerAddr([1; 16]);
        let target = DeviceIdentityHash([5; 16]);
        let big: Vec<DeviceIdentityHash> = (0..10).map(|i| DeviceIdentityHash([i; 16])).collect();
        apply_owner_device_update(&mut c, addr, big, hlc(1));
        // The cache stores devices sorted, so binary_search works (used by
        // resolve_link_origin_owner in Phase 3b).
        let stored = &c.devices.get(&addr).unwrap().devices;
        assert!(stored.binary_search(&target).is_ok());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml owner_device_cache_tests 2>&1 | tail -20
```

Expected: compile errors — types and function don't exist.

- [ ] **Step 3: Add the types in `owner_state_types.rs`**

After the `DeviceIdentityHash` definition (Task 1), add:

```rust
/// Per-OwnerAddr cache of known bound-device identity hashes. Replicated
/// across the user's bound devices via Flow A (owner-state CRDT sync).
/// Each entry maintained via LWW on `learned_at` HLC.
///
/// See ZEB-216 §"OwnerDeviceCache (Phase 1)".
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDeviceCache {
    #[serde(rename = "d")]
    pub devices: BTreeMap<OwnerAddr, OwnerDeviceEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDeviceEntry {
    /// Sorted ascending lex, deduped, capped at MAX_DEVICES_PER_OWNER.
    /// Sorted invariant means binary_search works for lookup.
    #[serde(rename = "v")]
    pub devices: Vec<DeviceIdentityHash>,
    /// HLC of when this entry was learned. LWW key for merge.
    #[serde(rename = "l")]
    pub learned_at: Hlc,
}
```

You may need to add `use std::collections::BTreeMap;` near the top if it's not already imported.

- [ ] **Step 4: Add the `OwnerState.owner_device_cache` field**

In `owner_state_crdt.rs` `OwnerState` struct (around line 22-36), add a new field:

```rust
    /// ZEB-216 Sub-B Phase 1: per-OwnerAddr device cache for DM unicast
    /// addressing. Replicates across the owner's bound devices via Flow A.
    #[serde(rename = "od", skip_serializing_if = "OwnerDeviceCache::is_empty", default)]
    pub owner_device_cache: OwnerDeviceCache,
```

Add an `is_empty` impl on `OwnerDeviceCache` in `owner_state_types.rs`:

```rust
impl OwnerDeviceCache {
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}
```

Update the `OwnerState` import line in `owner_state_crdt.rs` to include `OwnerDeviceCache`.

- [ ] **Step 5: Implement `apply_owner_device_update`**

In `owner_state_crdt.rs`, add (as an `impl OwnerState` method or as a free function — match the existing pattern. `apply_outbox`/`apply_inbox` are methods, so prefer a method):

```rust
impl OwnerState {
    /// Apply a device-list update for an OwnerAddr. LWW on `learned_at` HLC;
    /// devices are deduped + sorted + capped at MAX_DEVICES_PER_OWNER before
    /// storage to bound cache memory and prevent DoS via spoofed updates.
    /// See ZEB-216 §"OwnerDeviceCache (Phase 1)".
    pub fn apply_owner_device_update(
        &mut self,
        addr: OwnerAddr,
        devices: Vec<DeviceIdentityHash>,
        learned_at: Hlc,
    ) -> ApplyOutcome {
        if let Some(existing) = self.owner_device_cache.devices.get(&addr) {
            if existing.learned_at.is_strictly_newer_than(&learned_at)
                || existing.learned_at == learned_at
            {
                return ApplyOutcome::Rejected(RejectionReason::StaleHlc {
                    kind: "owner_device_entry",
                    device_id: learned_at.device_id.clone(),
                });
            }
        }
        let mut sanitized = devices;
        sanitized.sort();
        sanitized.dedup();
        sanitized.truncate(MAX_DEVICES_PER_OWNER);
        let was_present = self.owner_device_cache.devices.contains_key(&addr);
        self.owner_device_cache.devices.insert(
            addr,
            OwnerDeviceEntry { devices: sanitized, learned_at },
        );
        if was_present {
            ApplyOutcome::Merged { old_id: None }
        } else {
            ApplyOutcome::Inserted
        }
    }
}
```

You'll need to import `MAX_DEVICES_PER_OWNER`, `DeviceIdentityHash`, `OwnerDeviceCache`, `OwnerDeviceEntry` in `owner_state_crdt.rs`.

- [ ] **Step 6: Run the tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml owner_device_cache_tests 2>&1 | tail -30
```

Expected: all five tests pass; existing CRDT tests still pass.

- [ ] **Step 7: Verification gates + commit**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
git add -u
git commit -m "$(cat <<'EOF'
feat(zeb-216-phase1): OwnerDeviceCache + apply_owner_device_update

Per ZEB-216 §"OwnerDeviceCache (Phase 1)" with round 2 dedupe + cap fixes.
LWW on learned_at HLC; new field on OwnerState (replicates via Flow A);
input devices are sorted + deduped + capped at MAX_DEVICES_PER_OWNER=32
to bound cache memory and prevent cache-growth DoS via spoofed updates.
Sorted invariant lets resolve_link_origin_owner (Phase 3b) use
binary_search.
EOF
)"
```

---

## Task 5: dm_envelope module — MessagePayload + DmInvite + DmCidNotify + DmAck

**Files:**
- Create: `src-tauri/src/dm_envelope.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod dm_envelope;`)

**Context:** Per ZEB-216 §"Wire format" and §"Plaintext envelope". Four wire types plus the `DmPacket` discriminant codec (0x01=Invite, 0x02=CidNotify, 0x03=Ack). Each type uses two-letter CBOR field renames. `MessagePayload` is the plaintext envelope encrypted into the CAS storage_blob; the others ride on Reticulum unicast.

- [ ] **Step 1: Write failing tests for round-trip + discriminant codec**

Create `src-tauri/src/dm_envelope.rs` with the test module first:

```rust
//! ZEB-216 Sub-B Phase 1: DM wire envelope types + discriminant codec.
//!
//! See `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Wire format" and §"Plaintext envelope (Phase 1, recap from ZEB-219)".

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner_state_types::{
        ContentId, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, SpaceId, SpaceKind,
    };

    fn hlc(ms: u64) -> Hlc {
        Hlc { wall_ms: ms, logical: 0, device_id: "d".into() }
    }

    #[test]
    fn message_payload_round_trip_canonical_cbor() {
        let m = MessagePayload {
            body: b"hello bob".to_vec(),
            mime_type: "text/plain".into(),
            sender: OwnerAddr([1; 16]),
            sent_at: hlc(1),
        };
        let bytes = crate::owner_state_crypto::canonical_cbor_encode(&m).unwrap();
        let recovered: MessagePayload =
            crate::owner_state_crypto::canonical_cbor_decode(&bytes).unwrap();
        assert_eq!(m, recovered);
    }

    fn sample_invite() -> DmInvite {
        DmInvite {
            space_id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            inviter: OwnerAddr([1; 16]),
            content_key: DmContentKey::new([0xaa; 32]),
            sender_devices: vec![DeviceIdentityHash([7; 16])],
            created_at: hlc(1),
        }
    }

    fn sample_cidnotify() -> DmCidNotify {
        DmCidNotify {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId::from_bytes([0xee; 32]),
            sender_owner_addr: OwnerAddr([1; 16]),
            sender_devices: vec![DeviceIdentityHash([7; 16])],
        }
    }

    fn sample_ack() -> DmAck {
        DmAck {
            space_id: SpaceId([1; 16]),
            message_cid: ContentId::from_bytes([0xee; 32]),
            ack_from_owner_addr: OwnerAddr([2; 16]),
            ack_from_devices: vec![DeviceIdentityHash([8; 16])],
        }
    }

    #[test]
    fn dm_packet_invite_round_trip() {
        let p = DmPacket::Invite(sample_invite());
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x01);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_cidnotify_round_trip() {
        let p = DmPacket::CidNotify(sample_cidnotify());
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x02);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_ack_round_trip() {
        let p = DmPacket::Ack(sample_ack());
        let encoded = encode_packet(&p).unwrap();
        assert_eq!(encoded[0], 0x03);
        let decoded = decode_packet(&encoded).unwrap();
        assert_eq!(p, decoded);
    }

    #[test]
    fn dm_packet_unknown_discriminant_rejects() {
        let bytes = vec![0xff, 0xa0]; // garbage discriminant + empty CBOR map
        let err = decode_packet(&bytes).unwrap_err();
        assert!(matches!(err, DecodeError::UnknownDiscriminant(0xff)));
    }

    #[test]
    fn dm_packet_empty_bytes_rejects() {
        let err = decode_packet(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::Empty));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml dm_envelope 2>&1 | tail -20
```

Expected: compile errors — types and functions don't exist yet.

- [ ] **Step 3: Implement the module**

Above the test module in `dm_envelope.rs`:

```rust
use serde::{Deserialize, Serialize};

use crate::owner_state_crypto::{sealed::CanonicalPayloadSealed, CanonicalPayload};
use crate::owner_state_types::{
    ContentId, DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, SpaceId, SpaceKind,
};

/// Plaintext envelope encrypted into the CAS storage_blob. Bound by AAD
/// to the Space's dedupe_key; decrypt enforces (sender, sent_at) authenticity.
/// See ZEB-216 §"Plaintext envelope" / ZEB-219 §"Wire format".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessagePayload {
    #[serde(rename = "bd")] pub body: Vec<u8>,
    #[serde(rename = "mt")] pub mime_type: String,
    #[serde(rename = "se")] pub sender: OwnerAddr,
    #[serde(rename = "sa")] pub sent_at: Hlc,
}

/// Reticulum-unicast packet announcing a new DM Space and distributing
/// the per-Space content_key. Receiver MUST run the bootstrap sanity
/// gates (ZEB-216 §"Link-origin binding rule") before applying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmInvite {
    #[serde(rename = "si")] pub space_id: SpaceId,
    #[serde(rename = "kn")] pub kind: SpaceKind,
    /// Members sorted ascending lex (matches Space::members invariant).
    /// Cannot be used to identify the inviter — see `inviter` field.
    #[serde(rename = "me")] pub members: Vec<OwnerAddr>,
    /// Explicit inviter OwnerAddr. Receiver MUST verify
    /// `inviter ∈ members` and `from_identity_hash ∈ sender_devices`
    /// before prompting the user.
    #[serde(rename = "iv")] pub inviter: OwnerAddr,
    #[serde(rename = "ck")] pub content_key: DmContentKey,
    #[serde(rename = "sd")] pub sender_devices: Vec<DeviceIdentityHash>,
    #[serde(rename = "ca")] pub created_at: Hlc,
}

/// Reticulum-unicast packet notifying recipients that a new encrypted
/// message blob exists at `message_cid` in CAS. `sender_owner_addr` is
/// diagnostic only — receiver MUST resolve the actual sender via
/// link-origin binding (ZEB-216 §"Link-origin binding rule").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmCidNotify {
    #[serde(rename = "si")] pub space_id: SpaceId,
    #[serde(rename = "mc")] pub message_cid: ContentId,
    #[serde(rename = "so")] pub sender_owner_addr: OwnerAddr,
    #[serde(rename = "sd")] pub sender_devices: Vec<DeviceIdentityHash>,
}

/// Reticulum-unicast packet acknowledging receipt of a DmCidNotify.
/// `ack_from_owner_addr` is diagnostic only — receiver MUST resolve via
/// link-origin binding AND verify the resolved owner is in
/// `OutboxEntry.recipient_owners`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmAck {
    #[serde(rename = "si")] pub space_id: SpaceId,
    #[serde(rename = "mc")] pub message_cid: ContentId,
    #[serde(rename = "ao")] pub ack_from_owner_addr: OwnerAddr,
    #[serde(rename = "ad")] pub ack_from_devices: Vec<DeviceIdentityHash>,
}

/// Discriminated union of Reticulum DM packets. Wire layout:
/// `[u8 discriminant][CBOR-encoded body]` with discriminants
/// 0x01=Invite, 0x02=CidNotify, 0x03=Ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmPacket {
    Invite(DmInvite),
    CidNotify(DmCidNotify),
    Ack(DmAck),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("CBOR encode failed: {0}")]
    Cbor(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("packet is empty")]
    Empty,
    #[error("unknown discriminant byte 0x{0:02x}")]
    UnknownDiscriminant(u8),
    #[error("CBOR decode failed: {0}")]
    Cbor(String),
}

pub fn encode_packet(packet: &DmPacket) -> Result<Vec<u8>, EncodeError> {
    let (disc, body): (u8, Vec<u8>) = match packet {
        DmPacket::Invite(p) => (0x01, encode_body(p)?),
        DmPacket::CidNotify(p) => (0x02, encode_body(p)?),
        DmPacket::Ack(p) => (0x03, encode_body(p)?),
    };
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(disc);
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn decode_packet(bytes: &[u8]) -> Result<DmPacket, DecodeError> {
    let (disc, body) = bytes.split_first().ok_or(DecodeError::Empty)?;
    match disc {
        0x01 => Ok(DmPacket::Invite(decode_body(body)?)),
        0x02 => Ok(DmPacket::CidNotify(decode_body(body)?)),
        0x03 => Ok(DmPacket::Ack(decode_body(body)?)),
        other => Err(DecodeError::UnknownDiscriminant(*other)),
    }
}

fn encode_body<T: Serialize>(value: &T) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).map_err(|e| EncodeError::Cbor(e.to_string()))?;
    Ok(out)
}

fn decode_body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    ciborium::from_reader(bytes).map_err(|e| DecodeError::Cbor(e.to_string()))
}

// CanonicalPayload registrations — these wire types pass through
// `canonical_cbor_encode` from owner_state_crypto.
impl CanonicalPayloadSealed for MessagePayload {}
impl CanonicalPayload for MessagePayload {}
impl CanonicalPayloadSealed for DmInvite {}
impl CanonicalPayload for DmInvite {}
impl CanonicalPayloadSealed for DmCidNotify {}
impl CanonicalPayload for DmCidNotify {}
impl CanonicalPayloadSealed for DmAck {}
impl CanonicalPayload for DmAck {}
```

Add `mod dm_envelope;` to `lib.rs`.

- [ ] **Step 4: Run the tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml dm_envelope 2>&1 | tail -30
```

Expected: all six tests pass.

- [ ] **Step 5: Verification gates + commit**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-216-phase1): dm_envelope module — wire types + DmPacket codec

Per ZEB-216 §"Wire format" and §"Plaintext envelope". Four wire types
(MessagePayload, DmInvite, DmCidNotify, DmAck) with two-letter CBOR
field renames. DmPacket discriminant codec uses [u8 discriminant]
[CBOR body] layout: 0x01=Invite, 0x02=CidNotify, 0x03=Ack. Each type
also implements CanonicalPayload (sealed marker via owner_state_crypto)
so they can pass through canonical_cbor_encode for AAD computation
and persistence.
EOF
)"
```

---

## Task 6: dm_crypto module — encrypt + decrypt + AAD + sender-binding

**Files:**
- Create: `src-tauri/src/dm_crypto.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod dm_crypto;`)

**Context:** Per ZEB-216 §"Encryption helpers (Phase 1)" and §"Sender-binding check". ChaCha20-Poly1305 AEAD with random 12-byte nonce, version-byte 0x01 prefix on the storage_blob, AAD derived from `space.dedupe_key()`. Decrypt iterates current key + prior_content_keys in stored order. Length-gate before slicing prevents panics on short blobs.

- [ ] **Step 1: Write failing tests for the seven helper-level properties**

Create `src-tauri/src/dm_crypto.rs` with the test module first:

```rust
//! ZEB-216 Sub-B Phase 1: DM encrypt/decrypt + AAD + sender-binding helpers.
//!
//! See `docs/specs/2026-05-02-zeb-216-sub-b-dm-transport-design.md`
//! §"Encryption helpers (Phase 1)" and §"Sender-binding check".

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dm_envelope::MessagePayload;
    use crate::owner_state_types::{DmContentKey, Hlc, OwnerAddr, SpaceId, SpaceKind};

    fn payload(sender: OwnerAddr) -> MessagePayload {
        MessagePayload {
            body: b"hello".to_vec(),
            mime_type: "text/plain".into(),
            sender,
            sent_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        }
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = DmContentKey::new([0x55; 32]);
        let aad = b"some aad";
        let p = payload(OwnerAddr([1; 16]));
        let blob = encrypt_dm_message(&key, aad, &p).unwrap();
        // version + nonce + ciphertext + tag = at least 29 bytes
        assert!(blob.len() >= 29);
        assert_eq!(blob[0], 0x01);
        let recovered = decrypt_dm_message(&key, &[], aad, &blob).unwrap();
        assert_eq!(p, recovered);
    }

    #[test]
    fn aad_mismatch_rejects() {
        let key = DmContentKey::new([0x55; 32]);
        let p = payload(OwnerAddr([1; 16]));
        let blob = encrypt_dm_message(&key, b"aad-1", &p).unwrap();
        let err = decrypt_dm_message(&key, &[], b"aad-2", &blob).unwrap_err();
        assert!(matches!(err, DmDecryptError::AeadFailureAllKeys));
    }

    #[test]
    fn version_byte_unknown_rejects() {
        let key = DmContentKey::new([0x55; 32]);
        let p = payload(OwnerAddr([1; 16]));
        let mut blob = encrypt_dm_message(&key, b"aad", &p).unwrap();
        blob[0] = 0xff; // unknown version
        let err = decrypt_dm_message(&key, &[], b"aad", &blob).unwrap_err();
        assert!(matches!(err, DmDecryptError::UnknownVersion(0xff)));
    }

    #[test]
    fn length_gate_short_blob_rejects() {
        let key = DmContentKey::new([0x55; 32]);
        let short = vec![0x01; 28]; // one byte short of 29
        let err = decrypt_dm_message(&key, &[], b"aad", &short).unwrap_err();
        assert!(matches!(err, DmDecryptError::TruncatedBlob));
    }

    #[test]
    fn tampered_ciphertext_rejects() {
        let key = DmContentKey::new([0x55; 32]);
        let p = payload(OwnerAddr([1; 16]));
        let mut blob = encrypt_dm_message(&key, b"aad", &p).unwrap();
        let last_idx = blob.len() - 1;
        blob[last_idx] ^= 0xff; // flip last byte (the auth tag)
        let err = decrypt_dm_message(&key, &[], b"aad", &blob).unwrap_err();
        assert!(matches!(err, DmDecryptError::AeadFailureAllKeys));
    }

    #[test]
    fn prior_content_keys_fallback_succeeds() {
        let k1 = DmContentKey::new([0x11; 32]);
        let k2 = DmContentKey::new([0x22; 32]);
        let p = payload(OwnerAddr([1; 16]));
        // Encrypt under k1; decrypt with current=k2, prior=[k1] — fallback.
        let blob = encrypt_dm_message(&k1, b"aad", &p).unwrap();
        let recovered = decrypt_dm_message(&k2, &[k1], b"aad", &blob).unwrap();
        assert_eq!(p, recovered);
    }

    #[test]
    fn sender_binding_match_ok() {
        let p = payload(OwnerAddr([1; 16]));
        assert!(verify_sender_binding(&p, OwnerAddr([1; 16])).is_ok());
    }

    #[test]
    fn sender_binding_mismatch_rejects() {
        let p = payload(OwnerAddr([1; 16]));
        let err = verify_sender_binding(&p, OwnerAddr([2; 16])).unwrap_err();
        assert!(matches!(err, DmReceiveError::SenderImpersonation));
    }

    #[test]
    fn compute_aad_dm_uses_dedupe_key() {
        // Two DM Spaces with the same sorted members must yield the same AAD.
        let s1 = crate::owner_state_types::Space {
            id: SpaceId([1; 16]),
            kind: SpaceKind::Dm,
            parent: None, community_id: None, name: "x".into(),
            transport: None,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None, notification_pref: None, left_at: None,
            created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
            updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
            content_key: Some(DmContentKey::new([0xaa; 32])),
            prior_content_keys: vec![],
        };
        let mut s2 = s1.clone();
        s2.id = SpaceId([99; 16]); // different SpaceId
        s2.content_key = Some(DmContentKey::new([0xbb; 32])); // different key
        // Same members → same dedupe_key → same AAD.
        assert_eq!(compute_aad(&s1).unwrap(), compute_aad(&s2).unwrap());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml dm_crypto 2>&1 | tail -20
```

Expected: compile errors — module doesn't exist.

- [ ] **Step 3: Implement the module**

Above the test module:

```rust
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    ChaCha20Poly1305,
};

use crate::dm_envelope::MessagePayload;
use crate::owner_state_crypto::canonical_cbor_encode;
use crate::owner_state_types::{DmContentKey, OwnerAddr, Space};

/// Storage-blob layout per ZEB-219 §"Wire format":
///   version_byte(1) || nonce_12(12) || ciphertext(N) || poly1305_tag(16)
/// = N + 29 bytes minimum.
const STORAGE_BLOB_V1: u8 = 0x01;
const NONCE_LEN_V1: usize = 12;
const TAG_LEN: usize = 16;
const MIN_BLOB_LEN_V1: usize = 1 + NONCE_LEN_V1 + TAG_LEN; // 29

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmEncryptError {
    #[error("payload CBOR encode failed: {0}")]
    PayloadEncode(String),
    #[error("AEAD encryption failed")]
    AeadFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmDecryptError {
    #[error("storage_blob shorter than minimum 29 bytes")]
    TruncatedBlob,
    #[error("unknown storage_blob version byte 0x{0:02x}")]
    UnknownVersion(u8),
    #[error("AEAD decryption failed under all candidate keys (current + prior)")]
    AeadFailureAllKeys,
    #[error("plaintext CBOR decode failed: {0}")]
    PayloadDecode(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmReceiveError {
    #[error("payload sender does not match link-origin OwnerAddr (impersonation)")]
    SenderImpersonation,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AadComputeError {
    #[error("dedupe_key CBOR encode failed: {0}")]
    Encode(String),
}

/// Encrypt a MessagePayload into a v1 storage_blob bound by AAD.
/// The plaintext is canonical-CBOR-encoded MessagePayload bytes.
pub fn encrypt_dm_message(
    content_key: &DmContentKey,
    aad: &[u8],
    payload: &MessagePayload,
) -> Result<Vec<u8>, DmEncryptError> {
    let plaintext = canonical_cbor_encode(payload)
        .map_err(|e| DmEncryptError::PayloadEncode(e.to_string()))?;
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let cipher = ChaCha20Poly1305::new(content_key.as_bytes().into());
    let ciphertext_with_tag = cipher
        .encrypt(&nonce, Payload { msg: &plaintext, aad })
        .map_err(|_| DmEncryptError::AeadFailure)?;
    let mut blob = Vec::with_capacity(1 + NONCE_LEN_V1 + ciphertext_with_tag.len());
    blob.push(STORAGE_BLOB_V1);
    blob.extend_from_slice(nonce.as_slice());
    blob.extend_from_slice(&ciphertext_with_tag);
    Ok(blob)
}

/// Decrypt a v1 storage_blob, trying current key first then each prior
/// content_key in stored order. Length-gate enforced before slicing.
pub fn decrypt_dm_message(
    content_key: &DmContentKey,
    prior_content_keys: &[DmContentKey],
    aad: &[u8],
    storage_blob: &[u8],
) -> Result<MessagePayload, DmDecryptError> {
    if storage_blob.len() < MIN_BLOB_LEN_V1 {
        return Err(DmDecryptError::TruncatedBlob);
    }
    let version = storage_blob[0];
    let (nonce_slice, ciphertext_slice) = match version {
        STORAGE_BLOB_V1 => (&storage_blob[1..1 + NONCE_LEN_V1], &storage_blob[1 + NONCE_LEN_V1..]),
        // 0x02 reserved (XChaCha20-Poly1305 with 24-byte nonce)
        other => return Err(DmDecryptError::UnknownVersion(other)),
    };
    let nonce: [u8; NONCE_LEN_V1] = nonce_slice.try_into().expect("length-gated above");

    for key in std::iter::once(content_key).chain(prior_content_keys.iter()) {
        let cipher = ChaCha20Poly1305::new(key.as_bytes().into());
        if let Ok(plaintext) = cipher.decrypt(
            &nonce.into(),
            Payload { msg: ciphertext_slice, aad },
        ) {
            return ciborium::from_reader(&plaintext[..])
                .map_err(|e| DmDecryptError::PayloadDecode(e.to_string()));
        }
    }
    Err(DmDecryptError::AeadFailureAllKeys)
}

/// Receive-time check: the encrypted-payload `sender` field MUST match
/// the OwnerAddr resolved from the inbound Reticulum link's identity_hash.
/// Phase 3b wires `link_origin` from `OwnerDeviceCache` resolution.
pub fn verify_sender_binding(
    payload: &MessagePayload,
    link_origin: OwnerAddr,
) -> Result<(), DmReceiveError> {
    if payload.sender != link_origin {
        return Err(DmReceiveError::SenderImpersonation);
    }
    Ok(())
}

/// Compute the AAD for a DM Space's encrypted messages: canonical CBOR
/// encoding of the Space's `dedupe_key()`. Stable across cross-SpaceId
/// dedupe collapses (per ZEB-219 §"Why dedupe_key not space_id").
pub fn compute_aad(space: &Space) -> Result<Vec<u8>, AadComputeError> {
    canonical_cbor_encode(&space.dedupe_key())
        .map_err(|e| AadComputeError::Encode(e.to_string()))
}
```

You may need to add `impl CanonicalPayloadSealed/CanonicalPayload for DedupeKey` in `owner_state_types.rs` if it isn't already canonical-encodable. Check first:

```bash
grep -n "DedupeKey" src-tauri/src/owner_state_types.rs | head -10
```

If `DedupeKey` isn't already a `CanonicalPayload`, register it next to the existing macro invocation around line 272.

Add `mod dm_crypto;` to `lib.rs`.

- [ ] **Step 4: Run the tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml dm_crypto 2>&1 | tail -30
```

Expected: all nine tests pass.

- [ ] **Step 5: Verification gates + commit**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
git add -A
git commit -m "$(cat <<'EOF'
feat(zeb-216-phase1): dm_crypto module — encrypt/decrypt/AAD/sender-binding

Per ZEB-216 §"Encryption helpers (Phase 1)". ChaCha20-Poly1305 AEAD
with random 12-byte nonce, version-byte 0x01 prefix, length-gate
before slicing prevents panics on truncated blobs. Decrypt iterates
current key + prior_content_keys in stored order — fallback decryption
for messages encrypted under superseded keys. AAD derived from
space.dedupe_key() so it survives cross-SpaceId dedupe collapse.
verify_sender_binding blocks cross-owner sender impersonation by
comparing MessagePayload.sender against the link-origin OwnerAddr.
EOF
)"
```

---

## Task 7: Extend lww_merge_space + dedupe canonicalization with merge_prior_content_keys

**Files:**
- Modify: `src-tauri/src/owner_state_crdt.rs` (extend `lww_merge_space` to use `merge_prior_content_keys`; add the helper)

**Context:** Per ZEB-216 §"Dedupe-merge cap rule (Phase 1)" and ZEB-219 §"Cap policy". Two cases call this:
1. **Cross-SpaceId dedupe collapse**: different ULIDs collide on dedupe_key. Loser's content_key must roll into winner's prior_content_keys (then cap rule applies).
2. **Same-SpaceId LWW merge**: both sides have the same content_key (no rotation in v1), but their prior_content_keys lists may differ. Union, dedup, cap.

The helper `merge_prior_content_keys` handles both — for case 2, pass `loser_current = winner_current` and the contribution gets filtered out by the "remove current key from prior list" step.

The 5-Space convergence test from ZEB-219 is the canonical regression: K₃<K₂<K₄<K₅<K₁ lex, cap=2, two distinct merge orders → both yield prior_content_keys Vec equal to `[K₃, K₂]` (smallest two in ascending lex).

- [ ] **Step 1: Write failing test for the 5-Space convergence**

Add to `owner_state_crdt.rs` test module:

```rust
#[cfg(test)]
mod merge_prior_content_keys_tests {
    use super::*;
    use crate::owner_state_types::{
        DmContentKey, Hlc, OwnerAddr, Space, SpaceId, SpaceKind,
    };

    fn key(byte: u8) -> DmContentKey {
        DmContentKey::new([byte; 32])
    }

    fn dm_space(id_byte: u8, content_key: DmContentKey, hlc_ms: u64) -> Space {
        Space {
            id: SpaceId([id_byte; 16]),
            kind: SpaceKind::Dm,
            parent: None,
            community_id: None,
            name: "x".into(),
            transport: None,
            members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
            custom_name: None,
            notification_pref: None,
            left_at: None,
            created_at: Hlc { wall_ms: hlc_ms, logical: 0, device_id: "d".into() },
            updated_at: Hlc { wall_ms: hlc_ms, logical: 0, device_id: "d".into() },
            content_key: Some(content_key),
            prior_content_keys: vec![],
        }
    }

    /// 5-Space convergence test from ZEB-219 §"Why first N of sorted":
    /// K₃<K₂<K₄<K₅<K₁ lex, cap=2, two distinct merge orders → both yield
    /// the same prior_content_keys Vec (smallest two in ascending lex).
    #[test]
    fn dedupe_merge_prior_content_keys_5_space_convergence() {
        // Choose first bytes that give us the desired lex ordering:
        // K3 = [0x10..], K2 = [0x20..], K4 = [0x30..], K5 = [0x40..], K1 = [0x50..]
        // So K3 < K2 < K4 < K5 < K1 lex.
        let k1 = key(0x50);
        let k2 = key(0x20);
        let k3 = key(0x10);
        let k4 = key(0x30);
        let k5 = key(0x40);

        // Each of S1..S5 has a different ULID byte (so they're distinct
        // by id) but all share the same dedupe_key (sorted members).
        // S1 has the smallest id_byte so it'll be the dedupe winner.
        let s1 = dm_space(0x01, k1.clone(), 5);
        let s2 = dm_space(0x02, k2.clone(), 1);
        let s3 = dm_space(0x03, k3.clone(), 2);
        let s4 = dm_space(0x04, k4.clone(), 3);
        let s5 = dm_space(0x05, k5.clone(), 4);

        // Apply order P: [S2, S3, S4, S5, S1]
        let mut state_p = OwnerState::default();
        for s in [s2.clone(), s3.clone(), s4.clone(), s5.clone(), s1.clone()] {
            state_p.apply_space_with_canonicalization(s);
        }

        // Apply order Q: [S5, S4, S3, S2, S1]
        let mut state_q = OwnerState::default();
        for s in [s5.clone(), s4.clone(), s3.clone(), s2.clone(), s1.clone()] {
            state_q.apply_space_with_canonicalization(s);
        }

        // Convergence assertion: both orders yield byte-identical
        // prior_content_keys on the surviving (S1) Space.
        let p_winner = state_p.spaces.get(&SpaceId([0x01; 16])).expect("S1 survives");
        let q_winner = state_q.spaces.get(&SpaceId([0x01; 16])).expect("S1 survives");

        let p_prior: Vec<[u8; 32]> = p_winner.prior_content_keys.iter()
            .map(|k| *k.as_bytes()).collect();
        let q_prior: Vec<[u8; 32]> = q_winner.prior_content_keys.iter()
            .map(|k| *k.as_bytes()).collect();

        assert_eq!(p_prior, q_prior, "convergence: orders P and Q must yield identical prior_content_keys");

        // Identity-of-content assertion: cap=MAX_PRIOR_CONTENT_KEYS=16 for
        // production, but with 5 keys total all four losers fit. The loser
        // current_keys are k2..k5; winner current is k1, which MUST NOT
        // appear in prior. Sorted ascending: [k3, k2, k4, k5].
        assert_eq!(p_prior.len(), 4);
        assert_eq!(p_prior[0], *k3.as_bytes());
        assert_eq!(p_prior[1], *k2.as_bytes());
        assert_eq!(p_prior[2], *k4.as_bytes());
        assert_eq!(p_prior[3], *k5.as_bytes());
    }

    #[test]
    fn merge_prior_content_keys_filters_winner_current() {
        let winner_current = key(0x10);
        let loser_current = key(0x20);
        // Winner's prior includes a duplicate of winner_current — must
        // be filtered out.
        let winner_prior = vec![winner_current.clone(), key(0x30)];
        let loser_prior = vec![key(0x40)];
        let merged = merge_prior_content_keys(
            &winner_current, &winner_prior, &loser_current, &loser_prior,
        );
        let merged_bytes: Vec<[u8; 32]> = merged.iter().map(|k| *k.as_bytes()).collect();
        // Sorted ascending: 0x20, 0x30, 0x40 (no 0x10).
        assert_eq!(merged_bytes, vec![[0x20; 32], [0x30; 32], [0x40; 32]]);
    }

    #[test]
    fn merge_prior_content_keys_caps_at_max() {
        let winner_current = key(0xff);
        let winner_prior = vec![];
        let loser_current = key(0xfe);
        // Loser's prior has way more than MAX_PRIOR_CONTENT_KEYS entries.
        let loser_prior: Vec<DmContentKey> =
            (0u8..30).map(|i| key(i)).collect();
        let merged = merge_prior_content_keys(
            &winner_current, &winner_prior, &loser_current, &loser_prior,
        );
        // Cap is 16. Smallest 16 of {0..29, loser_current=0xfe} after
        // sort = [0..15] (loser_current and keys 16..29 don't make the cut).
        assert_eq!(merged.len(), crate::owner_state_types::MAX_PRIOR_CONTENT_KEYS);
        for (i, k) in merged.iter().enumerate() {
            assert_eq!(k.as_bytes(), &[i as u8; 32]);
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test --manifest-path src-tauri/Cargo.toml merge_prior_content_keys 2>&1 | tail -20
```

Expected: compile errors — `merge_prior_content_keys` doesn't exist; `lww_merge_space` doesn't merge prior_content_keys.

- [ ] **Step 3: Implement the helper and integrate into `lww_merge_space`**

In `owner_state_crdt.rs`, add:

```rust
use crate::owner_state_types::{
    DmContentKey, MAX_PRIOR_CONTENT_KEYS, /* ... existing imports ... */
};

/// Merge two sides' content keys per ZEB-216 §"Dedupe-merge cap rule":
///   1. Take winner.prior, plus loser.current as a one-element addition,
///      plus loser.prior.
///   2. Filter out winner.current (the active key MUST NOT appear in prior).
///   3. Sort ascending lex by raw key bytes.
///   4. Dedup (set-semantics on byte equality).
///   5. Truncate to MAX_PRIOR_CONTENT_KEYS.
///
/// For same-SpaceId merges, pass `loser_current == winner_current` — the
/// duplicate gets filtered in step 2 so the operation is the same.
pub(crate) fn merge_prior_content_keys(
    winner_current: &DmContentKey,
    winner_prior: &[DmContentKey],
    loser_current: &DmContentKey,
    loser_prior: &[DmContentKey],
) -> Vec<DmContentKey> {
    let mut all: Vec<DmContentKey> = winner_prior
        .iter()
        .cloned()
        .chain(std::iter::once(loser_current.clone()))
        .chain(loser_prior.iter().cloned())
        .filter(|k| k.as_bytes() != winner_current.as_bytes())
        .collect();
    all.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    all.dedup_by(|a, b| a.as_bytes() == b.as_bytes());
    all.truncate(MAX_PRIOR_CONTENT_KEYS);
    all
}
```

Update `lww_merge_space` to invoke it. The existing function returns a fresh `Space`; replace the trivial passthrough from Task 2 with:

```rust
// Note: for same-SpaceId merges, content_key on both sides MUST be
// equal (v1 has no rotation). The merge picks one (LWW on updated_at)
// and rolls the other side's prior_content_keys through the cap rule.
// For cross-SpaceId dedupe (caller swaps existing/incoming based on
// id ordering), the loser side's `content_key` becomes a "loser_current"
// contribution to the merged prior_content_keys.
let (winner_current, loser_current, winner_prior, loser_prior) = match (
    &existing.content_key, &incoming.content_key,
) {
    (Some(a), Some(b)) => {
        // For same-SpaceId merges, both sides have the same key, so
        // the cap operation is just "union the priors". For cross-SpaceId
        // collapse the caller already swapped existing/incoming so
        // existing IS the winner (smaller id).
        (a, b, &existing.prior_content_keys[..], &incoming.prior_content_keys[..])
    }
    (None, None) => {
        // Non-DM kinds — no content_key handling needed.
        // Fall through to the rest of lww_merge_space without merging
        // content_key fields.
        return /* current return path with content_key: None */;
    }
    _ => {
        // Mixed Some/None across the same dedupe_key would be an
        // invariant violation (both sides should be DM/group-DM with
        // content_key set). Fall back to LWW on the field; validate_invariants
        // will catch the inconsistency on the way out if it surfaces.
        // ... (see actual implementation when writing)
    }
};
let merged_prior = merge_prior_content_keys(
    winner_current, winner_prior, loser_current, loser_prior,
);
// Use winner's content_key as the merged content_key:
let merged_content_key = Some(winner_current.clone());
```

(The exact code shape depends on `lww_merge_space`'s current structure. Read it carefully and adapt — the principle is: the function knows which side wins per-field via `updated_at` HLC, but for `content_key`/`prior_content_keys` the merge follows the cap rule above instead of straight LWW.)

- [ ] **Step 4: Run the tests**

```bash
cargo test --manifest-path src-tauri/Cargo.toml 2>&1 | tail -30
```

Expected: all three new tests pass; existing tests still pass.

- [ ] **Step 5: Verification gates + commit**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
git add -u
git commit -m "$(cat <<'EOF'
feat(zeb-216-phase1): merge_prior_content_keys cap rule + lww_merge_space integration

Per ZEB-216 §"Dedupe-merge cap rule (Phase 1)" and ZEB-219 §"Cap policy".
Lex-sort merged set, dedup, cap at MAX_PRIOR_CONTENT_KEYS=16, filter
out winner.current. Order-independent (CRDT-convergent under multi-merge).
Integrated into lww_merge_space which is called by both same-SpaceId LWW
merge and cross-SpaceId dedupe collapse paths.

Includes the 5-Space convergence regression test from ZEB-219:
K₃<K₂<K₄<K₅<K₁ lex, two distinct merge orders → both yield byte-identical
prior_content_keys Vec [K₃, K₂, K₄, K₅] (smallest four in ascending lex,
all four losers fit under cap=16).
EOF
)"
```

---

## Task 8: Persistence round-trip with new fields + OwnerDeviceCache

**Files:**
- Modify (verify only — likely no code change needed): `src-tauri/src/owner_state_persist.rs`
- Add tests covering the round-trip

**Context:** Per ZEB-216 §"Modified Rust files". Persistence is serde-driven (CBOR via the existing crypto pipeline), so the new Space fields and `OwnerState.owner_device_cache` field should round-trip for free. This task verifies that assumption with explicit tests and surfaces any persistence-layer bug before Phase 2 builds on top.

- [ ] **Step 1: Write failing test for persistence round-trip with DM state**

Find the existing persistence test module (`grep -rn "fn.*persist.*round" src-tauri/src/owner_state_persist.rs` or look in `src-tauri/tests/` for an integration test). Add:

```rust
#[test]
fn persist_round_trip_with_dm_state() {
    use crate::owner_state_crdt::OwnerState;
    use crate::owner_state_types::{
        DeviceIdentityHash, DmContentKey, Hlc, OwnerAddr, OwnerDeviceEntry, Space,
        SpaceId, SpaceKind,
    };

    let mut state = OwnerState::default();

    // Insert a DM Space with content_key + prior_content_keys.
    let dm_space = Space {
        id: SpaceId([1; 16]),
        kind: SpaceKind::Dm,
        parent: None, community_id: None,
        name: "alice-bob".into(),
        transport: None,
        members: vec![OwnerAddr([1; 16]), OwnerAddr([2; 16])],
        custom_name: None, notification_pref: None, left_at: None,
        created_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        updated_at: Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
        content_key: Some(DmContentKey::new([0xaa; 32])),
        prior_content_keys: vec![DmContentKey::new([0xbb; 32])],
    };
    state.apply_space_with_canonicalization(dm_space.clone());

    // Insert OwnerDeviceCache entries.
    state.apply_owner_device_update(
        OwnerAddr([2; 16]),
        vec![DeviceIdentityHash([7; 16]), DeviceIdentityHash([8; 16])],
        Hlc { wall_ms: 1, logical: 0, device_id: "d".into() },
    );

    // Round-trip through the persistence path (use whichever serialize
    // entry point owner_state_persist provides — typically a save/load
    // pair operating on `OwnerState`).
    // ... call the existing persist + reload helpers ...

    // Assertions:
    let loaded_space = loaded.spaces.get(&SpaceId([1; 16])).expect("DM Space persisted");
    assert_eq!(
        loaded_space.content_key.as_ref().map(|k| *k.as_bytes()),
        Some([0xaa; 32]),
    );
    assert_eq!(loaded_space.prior_content_keys.len(), 1);
    assert_eq!(loaded_space.prior_content_keys[0].as_bytes(), &[0xbb; 32]);

    let cache_entry = loaded.owner_device_cache.devices.get(&OwnerAddr([2; 16]))
        .expect("OwnerDeviceCache entry persisted");
    assert_eq!(cache_entry.devices.len(), 2);
    assert_eq!(cache_entry.devices[0], DeviceIdentityHash([7; 16]));
}
```

(The exact test scaffolding depends on how owner_state_persist exposes its API — read the existing persistence tests for patterns.)

- [ ] **Step 2: Run the test to verify it fails or passes**

```bash
cargo test --manifest-path src-tauri/Cargo.toml persist_round_trip_with_dm_state 2>&1 | tail -20
```

If it passes immediately: serde + the existing persistence path already cover the new fields (expected — this is the "verify it's free" check).

If it fails: there's a persistence-layer assumption that needs fixing (e.g., a hardcoded list of fields somewhere; CanonicalPayload registration missing for OwnerState since adding the new field may have changed its encoded shape).

- [ ] **Step 3: Fix any failing assertion**

If something needs fixing in `owner_state_persist.rs`, do the minimum-change fix and re-run.

- [ ] **Step 4: Verification gates + commit**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
git add -u
git commit -m "$(cat <<'EOF'
test(zeb-216-phase1): persistence round-trip with DM state + OwnerDeviceCache

Verifies that the new Space.content_key, Space.prior_content_keys, and
OwnerState.owner_device_cache fields persist + reload correctly through
the existing owner_state_persist pipeline. Should be free with serde +
the canonical-CBOR registration already in place; this test surfaces
any drift before Phase 2 builds on top.
EOF
)"
```

---

## Task 9: Push branch + open PR

**Files:** none (process step)

**Context:** Per memory rule, never invent Linear IDs — Phase 1 implementation is part of ZEB-216 (the umbrella); the spec was shipped under PR #77. Reference ZEB-216 in the PR title/body without minting a sub-ID. Per memory rule, branch must stay on origin/main lineage; if main has moved during implementation, rebase before push.

- [ ] **Step 1: Verify branch is rebased on latest origin/main**

```bash
git fetch origin
git log --oneline origin/main..HEAD | head
git log --oneline HEAD..origin/main | head
```

If `HEAD..origin/main` is non-empty, rebase:

```bash
git rebase origin/main
```

- [ ] **Step 2: Final verification gates one more time**

```bash
cargo fmt --all --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npx vitest run --root .
npx tsc --noEmit
```

All five must pass clean.

- [ ] **Step 3: Push the branch**

```bash
git push -u origin zeb-216-sub-b-phase1-encryption-primitives
```

- [ ] **Step 4: Open the PR**

```bash
gh pr create --title "feat(zeb-216-phase1): DM encryption primitives — Space fields, dm_crypto, dm_envelope, OwnerDeviceCache" --body "$(cat <<'EOF'
## Summary

Phase 1 of [ZEB-216 Sub-B](https://linear.app/zeblith/issue/ZEB-216) (DM transport). Implements the encryption-primitives contract from the design spec ([PR #77](https://github.com/zeblithic/harmony-client/pull/77)) without any transport, IPC, or UI — pure data + crypto + CRDT.

### What ships

- **New wire newtypes**: `DmContentKey` (bstr(32) + ZeroizeOnDrop + redacted Debug) and `DeviceIdentityHash` (bstr(16))
- **Constants**: `MAX_PRIOR_CONTENT_KEYS = 16`, `MAX_DEVICES_PER_OWNER = 32`
- **Space struct fields**: `content_key: Option<DmContentKey>` and `prior_content_keys: Vec<DmContentKey>` with `serde(default)` for backward-compatible deserialization
- **Validate invariants extension**: DM/group-DM kinds MUST have content_key; non-DM MUST NOT; content_key ∉ prior_content_keys; prior cap ≤ 16
- **OwnerDeviceCache** + `apply_owner_device_update`: LWW on `learned_at` HLC, dedupe + cap input devices to bound cache memory and prevent DoS
- **dm_envelope.rs** (new module): `MessagePayload`, `DmInvite`, `DmCidNotify`, `DmAck`, `DmPacket` discriminated codec (0x01/0x02/0x03)
- **dm_crypto.rs** (new module): `encrypt_dm_message` / `decrypt_dm_message` (ChaCha20-Poly1305 AEAD, version-byte 0x01 prefix, length-gated, prior_content_keys fallback), `verify_sender_binding`, `compute_aad` (binds AAD to space.dedupe_key())
- **merge_prior_content_keys**: cap rule integrated into `lww_merge_space` for both same-SpaceId LWW and cross-SpaceId dedupe collapse paths. Order-independent (CRDT-convergent).
- **Persistence**: round-trip verified for new Space fields + OwnerDeviceCache through existing serde pipeline

### What does NOT ship (per phase decomposition)

- DM transport (Phase 2/3)
- send_dm IPC (Phase 2)
- harmony-runtime SendUnicastToDevice primitive (Phase 3a)
- Reticulum delivery + 30-day expiration (Phase 3b)
- NavService DM kinds + UI (Phase 4)

## Test plan

- [x] All Phase 1 unit tests green: bstr serialization, ZeroizeOnDrop, redacted Debug, Space invariants (4 cases), OwnerDeviceCache LWW + dedupe + cap, dm_crypto round-trip + AAD-mismatch + version-unknown + length-gate + tampered-ciphertext + prior-keys-fallback + sender-binding (mismatch + match), DmPacket discriminant codec all 3 variants + unknown-discriminant + empty-bytes
- [x] 5-Space convergence regression test (the canonical case from ZEB-219 §"Cap policy")
- [x] Persistence round-trip with DM state + OwnerDeviceCache
- [x] All existing Sub-A tests (Phase 1, 2, 3a, 3b) still green
- [x] cargo fmt + clippy + test + vitest + tsc all clean

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

The PR URL prints to stdout after creation; report it back.

- [ ] **Step 5: Mark all Phase 1 tasks complete**

(Process — no code change. Update tracking system.)

---

## What comes next (informational, not part of this plan)

After Phase 1 merges:
- **Phase 2** plan + implementation (dm_outbox skeleton + send_dm IPC + state machine, stub transport)
- **Phase 3a** plan + implementation (harmony-runtime companion PR adding `SendUnicastToDevice` + `UnicastReceived`)

Phase 2 and Phase 3a are independent of each other and could run in parallel. Phase 3b depends on both.
