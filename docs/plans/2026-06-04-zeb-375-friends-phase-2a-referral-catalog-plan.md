# Friends Phase 2a — Referral Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a Harmony user mark friends as `referrable` and browse a *signed* catalog of whom an existing friend could introduce them to, over a new `harmony/friend-pex/v1` ALPN — read-only, opt-in, authenticated, with the Phase-1b handshake left byte-for-byte untouched.

**Architecture:** A new `referral_catalog.rs` module defines three strict-CBOR wire types (`CatalogRequest`, `ReferralCatalog`, `ReferralEntry`) + their sign/verify cores, reusing the Phase-1b auth (`verify_enrolled_device`), codec (`FRIEND_MAX_PACKET_LEN` + `decode_strict` + `[u32 LE len][body]` framing), and serde helpers. A sibling `IrohFriendPexAcceptor` serves catalogs on a dedicated ALPN routed through the existing `MultiplexHandshakeDispatcher` (extended to a third slot). Two Tauri IPCs (`set_friend_referrable`, `browse_friend_referrals`) + FriendsPanel UI complete the slice. Browse resolves the friend via Case-D (`resolve_friend_case_d`) and dials the PEX ALPN.

**Tech Stack:** Rust (Tauri v2, ciborium, ed25519-dalek), Svelte 5 runes, TypeScript, cargo-nextest, vitest.

**Spec:** `docs/specs/2026-06-04-friends-phase-2a-referral-catalog-design.md` (ZEB-375).

**Gates (must stay green every commit):** from `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. From repo root: `npx tsc --noEmit`; `npx vitest run`.

**Do NOT commit:** `src-tauri/gen/schemas/*.json` (Windows build churn), `.playwright-scratch/`, unrelated stray docs. Stage files explicitly — never `git add -A`.

---

## Pre-flight (controller does once, before Task 1)

1. Discard the Windows schema churn so it can't ride along: `git checkout -- src-tauri/gen/schemas/windows-schema.json`.
2. Update main and branch from it (NOT from the `zeb-371-friends-phase-1b` tip, which is the pre-squash commit): `git fetch origin`, `git checkout main`, `git reset --hard origin/main` (confirm HEAD == `20be1cb…`), `git checkout -b zeb-375-friends-phase-2a-referral-catalog`.
3. The untracked spec `docs/specs/2026-06-04-friends-phase-2a-referral-catalog-design.md` and this plan follow across branches; commit them in Task 1's commit.

---

## File Structure

| File | Responsibility | Create/Modify |
|---|---|---|
| `src-tauri/src/referral_catalog.rs` | Wire types, preimages, codecs, sign/verify, catalog build, serve-decision core | **Create** |
| `src-tauri/src/lib.rs` | `pub mod referral_catalog;`; `set_friend_referrable` + `browse_friend_referrals` IPCs; invoke_handler registration; PEX acceptor wiring | Modify |
| `src-tauri/src/iroh_endpoint.rs` | `HARMONY_FRIEND_PEX_V1` const + advertise in both `.alpns` | Modify |
| `src-tauri/src/zenoh_iroh_transport.rs` | Accept-loop allowlist: forward PEX ALPN to the multiplexer | Modify |
| `src-tauri/src/iroh_friend_acceptor.rs` | `FriendDispatchTarget::Pex`, `route_handshake_alpn`, `MultiplexHandshakeDispatcher` 3rd slot; `IrohFriendPexAcceptor` IO handler | Modify |
| `src/lib/friend-service.ts` | `setReferrable`, `browseReferrals`, `ReferralView`/`ReferralCatalogDto` types | Modify |
| `src/lib/friend-service.test.ts` | vitest for the two new service methods | Modify |
| `src/lib/components/FriendsPanel.svelte` | Per-friend `referrable` toggle + read-only "browse referrals" view | Modify |
| `src-tauri/tests/wire_format_zeb375_pex_fixtures.rs` | Wire-format pin fixtures for the three new types | **Create** |

**Import anchors (verified):** auth core `iroh_friend_acceptor::{verify_enrolled_device, authenticate_friend_request}` (rs:398, 575); codec `FRIEND_MAX_PACKET_LEN=256*1024` (rs:70), `decode_strict` (rs:377); serde `crate::owner_state_types::{serialize_bytes_as_bstr, deserialize_bytes_from_bstr, OwnerAddr, Hlc}` (pub(crate)); `crate::friend_graph::{deserialize_capped_display, MAX_FRIEND_DISPLAY_LEN=256}`; `EnrollmentCert` via the same `use` path `iroh_friend_acceptor.rs` imports it; Case-D `pkarr_friend_publisher::resolve_friend_case_d(resolver, &[u8;32], &[u8;16]) -> Result<Option<Vec<u8>>,String>` (rs:132); resolve+dial template `connectivity_resolve_friend` (lib.rs:34358) + `connectivity_add_friend_by_key_inner` dial (lib.rs:34774); IPC list `lib.rs:35967`; CRDT-write+notify template `unfriend_inner` (lib.rs:33962); IPC to mirror `set_friend_auto_accept` (lib.rs:34569).

---

## Task 1: `referral_catalog.rs` — wire types, preimages, codecs

**Files:**
- Create: `src-tauri/src/referral_catalog.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod referral_catalog;` beside the other `pub mod friend_*;` declarations)

- [ ] **Step 1: Write failing tests** (in `referral_catalog.rs` `#[cfg(test)] mod tests`)

```rust
#[test]
fn catalog_request_round_trips() {
    let req = sample_request();
    let bytes = encode_catalog_request(&req).expect("encode");
    assert_eq!(decode_catalog_request(&bytes).expect("decode"), req);
}

#[test]
fn referral_catalog_round_trips() {
    let cat = sample_catalog();
    let bytes = encode_referral_catalog(&cat).expect("encode");
    assert_eq!(decode_referral_catalog(&bytes).expect("decode"), cat);
}

#[test]
fn decode_rejects_trailing_bytes() {
    let mut bytes = encode_catalog_request(&sample_request()).unwrap();
    bytes.push(0x00);
    assert!(decode_catalog_request(&bytes).is_err());
}

#[test]
fn decode_rejects_oversize() {
    // a body claiming > FRIEND_MAX_PACKET_LEN is rejected by length, not parsed
    let huge = vec![0u8; super::PEX_MAX_PACKET_LEN + 1];
    assert!(decode_referral_catalog(&huge).is_err());
}

#[test]
fn display_is_capped_on_decode() {
    // an entry display over MAX_FRIEND_DISPLAY_LEN is a hard decode error
    let mut cat = sample_catalog();
    cat.entries[0].display = Some("x".repeat(super::super::friend_graph::MAX_FRIEND_DISPLAY_LEN + 1));
    let bytes = encode_referral_catalog(&cat).unwrap();
    assert!(decode_referral_catalog(&bytes).is_err());
}

#[test]
fn request_and_catalog_preimages_are_domain_separated() {
    let a = OwnerAddr([1u8; 16]);
    let b = OwnerAddr([2u8; 16]);
    let req_pre = catalog_request_sig_preimage(a, b);
    let cat_pre = referral_catalog_sig_preimage(a, b, &[], &hlc(7));
    assert_ne!(req_pre, cat_pre, "distinct domains must never collide");
}
```

Add test helpers `sample_request()`, `sample_catalog()`, `hlc(n)` building deterministic values (fixed `OwnerAddr`, `sig: [9u8;64]`, a `mint_test_owner(0x42).cert` for the embedded `EnrollmentCert`).

- [ ] **Step 2: Run, verify it fails to compile** (`cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(referral_catalog)'`) — expected: module/types not defined.

- [ ] **Step 3: Implement the module**

```rust
//! ZEB-375 (Friends Phase 2a): referral-catalog wire types + codecs for the
//! `harmony/friend-pex/v1` awareness sub-protocol. Strict CBOR, single-char map
//! keys, bounded decode with trailing-byte rejection — mirrors
//! `iroh_friend_acceptor`'s handshake codec so the two sub-protocols share
//! one wire discipline.

use serde::{Deserialize, Serialize};

use crate::friend_graph::deserialize_capped_display;
use crate::owner_state_types::{
    deserialize_bytes_from_bstr, serialize_bytes_as_bstr, Hlc, OwnerAddr,
};
// EnrollmentCert: import via the SAME path iroh_friend_acceptor.rs uses (copy its `use`).
use <enrollment_cert_path>::EnrollmentCert;

/// Same bound as the handshake codec — a friend-PEX body never exceeds it.
pub const PEX_MAX_PACKET_LEN: usize = 256 * 1024;
/// Hard cap on entries served in one catalog (truncation is logged, never silent).
pub const MAX_REFERRAL_ENTRIES: usize = 256;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReferralCodecError {
    #[error("referral packet too large: {len} > {max}")]
    TooLarge { len: usize, max: usize },
    #[error("referral encode failed: {0}")]
    Encode(String),
    #[error("referral decode failed: {0}")]
    Decode(String),
    #[error("trailing bytes after referral CBOR")]
    TrailingBytes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferralEntry {
    #[serde(rename = "o")]
    pub peer_owner: OwnerAddr,
    #[serde(rename = "n", default, deserialize_with = "deserialize_capped_display")]
    pub display: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferralCatalog {
    #[serde(rename = "a")]
    pub author: OwnerAddr,
    #[serde(rename = "e")]
    pub entries: Vec<ReferralEntry>,
    #[serde(rename = "t")]
    pub at: Hlc,
    #[serde(rename = "c")]
    pub enrollment: EnrollmentCert,
    #[serde(rename = "s", serialize_with = "serialize_bytes_as_bstr", deserialize_with = "deserialize_bytes_from_bstr")]
    pub sig: [u8; 64],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRequest {
    #[serde(rename = "a")]
    pub from_addr: OwnerAddr,
    #[serde(rename = "d")]
    pub to_addr: OwnerAddr,
    #[serde(rename = "c")]
    pub enrollment: EnrollmentCert,
    #[serde(rename = "s", serialize_with = "serialize_bytes_as_bstr", deserialize_with = "deserialize_bytes_from_bstr")]
    pub sig: [u8; 64],
}

fn decode_strict<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ReferralCodecError> {
    if bytes.len() > PEX_MAX_PACKET_LEN {
        return Err(ReferralCodecError::TooLarge { len: bytes.len(), max: PEX_MAX_PACKET_LEN });
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let v = ciborium::from_reader(&mut cursor).map_err(|e| ReferralCodecError::Decode(e.to_string()))?;
    if cursor.position() as usize != bytes.len() {
        return Err(ReferralCodecError::TrailingBytes);
    }
    Ok(v)
}

pub fn encode_catalog_request(req: &CatalogRequest) -> Result<Vec<u8>, ReferralCodecError> {
    let mut out = Vec::new();
    ciborium::into_writer(req, &mut out).map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
    Ok(out)
}
pub fn decode_catalog_request(bytes: &[u8]) -> Result<CatalogRequest, ReferralCodecError> { decode_strict(bytes) }

pub fn encode_referral_catalog(cat: &ReferralCatalog) -> Result<Vec<u8>, ReferralCodecError> {
    let mut out = Vec::new();
    ciborium::into_writer(cat, &mut out).map_err(|e| ReferralCodecError::Encode(e.to_string()))?;
    Ok(out)
}
pub fn decode_referral_catalog(bytes: &[u8]) -> Result<ReferralCatalog, ReferralCodecError> { decode_strict(bytes) }

/// Bytes R's device-#2 key signs for a `CatalogRequest`. `"hcr1"` domain tag +
/// requester + target (binding `to_addr` blocks re-aiming a captured request).
pub fn catalog_request_sig_preimage(from_addr: OwnerAddr, to_addr: OwnerAddr) -> Vec<u8> {
    #[derive(Serialize)]
    struct P { d: &'static str, a: OwnerAddr, t: OwnerAddr }
    let mut out = Vec::new();
    ciborium::into_writer(&P { d: "hcr1", a: from_addr, t: to_addr }, &mut out).expect("fixed-shape encode is infallible");
    out
}

/// Bytes F's device-#2 key signs for a `ReferralCatalog`. `"hrc1"` domain tag +
/// author + subject (binding `subject` blocks re-showing a catalog to another
/// requester) + the served entries + clock.
pub fn referral_catalog_sig_preimage(author: OwnerAddr, subject: OwnerAddr, entries: &[ReferralEntry], at: &Hlc) -> Vec<u8> {
    #[derive(Serialize)]
    struct P<'a> { d: &'static str, a: OwnerAddr, s: OwnerAddr, e: &'a [ReferralEntry], t: &'a Hlc }
    let mut out = Vec::new();
    ciborium::into_writer(&P { d: "hrc1", a: author, s: subject, e: entries, t: at }, &mut out).expect("fixed-shape encode is infallible");
    out
}
```

Resolve `<enrollment_cert_path>` by copying the exact `use … EnrollmentCert;` line from `iroh_friend_acceptor.rs`. Add `pub mod referral_catalog;` in `lib.rs`.

- [ ] **Step 4: Run, verify pass** (`cargo nextest run --locked --features test-fixtures -E 'test(referral_catalog)'` → all pass) and `cargo fmt --all`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.

- [ ] **Step 5: Commit** (also stage the spec + this plan)

```bash
git add src-tauri/src/referral_catalog.rs src-tauri/src/lib.rs \
  docs/specs/2026-06-04-friends-phase-2a-referral-catalog-design.md \
  docs/plans/2026-06-04-zeb-375-friends-phase-2a-referral-catalog-plan.md
git commit -m "feat(zeb-375): referral-catalog wire types + codecs (friend-PEX awareness)"
```

---

## Task 2: Catalog sign + verify cores

**Files:** Modify `src-tauri/src/referral_catalog.rs` (+ its test module).

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn signed_catalog_verifies_and_tamper_is_rejected() {
    let owner = mint_test_owner(0x11);           // F
    let subject = OwnerAddr([0x22; 16]);          // R
    let entries = vec![ReferralEntry { peer_owner: OwnerAddr([3u8;16]), display: Some("bob".into()) }];
    let cat = sign_referral_catalog(&owner.device_key, owner.owner, subject, entries.clone(), hlc(7), owner.cert.clone());
    // valid
    assert!(verify_referral_catalog(&cat, owner.owner, subject).is_ok());
    // wrong subject
    assert!(verify_referral_catalog(&cat, owner.owner, OwnerAddr([0x99;16])).is_err());
    // wrong expected author
    assert!(verify_referral_catalog(&cat, OwnerAddr([0x88;16]), subject).is_err());
    // tampered entries
    let mut t = cat.clone(); t.entries[0].display = Some("eve".into());
    assert!(verify_referral_catalog(&t, owner.owner, subject).is_err());
}

#[test]
fn catalog_with_mismatched_cert_owner_is_rejected() {
    let f = mint_test_owner(0x11);
    let g = mint_test_owner(0x12);
    // sign with F's key but claim author == F while embedding G's cert
    let mut cat = sign_referral_catalog(&f.device_key, f.owner, OwnerAddr([2u8;16]), vec![], hlc(7), f.cert.clone());
    cat.enrollment = g.cert.clone();
    assert!(verify_referral_catalog(&cat, f.owner, OwnerAddr([2u8;16])).is_err());
}

#[test]
fn catalog_request_auth_enforces_to_addr_and_sig() {
    let r = mint_test_owner(0x21);
    let f_owner = OwnerAddr([0x42; 16]);
    let req = sign_catalog_request(&r.device_key, r.owner, f_owner, r.cert.clone());
    assert!(authenticate_catalog_request(&req, f_owner).is_ok());            // served by F
    assert!(authenticate_catalog_request(&req, OwnerAddr([0x43;16])).is_err()); // replayed to G → to_addr mismatch
    let mut bad = req.clone(); bad.sig[0] ^= 1;
    assert!(authenticate_catalog_request(&bad, f_owner).is_err());            // bad sig
}
```

Provide a `mint_test_owner`-derived helper exposing `{ owner: OwnerAddr, device_key: SigningKey, cert: EnrollmentCert }` (mirror `signed_request` in `iroh_friend_acceptor.rs` tests).

- [ ] **Step 2: Run, verify fail** (functions undefined).

- [ ] **Step 3: Implement**

```rust
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use crate::iroh_friend_acceptor::verify_enrolled_device;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReferralAuthError {
    #[error("enrollment/auth failed")]
    Auth,
    #[error("author mismatch")]
    AuthorMismatch,
    #[error("to_addr is not us")]
    WrongTarget,
    #[error("signature invalid")]
    SignatureInvalid,
}

pub fn sign_catalog_request(device2: &SigningKey, from_addr: OwnerAddr, to_addr: OwnerAddr, enrollment: EnrollmentCert) -> CatalogRequest {
    let sig = device2.sign(&catalog_request_sig_preimage(from_addr, to_addr)).to_bytes();
    CatalogRequest { from_addr, to_addr, enrollment, sig }
}

/// Verify a request: cert chain (Master + owner match) → device-#2 key, target
/// is us, sig over the `"hcr1"` preimage. Caller still checks "is this an Active
/// friend" separately (that's a FriendGraph read, not auth).
pub fn authenticate_catalog_request(req: &CatalogRequest, self_owner: OwnerAddr) -> Result<(), ReferralAuthError> {
    if req.to_addr != self_owner { return Err(ReferralAuthError::WrongTarget); }
    let device_key = verify_enrolled_device(&req.enrollment, req.from_addr).map_err(|_| ReferralAuthError::Auth)?;
    let vk = VerifyingKey::from_bytes(&device_key).map_err(|_| ReferralAuthError::SignatureInvalid)?;
    vk.verify_strict(&catalog_request_sig_preimage(req.from_addr, req.to_addr), &Signature::from_bytes(&req.sig))
        .map_err(|_| ReferralAuthError::SignatureInvalid)
}

pub fn sign_referral_catalog(device2: &SigningKey, author: OwnerAddr, subject: OwnerAddr, entries: Vec<ReferralEntry>, at: Hlc, enrollment: EnrollmentCert) -> ReferralCatalog {
    let sig = device2.sign(&referral_catalog_sig_preimage(author, subject, &entries, &at)).to_bytes();
    ReferralCatalog { author, entries, at, enrollment, sig }
}

/// Verify a catalog received from `expected_author`, addressed to us
/// (`expected_subject == self`): cert chain → device-#2 key, author matches the
/// friend we asked AND the embedded cert, sig over the `"hrc1"` preimage.
pub fn verify_referral_catalog(cat: &ReferralCatalog, expected_author: OwnerAddr, expected_subject: OwnerAddr) -> Result<(), ReferralAuthError> {
    if cat.author != expected_author { return Err(ReferralAuthError::AuthorMismatch); }
    let device_key = verify_enrolled_device(&cat.enrollment, cat.author).map_err(|_| ReferralAuthError::Auth)?;
    let vk = VerifyingKey::from_bytes(&device_key).map_err(|_| ReferralAuthError::SignatureInvalid)?;
    vk.verify_strict(&referral_catalog_sig_preimage(cat.author, expected_subject, &cat.entries, &cat.at), &Signature::from_bytes(&cat.sig))
        .map_err(|_| ReferralAuthError::SignatureInvalid)
}
```

- [ ] **Step 4: Run, verify pass** + fmt + clippy.
- [ ] **Step 5: Commit** — `feat(zeb-375): catalog request/response sign + verify (cert-chain + replay binding)`

---

## Task 3: Catalog build (filter + sign), pure

**Files:** Modify `src-tauri/src/referral_catalog.rs`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn collect_referrable_includes_only_active_referrable() {
    let mut fg = FriendGraph::default();
    fg.friends.insert(OwnerAddr([1;16]), entry(FriendStatus::Active,  true,  Some("a")));
    fg.friends.insert(OwnerAddr([2;16]), entry(FriendStatus::Active,  false, Some("b"))); // not referrable
    fg.friends.insert(OwnerAddr([3;16]), entry(FriendStatus::Pending, true,  Some("c"))); // not active
    fg.friends.insert(OwnerAddr([4;16]), entry(FriendStatus::Revoked, true,  Some("d"))); // tombstone
    let got = collect_referrable_entries(&fg);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].peer_owner, OwnerAddr([1;16]));
}

#[test]
fn collect_referrable_is_capped_and_sorted_stable() {
    let mut fg = FriendGraph::default();
    for i in 0..(MAX_REFERRAL_ENTRIES as u16 + 5) {
        let mut k = [0u8;16]; k[0] = (i >> 8) as u8; k[1] = i as u8;
        fg.friends.insert(OwnerAddr(k), entry(FriendStatus::Active, true, None));
    }
    let got = collect_referrable_entries(&fg);
    assert_eq!(got.len(), MAX_REFERRAL_ENTRIES);
}

#[test]
fn built_catalog_is_signed_for_subject() {
    let f = mint_test_owner(0x11);
    let mut fg = FriendGraph::default();
    fg.friends.insert(OwnerAddr([7;16]), entry(FriendStatus::Active, true, Some("g")));
    let subject = OwnerAddr([0x22;16]);
    let cat = build_referral_catalog(&fg, subject, f.owner, f.cert.clone(), &f.device_key, hlc(7));
    assert_eq!(cat.entries.len(), 1);
    assert!(verify_referral_catalog(&cat, f.owner, subject).is_ok());
}
```

`entry(status, referrable, display)` builds a `FriendEntry`. Import `FriendGraph`, `FriendEntry`, `FriendStatus` from `crate::friend_graph`.

- [ ] **Step 2: Run, verify fail.**
- [ ] **Step 3: Implement**

```rust
use crate::friend_graph::{FriendGraph, FriendStatus};

/// Active + `referrable` friends → entries, deterministic order (BTreeMap key
/// order), capped at `MAX_REFERRAL_ENTRIES`. Logs how many were dropped (no
/// silent truncation).
pub fn collect_referrable_entries(fg: &FriendGraph) -> Vec<ReferralEntry> {
    let mut out = Vec::new();
    let mut dropped = 0usize;
    for (owner, e) in fg.friends.iter() {
        if e.status == FriendStatus::Active && e.referrable {
            if out.len() < MAX_REFERRAL_ENTRIES {
                out.push(ReferralEntry { peer_owner: *owner, display: e.display.clone() });
            } else { dropped += 1; }
        }
    }
    if dropped > 0 {
        tracing::warn!(dropped, cap = MAX_REFERRAL_ENTRIES, "referral catalog truncated");
    }
    out
}

pub fn build_referral_catalog(fg: &FriendGraph, subject: OwnerAddr, self_owner: OwnerAddr, self_enrollment: EnrollmentCert, device2: &SigningKey, at: Hlc) -> ReferralCatalog {
    sign_referral_catalog(device2, self_owner, subject, collect_referrable_entries(fg), at, self_enrollment)
}
```

- [ ] **Step 4: Run, verify pass** + fmt + clippy.
- [ ] **Step 5: Commit** — `feat(zeb-375): build signed referral catalog from Active+referrable friends`

---

## Task 4: `IrohFriendPexAcceptor` — serve decision + IO handler

**Files:** Modify `src-tauri/src/iroh_friend_acceptor.rs` (or a new `iroh_friend_pex_acceptor.rs`; keep in `iroh_friend_acceptor.rs` to share private helpers like the framing). Decision: **new module `src-tauri/src/iroh_pex_acceptor.rs`** importing the public framing/auth — keeps the handshake file focused. Add `pub mod iroh_pex_acceptor;` to `lib.rs`.

- [ ] **Step 1: Write failing tests** (pure serve-decision core)

```rust
#[test]
fn serves_full_catalog_to_active_friend() {
    let f = mint_test_owner(0x11);        // us (the server)
    let r = mint_test_owner(0x22);        // requester, an Active friend of us
    let mut fg = FriendGraph::default();
    fg.friends.insert(r.owner, entry(FriendStatus::Active, false, Some("r"))); // r is our friend
    fg.friends.insert(OwnerAddr([7;16]), entry(FriendStatus::Active, true, Some("g"))); // referrable
    let req = sign_catalog_request(&r.device_key, r.owner, f.owner, r.cert.clone());
    let cat = serve_catalog_for_request(&fg, &req, f.owner, f.cert.clone(), &f.device_key, hlc(7)).expect("serve");
    assert_eq!(cat.entries.len(), 1);
    assert!(verify_referral_catalog(&cat, f.owner, r.owner).is_ok());
}

#[test]
fn serves_empty_catalog_to_non_friend() {
    let f = mint_test_owner(0x11);
    let stranger = mint_test_owner(0x33);          // authenticated, but NOT our friend
    let mut fg = FriendGraph::default();
    fg.friends.insert(OwnerAddr([7;16]), entry(FriendStatus::Active, true, Some("g")));
    let req = sign_catalog_request(&stranger.device_key, stranger.owner, f.owner, stranger.cert.clone());
    let cat = serve_catalog_for_request(&fg, &req, f.owner, f.cert.clone(), &f.device_key, hlc(7)).expect("serve");
    assert!(cat.entries.is_empty(), "no leak to non-friends");
    assert!(verify_referral_catalog(&cat, f.owner, stranger.owner).is_ok());
}

#[test]
fn rejects_request_addressed_to_someone_else() {
    let f = mint_test_owner(0x11);
    let r = mint_test_owner(0x22);
    let fg = FriendGraph::default();
    let req = sign_catalog_request(&r.device_key, r.owner, OwnerAddr([0x99;16]), r.cert.clone()); // to != us
    assert!(serve_catalog_for_request(&fg, &req, f.owner, f.cert.clone(), &f.device_key, hlc(7)).is_err());
}
```

- [ ] **Step 2: Run, verify fail.**
- [ ] **Step 3: Implement the pure decision + the IO handler**

```rust
// iroh_pex_acceptor.rs
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use iroh::endpoint::Connection;
use crate::owner_state_types::{OwnerAddr, Hlc};
use crate::owner_state_types::OwnerState; // adjust to real path for the CRDT root
use crate::friend_graph::{FriendGraph, FriendStatus};
use crate::referral_catalog::*;

/// Pure: authenticate + friend-gate + build. Active friend → full catalog;
/// authenticated non-friend → EMPTY (benign, signed, subject-bound); auth or
/// to_addr failure → Err (close the stream, serve nothing).
pub fn serve_catalog_for_request(
    fg: &FriendGraph, req: &CatalogRequest, self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert, device2: &ed25519_dalek::SigningKey, at: Hlc,
) -> Result<ReferralCatalog, ReferralAuthError> {
    authenticate_catalog_request(req, self_owner)?;
    let is_friend = fg.friends.get(&req.from_addr).map(|e| e.status == FriendStatus::Active).unwrap_or(false);
    let entries = if is_friend { collect_referrable_entries(fg) } else { Vec::new() };
    Ok(sign_referral_catalog(device2, self_owner, req.from_addr, entries, at, self_enrollment))
}

pub struct IrohFriendPexAcceptor {
    crdt_state: Arc<TokioMutex<OwnerState>>,
    self_owner: OwnerAddr,
    self_enrollment: EnrollmentCert,
    device2_signing_key: Arc<ed25519_dalek::SigningKey>,
    hlc_tracker: Arc<TokioMutex<std::collections::BTreeMap<String, Hlc>>>,
    device_id: String,
    config: crate::iroh_friend_acceptor::FriendAcceptorConfig,
}
// new(...) constructor taking the shared handles.

#[async_trait::async_trait]
impl crate::iroh_invite_acceptor::IrohHandshakeDispatcher for IrohFriendPexAcceptor {
    async fn handle_connection(&self, conn: Connection) {
        if let Err(e) = self.serve(&conn).await { tracing::debug!(?e, "pex serve ended"); }
    }
}

impl IrohFriendPexAcceptor {
    async fn serve(&self, conn: &Connection) -> Result<(), String> {
        let (mut send, mut recv) = tokio::time::timeout(self.config.io_deadline, conn.accept_bi()).await
            .map_err(|_| "accept_bi timeout".to_string())?.map_err(|e| e.to_string())?;
        // read [u32 LE len][body] with the PEX bound (mirror handle_friend_handshake_inbound)
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut len_buf)).await.map_err(|_| "len timeout")?.map_err(|e| e.to_string())?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > PEX_MAX_PACKET_LEN { return Err(format!("bad len {len}")); }
        let mut body = vec![0u8; len];
        tokio::time::timeout(self.config.io_deadline, recv.read_exact(&mut body)).await.map_err(|_| "body timeout")?.map_err(|e| e.to_string())?;
        let req = decode_catalog_request(&body).map_err(|e| e.to_string())?;
        // snapshot the friend graph + an HLC under lock; drop guards before IO
        let at = self.next_hlc().await;
        let cat = {
            let state = self.crdt_state.lock().await;
            serve_catalog_for_request(&state.friend_graph, &req, self.self_owner, self.self_enrollment.clone(), &self.device2_signing_key, at)
        };
        match cat {
            Ok(cat) => {
                let bytes = encode_referral_catalog(&cat).map_err(|e| e.to_string())?;
                let l = bytes.len() as u32;
                send.write_all(&l.to_le_bytes()).await.map_err(|e| e.to_string())?;
                send.write_all(&bytes).await.map_err(|e| e.to_string())?;
                send.finish().map_err(|e| e.to_string())?;
                Ok(())
            }
            Err(e) => Err(format!("serve rejected: {e:?}")), // closes the stream → fail-closed
        }
    }
    // next_hlc: mirror the acceptor's HLC bump (iroh_friend_acceptor.rs:924).
}
```

Adjust `OwnerState`/`friend_graph` access to the real field path (confirm how `IrohFriendHandshakeAcceptor` reaches `state.friend_graph`). `accept_bi`/`read_exact`/`write_all`/`finish` are the same iroh APIs the handshake handler uses.

- [ ] **Step 4: Run, verify pass** (`-E 'test(pex)'`) + fmt + clippy.
- [ ] **Step 5: Commit** — `feat(zeb-375): friend-PEX catalog acceptor (serve-decision + IO handler)`

---

## Task 5: Transport wiring — PEX ALPN + dispatcher third slot

**Files:** `iroh_endpoint.rs`, `zenoh_iroh_transport.rs`, `iroh_friend_acceptor.rs`, `lib.rs`.

- [ ] **Step 1: Write failing tests** (routing)

```rust
// in iroh_friend_acceptor.rs tests
#[test]
fn route_pex_alpn_targets_pex() {
    use crate::iroh_endpoint::alpn;
    assert_eq!(route_handshake_alpn(alpn::HARMONY_FRIEND_PEX_V1), FriendDispatchTarget::Pex);
    assert_eq!(route_handshake_alpn(alpn::HARMONY_FRIEND_V1), FriendDispatchTarget::Friend);
    assert_eq!(route_handshake_alpn(alpn::HARMONY_HANDSHAKE_V1), FriendDispatchTarget::Invite);
}
// in iroh_endpoint.rs tests
#[test]
fn pex_alpn_constant_is_correct() {
    assert_eq!(alpn::HARMONY_FRIEND_PEX_V1, b"harmony/friend-pex/v1");
}
```

- [ ] **Step 2: Run, verify fail.**
- [ ] **Step 3: Implement**
  - `iroh_endpoint.rs`: add `pub const HARMONY_FRIEND_PEX_V1: &[u8] = b"harmony/friend-pex/v1";` to `mod alpn`; add `alpn::HARMONY_FRIEND_PEX_V1.to_vec()` to BOTH `.alpns(vec![…])` (lines ~93 and ~317); extend the `alpn_constants_are_correct` test.
  - `iroh_friend_acceptor.rs`: add `Pex` to `FriendDispatchTarget`; in `route_handshake_alpn`, `if alpn == HARMONY_FRIEND_PEX_V1 { Pex } else if … HARMONY_FRIEND_V1 { Friend } else { Invite }`; add `pex: Arc<dyn IrohHandshakeDispatcher>` field to `MultiplexHandshakeDispatcher`, a 3rd ctor arg, and `FriendDispatchTarget::Pex => &self.pex` in `select_for_alpn`.
  - `zenoh_iroh_transport.rs:330`: extend the branch to `|| alpn_used == alpn::HARMONY_FRIEND_PEX_V1`.
  - `lib.rs:~4688`: construct the `IrohFriendPexAcceptor` (sharing `crdt_state`, `self_owner`, `own_enrollment_cert_for_friend`, `community_signing_key_arc`, `tracker`, `device_id`) and pass it as the 3rd arg to `MultiplexHandshakeDispatcher::new(invite_acceptor, friend_acceptor, pex_acceptor)`.

- [ ] **Step 4: Run, verify pass** + full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (catch wiring breakage) + fmt + clippy + `cargo check` MSRV-style.
- [ ] **Step 5: Commit** — `feat(zeb-375): register harmony/friend-pex/v1 ALPN + multiplex third slot`

---

## Task 6: `set_friend_referrable` IPC + service + UI toggle

**Files:** `lib.rs`, `src/lib/friend-service.ts`, `src/lib/friend-service.test.ts`, `src/lib/components/FriendsPanel.svelte`.

- [ ] **Step 1: Write failing tests**
  - Rust (pure core, in `lib.rs` friend tests or `friend_graph.rs`):
    ```rust
    #[test]
    fn apply_set_referrable_flips_and_bumps() {
        let mut fg = FriendGraph::default();
        fg.friends.insert(OwnerAddr([1;16]), entry(FriendStatus::Active, false, Some("a")));
        let updated = apply_set_referrable(&fg, OwnerAddr([1;16]), true, hlc(9)).expect("ok");
        assert!(updated.referrable);
        assert_eq!(updated.learned_at, hlc(9));
    }
    #[test]
    fn apply_set_referrable_unknown_is_error() {
        assert!(apply_set_referrable(&FriendGraph::default(), OwnerAddr([1;16]), true, hlc(9)).is_err());
    }
    ```
  - vitest (`friend-service.test.ts`):
    ```ts
    it('setReferrable invokes the IPC with camelCase args', async () => {
      const adapter = makeAdapter();
      (adapter.invoke as any).mockResolvedValue(undefined);
      const svc = new FriendService(); await svc.connectAdapter(adapter);
      await svc.setReferrable('aabb', true);
      expect(adapter.invoke).toHaveBeenCalledWith('set_friend_referrable', { ownerIdHex: 'aabb', referrable: true });
    });
    ```

- [ ] **Step 2: Run, verify fail.**
- [ ] **Step 3: Implement**
  - `apply_set_referrable(fg, owner, referrable, hlc) -> Result<FriendEntry, String>`: clone the entry, set `referrable`, set `learned_at`, return it (Err if absent).
  - IPC (mirror `set_friend_auto_accept`, lib.rs:34569):
    ```rust
    #[tauri::command(rename_all = "snake_case")]
    async fn set_friend_referrable(app: tauri::AppHandle, state: tauri::State<'_, Mutex<NodeState>>, owner_id_hex: String, referrable: bool) -> Result<(), String> {
        // snapshot crdt_state + sync_engine + tracker + device_id from NodeState (drop the std lock)
        // hlc = next_hlc(tracker, device_id); parse owner_id_hex -> OwnerAddr
        // { let mut s = crdt_state.lock().await; let e = apply_set_referrable(&s.friend_graph, owner, referrable, hlc)?; s.apply_friend_update(e); }
        // engine.notify_dirty(); emit friend-list-changed
        Ok(())
    }
    ```
    Register `set_friend_referrable` in the `generate_handler![…]` list (lib.rs:35967). Use the exact CRDT write+persist sequence from `unfriend_inner` (lib.rs:33962).
  - `friend-service.ts`: `async setReferrable(ownerIdHex: string, referrable: boolean): Promise<void> { await this.invoke<void>('set_friend_referrable', { ownerIdHex, referrable }); }`
  - `FriendsPanel.svelte`: in the friend row, a checkbox bound to `f.referrable` that calls `service.setReferrable(f.ownerIdHex, !f.referrable)` then refreshes.

- [ ] **Step 4: Run, verify pass** (`cargo nextest …`, `npx vitest run`, `npx tsc --noEmit`) + fmt + clippy.
- [ ] **Step 5: Commit** — `feat(zeb-375): set_friend_referrable IPC + FriendsPanel toggle`

---

## Task 7: `browse_friend_referrals` IPC + service + view

**Files:** `lib.rs`, `src/lib/friend-service.ts`, `src/lib/friend-service.test.ts`, `src/lib/components/FriendsPanel.svelte`.

- [ ] **Step 1: Write failing tests**
  - Rust (pure projection):
    ```rust
    #[test]
    fn project_referrals_marks_already_friends() {
        let mut fg = FriendGraph::default();
        fg.friends.insert(OwnerAddr([1;16]), entry(FriendStatus::Active, false, Some("known")));
        let cat = ReferralCatalog { author: OwnerAddr([9;16]),
            entries: vec![
                ReferralEntry{ peer_owner: OwnerAddr([1;16]), display: Some("known".into()) }, // already a friend
                ReferralEntry{ peer_owner: OwnerAddr([2;16]), display: Some("new".into()) },
            ], at: hlc(7), enrollment: mint_test_owner(0x9).cert, sig: [0;64] };
        let views = project_referrals(&cat, &fg);
        assert_eq!(views.len(), 2);
        assert!(views[0].already_friend);
        assert!(!views[1].already_friend);
    }
    ```
  - vitest: `browseReferrals('aabb')` invokes `browse_friend_referrals` with `{ ownerIdHex: 'aabb' }` and returns the parsed DTO.

- [ ] **Step 2: Run, verify fail.**
- [ ] **Step 3: Implement**
  - `ReferralView { owner_id_hex: String, display: Option<String>, already_friend: bool }` (serde `rename_all="camelCase"`); `project_referrals(cat, fg) -> Vec<ReferralView>` (already_friend = peer present Active|Pending).
  - IPC `browse_friend_referrals` (mirror `connectivity_resolve_friend` resolve + `connectivity_add_friend_by_key_inner` dial):
    1. snapshot handles from `NodeState` (pkarr_resolver, crdt_state, iroh_endpoint, self identity, sealed-secret decrypt material).
    2. look up friend (`OwnerAddr` from hex); require `status == Active`.
    3. decrypt `FriendEntry.sealed_secret` via `owner_state_crypto::decrypt_friend_secret`; `resolve_friend_case_d(&resolver, &secret, &friend_owner)`; decode routing blob; build the iroh dial target (mirror `connectivity_resolve_friend`).
    4. `iroh_endpoint.inner().connect(target, alpn::HARMONY_FRIEND_PEX_V1)`; `conn.open_bi()`; sign + send framed `CatalogRequest{ from_addr=self, to_addr=friend, … }`; read framed `ReferralCatalog`.
    5. `verify_referral_catalog(&cat, friend_owner, self_owner)`; `project_referrals(&cat, &friend_graph)`; return. Typed errors for: not-a-friend, unreachable, verify-failed.
    Register the IPC in `generate_handler!`.
  - `friend-service.ts`: `async browseReferrals(ownerIdHex): Promise<ReferralView[]> { return this.invoke('browse_friend_referrals', { ownerIdHex }); }` + `ReferralView` interface.
  - `FriendsPanel.svelte`: a "Browse referrals" button per friend → calls `browseReferrals` → renders the returned list read-only (name + short owner_id + an "already friends" badge). **No** request-intro action (that's 2b).

- [ ] **Step 4: Run, verify pass** + fmt + clippy + tsc + vitest.
- [ ] **Step 5: Commit** — `feat(zeb-375): browse_friend_referrals IPC + read-only referrals view`

---

## Task 8: Two-node browse integration test

**Files:** Create `src-tauri/tests/referral_catalog_roundtrip_integration.rs`.

- [ ] **Step 1: Write the test** (mirror `tests/friend_token_roundtrip_integration.rs`; `#[serial_test::serial]`, generous timeouts per ZEB-374)
  - Stand up two `IrohEndpoint`s (A = server, B = browser). Install A's `IrohFriendPexAcceptor` as the PEX dispatch target (via the multiplexer or a direct dispatcher on A's accept loop, whichever the harness exposes).
  - Seed A's `FriendGraph`: B is an Active friend of A; a third owner X is an Active **referrable** friend of A; a fourth owner Y is Active but **not** referrable.
  - B (an Active friend of A) signs a `CatalogRequest{to_addr=A}`, dials A on `HARMONY_FRIEND_PEX_V1`, sends it, reads the `ReferralCatalog`.
  - Assert: `verify_referral_catalog` passes; entries == exactly `[X]`; Y absent.
  - Second case: a non-friend C dials A → catalog verifies but `entries.is_empty()`.

- [ ] **Step 2: Run, verify it fails** (until A serves) then passes once wired. Run serially: `cargo nextest run --locked --features test-fixtures -E 'test(referral_catalog_roundtrip)' --test-threads 1`.
- [ ] **Step 3:** Fix any wiring gaps surfaced (this test is the real exercise of Tasks 4–5–7's IO paths).
- [ ] **Step 4: Run full suite** `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (serial re-run any `*_integration` UDP-port flakes per the known-flake note).
- [ ] **Step 5: Commit** — `test(zeb-375): two-node referral-catalog browse integration`

---

## Task 9: Wire-format pin fixtures + handshake-untouched proof

**Files:** Create `src-tauri/tests/wire_format_zeb375_pex_fixtures.rs`.

- [ ] **Step 1: Write fixtures** mirroring `tests/wire_format_zeb370_fixtures.rs`: `EXPECTED_*_HEX = "FILL_AFTER"` sentinels + `pin_hex` helper; deterministic constructors for `ReferralEntry`, `ReferralCatalog`, `CatalogRequest` (`mint_test_owner(0x42)` cert, `sig: [9u8;64]`, `at: hlc(7)`, fixed owners); encode via `encode_referral_catalog`/`encode_catalog_request`; structural assertions on CBOR map keys.
- [ ] **Step 2: Run** → fixtures panic printing the real hex (FILL_AFTER). Paste the hex into the constants.
- [ ] **Step 3: Re-run** → pins pass. Also run `cargo nextest run --locked -E 'test(wire_format_zeb370)'` and confirm the existing handshake fixtures are **unchanged/green** — the proof that the PEX ALPN left the Phase-1b handshake wire format byte-for-byte intact.
- [ ] **Step 4: Full gates green.**
- [ ] **Step 5: Commit** — `test(zeb-375): pin referral-catalog wire format; confirm handshake fixtures unchanged`

---

## Final (controller, after all tasks)

1. Dispatch a final `feature-dev:code-reviewer` over the whole branch diff (security focus: auth/replay bindings, fail-closed serving, no reachability leak).
2. Verify all gates green from a clean `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, fmt, clippy, tsc, vitest.
3. Confirm no `gen/schemas/*.json` / `.playwright-scratch/` staged.
4. Push `zeb-375-friends-phase-2a-referral-catalog`; open the PR (body references ZEB-375 + the spec); set ZEB-375 → In Progress/In Review.
5. Run the autonomous bot-review loop (CodeRabbit/Cursor/Qodo/CodeAnt/Greptile) to convergence, reading ground-truth comment bodies per [[feedback_verify_dont_trust_signals]] — NOT a monitor's count.

## Self-review notes (controller)
- Type names consistent across tasks: `CatalogRequest`, `ReferralCatalog`, `ReferralEntry`, `ReferralView`; fns `sign_catalog_request`/`authenticate_catalog_request`/`sign_referral_catalog`/`verify_referral_catalog`/`collect_referrable_entries`/`build_referral_catalog`/`serve_catalog_for_request`/`project_referrals`/`apply_set_referrable`.
- Domain tags: `"hcr1"` (request), `"hrc1"` (catalog) — distinct, asserted in Task 1.
- Every Rust task keeps the pure core unit-tested and the IO thin (IO exercised by Task 8), mirroring the Phase-1b acceptor's pure/IO split.
</content>
