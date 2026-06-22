# ZEB-536 Message Reactions (Spec 1, backend/headless) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add lightweight multi-reaction-set "reactions/acks" to channel messages — a new signed `React` event on the existing per-channel log, materialized read-time with last-writer-wins convergence, exposed over the headless `api` surface.

**Architecture:** Reactions are a new `SignedChannelEvent::React` variant in the existing append-only per-channel log (`community_channel_log.rs`), flowing through the *same* sign→encrypt→Zenoh→backfill→seal path as `Post`. Convergence is last-writer-wins per `(target, author, emoji)` by HLC, maintained in an in-memory `ReactionIndex` (rebuilt from the persisted log at boot, updated at the single `append` choke point). A `set_message_reaction` RPC verb drives it; reactions fold into the message DTO via the read path; a `channel-reaction-received` event fires on local + peer paths.

**Tech Stack:** Rust, Tauri, ciborium (canonical CBOR), ed25519-dalek, ChaCha20-Poly1305, tokio. Tests via `cargo nextest`.

## Global Constraints

- All test runs: `cd src-tauri && cargo nextest run --locked --all-targets --features test-fixtures` (scope during dev with `-E 'test(channel)'` or `-p harmony-app`). Source: repo `CLAUDE.md`.
- Lint before commit: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; format with `cargo fmt --all`.
- `--locked` and `--all-targets` are load-bearing (CLAUDE.md). Never weaken the keychain test gates (ZEB-428).
- Wire format: `SignedChannelEvent` is `#[serde(tag="tg", content="vl")]`; every inner field key is exactly 2 chars, declared in RFC-8949 bytewise-sorted order (ciborium emits in declaration order).
- TDD, watched-red: write the failing test, run it, watch it fail for the right reason, then implement. Frequent commits.
- Branch: `zeb-536-message-reactions` (already created off `main@1687dd8d`). Never `git add -A` — stage explicit paths. Commit trailer: `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- `MAX_REACTION_EMOJI_BYTES = 32`.
- Out of scope: Svelte UI (Spec 2), custom/hosted emoji via CAS (Spec 3), reactions in pre-fork snapshots (kept Post-only for v1).

---

## File Structure

- `src-tauri/src/community_channel_log.rs` — pure wire/crypto/persistence layer. Gets: `React` variant, `MAX_REACTION_EMOJI_BYTES`, `ChannelReactPayload`, `ChannelReactSignedSet`, `sign_channel_react`, `SignedChannelEvent` accessor methods, `ReactionDto`, `ReactionIndex`, `ChannelLog.reaction_index` field + maintenance + boot rebuild + `reactions_for`. Tasks 1–3.
- `src-tauri/src/community_channel_log_engine.rs` — async engine + DTOs. Gets: `ChannelMessageDto.reactions` field, `ChannelReactionReceivedPayload`, `ChannelLogEngine::react`, `emit_reaction_received`, `list_message_dtos`, inbound React emit branch, `ChannelLogEngineError::ReactionEmojiTooLarge`. Task 4.
- `src-tauri/src/community_fork.rs` — filter `React` out of the pre-fork snapshot (Post-only for v1). Task 1 (compile-fix).
- `src-tauri/src/api/rpc.rs` — `SetMessageReactionArgs`, `set_message_reaction` registration, curated-surface test. Task 5.
- `src-tauri/src/lib.rs` — `set_message_reaction_impl`, `#[tauri::command]` wrapper, `generate_handler!` entry, `list_channel_messages_impl` → `list_message_dtos`. Task 5.
- `src-tauri/tests/wire_format_channel_log_fixtures.rs` — React wire-pin fixture. Task 1.

---

## Task 1: `React` variant + sign/verify in the pure layer

Adding the enum variant is **compile-forcing**: every irrefutable `let SignedChannelEvent::Post {..}` and exhaustive match becomes an error until handled. This task adds the variant and makes the whole crate compile again with reactions signing and verifying.

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (enum ~156-192; `signed_set_canonical_cbor` 347-373; `sign_channel_event` 334-335; `would_accept` 505-530; `record` 537-546; `verify_channel_event` 666-673; `append` 920-924; `seal_and_persist` 1044, 1062; `max_hlc` 1279)
- Modify: `src-tauri/src/community_channel_log_engine.rs` (`message_dto_for_event` 818-826; `list_messages` 552-573)
- Modify: `src-tauri/src/community_fork.rs` (~354-360)
- Test: `src-tauri/src/community_channel_log.rs` `#[cfg(test)] mod tests` (after 1404); `src-tauri/tests/wire_format_channel_log_fixtures.rs`

**Interfaces:**
- Produces: `SignedChannelEvent::React { target: MessageId, community_id: SpaceId, channel_id: ChannelId, author: OwnerAddr, at: Hlc, emoji: String, add: bool, sig: [u8;64] }`; `pub fn sign_channel_react(payload: &ChannelReactPayload, signing_key: &ed25519_dalek::SigningKey) -> Result<SignedChannelEvent, ChannelEventError>`; accessor methods `community_id()/channel_id()/author()/at()/id()/sig()` on `SignedChannelEvent` (all `-> &T`; `id()` returns the message id for `Post`, the target id for `React`); `pub const MAX_REACTION_EMOJI_BYTES: usize = 32`.

- [ ] **Step 1: Write the failing sign/verify test**

Add to `community_channel_log.rs` `mod tests`. Construct `MockState`, the signing key, owner/community/channel ids, and `Hlc` **exactly as `verify_channel_event_happy_path` does** (community_channel_log.rs:1895) — copy its setup. Then:

```rust
#[tokio::test]
async fn sign_and_verify_react_round_trips() {
    // --- identical setup to verify_channel_event_happy_path: mk MockState
    //     `state`, `signing_key`, `author`, `community_id`, `channel_id`, `at` ---
    let payload = ChannelReactPayload {
        target: MessageId([7u8; 16]),
        community_id,
        channel_id,
        author,
        at: at.clone(),
        emoji: "👍".to_string(),
        add: true,
    };
    let event = sign_channel_react(&payload, &signing_key).expect("sign react");
    // wire round-trips through canonical CBOR (encrypt/decrypt under a ChannelKey)
    let key = derive_channel_key(&fixture_mk(), &community_id, &channel_id);
    let packet = encrypt_channel_packet(&key, &event).expect("encrypt");
    let decoded = decrypt_channel_packet(&key, &packet).expect("decrypt");
    assert_eq!(decoded, event);
    // verify passes for an authorized member
    let mut tracker = ChannelLogReplayTracker::new();
    verify_channel_event(&event, &community_id, &channel_id, &state, &mut tracker)
        .await
        .expect("verify react");
}
```

- [ ] **Step 2: Run it — watch it fail to compile**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(sign_and_verify_react_round_trips)'`
Expected: FAIL — `cannot find type ChannelReactPayload` / `function sign_channel_react not found`.

- [ ] **Step 3: Add the const, variant, payload, signed-set**

In `community_channel_log.rs`, add after `MessageId` (line 128):

```rust
/// ZEB-536: max byte length of a reaction emoji string. Room for a ZWJ
/// emoji sequence plus a short custom shortcode (Spec 3). Over-long
/// reactions fail `react()` locally and `verify_channel_event` inbound.
pub const MAX_REACTION_EMOJI_BYTES: usize = 32;
```

Replace the `// React { id, ci, ch, au, at, em, sg }` comment line (191) with the real variant (keys in RFC-8949 bytewise order `ad, at, au, ch, ci, em, id, sg`):

```rust
    /// ZEB-536: a reaction/ack targeting a prior message in this channel.
    /// Append-only — un-reacting is a fresh React with `add=false`, never
    /// a mutation. `id` is the TARGET message id (reactions sharing a
    /// target are deduped by the per-(channel,author,device) HLC lane,
    /// not by id). Convergence is LWW per (target, author, emoji) by HLC.
    #[serde(rename = "r")]
    React {
        #[serde(rename = "ad")]
        add: bool,
        #[serde(rename = "at")]
        at: Hlc,
        #[serde(rename = "au")]
        author: OwnerAddr,
        #[serde(rename = "ch")]
        channel_id: ChannelId,
        #[serde(rename = "ci")]
        community_id: SpaceId,
        #[serde(rename = "em")]
        emoji: String,
        #[serde(rename = "id")]
        target: MessageId,
        #[serde(
            rename = "sg",
            serialize_with = "crate::owner_state_types::serialize_bytes_as_bstr",
            deserialize_with = "crate::owner_state_types::deserialize_bytes_from_bstr"
        )]
        sig: [u8; 64],
    },
```

Add after `ChannelPostSignedSet` (line 244), the payload + signed-set (signed-set keys in order `ad, at, au, ch, ci, em, id`):

```rust
/// Caller-filled pre-signature payload for a reaction. Hand to
/// `sign_channel_react` to get a wire-ready `SignedChannelEvent::React`.
pub struct ChannelReactPayload {
    pub target: MessageId,
    pub community_id: SpaceId,
    pub channel_id: ChannelId,
    pub author: OwnerAddr,
    pub at: Hlc,
    pub emoji: String,
    pub add: bool,
}

/// Pre-signature signed-set for a React (everything except `sg`).
/// 2-char keys in RFC-8949 bytewise order: ad, at, au, ch, ci, em, id.
#[derive(Serialize)]
struct ChannelReactSignedSet<'a> {
    #[serde(rename = "ad")]
    add: bool,
    #[serde(rename = "at")]
    at: &'a Hlc,
    #[serde(rename = "au")]
    author: &'a OwnerAddr,
    #[serde(rename = "ch")]
    channel_id: &'a ChannelId,
    #[serde(rename = "ci")]
    community_id: &'a SpaceId,
    #[serde(rename = "em")]
    emoji: &'a str,
    #[serde(rename = "id")]
    target: &'a MessageId,
}
```

- [ ] **Step 4: Add accessors + `sign_channel_react`; convert `signed_set_canonical_cbor` to a match; fix `sign_channel_event`**

Add an accessor impl block (place after the enum, ~line 192):

```rust
impl SignedChannelEvent {
    /// Community id (both variants).
    pub fn community_id(&self) -> &SpaceId {
        match self {
            SignedChannelEvent::Post { community_id, .. }
            | SignedChannelEvent::React { community_id, .. } => community_id,
        }
    }
    pub fn channel_id(&self) -> &ChannelId {
        match self {
            SignedChannelEvent::Post { channel_id, .. }
            | SignedChannelEvent::React { channel_id, .. } => channel_id,
        }
    }
    pub fn author(&self) -> &OwnerAddr {
        match self {
            SignedChannelEvent::Post { author, .. }
            | SignedChannelEvent::React { author, .. } => author,
        }
    }
    pub fn at(&self) -> &Hlc {
        match self {
            SignedChannelEvent::Post { at, .. } | SignedChannelEvent::React { at, .. } => at,
        }
    }
    /// Post → message id; React → target message id.
    pub fn id(&self) -> &MessageId {
        match self {
            SignedChannelEvent::Post { id, .. } => id,
            SignedChannelEvent::React { target, .. } => target,
        }
    }
    pub fn sig(&self) -> &[u8; 64] {
        match self {
            SignedChannelEvent::Post { sig, .. } | SignedChannelEvent::React { sig, .. } => sig,
        }
    }
}
```

Add `sign_channel_react` after `sign_channel_event` (after line 337):

```rust
/// Sign a reaction payload. Mirrors `sign_channel_event`. Pure / sync.
pub fn sign_channel_react(
    payload: &ChannelReactPayload,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<SignedChannelEvent, ChannelEventError> {
    use ed25519_dalek::Signer;
    let mut event = SignedChannelEvent::React {
        add: payload.add,
        at: payload.at.clone(),
        author: payload.author,
        channel_id: payload.channel_id,
        community_id: payload.community_id,
        emoji: payload.emoji.clone(),
        target: payload.target,
        sig: [0u8; 64],
    };
    let canon = signed_set_canonical_cbor(&event)?;
    let new_sig = signing_key.sign(&canon).to_bytes();
    if let SignedChannelEvent::React { sig, .. } = &mut event {
        *sig = new_sig;
    }
    Ok(event)
}
```

Rewrite `signed_set_canonical_cbor` (347-373) as a match over both variants:

```rust
fn signed_set_canonical_cbor(event: &SignedChannelEvent) -> Result<Vec<u8>, ChannelEventError> {
    let mut canon = Vec::with_capacity(256);
    match event {
        SignedChannelEvent::Post {
            at, author, body, channel_id, community_id, id, content_kind, reply_to, sig: _,
        } => {
            let signed_set = ChannelPostSignedSet {
                at, author, body, channel_id, community_id, id,
                content_kind: *content_kind, reply_to,
            };
            ciborium::into_writer(&signed_set, &mut canon)
                .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
        }
        SignedChannelEvent::React {
            add, at, author, channel_id, community_id, emoji, target, sig: _,
        } => {
            let signed_set = ChannelReactSignedSet {
                add: *add, at, author, channel_id, community_id, emoji, target,
            };
            ciborium::into_writer(&signed_set, &mut canon)
                .map_err(|e| ChannelEventError::CborEncode(e.to_string()))?;
        }
    }
    Ok(canon)
}
```

In `sign_channel_event`, change the irrefutable mut-destructure (334-335) to:

```rust
    if let SignedChannelEvent::Post { sig, .. } = &mut event {
        *sig = new_sig;
    }
```

- [ ] **Step 5: Fix the replay tracker (`would_accept`, `record`) via accessors**

Replace the body of `would_accept` (506-512 destructure) with accessor reads:

```rust
    pub fn would_accept(&self, event: &SignedChannelEvent) -> Result<(), ChannelEventError> {
        let channel_id = event.channel_id();
        let author = event.author();
        let at = event.at();
        let key = (*channel_id, *author, at.device_id.clone());
        if let Some(prev) = self.last_seen.get(&key) {
            if !at.is_strictly_newer_than(prev) {
                return Err(ChannelEventError::Replay {
                    event_id: *event.id(),
                    author: *author,
                    device_id: at.device_id.clone(),
                    at: at.clone(),
                });
            }
        }
        Ok(())
    }
```

Replace `record` (538-545 destructure):

```rust
    pub fn record(&mut self, event: &SignedChannelEvent) {
        let key = (*event.channel_id(), *event.author(), event.at().device_id.clone());
        self.last_seen.insert(key, event.at().clone());
    }
```

- [ ] **Step 6: Fix `verify_channel_event` (accessors + emoji cap)**

Replace the destructure block (666-673) with accessor reads, and add the emoji-length gate after the misroute check:

```rust
    let community_id = event.community_id();
    let channel_id = event.channel_id();
    let author = event.author();
    let at = event.at();
    let sig = event.sig();

    // Step 3: misroute defense.
    if community_id != expected_community_id || channel_id != expected_channel_id {
        return Err(ChannelEventError::Misroute {
            expected_community: *expected_community_id,
            expected_channel: *expected_channel_id,
            got_community: *community_id,
            got_channel: *channel_id,
        });
    }

    // ZEB-536: bound reaction emoji size (cheap, pre-auth).
    if let SignedChannelEvent::React { emoji, .. } = event {
        if emoji.len() > MAX_REACTION_EMOJI_BYTES {
            return Err(ChannelEventError::NotAuthorized(format!(
                "reaction emoji {} bytes exceeds max {}",
                emoji.len(),
                MAX_REACTION_EMOJI_BYTES
            )));
        }
    }
```

The rest of the function is unchanged: `replay_tracker.would_accept(event)?`, `state.snapshot_at(channel_id, author, at)`, the power/delete gates, and the signature block (`signed_set_canonical_cbor(event)` now handles both variants; `sig` comes from the accessor). React gets the **same** membership/power/signature gates as Post; target existence is deliberately NOT checked (orphan tolerance).

- [ ] **Step 7: Fix the remaining compile-forced sites via accessors**

`append` (920-924) — replace the destructure with accessors:

```rust
        let community_id = event.community_id();
        let channel_id = event.channel_id();
        if *community_id != self.manifest.community_id || *channel_id != self.manifest.channel_id {
```

`seal_and_persist` min_by/max_by closures (1043-1045 and 1061-1063) — replace `.map(|e| { let SignedChannelEvent::Post { at, .. } = e; at })` with `.map(|e| e.at())` (both occurrences).

`max_hlc` (1278-1281) — replace `.chain(self.tail.iter().map(|e| { let SignedChannelEvent::Post { at, .. } = e; at }))` with `.chain(self.tail.iter().map(|e| e.at()))`.

In `community_channel_log_engine.rs`: `message_dto_for_event` (818-826) — make the Post destructure refutable-safe (it is only called on Post; list/projection filter it):

```rust
        let SignedChannelEvent::Post { id, author, at, body, reply_to, .. } = event else {
            unreachable!("message_dto_for_event called on non-Post event; callers filter to Post");
        };
```

`list_messages` (552-573): in the two `since` filters, replace `let SignedChannelEvent::Post { at, .. } = &ev; if !at.is_strictly_newer_than(...)` (and the tail one) with `if !ev.at().is_strictly_newer_than(since_hlc)`. Leave `list_messages` returning **all** events (Post + React) — backfill (engine:1644) and community_fork rely on it; reactions backfill for free.

- [ ] **Step 8: Keep the pre-fork snapshot Post-only**

Read `community_fork.rs:340-380`. After its `list_messages(None, ...)` call (~358), filter to Post so reactions don't leak into pre-fork snapshots (out of scope for v1) and any Post-assuming code stays valid:

```rust
                .list_messages(None, SNAPSHOT_TOTAL_CAP * 2)
                .await
                // ZEB-536: pre-fork snapshot is message-only for v1.
                .map(|evs| evs.into_iter()
                    .filter(|e| matches!(e, crate::community_channel_log::SignedChannelEvent::Post { .. }))
                    .collect::<Vec<_>>())
```

(Adapt to the actual `?`/match shape at the call site; the point is to drop `React` events from the snapshot. If `community_fork` already destructures `Post` exhaustively elsewhere, the compiler will flag it — filter there too.)

- [ ] **Step 9: Make the crate compile, then watch the Step-1 test pass**

Run: `cd src-tauri && cargo check --locked --all-targets --features test-fixtures`
Expected: compiles (fix any remaining match site the compiler flags — they are all "add a `React` arm" or "use an accessor").
Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(sign_and_verify_react_round_trips)'`
Expected: PASS.

- [ ] **Step 10: Add the negative + edge verify tests**

Add these to `mod tests` (same setup helpers as Step 1):

```rust
#[tokio::test]
async fn verify_react_rejects_tampered_emoji() {
    // sign with "👍", then mutate the emoji field in the event; verify must fail BadSignature.
    // (destructure `if let SignedChannelEvent::React { emoji, .. } = &mut event { *emoji = "👎".into(); }`)
}
#[tokio::test]
async fn verify_react_rejects_non_member() {
    // MockState configured so author is NOT Joined → NotAuthorized.
}
#[tokio::test]
async fn verify_react_rejects_oversized_emoji() {
    // emoji = "x".repeat(MAX_REACTION_EMOJI_BYTES + 1) → NotAuthorized.
}
#[tokio::test]
async fn verify_react_accepts_unknown_target() {
    // verify has no notion of the target message; an authorized React with an
    // arbitrary target id verifies OK (orphan tolerance is enforced here by ABSENCE of a check).
}
```

Mirror the assertion style of the existing `verify_channel_event_rejects_*` tests (1985, 2057, 2235). Run `-E 'test(verify_react)'` → all PASS.

- [ ] **Step 11: Add the wire-pin fixture**

In `tests/wire_format_channel_log_fixtures.rs`, add a `react_packet_is_byte_stable` test mirroring the existing Post fixture: build a `React` with fixed inputs, `encrypt_channel_packet_with_nonce(&key, &event, [0x11; 12])`, assert `decrypt_channel_packet` round-trips, and pin the packet hex. Capture the hex on first run (`println!("{}", hex::encode(&packet))`), paste it into an `assert_eq!(hex::encode(&packet), "…")`, and re-run.

- [ ] **Step 12: Lint + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_channel_log.rs src-tauri/src/community_channel_log_engine.rs src-tauri/src/community_fork.rs src-tauri/tests/wire_format_channel_log_fixtures.rs
git commit -m "feat(zeb-536): React channel-event variant + sign/verify

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: `ReactionDto` + `ReactionIndex` (pure LWW materialization)

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` (add types near the other pure types, e.g. after `ChannelLogReplayTracker` ~574)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `SignedChannelEvent::React`, accessors (Task 1).
- Produces: `pub struct ReactionDto { emoji: String, count: u32, mine: bool, reactors: Vec<String> }` (serde camelCase, `Serialize + Deserialize`); `pub struct ReactionIndex` with `pub fn apply(&mut self, event: &SignedChannelEvent)` and `pub fn reactions_for(&self, target: &MessageId, me: &OwnerAddr) -> Vec<ReactionDto>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn reaction_index_lww_toggle_and_counts() {
    let target = MessageId([1u8; 16]);
    let a = OwnerAddr([0xAA; 16]);
    let b = OwnerAddr([0xBB; 16]);
    let mut idx = ReactionIndex::default();
    let mk = |author, emoji: &str, add, wall| react_event(target, author, emoji, add, wall); // helper below
    idx.apply(&mk(a, "👍", true, 10));
    idx.apply(&mk(b, "👍", true, 11));
    idx.apply(&mk(a, "🎉", true, 12));
    // out-of-order + LWW: a's older un-react (wall 9) must NOT override the wall-10 react
    idx.apply(&mk(a, "👍", false, 9));
    let r = idx.reactions_for(&target, &a);
    // 👍 -> {a,b} present; 🎉 -> {a}
    let thumbs = r.iter().find(|d| d.emoji == "👍").unwrap();
    assert_eq!(thumbs.count, 2);
    assert!(thumbs.mine);
    assert_eq!(thumbs.reactors.len(), 2);
    // now a un-reacts 👍 with a NEWER hlc → count drops to 1, mine=false
    idx.apply(&mk(a, "👍", false, 20));
    let r2 = idx.reactions_for(&target, &a);
    let thumbs2 = r2.iter().find(|d| d.emoji == "👍").unwrap();
    assert_eq!(thumbs2.count, 1);
    assert!(!thumbs2.mine);
}
```

Add a `react_event` test helper that builds an **unsigned** `SignedChannelEvent::React` directly (no signing needed — `apply` only reads fields):

```rust
fn react_event(target: MessageId, author: OwnerAddr, emoji: &str, add: bool, wall: u64) -> SignedChannelEvent {
    SignedChannelEvent::React {
        add, author, target, emoji: emoji.to_string(),
        community_id: SpaceId([0u8; 16]),
        channel_id: ChannelId([0u8; 16]),
        at: Hlc { wall_ms: wall, logical: 0, device_id: format!("dev-{}", hex::encode(author.0)) },
        sig: [0u8; 64],
    }
}
```

(Confirm `Hlc`/`SpaceId`/`ChannelId`/`OwnerAddr` literal construction matches their definitions in `owner_state_types`/`community_membership`; adjust field names if needed.)

- [ ] **Step 2: Run it — watch it fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reaction_index_lww_toggle_and_counts)'`
Expected: FAIL — `ReactionIndex`/`ReactionDto` not found.

- [ ] **Step 3: Implement the types**

```rust
/// IPC-facing materialized reaction summary for one emoji on one message.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReactionDto {
    pub emoji: String,
    pub count: u32,
    /// True iff the local owner currently reacts with this emoji.
    pub mine: bool,
    /// Hex `OwnerAddr` of every member currently reacting with this emoji.
    pub reactors: Vec<String>,
}

/// In-memory LWW materialization of reactions over a channel's events.
/// Keyed target → emoji → author → (latest HLC, present). Derived view —
/// always reconstructable by folding the log through `apply`.
#[derive(Debug, Default, Clone)]
pub struct ReactionIndex {
    by_target: BTreeMap<MessageId, BTreeMap<String, BTreeMap<OwnerAddr, (Hlc, bool)>>>,
}

impl ReactionIndex {
    /// Fold one event in. Non-React events are ignored. LWW per
    /// (target, emoji, author): only the strictly-newest HLC wins.
    pub fn apply(&mut self, event: &SignedChannelEvent) {
        let SignedChannelEvent::React { target, author, at, emoji, add, .. } = event else {
            return;
        };
        let authors = self
            .by_target
            .entry(*target)
            .or_default()
            .entry(emoji.clone())
            .or_default();
        match authors.get(author) {
            Some((prev_hlc, _)) if !at.is_strictly_newer_than(prev_hlc) => { /* stale — ignore */ }
            _ => {
                authors.insert(*author, (at.clone(), *add));
            }
        }
    }

    /// Materialize the reaction summary for a message. Emoji with zero
    /// present reactors are omitted. Deterministic order (BTreeMap).
    pub fn reactions_for(&self, target: &MessageId, me: &OwnerAddr) -> Vec<ReactionDto> {
        let Some(by_emoji) = self.by_target.get(target) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (emoji, authors) in by_emoji {
            let present: Vec<&OwnerAddr> = authors
                .iter()
                .filter(|(_, (_, add))| *add)
                .map(|(a, _)| a)
                .collect();
            if present.is_empty() {
                continue;
            }
            out.push(ReactionDto {
                emoji: emoji.clone(),
                count: present.len() as u32,
                mine: present.iter().any(|a| *a == me),
                reactors: present.iter().map(|a| hex::encode(a.0)).collect(),
            });
        }
        out
    }
}
```

- [ ] **Step 4: Run it — watch it pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(reaction_index)'`
Expected: PASS.

- [ ] **Step 5: Add idempotency + empty + multi-target tests**

```rust
#[test] fn reaction_index_apply_is_idempotent() { /* apply same event twice → count 1 */ }
#[test] fn reaction_index_empty_for_unknown_target() { /* reactions_for unknown id → vec![] */ }
#[test] fn reaction_index_ignores_non_react_events() { /* apply a Post → no entries */ }
```

Run `-E 'test(reaction_index)'` → all PASS.

- [ ] **Step 6: Lint + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_channel_log.rs
git commit -m "feat(zeb-536): ReactionIndex LWW materialization + ReactionDto

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Wire the index into `ChannelLog` (maintenance + boot rebuild)

**Files:**
- Modify: `src-tauri/src/community_channel_log.rs` — `ChannelLog` struct (811-819), `new` (883-899), `append` (919-936), `reload` (1149-1243); add `rebuild_reaction_index` + `reactions_for`.
- Test: same file, `mod tests`.

**Interfaces:**
- Consumes: `ReactionIndex` (Task 2), accessors (Task 1).
- Produces: `ChannelLog.reaction_index: ReactionIndex`; `pub fn reactions_for(&self, target: &MessageId, me: &OwnerAddr) -> Vec<ReactionDto>` on `ChannelLog`.

- [ ] **Step 1: Write the failing test (index survives seal + reload)**

```rust
#[test]
fn channel_log_reactions_survive_seal_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let (cid, chid) = (SpaceId([3;16]), ChannelId([4;16]));
    let cfg = ChannelLogConfig { seal_threshold_events: 4 };
    let mut log = ChannelLog::new(cid, chid, dir.path().to_path_buf(), cfg.clone());
    // append a Post, then a React to it, enough to force a seal, then more
    let target = MessageId([9;16]);
    let me = OwnerAddr([0xAA;16]);
    log.append(post_event(target, me, cid, chid, 10)).unwrap();      // helper: signed-or-unsigned Post
    log.append(react_event_for(target, me, cid, chid, "👍", true, 11)).unwrap();
    // drive a seal
    log.append(post_event(MessageId([8;16]), me, cid, chid, 12)).unwrap();
    if log.append(post_event(MessageId([7;16]), me, cid, chid, 13)).unwrap() {
        log.seal_and_persist().unwrap();
    }
    log.flush_tail().unwrap();
    // reload from disk — index must be rebuilt from the sealed segment
    let (reloaded, _n) = ChannelLog::reload(cid, chid, dir.path().to_path_buf(), cfg).unwrap();
    let r = reloaded.reactions_for(&target, &me);
    assert_eq!(r.iter().find(|d| d.emoji == "👍").unwrap().count, 1);
}
```

(`post_event`/`react_event_for` build events with the given channel binding so `append`'s misroute check passes; reuse `react_event` from Task 2 with matching `cid/chid`.)

- [ ] **Step 2: Run it — watch it fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(channel_log_reactions_survive_seal_and_reload)'`
Expected: FAIL — no `reaction_index` field / no `reactions_for` method.

- [ ] **Step 3: Add the field + constructor init**

In the `ChannelLog` struct (811-819) add after `tail`:

```rust
    /// ZEB-536: derived LWW reaction view. Maintained in `append`;
    /// rebuilt from the persisted log in `reload`.
    reaction_index: ReactionIndex,
```

In `ChannelLog::new` (889-898) add `reaction_index: ReactionIndex::default(),` to the struct literal.

- [ ] **Step 4: Maintain it in `append`**

In `append` (after the misroute check from Task 1, before `self.tail.push(event)`), fold reactions in:

```rust
        // ZEB-536: maintain the derived reaction view at the single
        // append choke point (covers local react, inbound, backfill).
        if matches!(&event, SignedChannelEvent::React { .. }) {
            self.reaction_index.apply(&event);
        }
        self.tail.push(event);
        Ok(self.tail.len() >= self.config.seal_threshold_events)
```

Note: seal does NOT clear the index (it only moves already-counted events to disk), so reaction counts persist across seals within a process.

- [ ] **Step 5: Add `reactions_for` + `rebuild_reaction_index`; call rebuild in `reload`**

Add methods to `impl ChannelLog`:

```rust
    /// Materialized reactions for a message (ZEB-536).
    pub fn reactions_for(&self, target: &MessageId, me: &OwnerAddr) -> Vec<ReactionDto> {
        self.reaction_index.reactions_for(target, me)
    }

    /// Rebuild the reaction index from the persisted log. Reads each
    /// sealed segment once (transiently — peak extra memory is one
    /// segment), then folds the in-memory tail. One-time boot cost;
    /// acceptable for v1 (reactions are sparse, segments small). A
    /// persisted/summary index is a future optimization (out of scope).
    fn rebuild_reaction_index(&mut self) -> Result<(), ChannelLogPersistError> {
        let mut idx = ReactionIndex::default();
        for seg in &self.manifest.segments {
            for ev in self.read_segment(seg)? {
                idx.apply(&ev);
            }
        }
        for ev in &self.tail {
            idx.apply(ev);
        }
        self.reaction_index = idx;
        Ok(())
    }
```

In `reload` (1234-1242), replace the trailing `Ok((Self { manifest, tail, config, root }, total))` with:

```rust
        let mut log = Self {
            manifest,
            tail,
            config,
            root,
            reaction_index: ReactionIndex::default(),
        };
        log.rebuild_reaction_index()?;
        Ok((log, total))
```

- [ ] **Step 6: Run it — watch it pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(channel_log_reactions_survive_seal_and_reload)'`
Expected: PASS. Then run the broader `-E 'test(channel_log)'` to confirm no regression in existing log tests.

- [ ] **Step 7: Lint + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_channel_log.rs
git commit -m "feat(zeb-536): ChannelLog reaction index (append-maintained + boot rebuild)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Engine — `react()`, event emission, DTO reactions, `list_message_dtos`

**Files:**
- Modify: `src-tauri/src/community_channel_log_engine.rs` — `ChannelMessageDto` (130-149), error enum (add `ReactionEmojiTooLarge`), add `ChannelReactionReceivedPayload` (after 205), `react()` + `emit_reaction_received()` (after `publish`/`emit_message_received`), `process_inbound_packet` emit branch (998-999), add `list_message_dtos`.
- Test: same file, `mod tests`.

**Interfaces:**
- Consumes: `sign_channel_react`, `ChannelReactPayload`, `MAX_REACTION_EMOJI_BYTES`, `ReactionDto`, `ChannelLog::reactions_for`, accessors (Tasks 1-3); `reserve_next_hlc_for_device`.
- Produces: `pub async fn ChannelLogEngine::react(self: &Arc<Self>, target: MessageId, emoji: String, add: bool) -> Result<(), ChannelLogEngineError>`; `pub async fn list_message_dtos(&self, since: Option<Hlc>, limit: usize) -> Result<Vec<ChannelMessageDto>, ChannelLogEngineError>`; `channel-reaction-received` event.

- [ ] **Step 1: Write the failing engine round-trip test**

In the engine `mod tests`, using the existing test `fix` harness (see `list_messages_returns_hlc_ordered` at 2298 for fixture construction):

```rust
#[tokio::test]
async fn react_updates_index_and_lists_in_dto() {
    let fix = TestFixture::new().await; // same as existing engine tests
    let msg_id = fix.engine.publish(b"hi".to_vec(), None).await.expect("post");
    fix.engine.react(msg_id, "👍".to_string(), true).await.expect("react");
    let dtos = fix.engine.list_message_dtos(None, 100).await.expect("list dtos");
    let m = dtos.iter().find(|d| d.message_id == hex::encode(msg_id.0)).unwrap();
    assert_eq!(m.reactions.iter().find(|r| r.emoji == "👍").unwrap().count, 1);
    assert!(m.reactions.iter().find(|r| r.emoji == "👍").unwrap().mine);
    // un-react converges to empty
    fix.engine.react(msg_id, "👍".to_string(), false).await.expect("unreact");
    let dtos2 = fix.engine.list_message_dtos(None, 100).await.expect("list dtos");
    let m2 = dtos2.iter().find(|d| d.message_id == hex::encode(msg_id.0)).unwrap();
    assert!(m2.reactions.iter().all(|r| r.emoji != "👍"));
}
```

- [ ] **Step 2: Run it — watch it fail**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(react_updates_index_and_lists_in_dto)'`
Expected: FAIL — `react`/`list_message_dtos` not found; `reactions` field missing.

- [ ] **Step 3: Add the DTO field + error variant + payload struct**

In `ChannelMessageDto` (after `poll_id`, 148):

```rust
    /// ZEB-536: materialized reactions on this message (empty when none).
    #[serde(default)]
    pub reactions: Vec<crate::community_channel_log::ReactionDto>,
```

In `message_dto_for_event`, set `reactions: Vec::new(),` in the returned `ChannelMessageDto` literal (the read path fills it; `event_to_dto` callers/tests see empty, which is correct).

Add to `ChannelLogEngineError` (mirror `BodyTooLarge`):

```rust
    #[error("reaction emoji too large: {len} bytes (max {max})")]
    ReactionEmojiTooLarge { len: usize, max: usize },
```

Add after `ChannelBackfillProgressPayload` (205):

```rust
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChannelReactionReceivedPayload {
    pub community_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub reactor: String,
    pub emoji: String,
    pub add: bool,
    pub at: HlcDto,
}
```

- [ ] **Step 4: Add `react()` + `emit_reaction_received()`**

Add to `impl ChannelLogEngine` (after `publish`/`emit_message_received`):

```rust
    /// ZEB-536: react/un-react to a prior message. Mirrors `publish`:
    /// reserve HLC → sign → encrypt → record (loopback dedup) → append
    /// (updates the reaction index under the log lock) → broadcast →
    /// emit. `add=false` un-reacts.
    pub async fn react(
        self: &Arc<Self>,
        target: MessageId,
        emoji: String,
        add: bool,
    ) -> Result<(), ChannelLogEngineError> {
        if emoji.len() > crate::community_channel_log::MAX_REACTION_EMOJI_BYTES {
            return Err(ChannelLogEngineError::ReactionEmojiTooLarge {
                len: emoji.len(),
                max: crate::community_channel_log::MAX_REACTION_EMOJI_BYTES,
            });
        }
        let wall_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let hlc = crate::dm_outbox::reserve_next_hlc_for_device(
            &self.hlc_tracker,
            &self.self_device_id,
            wall_now_ms,
        )
        .await;
        let payload = crate::community_channel_log::ChannelReactPayload {
            target,
            community_id: self.community_id,
            channel_id: self.channel_id,
            author: self.self_owner,
            at: hlc,
            emoji,
            add,
        };
        let event = crate::community_channel_log::sign_channel_react(&payload, &self.signing_key)
            .map_err(ChannelLogEngineError::ChannelEvent)?;
        let packet = encrypt_channel_packet(&self.channel_key, &event)
            .map_err(ChannelLogEngineError::ChannelEvent)?;
        {
            let mut tracker = self.replay_tracker.lock().await;
            tracker.record(&event);
        }
        {
            let mut log = self.log.lock().await;
            if self.closing.load(Ordering::SeqCst) {
                return Err(ChannelLogEngineError::EngineShuttingDown);
            }
            log.append(event.clone())
                .map_err(ChannelLogEngineError::Persist)?;
        }
        if let Err(e) = self.publisher_tx.try_send(packet) {
            tracing::warn!(
                community_id = ?self.community_id,
                channel_id = ?self.channel_id,
                err = ?e,
                "publisher_tx full or closed; reaction broadcast skipped"
            );
        }
        self.flush_dirty.notify_one();
        self.emit_reaction_received(&event);
        Ok(())
    }

    fn emit_reaction_received(&self, event: &SignedChannelEvent) {
        let SignedChannelEvent::React { target, author, at, emoji, add, .. } = event else {
            return;
        };
        let payload = ChannelReactionReceivedPayload {
            community_id: hex::encode(self.community_id.0),
            channel_id: hex::encode(self.channel_id.0),
            message_id: hex::encode(target.0),
            reactor: hex::encode(author.0),
            emoji: emoji.clone(),
            add: *add,
            at: HlcDto {
                wall_ms: at.wall_ms,
                logical: at.logical,
                device_id: at.device_id.clone(),
            },
        };
        crate::node_event_sink::emit_ser(&*self.sink, "channel-reaction-received", &payload);
    }
```

- [ ] **Step 5: Branch the inbound emit + add `list_message_dtos`**

In `process_inbound_packet`, replace the final `self.emit_message_received(&event);` (998) with:

```rust
    match &event {
        SignedChannelEvent::React { .. } => self.emit_reaction_received(&event),
        _ => self.emit_message_received(&event),
    }
```

(The index is already updated for inbound reactions via `log.append` in step 3 of the append block.)

Add `list_message_dtos` (next to `list_messages`):

```rust
    /// ZEB-536 IPC read path: messages (Post only) with reactions folded
    /// in. Reuses `list_messages` for paging, then attaches the
    /// materialized reaction view under one log lock. Note: `limit`
    /// bounds events scanned, so a page dense with reactions may return
    /// fewer than `limit` messages — acceptable for v1 (clients page via
    /// `since`).
    pub async fn list_message_dtos(
        &self,
        since: Option<Hlc>,
        limit: usize,
    ) -> Result<Vec<ChannelMessageDto>, ChannelLogEngineError> {
        let events = self.list_messages(since, limit).await?;
        let log = self.log.lock().await;
        let mut out = Vec::with_capacity(events.len());
        for ev in &events {
            if !matches!(ev, SignedChannelEvent::Post { .. }) {
                continue;
            }
            let mut dto = self.message_dto_for_event(ev);
            dto.reactions = log.reactions_for(ev.id(), &self.self_owner);
            out.push(dto);
        }
        Ok(out)
    }
```

- [ ] **Step 6: Run it — watch it pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(react_updates_index_and_lists_in_dto)'`
Expected: PASS. Fix `event_to_dto_projects_post_fields` (2379) if it constructs `ChannelMessageDto` literally — add `reactions: vec![]`.

- [ ] **Step 7: Add the two-node convergence test**

Mirror the existing two-node engine test (search for a test pairing two engines / a loopback adapter, e.g. near 2620-2760). Assert: node A posts, node B `react`s, B emits `channel-reaction-received`, and `list_message_dtos` on **both** shows `count=1` with correct `mine`; then B un-reacts → both converge to no `👍`. If the test harness has no two-node fixture, assert convergence via feeding A's `react` event through B's `process_inbound_packet` directly.

Run `-E 'test(react)'` → all PASS.

- [ ] **Step 8: Lint + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
git add src-tauri/src/community_channel_log_engine.rs
git commit -m "feat(zeb-536): engine react() + channel-reaction-received + DTO reactions

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: RPC verb + IPC seam

**Files:**
- Modify: `src-tauri/src/api/rpc.rs` — add `SetMessageReactionArgs` (after `PostChannelMessageArgs` 150); register `set_message_reaction` in `build_registry` (Channels section, near the `post_channel_message` registration ~379); update the curated-surface test (`registry_has_exactly_the_curated_v1_surface` ~847).
- Modify: `src-tauri/src/lib.rs` — add `set_message_reaction_impl` (after `post_channel_message_impl` 20019) + `#[tauri::command]` wrapper (after `post_channel_message` 19957); add `set_message_reaction` to `generate_handler!` (45791, near `post_channel_message`); switch `list_channel_messages_impl` (20101-20108) to `list_message_dtos`.
- Test: `src-tauri/src/api/rpc.rs` `mod tests`.

**Interfaces:**
- Consumes: `ChannelLogEngine::react`, `list_message_dtos` (Task 4).
- Produces: `set_message_reaction` RPC verb + `pub(crate) async fn set_message_reaction_impl(state, community_id, channel_id, message_id, emoji, add) -> Result<(), String>`.

- [ ] **Step 1: Write the failing curated-surface test update**

In `rpc.rs` `mod tests`, add `"set_message_reaction"` to the expected verb vec in `registry_has_exactly_the_curated_v1_surface` (it asserts the exact sorted command list). Run it — FAIL (verb not registered yet).

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(registry_has_exactly_the_curated_v1_surface)'`
Expected: FAIL — mismatch (expected contains `set_message_reaction`, actual doesn't).

- [ ] **Step 2: Add the args struct + registration**

After `PostChannelMessageArgs` (150):

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMessageReactionArgs {
    community_id: String,
    channel_id: String,
    message_id: String,
    emoji: String,
    add: bool,
}
```

In `build_registry`, next to the `post_channel_message` `rpc!(...)`:

```rust
    rpc!(
        m,
        "set_message_reaction",
        SetMessageReactionArgs,
        |state, _sink, a| async move {
            crate::set_message_reaction_impl(
                state, a.community_id, a.channel_id, a.message_id, a.emoji, a.add,
            )
            .await
        }
    );
```

- [ ] **Step 3: Add the `_impl` seam + Tauri wrapper in `lib.rs`**

After `post_channel_message_impl` (20019):

```rust
/// ZEB-536: shared IPC/RPC seam — set/clear a reaction on a channel message.
pub(crate) async fn set_message_reaction_impl(
    state: &std::sync::Mutex<NodeState>,
    community_id: String,
    channel_id: String,
    message_id: String,
    emoji: String,
    add: bool,
) -> Result<(), String> {
    if community_id.len() != 32 {
        return Err("community_id must be 16 bytes (32 hex chars)".to_string());
    }
    if channel_id.len() != 32 {
        return Err("channel_id must be 16 bytes (32 hex chars)".to_string());
    }
    if message_id.len() != 32 {
        return Err("message_id must be 16 bytes (32 hex chars)".to_string());
    }
    let cid_bytes: [u8; 16] = hex::decode(&community_id)
        .map_err(|e| format!("invalid community_id hex: {e}"))?
        .try_into()
        .map_err(|_| "community_id length wrong".to_string())?;
    let chid_bytes: [u8; 16] = hex::decode(&channel_id)
        .map_err(|e| format!("invalid channel_id hex: {e}"))?
        .try_into()
        .map_err(|_| "channel_id length wrong".to_string())?;
    let mid_bytes: [u8; 16] = hex::decode(&message_id)
        .map_err(|e| format!("invalid message_id hex: {e}"))?
        .try_into()
        .map_err(|_| "message_id length wrong".to_string())?;
    let cid = crate::owner_state_types::SpaceId(cid_bytes);
    let chid = crate::community_membership::ChannelId(chid_bytes);
    let target = crate::community_channel_log::MessageId(mid_bytes);

    let registry = {
        let guard = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        guard
            .channel_log_registry
            .as_ref()
            .ok_or_else(|| "channel_log_registry missing — node not running".to_string())?
            .clone()
    };
    let engine = registry
        .engine(&cid, &chid)
        .await
        .ok_or_else(|| format!("no engine for {community_id}/{channel_id}"))?;

    engine
        .react(target, emoji, add)
        .await
        .map_err(|e| e.to_string())
}
```

After the `post_channel_message` wrapper (19957):

```rust
#[tauri::command]
async fn set_message_reaction(
    state_lock: tauri::State<'_, std::sync::Mutex<NodeState>>,
    community_id: String,
    channel_id: String,
    message_id: String,
    emoji: String,
    add: bool,
) -> Result<(), String> {
    set_message_reaction_impl(
        state_lock.inner(), community_id, channel_id, message_id, emoji, add,
    )
    .await
}
```

In `generate_handler!` (45791), add `set_message_reaction,` next to `post_channel_message,`.

- [ ] **Step 4: Switch the list read path to reactions-aware DTOs**

In `list_channel_messages_impl` (20100-20108), replace the `list_messages(...) + map(event_to_dto)` tail with:

```rust
    let dtos = engine
        .list_message_dtos(since_hlc, limit as usize)
        .await
        .map_err(|e| e.to_string())?;

    Ok(dtos)
```

- [ ] **Step 5: Run it — watch it pass**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(registry_has_exactly_the_curated_v1_surface)'`
Expected: PASS.

- [ ] **Step 6: Add a bad-args test**

```rust
#[tokio::test]
async fn set_message_reaction_rejects_bad_hex() {
    // dispatch "set_message_reaction" with messageId "zz" → RpcError::Command(...) about hex/length.
}
```

Mirror an existing rpc dispatch test. Run `-E 'test(set_message_reaction)'` → PASS.

- [ ] **Step 7: Full suite + lint + commit**

```bash
cd src-tauri && cargo fmt --all && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cd src-tauri && cargo nextest run --locked --all-targets --features test-fixtures
git add src-tauri/src/api/rpc.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-536): set_message_reaction RPC verb + reactions in list read path

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: Frontend type check + open PR

**Files:** none (verification + PR).

- [ ] **Step 1: Frontend type check** — the IPC DTO gained a `reactions` field. From repo root: `npx tsc --noEmit`. If a TS `ChannelMessageDto` type pins the shape, add `reactions?: { emoji: string; count: number; mine: boolean; reactors: string[] }[]` (optional — Spec 2 consumes it). Run `npx vitest run`.

- [ ] **Step 2: Full green gate** — `cd src-tauri && cargo nextest run --locked --all-targets --features test-fixtures` and `cargo clippy ... -D warnings` both clean. Confirm with output (verify-don't-trust).

- [ ] **Step 3: Push + open PR** — `git push -u origin zeb-536-message-reactions`; open a PR (`gh pr create`) titled `feat(zeb-536): message reactions (Spec 1, backend/headless)`, body summarizing the design + linking ZEB-536 and the spec, with the footer:

```text
🤖 Generated with [Claude Code](https://claude.com/claude-code)
```

Do NOT merge (Jake merges). Drop a pointer in `#fleet` (`on ZEB-536, PR #N — Spec 1 backend, ready for cross-WAN react test`). Let the bot pipeline run; drive to convergence.

---

## Post-merge (operational, not a code task)

Cross-WAN fleet validation with Ildwyn over `api` (headless): AVALON posts in a test channel; Ildwyn `set_message_reaction {emoji:"👍", add:true}`; assert AVALON receives `channel-reaction-received` and `list_channel_messages` materializes `👍 count=1`; AVALON reacts `✅`; both converge to `{👍:1, ✅:1}`; Ildwyn `add:false` → converges to `{✅:1}`. This also exercises AVALON's local Rust build (the less-used-node goal); file any AVALON-specific dev speed bumps.
