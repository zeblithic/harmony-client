# Long-form Profile Page over CAS (ZEB-345) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a public, content-addressed long-form profile document (bio + links + fields) referenced by a new additive `profile_page_root` CID on the signed `ProfileCardBroadcast`, fetched lazily over the existing ZEB-343 CAS-serve path and rendered in a right-side profile panel.

**Architecture:** Second CAS consumer after ZEB-343 avatars. The card stays a tiny CID index; the document lives in CAS as a canonical-CBOR `PublicDurable` object, served by the existing public-serve queryable with zero new transport code. Encode/validate authority is in Rust; the frontend only ever sees a validated DTO.

**Tech Stack:** Rust (Tauri IPC, ciborium canonical CBOR, ed25519-dalek), Svelte 5 runes, vitest, cargo-nextest.

**Spec:** `docs/specs/2026-05-31-zeb-345-profile-page-cas-design.md` (commit `d868dc9`).

**Standing rules:** branch `zeb-345-profile-page-cas` (off `origin/main` `af1f1a5`); commit per task; run `cargo fmt --all` + clippy + nextest (`--features test-fixtures`) + `npx tsc --noEmit` + `npx vitest run` as gates; never merge; one bundled push per review round.

---

## File Structure

**Create:**
- `src-tauri/src/profile_page_doc.rs` — the `ProfilePageDoc` codec, caps, `validate`/`encode`/`decode`.
- `src-tauri/tests/wire_format_profile_page_doc_fixtures.rs` — pinned canonical doc bytes.
- `src-tauri/tests/profile_page_cross_peer_integration.rs` — two-node author→fetch→DTO.
- `src/lib/profile-page-resolver.ts` — lazy CID→DTO resolver (twin of `avatar-resolver.ts`).
- `src/lib/components/ProfilePanel.svelte` — right-side profile surface.
- `src/lib/__tests__/profile-page-resolver.test.ts`
- `src/lib/__tests__/profile-panel.test.ts`

**Modify:**
- `src-tauri/src/profile_card_broadcast.rs` — `pp` field + thread through `sign_card`/`publish_card_once`/`insert_verified`/`CachedCard`/`DiscoveredCardInfo`/`get_cached` + test call sites.
- `src-tauri/src/lib.rs` — `ProfilePayload.profile_page_root`; `ingest_profile_doc` + `fetch_profile_doc` IPCs; thread `profile_page_root` through `publish_profile`/`publish_owner_card`/`republish_owner_card`; register the 2 new IPCs in the handler list.
- `src-tauri/src/profile_page_doc.rs` is added to `src-tauri/src/lib.rs` module list (`mod profile_page_doc;`).
- `src-tauri/tests/wire_format_profile_card_avatar_fixtures.rs` (or a new sibling) — add `pp` byte-identity + `0xA8` cases.
- `src/lib/member-card-service.ts` — thread `profilePageRoot` through `DiscoveredCardInfo`/`ResolvedCard`.
- `src/lib/components/ProfilePopover.svelte` — "View full profile" action.
- `src/lib/components/ProfileEditor.svelte` — "About" section (bio/links/fields).
- `src/App.svelte` — `ProfilePageResolver` construction + `openProfileOwnerId` state + panel render + save→ingest→republish + self-seed.

---

## Task 0: Pre-flight baseline

**Files:** none (verification only).

- [ ] **Step 1: Confirm branch + clean tree**

Run: `cd /Users/zeblith/work/zeblithic/harmony-client && git branch --show-current && git status -s`
Expected: `zeb-345-profile-page-cas`, only the committed spec/plan present.

- [ ] **Step 2: Backend baseline green**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: all pass (the iroh/zenoh transport orphan-flakes from ZEB-343 are known non-blocking; note any).

- [ ] **Step 3: Frontend baseline green**

Run (repo root): `npx tsc --noEmit && npx vitest run`
Expected: clean.

---

## Task 1: `profile_page_root` wire field + card threading

**Files:**
- Modify: `src-tauri/src/profile_card_broadcast.rs`
- Test: same file (`#[cfg(test)]`) + `src-tauri/tests/wire_format_profile_card_avatar_fixtures.rs`

The blast radius is every site that currently threads `avatar_cid`. Mirror it exactly, one field over, placed **immediately after `avatar_cid`** everywhere.

- [ ] **Step 1: Failing test — byte-identity + round-trip**

In `profile_card_broadcast.rs` tests, add:
```rust
#[test]
fn card_without_page_root_is_byte_identical_to_avatar_only() {
    // Two cards identical except one is constructed via the new pp-aware
    // sign_card with profile_page_root=None: bytes must match a card with no pp.
    let (signer, owner_id, cert) = test_signer_owner_cert(); // existing helper in this test mod
    let hlc = Hlc::new_for_test(1); // use whatever helper the existing tests use
    let with_none = sign_card(&signer, owner_id, "Ann".into(), "hi".into(),
        None, /* profile_page_root */ None, cert.clone(), hlc.clone()).unwrap();
    // map header must be 0xA6 (no av, no pp) -> 6 required pairs
    let bytes = canonical_cbor_encode(&with_none).unwrap();
    assert_eq!(bytes[0], 0xA6, "no-av/no-pp card must be a 6-entry map");
}

#[test]
fn card_with_page_root_round_trips_and_verifies() {
    let (signer, owner_id, cert) = test_signer_owner_cert();
    let hlc = Hlc::new_for_test(1);
    let root = [7u8; 32];
    let card = sign_card(&signer, owner_id, "Ann".into(), "hi".into(),
        None, Some(root), cert, hlc).unwrap();
    assert_eq!(card.profile_page_root, Some(root));
    assert_eq!(verify_card(&card).unwrap(), owner_id);
    let bytes = canonical_cbor_encode(&card).unwrap();
    let back: ProfileCardBroadcast = ciborium::de::from_reader(&bytes[..]).unwrap();
    assert_eq!(back.profile_page_root, Some(root));
}
```
(Use the exact existing test helpers — read the current test module for the real `Hlc`/cert constructors; the names above are placeholders for whatever the file already uses.)

- [ ] **Step 2: Run — expect compile failure** (`sign_card` arity, missing field).

- [ ] **Step 3: Add the field** to `ProfileCardBroadcast` after `avatar_cid` (lines ~48):
```rust
    #[serde(
        rename = "pp",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::owner_state_types::serialize_optional_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_optional_bytes_from_bstr"
    )]
    pub profile_page_root: Option<[u8; 32]>,
```

- [ ] **Step 4: Thread through every avatar site** (add `profile_page_root` param/field right after `avatar_cid`):
  - `sign_card` signature (after `avatar_cid: Option<[u8;32]>`) + the struct literal at the bottom.
  - `publish_card_once` signature + its `sign_card(...)` call.
  - `CachedCard` struct + `insert_verified`'s `CachedCard { ... }` literal (`profile_page_root: card.profile_page_root,`).
  - `DiscoveredCardInfo`: add `#[serde(rename = "profilePageRoot", skip_serializing_if = "Option::is_none")] pub profile_page_root: Option<String>,`.
  - `get_cached`: `profile_page_root: c.profile_page_root.map(hex::encode),`.
  - Every `sign_card(...)` / `publish_card_once(...)` call in the test module: insert `None,` (or `Some(...)`) for the new param.
  - `verify_card`: **no change**.

- [ ] **Step 5: Run** `cargo nextest run -p harmony-app --features test-fixtures -E 'test(profile_card)'` → PASS.

- [ ] **Step 6: Wire-format fixture** — in `tests/wire_format_profile_card_avatar_fixtures.rs` add a `0xA8` (avatar + pp) pinned-bytes case and a pp-only `0xA7` case, mirroring the existing avatar fixture's structure (deterministic cert/signer/HLC helpers from `test-fixtures`).

- [ ] **Step 7: Gate + commit**

Run: `cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
```bash
git add -A && git commit -m "feat(zeb-345): profile_page_root card field + threading (T1)"
```

---

## Task 2: `profile_page_doc.rs` codec + caps

**Files:**
- Create: `src-tauri/src/profile_page_doc.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod profile_page_doc;`)
- Test: in-file `#[cfg(test)]`

- [ ] **Step 1: Failing test** (create the file with only the test module + types, no impls yet won't compile — so write types + signatures returning `todo!()` first, then the tests). Tests:
```rust
#[test]
fn round_trip_encodes_and_decodes() {
    let doc = ProfilePageDoc { version: PROFILE_DOC_VERSION, bio: "hi\nthere".into(),
        links: vec![ProfileLink { label: "Site".into(), url: "https://example.com".into() }],
        fields: vec![ProfileField { key: "Pronouns".into(), value: "she/her".into() }] };
    let bytes = encode_profile_doc(&doc).unwrap();
    assert_eq!(decode_profile_doc(&bytes).unwrap(), doc);
}
#[test]
fn rejects_oversize_bio() {
    let doc = doc_with_bio("x".repeat(MAX_BIO_BYTES + 1));
    assert!(matches!(validate_profile_doc(&doc), Err(ProfileDocError::BioTooLong)));
}
#[test]
fn rejects_too_many_links() { /* MAX_LINKS+1 -> Err(TooManyLinks) */ }
#[test]
fn rejects_overlong_link_label_and_url() { /* > MAX_LINK_LABEL_BYTES / MAX_LINK_URL_BYTES */ }
#[test]
fn rejects_too_many_fields_and_overlong_key_value() { /* MAX_FIELDS / key / value */ }
#[test]
fn rejects_disallowed_link_scheme() {
    let doc = doc_with_link("L", "http://insecure.example"); // not https/harmony
    assert!(matches!(validate_profile_doc(&doc), Err(ProfileDocError::LinkSchemeNotAllowed)));
}
#[test]
fn accepts_https_and_harmony_schemes() {
    assert!(validate_profile_doc(&doc_with_link("a","https://x.example")).is_ok());
    assert!(validate_profile_doc(&doc_with_link("b","harmony:community/abc")).is_ok());
}
#[test]
fn rejects_total_bytes_over_cap() { /* many max-size fields -> Err(DocTooLarge) */ }
#[test]
fn decode_rejects_unknown_version() { /* hand-encode version=2 -> Err(UnsupportedVersion) */ }
#[test]
fn decode_rejects_oversize_blob() { assert!(decode_profile_doc(&vec![0u8; MAX_PROFILE_DOC_BYTES+1]).is_err()); }
```

- [ ] **Step 2: Run** → fail.

- [ ] **Step 3: Implement** `profile_page_doc.rs`:
```rust
//! ZEB-345 — long-form profile document (bio + links + fields), CAS-addressed.
//! Canonical CBOR, declaration-order keys; encode/validate authority lives here.
use crate::owner_state_crypto::{
    canonical_cbor_encode, sealed::CanonicalPayloadSealed, CanonicalPayload, CryptoError,
};
use serde::{Deserialize, Serialize};

pub const PROFILE_DOC_VERSION: u8 = 1;
pub const MAX_BIO_BYTES: usize = 4_096;
pub const MAX_LINKS: usize = 10;
pub const MAX_LINK_LABEL_BYTES: usize = 64;
pub const MAX_LINK_URL_BYTES: usize = 512;
pub const MAX_FIELDS: usize = 16;
pub const MAX_FIELD_KEY_BYTES: usize = 32;
pub const MAX_FIELD_VALUE_BYTES: usize = 256;
pub const MAX_PROFILE_DOC_BYTES: usize = 16_384;
const ALLOWED_LINK_SCHEMES: [&str; 2] = ["https://", "harmony:"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilePageDoc {
    #[serde(rename = "vn")] pub version: u8,
    #[serde(rename = "bo")] pub bio: String,
    #[serde(rename = "ln")] pub links: Vec<ProfileLink>,
    #[serde(rename = "fl")] pub fields: Vec<ProfileField>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileLink {
    #[serde(rename = "lb")] pub label: String,
    #[serde(rename = "ur")] pub url: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileField {
    #[serde(rename = "ky")] pub key: String,
    #[serde(rename = "vl")] pub value: String,
}
impl CanonicalPayloadSealed for ProfilePageDoc {}
impl CanonicalPayload for ProfilePageDoc {}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileDocError {
    #[error("bio exceeds {MAX_BIO_BYTES} bytes")] BioTooLong,
    #[error("more than {MAX_LINKS} links")] TooManyLinks,
    #[error("link label exceeds {MAX_LINK_LABEL_BYTES} bytes")] LinkLabelTooLong,
    #[error("link url exceeds {MAX_LINK_URL_BYTES} bytes")] LinkUrlTooLong,
    #[error("link scheme not in allowlist (https/harmony)")] LinkSchemeNotAllowed,
    #[error("more than {MAX_FIELDS} fields")] TooManyFields,
    #[error("field key exceeds {MAX_FIELD_KEY_BYTES} bytes")] FieldKeyTooLong,
    #[error("field value exceeds {MAX_FIELD_VALUE_BYTES} bytes")] FieldValueTooLong,
    #[error("encoded doc exceeds {MAX_PROFILE_DOC_BYTES} bytes")] DocTooLarge,
    #[error("unsupported profile doc version")] UnsupportedVersion,
    #[error("canonical CBOR encode failed: {0}")] Encode(#[from] CryptoError),
    #[error("CBOR decode failed")] Decode,
}

pub fn validate_profile_doc(doc: &ProfilePageDoc) -> Result<(), ProfileDocError> {
    if doc.bio.len() > MAX_BIO_BYTES { return Err(ProfileDocError::BioTooLong); }
    if doc.links.len() > MAX_LINKS { return Err(ProfileDocError::TooManyLinks); }
    for l in &doc.links {
        if l.label.len() > MAX_LINK_LABEL_BYTES { return Err(ProfileDocError::LinkLabelTooLong); }
        if l.url.len() > MAX_LINK_URL_BYTES { return Err(ProfileDocError::LinkUrlTooLong); }
        if !ALLOWED_LINK_SCHEMES.iter().any(|s| l.url.starts_with(s)) {
            return Err(ProfileDocError::LinkSchemeNotAllowed);
        }
    }
    if doc.fields.len() > MAX_FIELDS { return Err(ProfileDocError::TooManyFields); }
    for f in &doc.fields {
        if f.key.len() > MAX_FIELD_KEY_BYTES { return Err(ProfileDocError::FieldKeyTooLong); }
        if f.value.len() > MAX_FIELD_VALUE_BYTES { return Err(ProfileDocError::FieldValueTooLong); }
    }
    let bytes = canonical_cbor_encode(doc)?;
    if bytes.len() > MAX_PROFILE_DOC_BYTES { return Err(ProfileDocError::DocTooLarge); }
    Ok(())
}

pub fn encode_profile_doc(doc: &ProfilePageDoc) -> Result<Vec<u8>, ProfileDocError> {
    validate_profile_doc(doc)?;
    Ok(canonical_cbor_encode(doc)?)
}

pub fn decode_profile_doc(bytes: &[u8]) -> Result<ProfilePageDoc, ProfileDocError> {
    if bytes.len() > MAX_PROFILE_DOC_BYTES { return Err(ProfileDocError::DocTooLarge); }
    let doc: ProfilePageDoc =
        ciborium::de::from_reader(bytes).map_err(|_| ProfileDocError::Decode)?;
    if doc.version != PROFILE_DOC_VERSION { return Err(ProfileDocError::UnsupportedVersion); }
    validate_profile_doc(&doc)?;
    Ok(doc)
}
```
Add `mod profile_page_doc;` near the other `mod` lines in `lib.rs`.

- [ ] **Step 4: Run** the unit tests → PASS.

- [ ] **Step 5: Canonical fixture** — `tests/wire_format_profile_page_doc_fixtures.rs`: pin the exact `encode_profile_doc` bytes for a fixed v1 doc (assert against a hardcoded `&[u8]`), so any accidental schema/key-order change is caught.

- [ ] **Step 6: Gate + commit**
```bash
git add -A && git commit -m "feat(zeb-345): profile_page_doc codec + caps + fixtures (T2)"
```

---

## Task 3: `ingest_profile_doc` IPC

**Files:** Modify `src-tauri/src/lib.rs`. Test: in-file.

Twin of `ingest_avatar_bytes` (`lib.rs:8333`) + `ingest_avatar_bytes_inner` (`lib.rs:8310`), but input is structured and we encode the doc ourselves.

- [ ] **Step 1: Failing test** (mirror `ingest_avatar_bytes_yields_public_durable_cid` at `lib.rs:8573`):
```rust
#[tokio::test]
async fn ingest_profile_doc_yields_public_durable_cid() {
    // build ingest channel like the avatar test; call the inner; assert CID
    // leading bit is clear (PublicDurable) and refetch by GetLocal returns the bytes.
}
#[tokio::test]
async fn ingest_profile_doc_rejects_oversize_input() {
    // bio over MAX_BIO_BYTES -> Err
}
```

- [ ] **Step 2: Run** → fail.

- [ ] **Step 3: Implement.** Input DTOs + inner + IPC:
```rust
#[derive(serde::Deserialize)]
pub struct ProfileLinkInput { pub label: String, pub url: String }
#[derive(serde::Deserialize)]
pub struct ProfileFieldInput { pub key: String, pub value: String }

pub(crate) async fn ingest_profile_doc_inner(
    bio: String,
    links: Vec<ProfileLinkInput>,
    fields: Vec<ProfileFieldInput>,
    ingest_tx: &/* same type as ingest_avatar_bytes_inner uses */,
) -> Result<String, String> {
    use crate::profile_page_doc::*;
    let doc = ProfilePageDoc {
        version: PROFILE_DOC_VERSION,
        bio,
        links: links.into_iter().map(|l| ProfileLink { label: l.label, url: l.url }).collect(),
        fields: fields.into_iter().map(|f| ProfileField { key: f.key, value: f.value }).collect(),
    };
    let bytes = encode_profile_doc(&doc).map_err(|e| e.to_string())?; // validates caps + scheme
    let reader = std::io::Cursor::new(bytes);
    let (root, _size) = streaming_ingest(reader, ingest_tx, ChunkerConfig::DEFAULT, None)
        .await.map_err(|e| e.to_string())?;
    Ok(hex::encode(root.as_bytes())) // match how ingest_avatar_bytes_inner hex-encodes the root
}

#[tauri::command]
async fn ingest_profile_doc(
    bio: String,
    links: Vec<ProfileLinkInput>,
    fields: Vec<ProfileFieldInput>,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<String, String> {
    // clone ingest_tx from state exactly as ingest_avatar_bytes does, then:
    ingest_profile_doc_inner(bio, links, fields, &ingest_tx).await
}
```
(Read `ingest_avatar_bytes` for the exact `ingest_tx` type + the root→hex helper; copy them.)

- [ ] **Step 4: Register the IPC** in the `tauri::generate_handler![...]` list (same place `ingest_avatar_bytes` is registered).

- [ ] **Step 5: Run** → PASS.

- [ ] **Step 6: Gate + commit**
```bash
git add -A && git commit -m "feat(zeb-345): ingest_profile_doc IPC (T3)"
```

---

## Task 4: `fetch_profile_doc` IPC

**Files:** Modify `src-tauri/src/lib.rs`. Test: in-file.

Fetch via the existing `FetchRequest`/`fetch_content` path (`lib.rs:11159`), then byte-cap + decode → DTO.

- [ ] **Step 1: Failing test**
```rust
#[tokio::test]
async fn fetch_profile_doc_returns_dto_for_local_doc() {
    // ingest a doc (T3 inner) then fetch_profile_doc(cid) -> DTO matches input
}
#[tokio::test]
async fn fetch_profile_doc_rejects_oversize_or_malformed() { /* Err */ }
```

- [ ] **Step 2: Run** → fail.

- [ ] **Step 3: Implement.** DTO + IPC:
```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePageDocDto {
    pub bio: String,
    pub links: Vec<ProfileLinkDto>,
    pub fields: Vec<ProfileFieldDto>,
}
#[derive(serde::Serialize)] #[serde(rename_all = "camelCase")]
pub struct ProfileLinkDto { pub label: String, pub url: String }
#[derive(serde::Serialize)] #[serde(rename_all = "camelCase")]
pub struct ProfileFieldDto { pub key: String, pub value: String }

#[tauri::command]
async fn fetch_profile_doc(
    cid: String,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<ProfilePageDocDto, String> {
    if cid.is_empty() || !cid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid CID hex: {cid}"));
    }
    let fetch_tx = { /* clone guard.fetch_tx exactly as fetch_content does */ };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx.send(event_loop::FetchRequest { cid_hex: cid, reply: reply_tx })
        .await.map_err(|_| "event loop not running".to_string())?;
    let bytes = reply_rx.await
        .map_err(|_| "event loop dropped fetch request".to_string())??;
    if bytes.len() > crate::profile_page_doc::MAX_PROFILE_DOC_BYTES {
        return Err("profile doc exceeds size cap".to_string());
    }
    let doc = crate::profile_page_doc::decode_profile_doc(&bytes).map_err(|e| e.to_string())?;
    Ok(ProfilePageDocDto {
        bio: doc.bio,
        links: doc.links.into_iter().map(|l| ProfileLinkDto { label: l.label, url: l.url }).collect(),
        fields: doc.fields.into_iter().map(|f| ProfileFieldDto { key: f.key, value: f.value }).collect(),
    })
}
```
(`FetchRequest` already returns hash-verified bytes via ZEB-343 T4 verify-on-fetch; the size cap + `decode_profile_doc` add the doc-specific bound.)

- [ ] **Step 4: Register** `fetch_profile_doc` in the handler list.

- [ ] **Step 5: Run** → PASS. **Step 6:** Gate + commit `"feat(zeb-345): fetch_profile_doc IPC (T4)"`.

---

## Task 5: Thread `profile_page_root` through publish IPCs

**Files:** Modify `src-tauri/src/lib.rs`.

Mirror the `avatar_cid` decode-before-commit threading exactly (`publish_profile` `lib.rs:5592`, `publish_owner_card` `5654`, `republish_owner_card` `5734`).

- [ ] **Step 1: Add `profile_page_root: Option<String>` to `ProfilePayload`** (`lib.rs:1041`), after `avatar_cid`.

- [ ] **Step 2: `publish_profile`** — add a `profile_page_root_bytes` decode block mirroring `avatar_cid_bytes` (`lib.rs:5592`), **before** the Reticulum commit; pass it into `publish_owner_card`.

- [ ] **Step 3: `publish_owner_card`** — add `profile_page_root: Option<[u8;32]>` param after `avatar_cid`; pass into `sign_card(...)`.

- [ ] **Step 4: `republish_owner_card`** — add `profile_page_root: Option<String>` param; decode mirroring `avatar_cid_bytes` (`5743`); pass to `publish_owner_card`.

- [ ] **Step 5: Test** — extend an existing publish test (or add one) asserting a card published with a `profile_page_root` hex round-trips with the root set; malformed hex → `Err` with nothing published.

- [ ] **Step 6: Run** backend tests → PASS. Gate + commit `"feat(zeb-345): thread profile_page_root through publish IPCs (T5)"`.

---

## Task 6: Cross-peer integration test (author → fetch → DTO)

**Files:** Create `src-tauri/tests/profile_page_cross_peer_integration.rs`.

Twin `tests/profile_card_avatar_cross_peer_integration.rs` + `tests/cas_serve_two_node_integration.rs`.

- [ ] **Step 1: Write the test** — node A: `ingest_profile_doc_inner` a doc → CID; node B: fetch by CID over the two-node Zenoh harness; decode → DTO equals A's input. Include a negative: an **encrypted** CID is not served (reuse the ZEB-343 control-CID pattern so a slow-discovery false-pass can't happen).

- [ ] **Step 2: Run** `cargo nextest run --test profile_page_cross_peer_integration --features test-fixtures` → PASS (allow the known transport-orphan retry pattern; assert on the real fetch).

- [ ] **Step 3:** Gate + commit `"test(zeb-345): cross-peer profile doc fetch (T6)"`.

---

## Task 7: `ProfilePageResolver` (frontend, lazy)

**Files:** Create `src/lib/profile-page-resolver.ts` + `src/lib/__tests__/profile-page-resolver.test.ts`.

Twin `src/lib/avatar-resolver.ts` but DTO-valued and lazy (no eager wiring).

- [ ] **Step 1: Failing test** — `resolve(cid)` returns `undefined` first call + kicks off `invoke('fetch_profile_doc', {cid})`; on resolve, second `resolve` returns the cached DTO and `onChange` fired; failure sets a 30s cooldown; `destroy()` clears cache.

- [ ] **Step 2: Run** → fail.

- [ ] **Step 3: Implement** (structure mirrors `AvatarResolver`):
```ts
export interface ProfilePageDto { bio: string; links: {label:string;url:string}[]; fields: {key:string;value:string}[] }
export class ProfilePageResolver {
  onChange?: () => void;
  private adapter: TauriAdapter | null = null;
  private cache = new Map<string, ProfilePageDto>();
  private pending = new Set<string>();
  private failedAt = new Map<string, number>();
  private destroyed = false;
  connectAdapter(a: TauriAdapter) { this.adapter = a; }
  resolve(cid: string): ProfilePageDto | undefined {
    const c = this.cache.get(cid); if (c) return c;
    const f = this.failedAt.get(cid);
    const cooled = f !== undefined && Date.now() - f >= 30_000;
    if (!this.pending.has(cid) && (f === undefined || cooled)) this.fetch(cid);
    return undefined;
  }
  private async fetch(cid: string) {
    if (!this.adapter) return;
    this.pending.add(cid);
    try {
      const dto = await this.adapter.invoke('fetch_profile_doc', { cid }) as ProfilePageDto;
      if (this.destroyed) return;
      this.cache.set(cid, dto); this.onChange?.();
    } catch (e) {
      if (!this.destroyed) { console.warn(`profile doc fetch failed ${cid}:`, e); this.failedAt.set(cid, Date.now()); }
    } finally { this.pending.delete(cid); }
  }
  destroy() { this.destroyed = true; this.cache.clear(); this.pending.clear(); this.failedAt.clear(); }
}
```

- [ ] **Step 4: Run** → PASS. **Step 5:** `npx tsc --noEmit`. Commit `"feat(zeb-345): ProfilePageResolver (T7)"`.

---

## Task 8: `ProfilePanel.svelte` + popover entry

**Files:** Create `src/lib/components/ProfilePanel.svelte` + `src/lib/__tests__/profile-panel.test.ts`; Modify `src/lib/components/ProfilePopover.svelte`, `src/lib/member-card-service.ts`.

- [ ] **Step 1: Thread `profilePageRoot`** through `member-card-service.ts` — add to `DiscoveredCardInfo` + `ResolvedCard` (mirror `avatarCid`/`avatarUrl`), so a resolved card exposes `profilePageRoot?: string`.

- [ ] **Step 2: Failing panel test** — given a card + a resolver returning a DTO, `ProfilePanel` renders the bio (escaped, newlines preserved), link rows (with correct `href`), and field rows; with no `profilePageRoot` it renders header-only.

- [ ] **Step 3: Implement `ProfilePanel.svelte`** (Svelte 5 runes). Props: `ownerIdHex`, `card` (`{displayName,statusText,avatarUrl,profilePageRoot}`), `resolver: ProfilePageResolver`, `onClose`. Use `$derived` to call `resolver.resolve(card.profilePageRoot)`; subscribe to `resolver.onChange` to re-render. Layout per spec mockup (header: `<Avatar>` + name + status + copyable owner id; About: bio in a `white-space: pre-wrap` block; Links; Fields). Bio via `{dto.bio}` (auto-escaped). Links: see Task 11 for the scheme-split handler (stub `onLinkClick` here, wire in T11).

- [ ] **Step 4: Popover entry** — in `ProfilePopover.svelte` owner-card mode, add a **"View full profile"** button that calls a new `onViewProfile?: (ownerIdHex: string) => void` prop.

- [ ] **Step 5: Run** vitest + tsc → PASS. Commit `"feat(zeb-345): ProfilePanel surface + popover entry (T8)"`.

---

## Task 9: `ProfileEditor` "About" section

**Files:** Modify `src/lib/components/ProfileEditor.svelte`. Test: `src/lib/__tests__/` (extend or new).

- [ ] **Step 1: Failing test** — editing bio/adding a link/field then saving calls `invoke('ingest_profile_doc', {bio, links, fields})` and stages the returned CID; an all-empty About leaves `profilePageRoot` undefined (no ingest call).

- [ ] **Step 2: Run** → fail.

- [ ] **Step 3: Implement** — add `$state` for `bio`, `links: {label,url}[]`, `fields: {key,value}[]` (seed from the current profile if present — read via `fetch_profile_doc` on mount when the profile has a `profilePageRoot`). Add the About UI: bio `<textarea>` with a byte counter (mirror the display-name counter), add/remove link rows (label + URL inputs), add/remove field rows (key + value inputs). In the save handler:
```ts
let profilePageRoot: string | undefined;
const hasAbout = bio.trim() || links.length || fields.length;
if (hasAbout) profilePageRoot = await invoke('ingest_profile_doc', { bio, links, fields });
// include profilePageRoot in the emitted profile payload alongside avatarCid
```

- [ ] **Step 4: Run** → PASS. Commit `"feat(zeb-345): ProfileEditor About section (T9)"`.

---

## Task 10: `App.svelte` wiring

**Files:** Modify `src/App.svelte`.

Mirror the avatar wiring (resolver construction, member-card-received forwarding, seedSelf, handleProfileSave, publishProfileToNetwork, republishOwnerCard).

- [ ] **Step 1:** Construct + `connectAdapter` a `ProfilePageResolver`; on its `onChange`, trigger a re-render of the open panel (a `$state` bump).
- [ ] **Step 2:** Add `openProfileOwnerId = $state<string|null>(null)`; render `<ProfilePanel>` (right column) when set; pass the resolved card + resolver + `onClose`.
- [ ] **Step 3:** Wire `ProfilePopover`'s `onViewProfile` → set `openProfileOwnerId`.
- [ ] **Step 4:** `handleProfileSave` + `publishProfileToNetwork` carry `profile_page_root` (a CID hex — **no `blob:` sanitization needed**, unlike avatars). `republishOwnerCard` sends `profilePageRoot`. All `seedSelf` sites pass the self `profilePageRoot` so the owner's panel resolves locally.
- [ ] **Step 5:** `npx tsc --noEmit && npx vitest run` → PASS. Commit `"feat(zeb-345): App wiring for profile panel (T10)"`.

---

## Task 11: Render safety (escaping + link scheme split)

**Files:** Modify `src/lib/components/ProfilePanel.svelte`; Test: `src/lib/__tests__/profile-panel.test.ts`.

- [ ] **Step 1: Failing tests** — (a) a bio containing `<script>` renders as text, not markup; (b) an `https:` link renders `<a target="_blank" rel="noopener noreferrer">`; (c) a `harmony:` link calls the deep-link router instead of navigating; (d) a link whose scheme is somehow not allowlisted is rendered inert (no `href`).

- [ ] **Step 2: Run** → fail.

- [ ] **Step 3: Implement** `onLinkClick(url)`:
```ts
const ALLOWED = ['https://', 'harmony:'];
function linkOk(u: string) { return ALLOWED.some(s => u.startsWith(s)); }
function onLinkClick(e: MouseEvent, url: string) {
  if (!linkOk(url)) { e.preventDefault(); return; }
  if (url.startsWith('harmony:')) { e.preventDefault(); routeDeepLink(url); } // ZEB-338 router
  // https: falls through to default <a target=_blank rel=noopener>
}
```
Render links only when `linkOk(url)`; bio always via `{dto.bio}` text interpolation with `white-space: pre-wrap`.

- [ ] **Step 4: Run** → PASS. Commit `"feat(zeb-345): profile render safety + link scheme split (T11)"`.

---

## Task 12: Final gate sweep + push + PR

**Files:** none (verification).

- [ ] **Step 1: Full backend gate**

Run: `cd src-tauri && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: clean (note any known transport-orphan flakes; re-run that test to confirm it's not the diff).

- [ ] **Step 2: Full frontend gate**

Run (root): `npx tsc --noEmit && npx vitest run`
Expected: clean.

- [ ] **Step 3: Push + open PR**
```bash
git push -u origin zeb-345-profile-page-cas
gh pr create --repo zeblithic/harmony-client --title "ZEB-345: long-form profile page over CAS (profile_page_root)" --body "<summary + spec/plan links + test plan + ZEB-345 / related ZEB-343,341 / ZEB-344 follow-up note>"
```

- [ ] **Step 4:** Begin the autonomous bot-review loop (CodeRabbit / Cursor / CodeAnt / Qodo + 5 CI jobs). Address findings as one bundled push per round; resolve threads; never Greptile; never merge. Pushover Jake at ready-to-merge.
