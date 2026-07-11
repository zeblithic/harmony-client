# ZEB-669 Slice 1: Announce Attribution Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Attach the local ZenohId to `harmony/announce/*` publishes so `ObservedHolders` actually counts real peers — the shipped ×N "copies seen" counter (ZEB-612 S3) reads the sample attachment, which announce publishes never set, so `replicaCount` sits at ×1 in production.

**Architecture:** Every announce publish — the admit-time announce emitted by the storage tier through `runtime.tick()` AND the 60 s re-announce loop (`event_loop.rs:5777-5787`) — funnels through the single `RuntimeAction::Publish` arm in `dispatch_action` (`event_loop.rs:6180-6198`). That arm already attaches the zid for `harmony/compute/capacity/*` keys; the fix extends the condition to announce keys via a pure, unit-testable helper. The receive path is already correct and generic (`event_loop.rs:6273-6276` extracts any attachment as `source_zid`; the announce arm at `3801-3816` feeds `observed_holders.note()`).

**Tech Stack:** Rust, zenoh 1.x (in-process session loopback for tests — precedent: `community_channel_log_engine.rs:5479` opens real sessions in unit tests; `multi_thread` tokio flavor required, `current_thread` panics on `zenoh::open`).

## Global Constraints

- Spec: `docs/specs/2026-07-11-zeb-669-storage-buddies-design.md` §2 — attach zid to **all** announce publish sites; receiver unchanged; additive on the wire (older clients ignore attachments).
- Attribution stays **anonymous** (session zid only — Jake's hybrid decision §0.2): no owner identity is added to announcements.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.
- Gates (CLAUDE.md): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; iterative `scripts/test-select --context task`; final full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures` before PR.
- All cargo commands run from `src-tauri/`.

---

### Task 1: Failing behavioral tests — announce publishes must carry the zid attachment

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (new `#[cfg(test)] mod dispatch_attachment_tests` at end of file — the file has no test module today)

**Interfaces:**
- Consumes: `dispatch_action` (private fn, `event_loop.rs:6170` — reachable from a same-file test mod), `RuntimeAction::Publish` (from `harmony_runtime`), `crate::node_event_sink::RecordingSink::new()` (test stub, `node_event_sink.rs:59-77`; `Arc<RecordingSink>` implements `NodeEventSink`).
- Produces: tests `announce_publish_carries_own_zid_attachment`, `non_announce_publish_carries_no_attachment`, `publish_attaches_zid_matches_announce_and_capacity_only` (the third asserts against the Task 2 helper and is written in Task 2).

- [ ] **Step 1: Write the two failing loopback tests**

Append at the end of `src-tauri/src/event_loop.rs`:

```rust
#[cfg(test)]
mod dispatch_attachment_tests {
    use super::*;

    /// ZEB-669 slice 1 harness: publish through the production
    /// `dispatch_action` arm on an in-process zenoh session and return
    /// the attachment the subscriber observed. Zenoh requires the
    /// `multi_thread` tokio flavor (`current_thread` panics on
    /// `zenoh::open` — see `community_channel_log_engine.rs` fixtures).
    async fn publish_and_observe_attachment(key_expr: &str) -> Option<String> {
        let session = zenoh::open(zenoh::Config::default())
            .await
            .expect("zenoh open");
        let sub = session
            .declare_subscriber(key_expr)
            .await
            .expect("declare subscriber");
        let (zenoh_tx, _zenoh_rx) = mpsc::channel::<ZenohEvent>(8);
        let app: std::sync::Arc<dyn crate::node_event_sink::NodeEventSink> =
            std::sync::Arc::new(crate::node_event_sink::RecordingSink::new());
        let closing = Arc::new(AtomicBool::new(false));
        dispatch_action(
            RuntimeAction::Publish {
                key_expr: key_expr.to_string(),
                // Real announce payloads are a 4-byte BE u32 size
                // (`parse_content_announcement`); mirror that shape.
                payload: 1234u32.to_be_bytes().to_vec(),
            },
            &session,
            &zenoh_tx,
            &app,
            &closing,
            "zid-under-test",
        )
        .await;
        let sample = tokio::time::timeout(std::time::Duration::from_secs(10), sub.recv_async())
            .await
            .expect("sample within 10s")
            .expect("subscriber alive");
        sample
            .attachment()
            .and_then(|a| String::from_utf8(a.to_bytes().to_vec()).ok())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn announce_publish_carries_own_zid_attachment() {
        let key = format!("{}{}", crate::ANNOUNCE_PREFIX, "aa".repeat(32));
        assert_eq!(
            publish_and_observe_attachment(&key).await.as_deref(),
            Some("zid-under-test"),
            "announce publishes must attach the local zid so \
             ObservedHolders can attribute the announcing session"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_announce_publish_carries_no_attachment() {
        assert_eq!(
            publish_and_observe_attachment("harmony/profile/deadbeef").await,
            None,
            "the zid attachment stays scoped to capacity + announce keys"
        );
    }
}
```

- [ ] **Step 2: Run to verify the right failure**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dispatch_attachment)'`
Expected: **compile error** — `crate::ANNOUNCE_PREFIX` does not exist yet (defined in Task 2). This is the TDD anchor; do not stub the const here.

*(No commit yet — tests land with Task 2 so the branch never holds a broken build.)*

### Task 2: `ANNOUNCE_PREFIX` const + pure helper + extend the publish condition

**Files:**
- Modify: `src-tauri/src/lib.rs:1939` (const, next to `CAPACITY_PREFIX`), `src-tauri/src/lib.rs:14410` (`strip_prefix` literal), `src-tauri/src/lib.rs:14446` (`format!` literal)
- Modify: `src-tauri/src/event_loop.rs:2957` (subscribe key literal), `:3795-3801` (receiver comment + `starts_with` literal), `:6182-6188` (attachment condition → helper), helper fn above `dispatch_action`
- Test: helper unit test inside `dispatch_attachment_tests`

**Interfaces:**
- Produces: `crate::ANNOUNCE_PREFIX: &str = "harmony/announce/"` (crate-root const, private like `CAPACITY_PREFIX` — root-private items are visible to child modules); `fn publish_attaches_zid(key_expr: &str) -> bool` in `event_loop.rs`.

- [ ] **Step 1: Add the const in `lib.rs` (after line 1939's `CAPACITY_PREFIX`)**

```rust
const CAPACITY_PREFIX: &str = "harmony/compute/capacity/";
/// Content-availability announcements (`harmony/announce/{cid_hex}`).
/// Publishes on this prefix attach the local zid (ZEB-669 slice 1) so
/// receivers' `ObservedHolders` can attribute the announcing session.
const ANNOUNCE_PREFIX: &str = "harmony/announce/";
```

- [ ] **Step 2: Swap the two `lib.rs` literals**

At `lib.rs:14410` (inside `parse_content_announcement`):
```rust
let cid_hex = key_expr.strip_prefix(ANNOUNCE_PREFIX)?;
```
At `lib.rs:14446` (inside `collect_reannouncements`):
```rust
format!("{}{}", ANNOUNCE_PREFIX, hex::encode(entry.cid)),
```

- [ ] **Step 3: Add the helper + extend the condition in `event_loop.rs`**

Above `dispatch_action` (~line 6169):
```rust
/// Which publishes carry our ZenohId as a sample attachment. Capacity
/// beacons need it for hop-distance inference; content announcements
/// (ZEB-669 slice 1) need it so receivers can attribute the announcing
/// session — `ObservedHolders` reads the attachment, and without it the
/// ×N "copies seen" counter never counts real peers. The zid is a
/// transport-session id, not an owner identity: announcements stay
/// anonymous (ZEB-669 §0.2 hybrid attribution).
fn publish_attaches_zid(key_expr: &str) -> bool {
    key_expr.starts_with(crate::CAPACITY_PREFIX) || key_expr.starts_with(crate::ANNOUNCE_PREFIX)
}
```

Replace `event_loop.rs:6182-6188` (keep the spawn/put body unchanged):
```rust
            // Attach our ZenohId where receivers attribute the publisher:
            // capacity beacons (hop distance) and content announcements
            // (observed holders). See `publish_attaches_zid`.
            let zid_attachment = if publish_attaches_zid(&key_expr) {
                Some(own_zid.to_string())
            } else {
                None
            };
```

- [ ] **Step 4: Swap the subscribe-key literal**

At `:2957`: `key_expr: format!("{}*", crate::ANNOUNCE_PREFIX),`
(The receiver arm at `:3795-3817` is rewritten in Task 3 — leave it alone here.)

- [ ] **Step 5: Add the helper unit test inside `dispatch_attachment_tests`**

```rust
    #[test]
    fn publish_attaches_zid_matches_announce_and_capacity_only() {
        assert!(publish_attaches_zid("harmony/announce/aabb"));
        assert!(publish_attaches_zid("harmony/compute/capacity/node1"));
        assert!(!publish_attaches_zid("harmony/profile/aabb"));
        assert!(!publish_attaches_zid("harmony/vines/aabb/follows"));
        // Prefix discipline: no trailing-slash bypass.
        assert!(!publish_attaches_zid("harmony/announcements/aabb"));
    }
```

- [ ] **Step 6: Run the task tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(dispatch_attachment) or test(parse_content_announcement) or test(collect_reannouncements)'`
Expected: PASS (both loopback tests, helper test, and the existing announce parse/collect pins).

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/event_loop.rs
git commit -m "ZEB-669 S1: attach zid to announce publishes — feed the observed-holders counter

The x-N 'copies seen' counter (ZEB-612 S3) reads the announcer zid from
the sample attachment, but only capacity publishes ever attached one, so
real cross-peer announces arrived unattributable and replicaCount sat at
x1 (self) in production. All announce publishes funnel through the
single dispatch_action Publish arm; the attachment condition now covers
ANNOUNCE_PREFIX via a pure helper. Receiver path unchanged (already
correct). Additive on the wire; announcements stay anonymous (session
zid only, per the ZEB-669 hybrid-attribution decision).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

### Task 3: Extract the receive arm into a testable helper + spec'd receive-side pins

The spec (§2) requires an observed-holders feed test and a self-announce-exclusion pin, but the receive logic lives inline in the select loop (`event_loop.rs:3795-3817`) where no test can reach it. Extract it verbatim into a pure-ish helper and pin all four behaviors.

**Files:**
- Modify: `src-tauri/src/event_loop.rs:3795-3817` (arm body → helper call), helper fn near `publish_attaches_zid`, new `#[cfg(test)] mod note_announce_sample_tests`

**Interfaces:**
- Consumes: `crate::ANNOUNCE_PREFIX` (Task 2), `crate::parse_content_announcement` (`lib.rs:14406`), `crate::observed_holders::ObservedHolders` (`new()`, `note(&str, &str, u64)`, `peer_count(&str) -> u32`).
- Produces: `fn note_announce_sample(observed_holders: &Arc<std::sync::Mutex<crate::observed_holders::ObservedHolders>>, key_expr: &str, payload: &[u8], source_zid: Option<&str>, own_zid: &str, now_ms: u64)`.

- [ ] **Step 1: Write the four failing tests** (append to `event_loop.rs`)

```rust
#[cfg(test)]
mod note_announce_sample_tests {
    use super::*;

    fn holders() -> Arc<std::sync::Mutex<crate::observed_holders::ObservedHolders>> {
        Arc::new(std::sync::Mutex::new(
            crate::observed_holders::ObservedHolders::new(),
        ))
    }

    fn announce_key() -> (String, String) {
        let cid_hex = "ab".repeat(32);
        (format!("{}{cid_hex}", crate::ANNOUNCE_PREFIX), cid_hex)
    }

    /// Real announce payloads are a 4-byte BE u32 size.
    const PAYLOAD: [u8; 4] = 1234u32.to_be_bytes();

    #[test]
    fn foreign_zid_announce_feeds_the_holder_map() {
        let h = holders();
        let (key, cid_hex) = announce_key();
        note_announce_sample(&h, &key, &PAYLOAD, Some("peer-zid"), "own-zid", 10);
        assert_eq!(h.lock().unwrap().peer_count(&cid_hex), 1);
    }

    #[test]
    fn own_zid_announce_does_not_self_count() {
        let h = holders();
        let (key, cid_hex) = announce_key();
        note_announce_sample(&h, &key, &PAYLOAD, Some("own-zid"), "own-zid", 10);
        assert_eq!(h.lock().unwrap().peer_count(&cid_hex), 0);
    }

    #[test]
    fn missing_source_zid_is_skipped() {
        let h = holders();
        let (key, cid_hex) = announce_key();
        note_announce_sample(&h, &key, &PAYLOAD, None, "own-zid", 10);
        assert_eq!(h.lock().unwrap().peer_count(&cid_hex), 0);
    }

    #[test]
    fn non_announce_key_is_ignored() {
        let h = holders();
        let (_, cid_hex) = announce_key();
        note_announce_sample(
            &h,
            &format!("harmony/profile/{cid_hex}"),
            &PAYLOAD,
            Some("peer-zid"),
            "own-zid",
            10,
        );
        assert_eq!(h.lock().unwrap().peer_count(&cid_hex), 0);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(note_announce_sample)'`
Expected: compile error — `note_announce_sample` not defined.

- [ ] **Step 3: Extract the helper** (below `publish_attaches_zid`)

```rust
/// ZEB-612 S3 receive path, extracted for testability (ZEB-669 S1):
/// record a distinct announcing session per CID. Own announcements loop
/// back on the local session — exclude `own_zid` so `replicaCount = 1
/// (self) + peers` doesn't double-count. Samples without source info
/// can't be attributed and are skipped (the count is an observed lower
/// bound); announce publishes attach the zid (`publish_attaches_zid`),
/// so real peer announces are attributable from this build onward.
fn note_announce_sample(
    observed_holders: &Arc<std::sync::Mutex<crate::observed_holders::ObservedHolders>>,
    key_expr: &str,
    payload: &[u8],
    source_zid: Option<&str>,
    own_zid: &str,
    now_ms: u64,
) {
    if !key_expr.starts_with(crate::ANNOUNCE_PREFIX) {
        return;
    }
    if let (Some(zid), Some(a)) = (source_zid, crate::parse_content_announcement(key_expr, payload))
    {
        if zid != own_zid {
            // Poison-resilient: the holder map is a best-effort cache —
            // keep serving it rather than re-panicking the loop.
            observed_holders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .note(&a.cid, zid, now_ms);
        }
    }
}
```

Replace the arm body at `:3795-3817` (delete the inline `if key_expr.starts_with…` block and its comment) with:

```rust
                        note_announce_sample(
                            &observed_holders,
                            &key_expr,
                            &payload,
                            source_zid.as_deref(),
                            &own_zid,
                            start.elapsed().as_millis() as u64,
                        );
```

(Verify the surrounding capture names — `observed_holders`, `source_zid`, `own_zid`, `start` — match the loop's locals exactly before replacing; adjust `&`/`as_deref` to the actual types at the call site.)

- [ ] **Step 4: Run the tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(note_announce_sample) or test(dispatch_attachment)'`
Expected: PASS (all 4 + the Task 1/2 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/event_loop.rs
git commit -m "ZEB-669 S1: extract note_announce_sample + receive-side pins

The announce receive arm was inline in the select loop and untestable;
extracting it lets the spec'd pins land: foreign-zid feed, self-announce
exclusion, missing-source skip, non-announce ignore. Behavior unchanged.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc"
```

### Task 4: Gates + PR

- [ ] **Step 1: Formatter + clippy**

Run: `cd src-tauri && cargo fmt --all && cargo fmt --all -- --check && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
Expected: clean (commit any fmt-only diff into the Task 2 commit via `--amend` only if unpushed; otherwise a `style:` commit).

- [ ] **Step 2: Iterative selection gate**

Run: `scripts/test-select --context task` (repo root). Paste the printed `round=… bucket=…` line into the PR notes.
Expected: PASS.

- [ ] **Step 3: Full sweep (final gate)**

Run: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
Expected: all pass (~4,270 tests).

- [ ] **Step 4: Open PR**

Push branch, open PR against main titled `ZEB-669 S1: attach zid to announce publishes (feed the observed-holders counter)`, body explains the bug + fix + wire-compat note, ends with the Claude Code attribution + session URL. Fire `@coderabbitai review` once at open.
