# Channel-message Mentions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.
>
> **Repo rule:** This repo does NOT use markdown `- [ ]` checkbox TODO tracking (CodeAnt flags it; Jake's standing ruling). Steps are plain **bold numbered** items. Track progress with TodoWrite, not checkboxes in this file.

**Goal:** Add an optional, signed `mentions` list (owner-ids) to channel-message Post events so a recipient can tell a message addresses them, surfaced through the engine DTO / `channel-message-received` event / `post_channel_message` IPC.

**Architecture:** `mentions` mirrors the existing optional `reply_to` field *exactly* — it lives **inside** the signature (tamper-evident) as a new 2-char CBOR key `mn`, sorted canonically between `kd` and `rt`. Because it is `skip_serializing_if = Option::is_none`, a mention-less message is byte-identical to a pre-feature message (no flag-day). Recipients derive "mentions me" locally (`self ∈ mentions`); there is no server-side `mentionsMe` flag.

**Tech Stack:** Rust (ciborium canonical CBOR, ed25519-dalek, serde, thiserror, tokio, cargo-nextest), TypeScript/Svelte frontend (vitest), Tauri IPC.

**Spec:** `docs/specs/2026-06-21-channel-message-mentions-design.md` (ZEB-534, parent epic ZEB-533).

---

## THE load-bearing invariant (read before touching any wire code)

RFC 8949 §4.2.1 canonical CBOR orders map keys length-first then bytewise. All our keys are 2 chars, so it reduces to bytewise sort of the key bytes:

- `kd` = `0x6b 0x64`
- `mn` = `0x6d 0x6e`  ← **new**
- `rt` = `0x72 0x74`

So `kd` < `mn` < `rt`. ciborium emits map entries in **struct-declaration order**, so `mn` MUST be declared between `content_kind` (`kd`) and `reply_to` (`rt`) in BOTH `SignedChannelEvent::Post` and `ChannelPostSignedSet`. Get this order wrong and a strict RFC-8949 reader produces different bytes → silent signature-verification failures.

**No-flag-day proof:** `mentions: None` ⟹ `mn` omitted ⟹ canonical CBOR identical to pre-feature ⟹ identical signature. The two existing wire pins (`signed_channel_event_post_wire_bytes_pinned`, `backfill_reply_packet_wire_bytes_pinned`) must stay **byte-for-byte unchanged** after this work — only their `ChannelPostPayload` literal gains `mentions: None`. If either pin's expected hex changes, something is wrong; stop and investigate.

---

## File / change-site map

**Rust — signed core (`src/community_channel_log.rs`):**
- `ChannelPostPayload<'a>` (~line 197): new `mentions: Option<Vec<OwnerAddr>>` input field.
- `SignedChannelEvent::Post` (~line 156): new `mn` field between `kd` and `rt`.
- `ChannelPostSignedSet<'a>` (~line 227): new `mn` field between `kd` and `rt` (this is what changes the signed digest).
- `sign_channel_event` construction (~line 317) + `signed_set_canonical_cbor` destructure/build (~line 348): thread `mentions`.

**Rust — engine surface (`src/community_channel_log_engine.rs`):**
- `ChannelMessageDto` (~line 128): new `mentions: Option<Vec<String>>` (hex), `skip_serializing_if`.
- `message_dto_for_event` (~line 809): extract + hex-encode.
- `MAX_MENTIONS` const (~line 326) + `ChannelLogEngineError::TooManyMentions` (~line 58).
- `publish()` (~line 606): new `mentions` param, bounds-check, thread into payload.

**Rust — IPC (`src/api/rpc.rs`, `src/lib.rs`):**
- `PostChannelMessageArgs` (rpc.rs ~145) + the `post_channel_message` rpc registration (rpc.rs ~381).
- Tauri `post_channel_message` command (lib.rs ~19949) + `post_channel_message_impl` (lib.rs ~19960): new `mentions: Option<Vec<String>>`, parse hex → `OwnerAddr`.

**Frontend (`src/lib/channel-message-service.ts`):**
- `ChannelMessageDto` interface (~line 9): `mentions?: string[]`.
- `postMessage` (~line 106): optional `mentions?: string[]` param, threaded into the invoke.

**Wire fixtures (`src-tauri/tests/wire_format/channel_log_fixtures.rs`):**
- Two existing fixtures gain `mentions: None` (pins stay identical); one new mention-bearing pin added.

**Mechanical compile-fix sweep (every `ChannelPostPayload {`, `SignedChannelEvent::Post {` construction, and exhaustive destructure that adding a field breaks):** enumerated in Task 1, Step 5.

---

## Task 1: Signed `mentions` field in the channel-post core + sign/verify path

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (types, sign, canonical, + same-file test sweep & new unit tests)
- Modify: `src-tauri/src/community_channel_log_engine.rs` (one ChannelPostPayload literal — compile-fix)
- Modify: `src-tauri/src/community_fork.rs` (one Post construction — compile-fix)
- Modify: `src-tauri/tests/channel_backfill_integration.rs` (one literal — compile-fix)
- Modify: `src-tauri/tests/wire_format/channel_log_fixtures.rs` (two literals — compile-fix; pins must stay green)

**Step 1: Add `mentions` to `ChannelPostPayload`.**

In `src/community_channel_log.rs`, the struct currently ends with `pub reply_to: Option<MessageId>,`. Add a field after it:

```rust
pub struct ChannelPostPayload<'a> {
    pub id: MessageId,
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub author: OwnerAddr,
    pub at: Hlc,
    pub content_kind: u8,
    pub body: &'a str,
    pub reply_to: Option<MessageId>,
    /// ZEB-534: owner-ids this post addresses. `None` is wire-identical
    /// to a pre-feature post. Carried into the signed set (tamper-
    /// evident), mirroring `reply_to`. Owned `Vec` (not a borrow) because
    /// `sign_channel_event` moves it into the owned event variant.
    pub mentions: Option<Vec<OwnerAddr>>,
}
```

**Step 2: Add the `mn` field to `SignedChannelEvent::Post` — BETWEEN `kd` and `rt`.**

The variant currently has `content_kind` (`kd`) immediately followed by `reply_to` (`rt`). Insert `mentions` between them:

```rust
        #[serde(rename = "kd")]
        content_kind: u8,
        #[serde(rename = "mn", skip_serializing_if = "Option::is_none", default)]
        mentions: Option<Vec<OwnerAddr>>,
        #[serde(rename = "rt", skip_serializing_if = "Option::is_none", default)]
        reply_to: Option<MessageId>,
```

Also update the variant's field-order doc comment (the `// Field order ... at, au, bd, ch, ci, id, kd, rt` lines just above the fields) to read `at, au, bd, ch, ci, id, kd, mn, rt`.

**Step 3: Add the `mn` field to `ChannelPostSignedSet` — BETWEEN `kd` and `rt`.**

This struct is what the signature covers. Insert between `content_kind` and `reply_to` (no `default` here — the signed set is serialize-only):

```rust
    #[serde(rename = "kd")]
    content_kind: u8,
    #[serde(rename = "mn", skip_serializing_if = "Option::is_none")]
    mentions: &'a Option<Vec<OwnerAddr>>,
    #[serde(rename = "rt", skip_serializing_if = "Option::is_none")]
    reply_to: &'a Option<MessageId>,
```

Update this struct's field-order doc comment similarly (`... id, kd, mn, rt`).

**Step 4: Thread `mentions` through `sign_channel_event` and `signed_set_canonical_cbor`.**

In `sign_channel_event` the event construction (`let mut event = SignedChannelEvent::Post { ... }`) must set the new field (clone — `Vec` is not `Copy`). Place it before `reply_to` to match wire order:

```rust
        content_kind: payload.content_kind,
        mentions: payload.mentions.clone(),
        reply_to: payload.reply_to,
        sig: [0u8; 64], // placeholder — overwritten below
```

In `signed_set_canonical_cbor`, add `mentions` to BOTH the destructure and the `ChannelPostSignedSet` build:

```rust
    let SignedChannelEvent::Post {
        at,
        author,
        body,
        channel_id,
        community_id,
        id,
        content_kind,
        mentions,
        reply_to,
        sig: _,
    } = event;
    let signed_set = ChannelPostSignedSet {
        at,
        author,
        body,
        channel_id,
        community_id,
        id,
        content_kind: *content_kind,
        mentions,
        reply_to,
    };
```

(`mentions` here is `&Option<Vec<OwnerAddr>>`, matching the signed-set field type — no clone.)

**Step 5: Mechanical compile-fix sweep — add the field to every other construction/destructure so the workspace compiles.**

This change breaks all other `ChannelPostPayload {` literals, the non-production `SignedChannelEvent::Post {` construction, and the one exhaustive Post destructure. Apply exactly:

*Add `mentions: None,` to each `ChannelPostPayload { ... }` literal at:*
- `src/community_channel_log.rs`: `fixture_payload` (~1453), `fixture_signed_event` (~1586), and the test literals at ~1663, ~1677, ~1706, ~1720, ~1957, ~2028, ~2156, ~2828, ~2861.
- `src/community_channel_log_engine.rs`: `make_signed_event` (~2252), and `publish`'s payload (~646). *(publish's literal gets the real value in Task 3; `None` here is temporary so the workspace compiles after Task 1.)*
- `src/tests`/integration: `tests/channel_backfill_integration.rs` (~285).
- `tests/wire_format/channel_log_fixtures.rs`: BOTH literals (~49 and ~107). These keep the existing pins byte-identical — do NOT touch the expected hex.

*Add `mentions: None,` to the one non-production Post construction:*
- `src/community_fork.rs` `make_event` (~977) — the `SignedChannelEvent::Post { ... }` literal; add `mentions: None,` (place before `reply_to: None,`).

*Fix the one exhaustive Post destructure (no `..`):*
- `src/community_channel_log.rs` `sign_channel_event_round_trip` (~1470): the destructure lists every field. Add `mentions,` to the binding (before `reply_to,`) and add an assertion `assert_eq!(mentions, payload.mentions);` after the existing `assert_eq!(reply_to, payload.reply_to);`.

> Note: all *other* Post destructures (`signed_set_canonical_cbor` is handled in Step 4; `verify_channel_event`, the replay-tracker fns, `message_dto_for_event`, `community_fork` reads, `lib.rs:27239` match arm, and the integration-test reads) use `..` and are unaffected. If `cargo build` surfaces a destructure not listed here, add `mentions: _,` (or `..` if it already has other ignored fields) — but the list above is exhaustive against current `main`.

**Step 6: Write the new unit tests (TDD — write now, they should compile against Steps 1-5 and pass).**

Add to the `#[cfg(test)] mod tests` in `src/community_channel_log.rs`:

```rust
#[test]
fn sign_channel_event_carries_mentions() {
    let key = fixture_signing_key(0xa1);
    let m = vec![fixture_owner_addr(0xb2), fixture_owner_addr(0xc3)];
    let payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id: fixture_community(0xc0),
        channel_id: fixture_channel(0x01),
        author: fixture_owner_addr(0xa1),
        at: fixture_hlc(100_000, "a-dev"),
        content_kind: 0,
        body: "ping",
        reply_to: None,
        mentions: Some(m.clone()),
    };
    let signed = sign_channel_event(&payload, &key).expect("sign");
    let SignedChannelEvent::Post { mentions, .. } = signed;
    assert_eq!(mentions, Some(m));
}

#[test]
fn mentions_none_omits_mn_key_some_includes_it() {
    // CBOR text key "mn" encodes as 62 6d 6e (text-string len-2 + 'm','n').
    const MN_KEY_HEX: &str = "626d6e";
    let key = fixture_signing_key(0xa1);

    // mentions: None  -> mn key absent (wire-identical to pre-feature).
    let (none_payload, _k) = fixture_payload("no mentions");
    let none_event = sign_channel_event(&none_payload, &key).expect("sign");
    let mut none_bytes = Vec::new();
    ciborium::into_writer(&none_event, &mut none_bytes).expect("encode");
    assert!(
        !hex::encode(&none_bytes).contains(MN_KEY_HEX),
        "mentions:None must omit the mn key"
    );

    // mentions: Some -> mn key present.
    let some_payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id: fixture_community(0xc0),
        channel_id: fixture_channel(0x01),
        author: fixture_owner_addr(0xa1),
        at: fixture_hlc(100_000, "a-dev"),
        content_kind: 0,
        body: "x",
        reply_to: None,
        mentions: Some(vec![fixture_owner_addr(0xb2)]),
    };
    let some_event = sign_channel_event(&some_payload, &key).expect("sign");
    let mut some_bytes = Vec::new();
    ciborium::into_writer(&some_event, &mut some_bytes).expect("encode");
    assert!(
        hex::encode(&some_bytes).contains(MN_KEY_HEX),
        "mentions:Some must include the mn key"
    );
}

#[tokio::test]
async fn verify_channel_event_accepts_post_with_mentions() {
    // Mirrors verify_channel_event_happy_path but with mentions populated:
    // proves the signature (which now covers mn) verifies end-to-end.
    let state = fixture_state_with_alice_joined();
    let mut tracker = ChannelLogReplayTracker::new();
    let (key, author, _pub64) = fixture_identity(0xa1);
    let payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id: fixture_community(0xc0),
        channel_id: fixture_channel(0x01),
        author,
        at: fixture_hlc(100_000, "a-dev"),
        content_kind: 0,
        body: "hi @bob",
        reply_to: None,
        mentions: Some(vec![fixture_owner_addr(0xb2)]),
    };
    let event = sign_channel_event(&payload, &key).expect("sign");
    verify_channel_event(
        &event,
        &fixture_community(0xc0),
        &fixture_channel(0x01),
        &state,
        &mut tracker,
    )
    .await
    .expect("verify accepts post with mentions");
}
```

**Step 7: Run the new + existing channel-log unit tests (lib-only — avoids the integration relink cost).**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(channel_event) or test(mentions) or test(signed_set) or test(sign_channel_event)'`
Expected: PASS, including `sign_channel_event_carries_mentions`, `mentions_none_omits_mn_key_some_includes_it`, `verify_channel_event_accepts_post_with_mentions`, and the extended `sign_channel_event_round_trip`.

**Step 8: Confirm the workspace still compiles (catches any missed sweep site).**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: clean compile. If a literal/destructure error appears, fix it per Step 5's rule, then re-run.

**Step 9: Commit.**

```bash
git add src-tauri/src/community_channel_log.rs src-tauri/src/community_channel_log_engine.rs src-tauri/src/community_fork.rs src-tauri/tests/channel_backfill_integration.rs src-tauri/tests/wire_format/channel_log_fixtures.rs
git commit -m "$(cat <<'EOF'
feat(channel-log): signed mentions field on channel-post events

Add optional mentions: Vec<OwnerAddr> to SignedChannelEvent::Post under
the canonical CBOR key `mn` (sorts between kd and rt), inside the
signature. mentions:None is byte-identical to a pre-feature post.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Surface `mentions` through the engine DTO + `channel-message-received` event

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`ChannelMessageDto`, `message_dto_for_event`, new test)

**Step 1: Add `mentions` to `ChannelMessageDto`.**

After the `poll_id` field (the last field, with its `skip_serializing_if`), add:

```rust
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_id: Option<String>,
    /// ZEB-534: owner-ids (lowercase hex) this message addresses. Omitted
    /// when the post carries no mentions so existing consumers never see
    /// `mentions: null`. Recipients derive "mentions me" as
    /// `self_owner_hex ∈ mentions`. `ChannelMessageReceivedPayload` carries
    /// the full DTO, so this rides the live event automatically.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mentions: Option<Vec<String>>,
```

**Step 2: Project `mentions` in `message_dto_for_event`.**

Add `mentions` to the destructure (it currently lists `id, author, at, body, reply_to, ..`) and to the `ChannelMessageDto { ... }` construction:

```rust
        let SignedChannelEvent::Post {
            id,
            author,
            at,
            body,
            mentions,
            reply_to,
            ..
        } = event;
```

and in the returned struct, after `reply_to: reply_to.map(|m| hex::encode(m.0)),`:

```rust
            reply_to: reply_to.map(|m| hex::encode(m.0)),
            mentions: mentions
                .as_ref()
                .map(|v| v.iter().map(|a| hex::encode(a.0)).collect()),
            kind,
            poll_id,
```

**Step 3: Write the DTO projection test.**

Add to the engine's `#[cfg(test)] mod tests` (mirrors `event_to_dto_projects_post_fields`):

```rust
#[tokio::test]
async fn event_to_dto_projects_mentions_as_hex() {
    let fix = build_engine_fixture(8, 250, 1000).await;
    let m0 = OwnerAddr([0xb2; 16]);
    let m1 = OwnerAddr([0xc3; 16]);
    let id = {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        MessageId(b)
    };
    let payload = ChannelPostPayload {
        id,
        community_id: fix.community_id,
        channel_id: fix.channel_id,
        author: fix.self_owner,
        at: Hlc { wall_ms: 5_000, logical: 0, device_id: "device-x".to_string() },
        content_kind: 0,
        body: "hi @bob",
        reply_to: None,
        mentions: Some(vec![m0, m1]),
    };
    let ev = sign_channel_event(&payload, &fix.signing_key).expect("sign");
    let dto = fix.engine.event_to_dto(&ev);
    assert_eq!(dto.mentions, Some(vec![hex::encode(m0.0), hex::encode(m1.0)]));

    // Mention-less event omits the field.
    let ev_none = make_signed_event(
        fix.community_id,
        fix.channel_id,
        fix.self_owner,
        Hlc { wall_ms: 5_001, logical: 0, device_id: "device-x".to_string() },
        "no mentions",
        &fix.signing_key,
    );
    assert!(fix.engine.event_to_dto(&ev_none).mentions.is_none());
}
```

**Step 4: Run the engine DTO tests.**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(event_to_dto)'`
Expected: PASS, including `event_to_dto_projects_mentions_as_hex`.

**Step 5: Commit.**

```bash
git add src-tauri/src/community_channel_log_engine.rs
git commit -m "$(cat <<'EOF'
feat(channel-log): carry mentions hex through ChannelMessageDto

message_dto_for_event hex-encodes each mention OwnerAddr; the DTO rides
channel-message-received and list_channel_messages unchanged.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Accept + bound `mentions` in `ChannelLogEngine::publish`

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` (const, error, `publish`, its in-file test call sites, new test)
- Modify: `src-tauri/src/lib.rs` (two engine.publish call sites — compile-fix)
- Modify: `src-tauri/tests/channel_backfill_integration.rs` + `src-tauri/tests/community_channel/community_channel_messages_integration.rs` (publish call sites — compile-fix)

**Step 1: Add the `MAX_MENTIONS` const next to `MAX_BODY_BYTES`.**

```rust
const MAX_BODY_BYTES: usize = 64 * 1024;

/// ZEB-534: hard cap on mentions per channel post. Bounds the signed-set
/// size and each recipient's "mentions me" scan. Membership-gating of the
/// targets is out of scope for v1.
const MAX_MENTIONS: usize = 64;
```

**Step 2: Add the `TooManyMentions` error variant next to `BodyTooLarge`.**

In `ChannelLogEngineError`:

```rust
    #[error("body too large: {len} bytes (max {max})")]
    BodyTooLarge { len: usize, max: usize },

    #[error("too many mentions: {count} (max {max})")]
    TooManyMentions { count: usize, max: usize },
```

**Step 3: Add the `mentions` param to `publish` + validate + thread into the payload.**

Change the signature (new param LAST — additive) and add the bounds check right after the body check:

```rust
    pub async fn publish(
        self: &Arc<Self>,
        body: Vec<u8>,
        reply_to: Option<MessageId>,
        mentions: Option<Vec<OwnerAddr>>,
    ) -> Result<MessageId, ChannelLogEngineError> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ChannelLogEngineError::BodyTooLarge {
                len: body.len(),
                max: MAX_BODY_BYTES,
            });
        }
        if let Some(m) = &mentions {
            if m.len() > MAX_MENTIONS {
                return Err(ChannelLogEngineError::TooManyMentions {
                    count: m.len(),
                    max: MAX_MENTIONS,
                });
            }
        }
```

In the `ChannelPostPayload { ... }` literal inside `publish`, replace the temporary `mentions: None,` (added in Task 1) with `mentions,` (move the validated value in):

```rust
            content_kind: 0,
            body: &body_str,
            reply_to,
            mentions,
```

**Step 4: Update every `engine.publish(...)` call site to pass the new arg.**

All current callers pass two args; append the third.

*Production (`src/lib.rs`)* — the two internal post paths that send a body with no mentions:
- `src/lib.rs` ~32552: `engine.publish(body, None, None)`
- `src/lib.rs` ~33187: `engine.publish(body, None, None)`
- `src/lib.rs` ~20015 (`post_channel_message_impl`): set to `.publish(body, reply_to_msg_id, None)` *for now* — Task 4 replaces the `None` with the parsed mentions.

*Engine in-file tests (`src/community_channel_log_engine.rs`):* call sites ~2512, ~2544, ~2595, ~2751, ~3497 — append `, None` (e.g. `.publish(body.clone(), None, None)`).

*Integration tests:* `tests/channel_backfill_integration.rs` ~497, ~619, ~760, ~825 and `tests/community_channel/community_channel_messages_integration.rs` ~414, ~483 — append `, None`.

**Step 5: Write the bounds test (mirrors the oversized-body test).**

Add to the engine test module:

```rust
#[tokio::test]
async fn publish_rejects_too_many_mentions() {
    let fix = build_engine_fixture(8, 250, 1000).await;
    let too_many: Vec<OwnerAddr> = (0..=MAX_MENTIONS).map(|i| OwnerAddr([i as u8; 16])).collect();
    assert_eq!(too_many.len(), MAX_MENTIONS + 1);
    let err = fix
        .engine
        .publish(b"hi".to_vec(), None, Some(too_many))
        .await
        .expect_err("over-cap mentions must error");
    assert!(
        matches!(err, ChannelLogEngineError::TooManyMentions { count, max }
            if count == MAX_MENTIONS + 1 && max == MAX_MENTIONS),
        "got: {err:?}"
    );
}
```

**Step 6: Run the engine publish tests (lib-only).**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(publish)'`
Expected: PASS, including `publish_rejects_too_many_mentions` and the existing `publish_rejects_oversized_body`.

**Step 7: Confirm the integration tests still compile (they were touched).**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: clean compile.

**Step 8: Commit.**

```bash
git add src-tauri/src/community_channel_log_engine.rs src-tauri/src/lib.rs src-tauri/tests/channel_backfill_integration.rs src-tauri/tests/community_channel/community_channel_messages_integration.rs
git commit -m "$(cat <<'EOF'
feat(channel-log): publish() accepts + bounds mentions

New MAX_MENTIONS cap (64) and TooManyMentions error; publish threads the
validated mentions list into the signed payload.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: IPC surface (RPC args, Tauri command, frontend service)

**Files:**
- Modify: `src-tauri/src/api/rpc.rs` (`PostChannelMessageArgs`, rpc registration)
- Modify: `src-tauri/src/lib.rs` (`post_channel_message` command + `post_channel_message_impl`, existing + new IPC tests)
- Modify: `src/lib/channel-message-service.ts` (DTO interface + `postMessage`)
- Modify: `src/lib/__tests__/channel-message-service.test.ts` (update 2 existing asserts + 1 new test)

**Step 1: Add `mentions` to `PostChannelMessageArgs` and the rpc registration.**

In `src/api/rpc.rs`:

```rust
struct PostChannelMessageArgs {
    community_id: String,
    channel_id: String,
    body: Vec<u8>,
    reply_to: Option<String>,
    mentions: Option<Vec<String>>,
}
```

and in the `rpc!(... "post_channel_message", PostChannelMessageArgs, ...)` body, pass it through:

```rust
            crate::post_channel_message_impl(
                state,
                a.community_id,
                a.channel_id,
                a.body,
                a.reply_to,
                a.mentions,
            )
            .await
```

**Step 2: Add `mentions` to the Tauri command + impl, parse hex → `OwnerAddr`.**

In `src/lib.rs`, the `post_channel_message` command (the `#[tauri::command]` fn) and `post_channel_message_impl` both gain a trailing `mentions: Option<Vec<String>>`:

```rust
async fn post_channel_message(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
    body: Vec<u8>,
    reply_to: Option<String>,
    mentions: Option<Vec<String>>,
) -> Result<String, String> {
    post_channel_message_impl(
        state_lock.inner(),
        community_id,
        channel_id,
        body,
        reply_to,
        mentions,
    )
    .await
}

pub(crate) async fn post_channel_message_impl(
    state: &std::sync::Mutex<NodeState>,
    community_id: String,
    channel_id: String,
    body: Vec<u8>,
    reply_to: Option<String>,
    mentions: Option<Vec<String>>,
) -> Result<String, String> {
```

Inside `post_channel_message_impl`, after the existing `reply_to_msg_id` parsing block (and before the `registry` lookup), parse the mentions (mirrors the reply_to hex parse):

```rust
    let mention_addrs: Option<Vec<crate::owner_state_types::OwnerAddr>> = match mentions {
        Some(list) => {
            let mut out = Vec::with_capacity(list.len());
            for s in list {
                if s.len() != 32 {
                    return Err("each mention must be 16 bytes (32 hex chars)".to_string());
                }
                let bytes: [u8; 16] = hex::decode(&s)
                    .map_err(|e| format!("invalid mention hex: {e}"))?
                    .try_into()
                    .map_err(|_| "mention length wrong".to_string())?;
                out.push(crate::owner_state_types::OwnerAddr(bytes));
            }
            Some(out)
        }
        None => None,
    };
```

Then change the publish call (the `None` placeholder left in Task 3) to pass the parsed list:

```rust
    let msg_id = engine
        .publish(body, reply_to_msg_id, mention_addrs)
        .await
        .map_err(|e| e.to_string())?;
    Ok(hex::encode(msg_id.0))
```

**Step 3: Update the existing `post_channel_message(...)` test call sites + add validation tests.**

The command-fn signature changed, so the 5 existing IPC tests that call `post_channel_message(state, cid, chid, body, reply_to)` must append a trailing `None`:
- `post_channel_message_rejects_short_community_id`, `_rejects_short_channel_id`, `_rejects_bad_hex`, `_rejects_short_reply_to`, `_errors_when_registry_missing` — add `, None` as the last argument to each `post_channel_message(...)` call.

Then add two new tests next to them:

```rust
#[tokio::test]
async fn post_channel_message_rejects_bad_mention_hex() {
    let app = mock_app_with_default_node_state();
    let state = app.state::<StdMutex<NodeState>>();
    let err = post_channel_message(
        state,
        "00".repeat(16),
        "00".repeat(16),
        vec![1],
        None,
        Some(vec!["zz".repeat(16)]),
    )
    .await
    .expect_err("bad mention hex must error");
    assert!(err.contains("invalid mention hex"), "got: {err}");
}

#[tokio::test]
async fn post_channel_message_rejects_short_mention() {
    let app = mock_app_with_default_node_state();
    let state = app.state::<StdMutex<NodeState>>();
    let err = post_channel_message(
        state,
        "00".repeat(16),
        "00".repeat(16),
        vec![1],
        None,
        Some(vec!["ab".into()]),
    )
    .await
    .expect_err("short mention must error");
    assert!(err.contains("each mention must be 16 bytes"), "got: {err}");
}
```

**Step 4: Run the IPC tests (lib-only).**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(post_channel_message)'`
Expected: PASS, including the two new reject tests.

**Step 5: Add `mentions` to the frontend DTO + `postMessage`.**

In `src/lib/channel-message-service.ts`, after the `pollId?: string;` field of `ChannelMessageDto`:

```ts
  pollId?: string;
  /**
   * ZEB-534: owner-ids (hex) this message addresses, or absent if none.
   * Recipients derive "mentions me" as `selfOwnerHex` ∈ mentions. GUI
   * render/notify is a follow-up; this field just carries the data.
   */
  mentions?: string[];
```

Update `postMessage` to accept and forward an optional `mentions`:

```ts
  async postMessage(
    communityId: string,
    channelId: string,
    body: string,
    replyTo?: string,
    mentions?: string[],
  ): Promise<string> {
    if (!this.adapter) throw new Error('ChannelMessageService.postMessage: adapter not connected');
    const bodyBytes = Array.from(new TextEncoder().encode(body));
    const messageId = await this.adapter.invoke('post_channel_message', {
      communityId,
      channelId,
      body: bodyBytes,
      replyTo,
      mentions,
    }) as string;
    return messageId;
  }
```

**Step 6: Update the frontend tests (existing asserts + new case).**

In `src/lib/__tests__/channel-message-service.test.ts`, the two existing `toHaveBeenCalledWith('post_channel_message', { ... })` assertions now also receive `mentions`. Add `mentions: undefined,` to the expected object in BOTH `postMessage invokes post_channel_message with camelCase args` and `postMessage forwards replyTo when provided`. Then add a new test:

```ts
it('postMessage forwards mentions when provided', async () => {
  await service.connectAdapter(adapter);
  (adapter.invoke as any).mockResolvedValue('mid');
  await service.postMessage(
    'aa'.repeat(16),
    'bb'.repeat(16),
    'hi',
    undefined,
    ['cc'.repeat(16), 'dd'.repeat(16)],
  );
  expect(adapter.invoke).toHaveBeenCalledWith('post_channel_message', {
    communityId: 'aa'.repeat(16),
    channelId: 'bb'.repeat(16),
    body: Array.from(new TextEncoder().encode('hi')),
    replyTo: undefined,
    mentions: ['cc'.repeat(16), 'dd'.repeat(16)],
  });
});
```

**Step 7: Run the frontend gates (from repo root).**

Run: `npx tsc --noEmit && npx vitest run src/lib/__tests__/channel-message-service.test.ts`
Expected: tsc clean; vitest PASS including `postMessage forwards mentions when provided`.

**Step 8: Commit.**

```bash
git add src-tauri/src/api/rpc.rs src-tauri/src/lib.rs src/lib/channel-message-service.ts src/lib/__tests__/channel-message-service.test.ts
git commit -m "$(cat <<'EOF'
feat(ipc): mentions on post_channel_message + channel DTO

post_channel_message accepts a hex owner-id list (validated, parsed to
OwnerAddr); frontend ChannelMessageDto + postMessage carry mentions.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Wire-format fixtures — prove no-flag-day + pin the populated path

**Files:**
- Modify: `src-tauri/tests/wire_format/channel_log_fixtures.rs`

**Step 1: Confirm the two existing pins are still byte-identical.**

The two existing `ChannelPostPayload` literals got `mentions: None` in Task 1, Step 5. Their expected hex must be unchanged. Run:

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'binary(wire_format_tests) and test(channel)'`
Expected: PASS — `signed_channel_event_post_wire_bytes_pinned` and `backfill_reply_packet_wire_bytes_pinned` both green with their ORIGINAL hex. If either fails, the no-flag-day invariant is violated — stop and investigate (likely a wrong field-declaration order from Task 1).

**Step 2: Add a mention-bearing fixture + a new pin test (TDD: write with a placeholder, capture the real hex, paste).**

Add to `src-tauri/tests/wire_format/channel_log_fixtures.rs`:

```rust
fn fixture_with_mentions() -> SignedChannelEvent {
    let key = ed25519_dalek::SigningKey::from_bytes(&[0xa1; 32]);
    let payload = ChannelPostPayload {
        id: MessageId([0x11; 16]),
        community_id: SpaceId([0xc0; 16]),
        channel_id: ChannelId([0x01; 16]),
        author: OwnerAddr([0xa1; 16]),
        at: Hlc {
            wall_ms: 100_000,
            logical: 0,
            device_id: "a-dev".to_string(),
        },
        content_kind: 0,
        body: "hello",
        reply_to: None,
        mentions: Some(vec![OwnerAddr([0xb2; 16]), OwnerAddr([0xc3; 16])]),
    };
    sign_channel_event(&payload, &key).expect("sign")
}

#[test]
fn signed_channel_event_post_with_mentions_wire_bytes_pinned() {
    let event = fixture_with_mentions();
    let mut bytes = Vec::new();
    ciborium::into_writer(&event, &mut bytes).expect("encode");
    // Field order: at, au, bd, ch, ci, id, kd, mn, rt (skipped), sg.
    // The `mn` array sits between kd and sg. To (re)generate this hex,
    // temporarily uncomment the eprintln below, run with --nocapture,
    // paste the printed value, then remove the eprintln.
    // eprintln!("WITH_MENTIONS: {}", hex::encode(&bytes));
    let expected_hex = "PLACEHOLDER_REGENERATE";
    assert_eq!(hex::encode(&bytes), expected_hex);
}
```

**Step 3: Generate the real hex and paste it.**

Run with the `eprintln!` line uncommented:
`cd src-tauri && cargo test --locked --features test-fixtures --test wire_format_tests signed_channel_event_post_with_mentions_wire_bytes_pinned -- --nocapture`
Copy the `WITH_MENTIONS: <hex>` value from stderr, replace `PLACEHOLDER_REGENERATE` with it, and re-comment the `eprintln!` line.

**Step 4: Verify the new pin holds.**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'binary(wire_format_tests) and test(channel)'`
Expected: all channel wire pins PASS, including the new mention-bearing one.

**Step 5: Sanity-check the new hex contains `mn` and the two mention addrs.**

The pasted hex must contain `626d6e` (the `mn` key) followed shortly by `82` (CBOR array-of-2) and the two 16-byte bstrs `50b2b2…` / `50c3c3…` (`50` = bstr len-16). This is a manual eyeball check, not a code step — if `626d6e` is absent, the fixture didn't populate mentions.

**Step 6: Commit.**

```bash
git add src-tauri/tests/wire_format/channel_log_fixtures.rs
git commit -m "$(cat <<'EOF'
test(wire): pin mention-bearing channel Post; keep mention-less pins

Existing pins stay byte-identical (no flag-day); new pin locks the
populated mn-key wire shape.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Full gate

**Files:** none (verification only)

**Step 1: Format.**

Run: `cd src-tauri && cargo fmt --all -- --check`
Expected: no output (clean). If it reports diffs, run `cargo fmt --all`, review, and amend the relevant commit.

**Step 2: Clippy.**

Run: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: no warnings. (This is the first `--all-targets` build that relinks integration binaries — expect a longer compile; do not interrupt.)

**Step 3: Full Rust test suite.**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: all PASS. Watch specifically for the channel-log unit tests, engine DTO/publish tests, the IPC reject tests, and the three wire pins.

**Step 4: Frontend gates (from repo root).**

Run: `npx tsc --noEmit && npx vitest run`
Expected: tsc clean; all vitest suites PASS.

**Step 5: If anything fails, fix at the source and amend the owning commit; re-run the failing gate, then the full gate.**

---

## Task 7 (OPTIONAL — stretch, not required for the PR): co-located two-engine e2e

Spec test-plan item 5 is explicitly optional. The unit + DTO + publish + IPC + wire coverage above fully exercises the mechanics; mentions ride the *same* DTO/event/list paths already e2e-proven for ordinary messages. If you want belt-and-suspenders coverage, add a co-located (no live-WAN) test in the `e2e-harness` crate that posts with `mentions` from engine A and asserts engine B's `list_channel_messages` + `channel-message-received` carry the hex list and that B derives `mentionsMe`. Model it on the existing co-located multi-engine channel tests (e.g. `s8`/`s9`). Skip if it would balloon scope — note the omission in the PR description.

---

## Self-Review (run against the spec after writing — completed)

**1. Spec coverage:**
- Signed `mn` field between `kd`/`rt`, inside signature → Task 1 (Steps 2-4). ✓
- `OwnerAddr` 16-byte bstr reuse → Task 1 (already the `author` type). ✓
- No-flag-day (None byte-identical; existing pin unchanged; new pin for populated) → Task 1 Step 6 + Task 5. ✓
- DTO `mentions: Option<Vec<String>>` camelCase hex; event carries it; no server `mentionsMe` → Task 2. ✓
- Post path: RPC args, Tauri command/impl hex-parse, publish param → Tasks 3-4. ✓
- Frontend DTO + send param → Task 4 (Steps 5-6). ✓
- `MAX_MENTIONS` (64) + `TooManyMentions` + malformed-hex error → Tasks 3-4. ✓
- Membership-gating, @everyone, GUI render/notify, server mentionsMe flag = OUT of scope → not implemented (correct). ✓

**2. Placeholder scan:** The only literal "PLACEHOLDER" is the wire-pin hex, which is *intentionally* generated empirically in Task 5 Step 3 (this is the established repo procedure for wire fixtures, not an unfilled gap). No other placeholders.

**3. Type consistency:** `mentions` is `Option<Vec<OwnerAddr>>` in payload/event/signed-set/publish (Rust core), `Option<Vec<String>>` (hex) in the DTO/RPC args/command, `string[]` in TS. The hex boundary is exactly: encode in `message_dto_for_event` (Task 2), decode in `post_channel_message_impl` (Task 4). `MAX_MENTIONS`, `TooManyMentions { count, max }`, and the error string literals (`"invalid mention hex"`, `"each mention must be 16 bytes (32 hex chars)"`) are used identically in production code and the asserting tests. ✓
