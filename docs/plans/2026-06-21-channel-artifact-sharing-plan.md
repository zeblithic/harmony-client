# Channel Artifact Sharing (CAS) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.
>
> **Repo rule:** This repo does NOT use markdown `- [ ]` checkbox TODO tracking (CodeAnt flags it; Jake's standing ruling). Steps are plain **bold numbered** items. Track progress with TodoWrite, not checkboxes in this file.

**Goal:** Let a channel message reference one or more encrypted (members-only) or public artifacts (logs/diffs/files) stored in CAS — an optional signed `attachments` list on the channel Post event, ingested via the existing chunking DAG and fetched/decrypted on demand.

**Architecture:** `attachments` mirrors the `mentions`/`reply_to` pattern — an optional signed field (CBOR key `pa`, between `mn` and `rt`) carrying `ChannelAttachment{cid, mime, name, size}`. Bytes are stored by **reusing** harmony-content's FastCDC + Merkle-DAG chunker through the existing `streaming_ingest`/`fetch_recursive` paths (no third ingest path). Encrypted artifacts are `encrypt_blob`(whole file)→chunk-ciphertext, with every CID `encrypted`-flagged and allowlisted for member-to-member serving; `cid.flags().encrypted` drives decrypt-on-fetch. Public artifacts skip encryption and reuse an already-public copy when one exists (deterministic CID).

**Tech Stack:** Rust (harmony-content `ContentId`/`ContentFlags`/`dag`/`bundle`, ChaCha20-Poly1305 via `encrypt_blob`, ciborium canonical CBOR, ed25519-dalek, tokio, cargo-nextest), TypeScript/Svelte (vitest), Tauri IPC.

**Spec:** `docs/specs/2026-06-21-channel-artifact-sharing-design.md` (ZEB-535, parent epic ZEB-533).

**Ships as TWO PRs** (per the approved split). PR 1 is the backend data/logic layer (fully unit + integration tested, not yet user-reachable). PR 2 adds the IPC commands + frontend service. Each PR runs the full gate (final task) before opening.

---

## Load-bearing invariants (read before touching wire or serve code)

### A. Canonical CBOR ordering (same discipline as ZEB-534 mentions)

RFC 8949 §4.2.1 orders map keys length-first then bytewise. All post keys are 2 chars → bytewise sort of the key bytes. The new `attachments` key `pa` = `0x70 0x61`:

```
at(6174) au(6175) bd(6264) ch(6368) ci(6369) id(6964) kd(6b64) mn(6d6e) pa(7061) rt(7274) sg(7367)
```

`mn`(0x6d6e) < `pa`(0x7061) < `rt`(0x7274), so `attachments` MUST be declared **between `mentions` and `reply_to`** in BOTH `SignedChannelEvent::Post` and `ChannelPostSignedSet`. Wrong order → silent signature-verification failure.

Inside each `ChannelAttachment`, the nested map keys are `cd`(cid, 0x6364) < `mi`(mime, 0x6d69) < `nm`(name, 0x6e6d) < `sz`(size, 0x737a) → declare fields **cid, mime, name, size** in that order.

**No-flag-day:** `attachments: None` (with `skip_serializing_if = Option::is_none`) omits `pa` ⟹ canonical CBOR byte-identical to a pre-feature post ⟹ identical signature. The two existing wire pins (`signed_channel_event_post_wire_bytes_pinned`, `backfill_reply_packet_wire_bytes_pinned`) AND the ZEB-534 mention pin (`signed_channel_event_post_with_mentions_wire_bytes_pinned`) must stay **byte-for-byte unchanged** — only their `ChannelPostPayload` literals gain `attachments: None`.

### B. Encryption ↔ chunking (encrypt-whole → chunk-ciphertext → decrypt-after-assemble)

- **Encrypt:** `encrypt_blob(&epoch_key, &plaintext)` (returns `nonce ++ ciphertext`). Chunk the **ciphertext** through `streaming_ingest_with_options(.., flags = ContentFlags{encrypted:true}, serveable = true)`. Every leaf + bundle CID is encrypted-flagged and allowlisted.
- **Fetch:** the existing FetchRequest path reassembles the opaque bytes via `fetch_recursive`. If `cid.flags().encrypted`, `decrypt_blob(&epoch_key, &bytes)` → plaintext (the nonce is the prepended prefix — `decrypt_blob` reads it). Else use bytes directly.
- The encrypted-artifact path holds the whole plaintext + ciphertext in memory (v1 cap = `MAX_ARTIFACT_BYTES`, default 1 GiB). Public artifacts stream natively (no whole-file buffer).

### C. Serve authorization (the one net-new primitive)

The serve gate is `content_cid_servable(cid) = !cid.flags().encrypted || serve_allowlist.contains(cid)` (`event_loop.rs:7466`). An encrypted CID not in the allowlist is **silently** not served (the queryable `continue`s) → a fetch **stalls**. A chunked encrypted artifact is many CIDs, so **every** one must be allowlisted. We add a `serveable` flag that flows: ingest → IngestRequest → ingest handler allowlists; fetch admission → `CasOp::PutLocal{serveable}` → PutLocal arm allowlists. `serve_allowlist` is in scope across the `run` select loop (defined `event_loop.rs:798`, cloned into the serve queryable `2497`).

---

## File / change-site map

**harmony-content imports** (new artifact module): `use harmony_content::cid::{ContentId, ContentFlags, CidType, MAX_PAYLOAD_SIZE};`, `use harmony_content::chunker::ChunkerConfig;`.

**PR 1 — backend:**
- `src/community_channel_log.rs`: `ChannelAttachment` struct; `attachments` field on `ChannelPostPayload`, `SignedChannelEvent::Post`, `ChannelPostSignedSet`; `MAX_ATTACHMENTS`; `TooManyAttachments` / `AttachmentFieldTooLong` errors; sign + canonical threading; verify cap; compile-fix sweep; unit tests.
- `src/community_channel_log_engine.rs`: `ChannelAttachmentDto`; `ChannelMessageDto.attachments`; `message_dto_for_event` projection; `publish()` gains `attachments` param + cap + normalize; tests.
- `src/event_loop.rs`: `IngestRequest.serveable` (struct ~274) + ingest handler allowlist (`3263`); `CasOp::PutLocal.serveable` + PutLocal arm allowlist (`3298`); `FetchRequest.serveable` (struct ~265) + fetch handler thread (`3173`); `wrap_fetch_one_with_admission` serveable (`5710`).
- `src/content_store.rs`: `CasOp::PutLocal` gains `serveable: bool`; `RuntimeContentStore::put`/`put_serveable` set it.
- `src/lib.rs`: `streaming_ingest_with_options` + `build_bundle_tree_with_options` (existing `streaming_ingest`/`build_bundle_tree` delegate with defaults); `send_ingest` gains serveable; `current_epoch_key_for` helper; `MAX_ARTIFACT_BYTES`; `ingest_channel_artifact_impl`; `download_channel_artifact_impl`; tests.
- `src/community_fork.rs` + `tests/*`: compile-fix sweep (`attachments: None`).
- `tests/wire_format/channel_log_fixtures.rs`: three literals get `attachments: None` (pins unchanged); one new populated pin.

**PR 2 — IPC + frontend:**
- `src/api/rpc.rs`: `IngestChannelArtifactArgs`, `DownloadChannelArtifactArgs`, `PostChannelMessageArgs.attachments`; three registrations.
- `src/lib.rs`: `ingest_channel_artifact` + `download_channel_artifact` Tauri commands; `post_channel_message`/`_impl` gain `attachments`.
- `src/lib/channel-message-service.ts`: `ChannelAttachmentDto`; `ChannelMessageDto.attachments`; `postMessage` attachments param; `ingestArtifact`/`downloadArtifact` facades.
- `src/lib/__tests__/channel-message-service.test.ts`: updated asserts + new tests.

---

# PR 1 — Backend: artifact storage, encryption, serve-authorization, signed field

## Task 1: `ChannelAttachment` + signed `attachments` field + sign/verify

**Files:**
- Modify: `src/community_channel_log.rs` (types, sign, canonical, verify cap, sweep, tests)
- Modify: `src/community_channel_log_engine.rs`, `src/community_fork.rs`, `tests/channel_backfill_integration.rs`, `tests/wire_format/channel_log_fixtures.rs` (compile-fix sweep)

**Step 1: Define `ChannelAttachment` and its constant/error.** In `src/community_channel_log.rs`, near `MAX_MENTIONS` (~line 150), add:

```rust
/// ZEB-535: hard cap on attachments per channel post. Bounds the signed-set
/// size and the per-message fetch fan-out. Enforced at mint (`publish`), the
/// IPC boundary, AND inbound verification (`verify_channel_event`).
pub(crate) const MAX_ATTACHMENTS: usize = 16;

/// ZEB-535: max bytes for an attachment's `name`/`mime` string fields (each).
pub(crate) const MAX_ATTACHMENT_FIELD_BYTES: usize = 255;

/// ZEB-535: a CAS artifact referenced by a channel post. `cid` is the root
/// (Book or Bundle) of the stored bytes; `cid.flags().encrypted` tells the
/// receiver whether to decrypt with the community epoch key. `name`/`mime`/
/// `size` are signed (tamper-evident) and packet-encrypted (confidential).
/// `size` is the PLAINTEXT length, cross-checked on fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAttachment {
    #[serde(
        rename = "cd",
        serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
        deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
    )]
    pub cid: [u8; 32],
    #[serde(rename = "mi")]
    pub mime: String,
    #[serde(rename = "nm")]
    pub name: String,
    #[serde(rename = "sz")]
    pub size: u64,
}
```

> Note: `cid` is stored as a raw 32-byte string (`ContentId::to_bytes()` output) using the same bstr serde helpers `sig` uses. Field order cd, mi, nm, sz matches the nested canonical order (invariant A). If `serialize_bytes_as_bstr` is generic over `&[u8]`/`[u8; N]`, reuse it; if it is `[u8; 64]`-specific, add a sibling `serialize_bytes32_as_bstr`/`deserialize_bytes32_from_bstr` in `owner_state_types.rs` modeled on the existing one and use those here.

**Step 2: Add `attachments` to `ChannelPostPayload`** (after `mentions`):

```rust
    pub mentions: Option<Vec<OwnerAddr>>,
    /// ZEB-535: CAS artifacts this post references. `None` is wire-identical
    /// to a pre-feature post. Carried into the signed set (tamper-evident).
    pub attachments: Option<Vec<ChannelAttachment>>,
```

**Step 3: Add the `pa` field to `SignedChannelEvent::Post` — BETWEEN `mn` and `rt`:**

```rust
        #[serde(rename = "mn", skip_serializing_if = "Option::is_none", default)]
        mentions: Option<Vec<OwnerAddr>>,
        #[serde(rename = "pa", skip_serializing_if = "Option::is_none", default)]
        attachments: Option<Vec<ChannelAttachment>>,
        #[serde(rename = "rt", skip_serializing_if = "Option::is_none", default)]
        reply_to: Option<MessageId>,
```

Update the variant's field-order doc comment to `... kd, mn, pa, rt`.

**Step 4: Add the `pa` field to `ChannelPostSignedSet` — BETWEEN `mn` and `rt`** (serialize-only, no `default`):

```rust
    #[serde(rename = "mn", skip_serializing_if = "Option::is_none")]
    mentions: &'a Option<Vec<OwnerAddr>>,
    #[serde(rename = "pa", skip_serializing_if = "Option::is_none")]
    attachments: &'a Option<Vec<ChannelAttachment>>,
    #[serde(rename = "rt", skip_serializing_if = "Option::is_none")]
    reply_to: &'a Option<MessageId>,
```

Update this struct's field-order doc comment similarly (`... mn, pa, rt`).

**Step 5: Thread `attachments` through `sign_channel_event` and `signed_set_canonical_cbor`.** In `sign_channel_event`'s `SignedChannelEvent::Post { .. }` construction, add (before `reply_to`):

```rust
        mentions: payload.mentions.clone(),
        attachments: payload.attachments.clone(),
        reply_to: payload.reply_to,
```

In `signed_set_canonical_cbor`, add `attachments` to the destructure (before `reply_to`) and the `ChannelPostSignedSet` build (before `reply_to`):

```rust
        mentions,
        attachments,
        reply_to,
        sig: _,
    } = event;
    let signed_set = ChannelPostSignedSet {
        // ... existing fields ...
        mentions,
        attachments,
        reply_to,
    };
```

**Step 6: Enforce `MAX_ATTACHMENTS` + field-length caps in `verify_channel_event`** (inbound — a remote peer can sign an oversized `pa`). Find the existing ZEB-534 mentions cap block in `verify_channel_event` (`if let Some(m) = mentions { if m.len() > MAX_MENTIONS ...}`) and add an analogous block immediately after it, binding `attachments` from the `SignedChannelEvent::Post { .. attachments, .. }` destructure (add `attachments` to that destructure):

```rust
    if let Some(a) = attachments {
        if a.len() > MAX_ATTACHMENTS {
            return Err(ChannelEventError::TooManyAttachments {
                count: a.len(),
                max: MAX_ATTACHMENTS,
            });
        }
        for att in a {
            if att.name.len() > MAX_ATTACHMENT_FIELD_BYTES
                || att.mime.len() > MAX_ATTACHMENT_FIELD_BYTES
            {
                return Err(ChannelEventError::AttachmentFieldTooLong {
                    max: MAX_ATTACHMENT_FIELD_BYTES,
                });
            }
        }
    }
```

Add the error variants to `ChannelEventError` (next to the ZEB-534 `TooManyMentions`):

```rust
    #[error("too many attachments: {count} (max {max})")]
    TooManyAttachments { count: usize, max: usize },
    #[error("attachment name/mime too long (max {max} bytes)")]
    AttachmentFieldTooLong { max: usize },
```

**Step 7: Mechanical compile-fix sweep — add `attachments: None,` to every `ChannelPostPayload { .. }` literal and the one non-`..` Post destructure.** Apply `attachments: None,` (place after `mentions: ...,`) to each `ChannelPostPayload { .. }` literal in:
- `src/community_channel_log.rs`: every fixture/test literal (the same set that got `mentions:` — `fixture_payload`, `fixture_signed_event`, and the in-test literals). Run `cargo build` to enumerate; add to each.
- `src/community_channel_log_engine.rs`: `make_signed_event` and `publish`'s payload literal (the latter gets the real value in Task 2; `None` here is temporary).
- `tests/channel_backfill_integration.rs`, `tests/wire_format/channel_log_fixtures.rs` (all three literals — pins stay identical; do NOT touch expected hex), `src/community_fork.rs` `make_event` (the `SignedChannelEvent::Post { .. }` literal — add `attachments: None,` before `reply_to`).

For the one exhaustive Post destructure `sign_channel_event_round_trip` (lists every field, no `..`): add `attachments,` to the binding (before `reply_to,`) and `assert_eq!(attachments, payload.attachments);` after the `reply_to` assert.

> All other Post destructures use `..` and are unaffected EXCEPT `signed_set_canonical_cbor` (Step 5), `verify_channel_event` (Step 6), and `message_dto_for_event` (Task 2). If `cargo build` surfaces another non-`..` destructure, add `attachments: _,`.

**Step 8: Write unit tests** (in `src/community_channel_log.rs` test module):

```rust
fn fixture_attachment(tag: u8) -> ChannelAttachment {
    ChannelAttachment {
        cid: [tag; 32],
        mime: "text/plain".to_string(),
        name: format!("log-{tag}.txt"),
        size: 1234,
    }
}

#[test]
fn sign_channel_event_carries_attachments() {
    let key = fixture_signing_key(0xa1);
    let atts = vec![fixture_attachment(0xb2), fixture_attachment(0xc3)];
    let payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id: fixture_community(0xc0),
        channel_id: fixture_channel(0x01),
        author: fixture_owner_addr(0xa1),
        at: fixture_hlc(100_000, "a-dev"),
        content_kind: 0,
        body: "see log",
        reply_to: None,
        mentions: None,
        attachments: Some(atts.clone()),
    };
    let signed = sign_channel_event(&payload, &key).expect("sign");
    let SignedChannelEvent::Post { attachments, .. } = signed;
    assert_eq!(attachments, Some(atts));
}

#[test]
fn attachments_none_omits_pa_key_some_includes_it() {
    // CBOR text key "pa" encodes as 62 70 61 (text-str len-2 + 'p','a').
    const PA_KEY_HEX: &str = "627061";
    let key = fixture_signing_key(0xa1);

    let (none_payload, _k) = fixture_payload("no attachments");
    let none_event = sign_channel_event(&none_payload, &key).expect("sign");
    let mut none_bytes = Vec::new();
    ciborium::into_writer(&none_event, &mut none_bytes).expect("encode");
    assert!(
        !hex::encode(&none_bytes).contains(PA_KEY_HEX),
        "attachments:None must omit the pa key"
    );

    let some_payload = ChannelPostPayload {
        attachments: Some(vec![fixture_attachment(0xb2)]),
        ..none_payload
    };
    let some_event = sign_channel_event(&some_payload, &key).expect("sign");
    let mut some_bytes = Vec::new();
    ciborium::into_writer(&some_event, &mut some_bytes).expect("encode");
    assert!(
        hex::encode(&some_bytes).contains(PA_KEY_HEX),
        "attachments:Some must include the pa key"
    );
}
```

> If `fixture_payload` returns a tuple that can't be spread with `..`, build the `some_payload` literal in full (mirroring `sign_channel_event_carries_attachments`) instead of struct-update syntax.

Add a verify-cap test mirroring `verify_channel_event_rejects_post_over_mention_cap`:

```rust
#[tokio::test]
async fn verify_channel_event_rejects_post_over_attachment_cap() {
    let state = fixture_state_with_alice_joined();
    let mut tracker = ChannelLogReplayTracker::new();
    let (key, author, _pub64) = fixture_identity(0xa1);
    let too_many: Vec<ChannelAttachment> =
        (0..=MAX_ATTACHMENTS).map(|i| fixture_attachment(i as u8)).collect();
    let payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id: fixture_community(0xc0),
        channel_id: fixture_channel(0x01),
        author,
        at: fixture_hlc(100_000, "a-dev"),
        content_kind: 0,
        body: "x",
        reply_to: None,
        mentions: None,
        attachments: Some(too_many),
    };
    let event = sign_channel_event(&payload, &key).expect("sign");
    let err = verify_channel_event(
        &event, &fixture_community(0xc0), &fixture_channel(0x01), &state, &mut tracker,
    )
    .await
    .expect_err("over-cap attachments must be rejected");
    assert!(matches!(err, ChannelEventError::TooManyAttachments { .. }), "got: {err:?}");
}
```

**Step 9: Run the channel-log unit tests (lib-only).**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(channel_event) or test(attachment) or test(signed_set) or test(sign_channel_event)'`
Expected: PASS including the three new tests + the extended round-trip.

**Step 10: Confirm the workspace compiles.**
Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: clean. Fix any missed sweep site per Step 7.

**Step 11: Commit.**
```bash
git add src-tauri/src/community_channel_log.rs src-tauri/src/community_channel_log_engine.rs src-tauri/src/community_fork.rs src-tauri/tests/channel_backfill_integration.rs src-tauri/tests/wire_format/channel_log_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(channel-log): signed attachments field on channel-post events

Optional attachments: Vec<ChannelAttachment{cid,mime,name,size}> under the
canonical CBOR key `pa` (between mn and rt), inside the signature. None is
byte-identical to a pre-feature post. Cap + field-length enforced inbound.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: DTO projection + `publish()` accepts attachments

**Files:**
- Modify: `src/community_channel_log_engine.rs` (DTO, projection, publish, tests)
- Modify: `src/lib.rs` + integration tests (publish call sites — compile-fix)

**Step 1: Add `ChannelAttachmentDto` + the DTO field.** In `src/community_channel_log_engine.rs`, near `ChannelMessageDto`:

```rust
/// ZEB-535: IPC-facing attachment (hex cid + metadata). `encrypted` is
/// derived from the CID flag so the frontend can label members-only vs
/// public without re-parsing the CID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAttachmentDto {
    pub cid: String,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub encrypted: bool,
}
```

After `ChannelMessageDto.mentions`:

```rust
    /// ZEB-535: CAS artifacts this message references; omitted when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<ChannelAttachmentDto>>,
```

**Step 2: Project in `message_dto_for_event`.** Add `attachments` to the destructure (before `..`) and build the DTO list, deriving `encrypted` from the CID header via `ContentId::from_bytes(att.cid).flags().encrypted`:

```rust
        let SignedChannelEvent::Post {
            id, author, at, body, mentions, attachments, reply_to, ..
        } = event;
```

and in the returned struct (after the `mentions` projection):

```rust
            attachments: attachments.as_ref().filter(|v| !v.is_empty()).map(|v| {
                v.iter()
                    .map(|a| ChannelAttachmentDto {
                        cid: hex::encode(a.cid),
                        mime: a.mime.clone(),
                        name: a.name.clone(),
                        size: a.size,
                        encrypted: harmony_content::cid::ContentId::from_bytes(a.cid)
                            .flags()
                            .encrypted,
                    })
                    .collect()
            }),
```

**Step 3: Add `attachments` to `publish()`** (param LAST, after `mentions`), with cap + empty-normalization mirroring mentions. Update the signature, add the bounds check after the mentions check, normalize, and set the payload field:

```rust
    pub async fn publish(
        self: &Arc<Self>,
        body: Vec<u8>,
        reply_to: Option<MessageId>,
        mentions: Option<Vec<OwnerAddr>>,
        attachments: Option<Vec<ChannelAttachment>>,
    ) -> Result<MessageId, ChannelLogEngineError> {
        // ... existing body + mentions checks ...
        if let Some(a) = &attachments {
            if a.len() > crate::community_channel_log::MAX_ATTACHMENTS {
                return Err(ChannelLogEngineError::TooManyAttachments {
                    count: a.len(),
                    max: crate::community_channel_log::MAX_ATTACHMENTS,
                });
            }
        }
        let attachments = attachments.filter(|a| !a.is_empty());
```

Add the error variant to `ChannelLogEngineError` (next to `TooManyMentions`):

```rust
    #[error("too many attachments: {count} (max {max})")]
    TooManyAttachments { count: usize, max: usize },
```

In `publish`'s `ChannelPostPayload { .. }` literal, replace the temporary `attachments: None,` (Task 1 sweep) with `attachments,`.

Add `use crate::community_channel_log::{ChannelAttachment, MAX_ATTACHMENTS};` (extend the existing `MAX_MENTIONS` import) if not already imported.

**Step 4: Update every `engine.publish(...)` call site to pass the new arg.** All callers currently pass three args (`body, reply_to, mentions`); append a fourth:
- `src/lib.rs`: the internal post paths and `post_channel_message_impl`'s `.publish(body, reply_to_msg_id, mention_addrs)` → append `, None` (PR 2 Task 2 replaces this last `None` with the parsed attachments).
- Engine in-file tests + `tests/channel_backfill_integration.rs` + `tests/community_channel/community_channel_messages_integration.rs`: append `, None` to each `.publish(...)`.

**Step 5: Tests** (engine test module):

```rust
#[tokio::test]
async fn event_to_dto_projects_attachments() {
    let fix = build_engine_fixture(8, 250, 1000).await;
    // Build an attachment with an ENCRYPTED-flagged CID so `encrypted` is true.
    let enc_cid = harmony_content::cid::ContentId::for_book(
        b"ct",
        harmony_content::cid::ContentFlags { encrypted: true, ..Default::default() },
    )
    .expect("cid")
    .to_bytes();
    let att = crate::community_channel_log::ChannelAttachment {
        cid: enc_cid, mime: "text/plain".into(), name: "log.txt".into(), size: 9,
    };
    let id = { use rand::RngCore; let mut b=[0u8;16]; rand::thread_rng().fill_bytes(&mut b); MessageId(b) };
    let payload = ChannelPostPayload {
        id, community_id: fix.community_id, channel_id: fix.channel_id, author: fix.self_owner,
        at: Hlc { wall_ms: 5_000, logical: 0, device_id: "device-x".to_string() },
        content_kind: 0, body: "see log", reply_to: None, mentions: None,
        attachments: Some(vec![att.clone()]),
    };
    let ev = sign_channel_event(&payload, &fix.signing_key).expect("sign");
    let dto = fix.engine.event_to_dto(&ev);
    let got = dto.attachments.expect("attachments present");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].cid, hex::encode(enc_cid));
    assert_eq!(got[0].name, "log.txt");
    assert!(got[0].encrypted, "encrypted-flagged cid projects encrypted=true");
}

#[tokio::test]
async fn publish_rejects_too_many_attachments() {
    let fix = build_engine_fixture(8, 250, 1000).await;
    let too_many: Vec<crate::community_channel_log::ChannelAttachment> =
        (0..=crate::community_channel_log::MAX_ATTACHMENTS)
            .map(|i| crate::community_channel_log::ChannelAttachment {
                cid: [i as u8; 32], mime: "x".into(), name: "n".into(), size: 1,
            })
            .collect();
    let err = fix.engine.publish(b"hi".to_vec(), None, None, Some(too_many)).await
        .expect_err("over-cap must error");
    assert!(matches!(err, ChannelLogEngineError::TooManyAttachments { .. }), "got: {err:?}");
}
```

**Step 6: Run engine tests (lib-only).**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(event_to_dto) or test(publish)'`
Expected: PASS including the two new tests.

**Step 7: Confirm integration tests still compile.**
Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: clean.

**Step 8: Commit.**
```bash
git add src-tauri/src/community_channel_log_engine.rs src-tauri/src/lib.rs src-tauri/tests/channel_backfill_integration.rs src-tauri/tests/community_channel/community_channel_messages_integration.rs
git commit -m "$(cat <<'EOF'
feat(channel-log): project attachments through DTO; publish() accepts them

ChannelMessageDto.attachments (hex cid + metadata + derived encrypted flag);
publish bounds + normalizes the attachment list into the signed payload.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `serveable` plumbing for subtree authorization

**Files:**
- Modify: `src/content_store.rs` (`CasOp::PutLocal.serveable`, RuntimeContentStore put/put_serveable)
- Modify: `src/event_loop.rs` (IngestRequest/FetchRequest structs, ingest handler, PutLocal arm, fetch handler, `wrap_fetch_one_with_admission`)
- Modify: `src/lib.rs` (`streaming_ingest_with_options`, `build_bundle_tree_with_options`, `send_ingest`, caller updates)

**Step 1: Add `serveable` to `CasOp::PutLocal`.** In `src/content_store.rs`, the `PutLocal` variant gains `serveable: bool`:

```rust
    PutLocal {
        cid: ContentId,
        blob: Vec<u8>,
        serveable: bool,
        reply: Option<tokio::sync::oneshot::Sender<Result<(), ContentStoreError>>>,
    },
```

Update `RuntimeContentStore::put` to send `serveable: false`, and `put_serveable` to send `serveable: true` AND keep its allowlist.allow (the allowlist is also updated event-loop-side now, but `put_serveable`'s direct allowlist.allow stays for the SyncEngine path that holds a `serve_allowlist`):

```rust
    // in put():
    .send(CasOp::PutLocal { cid, blob, serveable: false, reply: Some(reply_tx) })
    // in put_serveable(): set serveable: true on the PutLocal it sends, keep allowlist.allow(cid).
```

**Step 2: Add `serveable` to `IngestRequest` and `FetchRequest`.** In `src/event_loop.rs`:

```rust
pub struct IngestRequest {
    pub cid_hex: String,
    pub data: Vec<u8>,
    pub serveable: bool, // ZEB-535: allowlist this CID for member-to-member serve
    pub reply: oneshot::Sender<Result<(), String>>,
}

pub struct FetchRequest {
    pub cid_hex: String,
    pub reply: oneshot::Sender<Result<Vec<u8>, String>>,
    pub max_bytes: Option<usize>,
    pub serveable: bool, // ZEB-535: re-serve fetched (encrypted) artifact books
}
```

**Step 3: Allowlist on ingest.** In the ingest handler (`event_loop.rs:3263`), after the successful `runtime.tick()` + before `req.reply.send(Ok(()))`, allowlist when requested:

```rust
                    if req.serveable {
                        if let Ok(b) = hex::decode(&req.cid_hex) {
                            if let Ok(arr) = <[u8; 32]>::try_from(b) {
                                serve_allowlist.allow(ContentId::from_bytes(arr));
                            }
                        }
                    }
                    let _ = req.reply.send(Ok(()));
```

**Step 4: Allowlist on PutLocal.** In the `CasOp::PutLocal` arm (`event_loop.rs:3298`), destructure `serveable` and, after the admit `runtime.tick()`/commit, allowlist when true:

```rust
                    CasOp::PutLocal { cid, blob, serveable, reply } => {
                        // ... existing admit (push SubscriptionMessage + tick) ...
                        if serveable {
                            serve_allowlist.allow(cid);
                        }
                        // ... existing reply ...
                    }
```

> The `GetOrFetch` admit path that also constructs a `PutLocal` (event_loop.rs ~1625, the fire-and-forget admit) must set `serveable: false`. Update that and any other `CasOp::PutLocal { .. }` constructor the compiler flags.

**Step 5: Thread `serveable` through the fetch admission.** In the fetch handler (`event_loop.rs:3173`), capture `req.serveable` before the spawn and pass it to a serveable-aware wrapper:

```rust
                let serveable = req.serveable;
                // ...
                    let fetch_one_with_admit =
                        wrap_fetch_one_with_admission(fetch_one, cas_op_tx_for_fetch, serveable);
```

Update `wrap_fetch_one_with_admission` (`event_loop.rs:5710`) to take `serveable: bool` and send `CasOp::PutLocal { cid, blob, serveable, reply: Some(..) }`.

**Step 6: Add options-aware ingest + keep old signatures delegating.** In `src/lib.rs`, define:

```rust
/// ZEB-535: ingest options. Default = unencrypted, not serveable (the avatar
/// + file-vault behavior, unchanged).
#[derive(Clone, Copy, Default)]
pub struct IngestOptions {
    pub flags: harmony_content::cid::ContentFlags,
    pub serveable: bool,
}
```

Rename the existing `streaming_ingest` body to `streaming_ingest_with_options(reader, ingest_tx, chunker_config, cancel, opts: IngestOptions)`, replacing the hardcoded `ContentFlags::default()` at the leaf `for_book` with `opts.flags`, and passing `opts` into `build_bundle_tree_with_options`. Keep a thin delegate:

```rust
pub async fn streaming_ingest<R>(reader: R, ingest_tx: &Sender<event_loop::IngestRequest>,
    chunker_config: ChunkerConfig, cancel: Option<&Arc<AtomicBool>>) -> Result<(ContentId, u64), IngestError>
where R: AsyncRead + Unpin {
    streaming_ingest_with_options(reader, ingest_tx, chunker_config, cancel, IngestOptions::default()).await
}
```

Do the same for `build_bundle_tree` → `build_bundle_tree_with_options(leaf_cids, total_size, ingest_tx, opts)` (use `opts.flags` in `build_with_flags`). Update `send_ingest` to accept `serveable: bool` and set it on the `IngestRequest`; the options paths pass `opts.serveable`, the legacy delegate passes `false`. Update every `send_ingest`/`IngestRequest { .. }` / `FetchRequest { .. }` constructor the compiler flags to set `serveable: false` (avatar + file-vault + any test harness).

**Step 7: Test the encrypted/serveable ingest produces flagged + allowlisted CIDs.** Add a lib test (in `src/lib.rs` test module, gated `#[cfg(test)]`) that runs `streaming_ingest_with_options` against a small multi-chunk ciphertext with `flags.encrypted = true, serveable = true` through a mock ingest channel, and asserts every received `IngestRequest.serveable == true` and every CID parses with `flags().encrypted == true`. Model it on existing `streaming_ingest` tests (search `streaming_ingest` in tests). If no harness exists, assert at minimum that `IngestOptions{flags: ContentFlags{encrypted:true,..}, serveable:true}` yields a root CID whose `flags().encrypted` is true (single-chunk path).

**Step 8: Run + check.**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(ingest) or test(streaming)'`
Then: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: PASS + clean compile.

**Step 9: Commit.**
```bash
git add src-tauri/src/content_store.rs src-tauri/src/event_loop.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(cas): serveable flag for subtree authorization

Thread a serveable flag through ingest (IngestRequest) and fetch admission
(CasOp::PutLocal) so a chunked encrypted artifact's CIDs are allowlisted for
member-to-member serving on the sharer and re-served by fetchers. Adds
streaming_ingest_with_options(flags, serveable); old entrypoints delegate.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Artifact ingest backend (`ingest_channel_artifact_impl`)

**Files:**
- Modify: `src/lib.rs` (const, epoch-key helper, impl, tests)

**Step 1: Add the cap + epoch-key helper.** In `src/lib.rs`:

```rust
/// ZEB-535: default plaintext cap for a single channel artifact (1 GiB).
/// Community/operator-configurable later; the wire structure supports far more.
pub(crate) const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
```

Add a helper that resolves a community's current epoch key from the OwnerState CRDT held by `NodeState` (the same `spaces[community_id].current_epoch_key` that `community_state_sync::live_epoch_key` reads). Locate the OwnerState CRDT handle field on `NodeState` (search `OwnerState` / `current_epoch_key` usage in `lib.rs`), then:

```rust
async fn current_epoch_key_for(
    state: &std::sync::Mutex<NodeState>,
    community_id: &SpaceId,
) -> Result<crate::owner_state_types::EpochKey, String> {
    // Clone the Arc<Mutex<OwnerState>> out under the std mutex, then lock the
    // async/owner mutex to read the live epoch key (mirrors live_epoch_key).
    // Return a clear error if the space/epoch isn't established yet.
    // ... implement against the actual NodeState field + OwnerState API ...
}
```

> The exact OwnerState field/accessor must match `live_epoch_key` (`community_state_sync.rs:2506`): `guard.spaces.get(community_id)` → `space.current_epoch_key.clone()`. Reuse `live_epoch_key` directly if its signature is callable from here; otherwise inline the same lookup. Error string e.g. `"no live epoch key for community (not joined / epoch not established)"`.

**Step 2: Implement `ingest_channel_artifact_impl`.**

```rust
pub(crate) async fn ingest_channel_artifact_impl(
    state: &std::sync::Mutex<NodeState>,
    community_id: String,
    source_path: String,
    name: Option<String>,
    mime: Option<String>,
    encrypt: bool,
) -> Result<crate::community_channel_log_engine::ChannelAttachmentDto, String> {
    use harmony_content::chunker::ChunkerConfig;
    use harmony_content::cid::{ContentFlags, ContentId};

    let cid_bytes16: [u8; 16] = decode_hex16(&community_id)?; // reuse existing hex-16 parse helper
    let space = SpaceId(cid_bytes16);

    // 1. Stat + cap (reject before reading).
    let meta = tokio::fs::metadata(&source_path).await.map_err(|e| format!("stat: {e}"))?;
    let size = meta.len();
    if size > MAX_ARTIFACT_BYTES {
        return Err(format!("artifact too large: {size} > {MAX_ARTIFACT_BYTES}"));
    }
    let name = name.unwrap_or_else(|| {
        std::path::Path::new(&source_path)
            .file_name().and_then(|s| s.to_str()).unwrap_or("artifact").to_string()
    });
    let mime = mime.unwrap_or_else(|| detect_mime(&name)); // small extension→mime helper; default "application/octet-stream"
    if name.len() > crate::community_channel_log::MAX_ATTACHMENT_FIELD_BYTES
        || mime.len() > crate::community_channel_log::MAX_ATTACHMENT_FIELD_BYTES {
        return Err("attachment name/mime too long".to_string());
    }

    let ingest_tx = {
        let g = state.lock().map_err(|e| format!("lock: {e}"))?;
        g.ingest_tx.clone().ok_or_else(|| "not connected".to_string())?
    };

    let root: ContentId = if encrypt {
        // Encrypted: read whole file -> encrypt_blob -> chunk ciphertext, all
        // CIDs encrypted-flagged + serveable (subtree authorization).
        let plaintext = tokio::fs::read(&source_path).await.map_err(|e| format!("read: {e}"))?;
        let epoch_key = current_epoch_key_for(state, &space).await?;
        let ciphertext = crate::community_state_sync::encrypt_blob(&epoch_key, &plaintext)
            .map_err(|e| format!("encrypt: {e:?}"))?;
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(ciphertext));
        let (root, _n) = streaming_ingest_with_options(
            reader, &ingest_tx, ChunkerConfig::DEFAULT, None,
            IngestOptions { flags: ContentFlags { encrypted: true, ..Default::default() }, serveable: true },
        ).await.map_err(|e| e.to_string())?;
        root
    } else {
        // Public: stream plaintext from disk (no whole-file buffer), default
        // flags. Unencrypted CIDs are served by the gate without allowlisting.
        // (Deterministic-CID reuse of an already-public copy is a public-path
        // refinement; for v1 we re-ingest — dedup happens book-granular in CAS.)
        let file = tokio::fs::File::open(&source_path).await.map_err(|e| format!("open: {e}"))?;
        let (root, _n) = streaming_ingest_with_options(
            tokio::io::BufReader::new(file), &ingest_tx, ChunkerConfig::DEFAULT, None,
            IngestOptions::default(),
        ).await.map_err(|e| e.to_string())?;
        root
    };

    Ok(crate::community_channel_log_engine::ChannelAttachmentDto {
        cid: hex::encode(root.to_bytes()),
        mime,
        name,
        size,
        encrypted: root.flags().encrypted,
    })
}
```

> `decode_hex16` / `detect_mime`: if a hex-16 decode helper already exists (the reply-to/community parse in `post_channel_message_impl`), reuse it; otherwise add a tiny local one. `detect_mime` is a minimal extension map (`txt`→text/plain, `json`→application/json, `png`→image/png, `diff`/`patch`→text/x-diff, else `application/octet-stream`). Keep it small — rich detection is Future Work.

> **Public-copy reuse (deterministic CID):** the spec's "reference an existing public copy" optimization is deliberately deferred to a fast-follow within the public branch — it needs a bounded `get_local`/Zenoh existence probe on the deterministic root. For v1, public ingest re-streams; CAS book-granular dedup already avoids duplicate storage of identical books. Note this in the PR description.

**Step 3: Tests.** Because `ingest_channel_artifact_impl` needs the event loop, prefer a focused unit test of the pure pieces (cap rejection, name/mime defaulting, `detect_mime`) plus deferring the full ingest to the Task 7 two-node integration test. Add:

```rust
#[tokio::test]
async fn ingest_channel_artifact_rejects_oversized() {
    // Build a NodeState with no ingest needed — the size check precedes ingest.
    // Use a temp file whose length is reported > MAX_ARTIFACT_BYTES via a
    // sparse file if feasible; otherwise unit-test the cap as a pure function.
    // ... assert err contains "artifact too large" ...
}

#[test]
fn detect_mime_maps_known_extensions() {
    assert_eq!(detect_mime("a.txt"), "text/plain");
    assert_eq!(detect_mime("a.json"), "application/json");
    assert_eq!(detect_mime("a.unknownext"), "application/octet-stream");
}
```

> If constructing a `NodeState` for the oversized test is heavy, factor the cap+name+mime logic into a pure helper `prepare_artifact_meta(path_meta_len, source_path, name, mime) -> Result<(String,String,u64),String>` and unit-test that directly; `ingest_channel_artifact_impl` calls it. This keeps the cap/validation testable without the event loop.

**Step 4: Run + check.**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(artifact) or test(detect_mime)'`
Then: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: PASS + clean.

**Step 5: Commit.**
```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(cas): channel artifact ingest (encrypt-whole -> chunk -> allowlist)

ingest_channel_artifact_impl: stat+cap, optional epoch-key encryption of the
whole file, chunk the (cipher|plain) bytes via streaming_ingest_with_options
with encrypted+serveable flags, return ChannelAttachmentDto.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Artifact fetch backend (`download_channel_artifact_impl`)

**Files:**
- Modify: `src/lib.rs` (impl, tests)

**Step 1: Implement `download_channel_artifact_impl`** (mirrors `fetch_avatar`'s FetchRequest pattern, adds decrypt + size-verify + atomic write):

```rust
pub(crate) async fn download_channel_artifact_impl(
    state: &std::sync::Mutex<NodeState>,
    community_id: String,
    cid: String,
    dest_path: String,
    expected_size: u64,
    max_bytes: Option<u64>,
) -> Result<u64, String> {
    use harmony_content::cid::ContentId;
    let cid_bytes: [u8; 32] = hex::decode(&cid).ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| "invalid cid hex".to_string())?;
    let content_id = ContentId::from_bytes(cid_bytes);
    let encrypted = content_id.flags().encrypted;

    let cap = max_bytes.map(|m| m.min(MAX_ARTIFACT_BYTES)).unwrap_or(MAX_ARTIFACT_BYTES);
    if expected_size > cap {
        return Err(format!("expected_size {expected_size} exceeds cap {cap}"));
    }

    let fetch_tx = {
        let g = state.lock().map_err(|e| format!("lock: {e}"))?;
        g.fetch_tx.clone().ok_or_else(|| "not connected".to_string())?
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    fetch_tx.send(event_loop::FetchRequest {
        cid_hex: cid,
        reply: reply_tx,
        max_bytes: Some(cap as usize),
        serveable: encrypted, // re-serve encrypted artifact books
    }).await.map_err(|_| "event loop not running".to_string())?;
    let bytes = reply_rx.await.map_err(|_| "event loop dropped fetch".to_string())??;

    let plaintext = if encrypted {
        let space = SpaceId(decode_hex16(&community_id)?);
        let epoch_key = current_epoch_key_for(state, &space).await?;
        crate::community_state_sync::decrypt_blob(&epoch_key, &bytes)
            .map_err(|e| format!("decrypt: {e:?}"))?
    } else {
        bytes
    };

    if plaintext.len() as u64 != expected_size {
        return Err(format!("size mismatch: got {} expected {expected_size}", plaintext.len()));
    }

    // Atomic write: temp file in the dest dir, then rename.
    let dest = std::path::Path::new(&dest_path);
    let tmp = dest.with_extension("partial");
    tokio::fs::write(&tmp, &plaintext).await.map_err(|e| format!("write tmp: {e}"))?;
    tokio::fs::rename(&tmp, dest).await.map_err(|e| format!("rename: {e}"))?;
    Ok(plaintext.len() as u64)
}
```

**Step 2: Tests.** The decrypt + size-verify + atomic-write logic past the fetch is unit-testable by factoring the post-fetch path into a pure helper `finalize_artifact(bytes, encrypted, epoch_key_opt, expected_size, dest_path) -> Result<u64,String>` and testing: (a) public bytes written to dest == input; (b) size mismatch → err; (c) encrypted bytes round-trip through `encrypt_blob`/`decrypt_blob` and verify. Example:

```rust
#[tokio::test]
async fn finalize_artifact_writes_public_and_verifies_size() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.txt");
    let n = finalize_artifact(b"hello".to_vec(), false, None, 5, dest.to_str().unwrap()).await.unwrap();
    assert_eq!(n, 5);
    assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"hello");
}

#[tokio::test]
async fn finalize_artifact_rejects_size_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.txt");
    let err = finalize_artifact(b"hello".to_vec(), false, None, 99, dest.to_str().unwrap()).await
        .expect_err("size mismatch");
    assert!(err.contains("size mismatch"), "got: {err}");
    assert!(!dest.exists(), "no file written on mismatch");
}

#[tokio::test]
async fn finalize_artifact_decrypts_encrypted() {
    let key = crate::owner_state_types::EpochKey::from_bytes([7u8; 32]); // use the real constructor
    let ct = crate::community_state_sync::encrypt_blob(&key, b"secret-log").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.txt");
    let n = finalize_artifact(ct, true, Some(key), 10, dest.to_str().unwrap()).await.unwrap();
    assert_eq!(n, 10);
    assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"secret-log");
}
```

> Use the real `EpochKey` constructor (check `owner_state_types.rs` for `from_bytes`/`new`). `finalize_artifact` takes `epoch_key_opt: Option<EpochKey>` and is what `download_channel_artifact_impl` calls after the fetch. Add `tempfile` to `[dev-dependencies]` only if not already present (it is used elsewhere — verify).

**Step 3: Run + check.**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(finalize_artifact) or test(download_channel_artifact)'`
Then: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: PASS + clean.

**Step 4: Commit.**
```bash
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(cas): channel artifact download (fetch -> decrypt -> verify -> write)

download_channel_artifact_impl reuses the FetchRequest path (serveable for
encrypted), decrypts with the community epoch key when the CID is flagged,
verifies plaintext length against the declared size, writes atomically.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Wire-format fixtures — no-flag-day + populated pin

**Files:**
- Modify: `tests/wire_format/channel_log_fixtures.rs`

**Step 1: Confirm the three existing pins are byte-identical** after the Task 1 `attachments: None` sweep.
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'binary(wire_format_tests) and test(channel)'`
Expected: PASS — `signed_channel_event_post_wire_bytes_pinned`, `backfill_reply_packet_wire_bytes_pinned`, `signed_channel_event_post_with_mentions_wire_bytes_pinned` all green with ORIGINAL hex. If any changed, a field order is wrong — stop and fix Task 1.

**Step 2: Add a populated-attachments fixture + pin** (TDD: placeholder hex, generate, paste):

```rust
fn fixture_with_attachments() -> SignedChannelEvent {
    let key = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);
    let payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id: SpaceId([0xc0; 16]),
        channel_id: ChannelId([0x01; 16]),
        author: OwnerAddr([0xa1; 16]),
        at: Hlc { wall_ms: 100_000, logical: 0, device_id: "a-dev".to_string() },
        content_kind: 0,
        body: "see log",
        reply_to: None,
        mentions: None,
        attachments: Some(vec![ChannelAttachment {
            cid: [0xb2; 32],
            mime: "text/plain".to_string(),
            name: "log.txt".to_string(),
            size: 42,
        }]),
    };
    sign_channel_event(&payload, &key).expect("sign")
}

#[test]
fn signed_channel_event_post_with_attachments_wire_bytes_pinned() {
    let event = fixture_with_attachments();
    let mut bytes = Vec::new();
    ciborium::into_writer(&event, &mut bytes).expect("encode");
    // To (re)generate: uncomment the eprintln, run with --nocapture, paste, re-comment.
    // eprintln!("WITH_ATTACHMENTS: {}", hex::encode(&bytes));
    let expected_hex = "PLACEHOLDER_REGENERATE";
    assert_eq!(hex::encode(&bytes), expected_hex);
}
```

**Step 3: Generate + paste the hex.**
Run: `cd src-tauri && cargo test --locked --features test-fixtures --test wire_format_tests signed_channel_event_post_with_attachments_wire_bytes_pinned -- --nocapture`
Paste the printed `WITH_ATTACHMENTS:` value over `PLACEHOLDER_REGENERATE`, re-comment the `eprintln!`.

**Step 4: Verify.**
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'binary(wire_format_tests) and test(channel)'`
Expected: all channel wire pins PASS including the new one. Sanity: the new hex must contain `627061` (the `pa` key) followed by `81` (array-of-1) and an inner map with `6263 64` (`cd` key) + `50 b2b2…` (32-byte cid bstr → `5820` for len-32... NOTE: a 32-byte bstr is `0x58 0x20` then 32 bytes). If `627061` is absent, the fixture didn't populate attachments.

**Step 5: Commit.**
```bash
git add src-tauri/tests/wire_format/channel_log_fixtures.rs
git commit -m "$(cat <<'EOF'
test(wire): pin attachment-bearing channel Post; keep prior pins

Existing pins stay byte-identical (no flag-day); new pin locks the populated
pa-key wire shape.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Two-node integration — subtree authorization + round-trip

**Files:**
- Modify: `tests/cas_serve_two_node_integration.rs` (or a sibling in the same harness)

**Step 1: Positive round-trip.** Model on the existing two-node CAS test. Node A `ingest_channel_artifact_impl` an ENCRYPTED multi-book artifact (input larger than one book — e.g. 1.5 MB of pseudo-random bytes so the chunker produces ≥2 leaves + ≥1 bundle, all encrypted+serveable). Node B `download_channel_artifact_impl` by the returned CID; assert the written file equals the original plaintext and `bytes_written == size`.

> Use a deterministic shared epoch key fixture across A and B (both must hold the same community epoch key for decrypt to work). If the existing harness wires real community state, join B to A's community; otherwise inject the same `EpochKey` into both `current_epoch_key_for` paths via the test seam.

**Step 2: Negative — un-allowlisted interior stalls.** Ingest the same artifact WITHOUT serveable (call `streaming_ingest_with_options` with `serveable:false` but `encrypted:true`), then attempt the B-side download with a short timeout; assert it errors/stalls (the encrypted interior CIDs aren't served). This guards the silent-stall failure mode.

```rust
// Pseudostructure — adapt to the harness's node setup helpers:
#[tokio::test]
async fn encrypted_multi_book_artifact_round_trips_between_nodes() { /* Step 1 */ }

#[tokio::test]
async fn unserved_encrypted_artifact_fetch_does_not_complete() { /* Step 2, with tokio timeout */ }
```

**Step 3: Run.**
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(artifact) and test(node)'` (adjust filter to the new test names/binary).
Expected: both PASS.

**Step 4: Commit.**
```bash
git add src-tauri/tests/cas_serve_two_node_integration.rs
git commit -m "$(cat <<'EOF'
test(cas): two-node encrypted artifact round-trip + unserved-stall guard

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: PR 1 full gate

**Step 1: Format.** `cd src-tauri && cargo fmt --all -- --check` → clean (else `cargo fmt --all` + amend).
**Step 2: Clippy.** `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` → no warnings.
**Step 3: Full Rust suite.** `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures` → all PASS (watch the channel-log unit tests, engine DTO/publish, serveable plumbing, wire pins, two-node artifact tests).
**Step 4: Frontend (unchanged in PR 1, but run for safety).** From repo root: `npx tsc --noEmit && npx vitest run` → clean.
**Step 5: Open PR 1.** Title (no ZEB-NNN): `Channel artifact sharing — backend (CAS storage, encryption, serve-authorization)`. Body references the spec/plan paths, summarizes the signed `attachments` field + the serveable subtree-authorization + the ingest/fetch impls, and notes: GUI + IPC come in PR 2; public-copy deterministic-CID reuse + re-serve hardening are flagged Future Work.

---

# PR 2 — IPC + frontend service

> Open only after PR 1 merges (one-PR-per-repo rule). Rebase this branch onto the merged main first.

## Task 9: Tauri commands + RPC registration

**Files:**
- Modify: `src/api/rpc.rs` (two new args structs + registrations)
- Modify: `src/lib.rs` (two `#[tauri::command]` wrappers)

**Step 1: Tauri command wrappers** in `src/lib.rs` (thin — delegate to the impls from PR 1):

```rust
#[tauri::command]
async fn ingest_channel_artifact(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String, source_path: String,
    name: Option<String>, mime: Option<String>, encrypt: Option<bool>,
) -> Result<crate::community_channel_log_engine::ChannelAttachmentDto, String> {
    ingest_channel_artifact_impl(state_lock.inner(), community_id, source_path, name, mime, encrypt.unwrap_or(true)).await
}

#[tauri::command]
async fn download_channel_artifact(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String, cid: String, dest_path: String, expected_size: u64, max_bytes: Option<u64>,
) -> Result<u64, String> {
    download_channel_artifact_impl(state_lock.inner(), community_id, cid, dest_path, expected_size, max_bytes).await
}
```

Register both in the Tauri `invoke_handler!`/`generate_handler!` list next to `post_channel_message`/`fetch_avatar`.

**Step 2: RPC args + registrations** in `src/api/rpc.rs`:

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestChannelArtifactArgs {
    community_id: String, source_path: String,
    name: Option<String>, mime: Option<String>, encrypt: Option<bool>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadChannelArtifactArgs {
    community_id: String, cid: String, dest_path: String, expected_size: u64, max_bytes: Option<u64>,
}
```

```rust
rpc!(m, "ingest_channel_artifact", IngestChannelArtifactArgs, |state, _sink, a| async move {
    crate::ingest_channel_artifact_impl(state, a.community_id, a.source_path, a.name, a.mime, a.encrypt.unwrap_or(true)).await
});
rpc!(m, "download_channel_artifact", DownloadChannelArtifactArgs, |state, _sink, a| async move {
    crate::download_channel_artifact_impl(state, a.community_id, a.cid, a.dest_path, a.expected_size, a.max_bytes).await
});
```

> Verify the existing `PostChannelMessageArgs` uses `#[serde(rename_all = "camelCase")]` and match it (the JS callers send camelCase). If the existing args structs rely on serde's default field names + camelCase JS keys, follow whatever the existing `post_channel_message` registration does.

**Step 3: IPC tests** (in `src/lib.rs` test module): bad-cid-hex rejection for `download_channel_artifact`, and `encrypt` defaulting (None → true) for `ingest`. Keep them pure where possible (e.g. assert the command rejects an invalid cid before touching the event loop).

**Step 4: Run + check.**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(ingest_channel_artifact) or test(download_channel_artifact)'`
Then: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: PASS + clean.

**Step 5: Commit.**
```bash
git add src-tauri/src/api/rpc.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(ipc): ingest_channel_artifact + download_channel_artifact commands

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: `attachments` on `post_channel_message`

**Files:**
- Modify: `src/api/rpc.rs` (`PostChannelMessageArgs.attachments`)
- Modify: `src/lib.rs` (`post_channel_message` command + `post_channel_message_impl`)

**Step 1: Add `attachments` to `PostChannelMessageArgs`:**

```rust
    mentions: Option<Vec<String>>,
    attachments: Option<Vec<crate::community_channel_log_engine::ChannelAttachmentDto>>,
```

and pass `a.attachments` as the new trailing arg in the registration's `post_channel_message_impl(..)` call.

**Step 2: Add `attachments` to the command + impl.** Both gain a trailing `attachments: Option<Vec<ChannelAttachmentDto>>`. In `post_channel_message_impl`, after the mentions parse, convert the DTOs to signed `ChannelAttachment` (hex cid → `[u8;32]`, cap-check), then pass to publish:

```rust
    let attachment_vals: Option<Vec<crate::community_channel_log::ChannelAttachment>> = match attachments {
        Some(list) => {
            if list.len() > crate::community_channel_log::MAX_ATTACHMENTS {
                return Err(format!("too many attachments: {} (max {})",
                    list.len(), crate::community_channel_log::MAX_ATTACHMENTS));
            }
            let mut out = Vec::with_capacity(list.len());
            for a in list {
                let cid: [u8; 32] = hex::decode(&a.cid).ok()
                    .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    .ok_or_else(|| "invalid attachment cid hex".to_string())?;
                if a.name.len() > crate::community_channel_log::MAX_ATTACHMENT_FIELD_BYTES
                    || a.mime.len() > crate::community_channel_log::MAX_ATTACHMENT_FIELD_BYTES {
                    return Err("attachment name/mime too long".to_string());
                }
                out.push(crate::community_channel_log::ChannelAttachment {
                    cid, mime: a.mime, name: a.name, size: a.size,
                });
            }
            Some(out)
        }
        None => None,
    };
```

Change the `.publish(body, reply_to_msg_id, mention_addrs, None)` from PR 1 Task 2 to `.publish(body, reply_to_msg_id, mention_addrs, attachment_vals)`.

**Step 3: Update the existing `post_channel_message(...)` test call sites** (signature changed) — append a trailing `None`. Add a reject test for bad attachment cid hex.

**Step 4: Run + check.**
Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(post_channel_message)'`
Then `cargo check --locked --all-targets --features test-fixtures`.
Expected: PASS + clean.

**Step 5: Commit.**
```bash
git add src-tauri/src/api/rpc.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(ipc): attachments on post_channel_message

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Frontend service

**Files:**
- Modify: `src/lib/channel-message-service.ts`
- Modify: `src/lib/__tests__/channel-message-service.test.ts`

**Step 1: Types + DTO field.** Add the attachment interface and field:

```ts
export interface ChannelAttachmentDto {
  cid: string;
  mime: string;
  name: string;
  size: number;
  encrypted: boolean;
}
```

In `ChannelMessageDto`, after `mentions?: string[];`:

```ts
  /** ZEB-535: CAS artifacts this message references; absent if none. */
  attachments?: ChannelAttachmentDto[];
```

**Step 2: `postMessage` attachments param** (forward empty as undefined, like mentions):

```ts
  async postMessage(
    communityId: string, channelId: string, body: string,
    replyTo?: string, mentions?: string[], attachments?: ChannelAttachmentDto[],
  ): Promise<string> {
    // ... existing adapter check + bodyBytes ...
    try {
      const messageId = await this.adapter.invoke('post_channel_message', {
        communityId, channelId, body: bodyBytes, replyTo,
        mentions: mentions && mentions.length > 0 ? mentions : undefined,
        attachments: attachments && attachments.length > 0 ? attachments : undefined,
      }) as string;
      return messageId;
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(msg);
    }
  }
```

**Step 3: `ingestArtifact` + `downloadArtifact` facades** (with rejection normalization):

```ts
  async ingestArtifact(
    communityId: string, sourcePath: string,
    opts?: { name?: string; mime?: string; encrypt?: boolean },
  ): Promise<ChannelAttachmentDto> {
    if (!this.adapter) throw new Error('ChannelMessageService.ingestArtifact: adapter not connected');
    try {
      return await this.adapter.invoke('ingest_channel_artifact', {
        communityId, sourcePath,
        name: opts?.name, mime: opts?.mime, encrypt: opts?.encrypt ?? true,
      }) as ChannelAttachmentDto;
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }

  async downloadArtifact(
    communityId: string, attachment: ChannelAttachmentDto, destPath: string, maxBytes?: number,
  ): Promise<number> {
    if (!this.adapter) throw new Error('ChannelMessageService.downloadArtifact: adapter not connected');
    try {
      return await this.adapter.invoke('download_channel_artifact', {
        communityId, cid: attachment.cid, destPath, expectedSize: attachment.size, maxBytes,
      }) as number;
    } catch (e: unknown) {
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }
```

**Step 4: Tests.** Update the two existing `post_channel_message` invoke assertions to include `attachments: undefined`. Add: `postMessage forwards attachments when provided`; `postMessage sends empty attachments as undefined`; `ingestArtifact forwards args + defaults encrypt=true`; `downloadArtifact maps attachment.size to expectedSize`; `ingestArtifact normalizes a raw-string rejection into an Error`.

```ts
it('downloadArtifact maps attachment.size to expectedSize', async () => {
  await service.connectAdapter(adapter);
  (adapter.invoke as any).mockResolvedValue(42);
  const att = { cid: 'ab'.repeat(32), mime: 'text/plain', name: 'l.txt', size: 42, encrypted: true };
  await service.downloadArtifact('aa'.repeat(16), att, '/tmp/l.txt');
  expect(adapter.invoke).toHaveBeenCalledWith('download_channel_artifact', {
    communityId: 'aa'.repeat(16), cid: att.cid, destPath: '/tmp/l.txt', expectedSize: 42, maxBytes: undefined,
  });
});
```

**Step 5: Frontend gates.**
Run (repo root): `npx tsc --noEmit && npx vitest run src/lib/__tests__/channel-message-service.test.ts`
Expected: tsc clean; all tests PASS.

**Step 6: Commit.**
```bash
git add src/lib/channel-message-service.ts src/lib/__tests__/channel-message-service.test.ts
git commit -m "$(cat <<'EOF'
feat(frontend): channel artifact attachments in ChannelMessageService

ChannelMessageDto.attachments; postMessage attachments param; ingestArtifact
+ downloadArtifact facades with IPC rejection normalization.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: PR 2 full gate

**Step 1:** `cd src-tauri && cargo fmt --all -- --check`
**Step 2:** `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
**Step 3:** `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
**Step 4:** repo root `npx tsc --noEmit && npx vitest run`
**Step 5:** Open PR 2. Title (no ZEB-NNN): `Channel artifact sharing — IPC + frontend service`. Body references spec/plan + PR 1, summarizes the three IPCs + the service facades, notes GUI rendering is still a follow-up.

---

## Self-Review (run against the spec after writing — completed)

**1. Spec coverage:**
- `ChannelAttachment{cid,mime,name,size}` signed field, CBOR `pa` between `mn`/`rt`, nested cd/mi/nm/sz, no-flag-day → Task 1. ✓
- `MAX_ATTACHMENTS = 16` at three gates (mint=publish Task 2; verify Task 1; IPC Task 10) + name/mime length caps → Tasks 1/2/10. ✓
- Encryption Model A (epoch key), encrypt-whole→chunk-ciphertext, `cid.flags().encrypted` drives decrypt → Tasks 4/5. ✓
- Reuse streaming_ingest (flags threaded, no third path) → Task 3. ✓
- Subtree serve-authorization (ingest allowlist + re-serve on fetch) → Task 3 + used in Tasks 4/5; negative stall test → Task 7. ✓
- Configurable cap default 1 GiB (`MAX_ARTIFACT_BYTES`) → Task 4 (the "configurable" knob surface is Future Work; v1 ships the constant default — noted). ✓
- DTO projection + empty normalization → Task 2. ✓
- Public path (no encryption; unencrypted served by gate) → Task 4; deterministic-CID public-copy REUSE deferred to fast-follow (noted in Task 4 + PR1 body). ✓ (scope note)
- Atomic write, size cross-check, error handling → Task 5. ✓
- Two-PR split → PR 1 Tasks 1-8, PR 2 Tasks 9-12. ✓
- Out-of-scope (GUI, DM, Model B, streaming AEAD, GC) → not implemented. ✓

**2. Placeholder scan:** the only literal "PLACEHOLDER" is the wire-pin hex (Task 6), generated empirically per the repo's established fixture procedure — intentional, not a gap. The `current_epoch_key_for` / `decode_hex16` / `detect_mime` / `finalize_artifact` / harness-node helpers are flagged for the implementer to bind to the exact existing accessors (signatures given); these are reuse-anchors, not unfilled logic.

**3. Type consistency:** `ChannelAttachment{cid:[u8;32], mime:String, name:String, size:u64}` (Rust signed core) ↔ `ChannelAttachmentDto{cid:String(hex), mime, name, size:u64, encrypted:bool}` (DTO/IPC) ↔ `ChannelAttachmentDto{cid,mime,name,size:number,encrypted:boolean}` (TS). Hex boundary: encode in `message_dto_for_event` + `ingest_*_impl` (Tasks 2/4), decode in `post_channel_message_impl` + `download_*_impl` (Tasks 10/5). `publish(body, reply_to, mentions, attachments)`, `MAX_ATTACHMENTS`, `TooManyAttachments{count,max}`, `IngestOptions{flags,serveable}`, `CasOp::PutLocal{..serveable..}`, `FetchRequest{..serveable}`, `IngestRequest{..serveable}` used identically across producer + asserting tests. ✓

## Deferred-to-fast-follow (flag in PR descriptions, not silently dropped)
- Public-copy deterministic-CID reuse (re-stream for now; book-dedup mitigates).
- "Configurable" cap surface (ship the 1 GiB constant; community/operator knob later).
- Rich MIME detection (small extension map for v1).

---

# PR 2 — ZEB-539 hardening (design addendum, added at PR2 time)

> Surfaced by CodeAnt/CodeRabbit/Greptile on PR1 (#312). Folded into PR2 because PR2 Task 9 wires
> `download_channel_artifact` as a live command, removing the `#[allow(dead_code)]` boundary that kept
> the unhardened re-serve path unreachable. Landing the fix in the same PR means no released build ever
> carries the gap. This addendum **supersedes** the original Task 5 / Task 9 download signature.

## Problem (recap)
`download_channel_artifact_impl` issued `FetchRequest { serveable: encrypted }`, so every fetched
encrypted book was allowlisted for member-to-member re-serve **during** the fetch — i.e. *before* the
assembled artifact was validated, and with no check that the CID is a legitimate channel attachment.
The serve gate is `content_cid_servable = !cid.flags().encrypted || serve_allowlist.contains(cid)`
(event_loop.rs:7583); the allowlist (`CommunityServeAllowlist`, content_store.rs:30) is a per-node
`HashSet<ContentId>` mutated only inside the event loop via `CasOp::PutLocal { serveable: true }`.

## Design

**Revised download contract (supersedes Task 5/9):**
```rust
pub(crate) async fn download_channel_artifact_impl(
    state: &std::sync::Mutex<NodeState>,
    community_id: String,
    channel_id: String,   // NEW — required for attachment authorization
    cid: String,
    dest_path: String,
    max_bytes: Option<u64>,
    // expected_size REMOVED — derived from the signed ChannelAttachment (source of truth)
) -> Result<u64, String>
```

**Step 1 — Authorize before fetching.** New channel-log-engine accessor (encapsulates the scan so a
future in-memory index is a drop-in):
```rust
// community_channel_log_engine.rs — scans ALL stored events (segments + tail), not a recent window,
// so an attachment shared long ago is still authorizable. Returns the signed record (authoritative).
pub async fn find_attachment(&self, cid: &[u8; 32])
    -> Result<Option<crate::community_channel_log::ChannelAttachment>, ChannelLogEngineError>;
```
The impl looks up the `(community_id, channel_id)` engine, calls `find_attachment(&cid_bytes)`; `None`
⇒ reject (`"unknown or unauthorized attachment"`). The returned attachment's `size` is the
**authoritative `expected_size`** for the fetch cap and the `finalize_artifact` size verify. Applies to
public *and* encrypted downloads (cheap hygiene: stops the command being a fetch-arbitrary-CID gadget).

**Step 2 — Fetch with `serveable: false`.** Never allowlist during the fetch. `fetch_cap` math is
unchanged but uses the authoritative size (`+ BLOB_ENCRYPTION_OVERHEAD` when encrypted).

**Step 3 — Validate, then allowlist the subtree.** After `finalize_artifact` returns `Ok` (decrypt +
size verify) **and only for encrypted artifacts** (public is servable by the gate regardless), register
the artifact's full local CID subtree for re-serve so this node can swarm it to other members
(preserving PR1's member-to-member re-serve property). New CasOp:
```rust
// content_store.rs CasOp:
AllowServeSubtree {
    root: ContentId,
    reply: tokio::sync::oneshot::Sender<Result<usize, ContentStoreError>>, // # CIDs allowlisted
},
```
Event-loop handler (event_loop.rs, near the PutLocal arm): spawn a task (do not block the select loop)
that walks the DAG **locally** from `root` — `CasOp::GetLocal` for each node (never `GetOrFetch`; all
books are already local post-fetch) + `harmony_content::bundle::parse_bundle` to extract children —
and calls `serve_allowlist.allow(cid)` for every CID including the root. Bounded by the artifact's chunk
count (≤ ~1 GiB / 1 MiB ≈ 1k books). The walk reuses the same DAG-traversal shape as
`tests/cas_serve_two_node_integration.rs::walk_cross_node`, but local-only.

## Task slicing (supersedes "PR 2 Tasks 9–12")
- **T1 — ZEB-539 primitives:** `find_attachment` engine accessor (+ unit test over segments+tail) and
  `CasOp::AllowServeSubtree` + the event-loop local-walk handler (+ coverage). No download behavior
  change yet.
- **T2 — download rework + wire:** rework `download_channel_artifact_impl` to the revised contract
  (channel_id, authorize via `find_attachment`, authoritative size, `serveable:false`, post-finalize
  `AllowServeSubtree` for encrypted); rewrite the ZEB-539 SECURITY comment to describe the *fix*; add
  the `download_channel_artifact` Tauri command + RPC with the new signature; update download tests.
- **T3 — ingest wire:** `ingest_channel_artifact` Tauri command + RPC (original Task 9 ingest half).
- **T4 — post_channel_message attachments:** original Task 10, unchanged.
- **T5 — frontend service:** original Task 11, except `downloadArtifact(communityId, channelId,
  attachment, destPath, maxBytes)` (channelId added, expectedSize dropped; size is server-derived).
- **T6 — full gate + open PR2.**
