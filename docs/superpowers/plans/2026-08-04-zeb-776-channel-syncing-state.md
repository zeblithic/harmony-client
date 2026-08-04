# ZEB-776 — Channel-syncing state after invite redemption — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After redeeming an invite, surface the community's channels immediately (from the invite's already-seeded `epoch_snapshot` bootstrap hint) with a `syncing` flag, and stop `list_channel_messages` from returning the misleading `"no engine"` error for a channel that is merely still converging.

**Architecture:** Read-path only. `list_channels` builds rows from two sources — the hint-blind real-CRDT materialize (`materialize_now`, `syncing:false`) merged with epoch-snapshot hint channels not yet confirmed (`syncing:true`) — via a shared resolution helper and a pure merge function. `list_channel_messages` returns `Ok(vec![])` for a channel that is known to the community engine but whose per-channel log engine has not spawned yet, reserving the `"no engine"` error for genuinely-unknown channels. Frontend gets a `syncing` field and a small "still syncing" affordance. No change to CRDT semantics, convergence timing, or the write paths.

**Tech Stack:** Rust (Tauri backend, `src-tauri/`), TypeScript + Svelte 5 (`src/`). Tests: `cargo nextest` (Rust), `vitest` + `@testing-library/svelte` (frontend).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-04-zeb-776-channel-syncing-state-design.md` (all values below trace to it).
- Rust gates run from `src-tauri/`; frontend gates (`npx tsc --noEmit`, `npx vitest run`) run from repo root.
- Rust test commands use `--locked --features test-fixtures`. Full sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Clippy: `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`. Format: `cargo fmt --all`.
- `syncing` is **always emitted** on `ChannelInfoDto` (a plain `bool`, no `skip_serializing_if`) — scripted/RPC callers must get an unambiguous signal. `ChannelInfoDto` derives `#[serde(rename_all = "camelCase")]`, so it wires as `syncing`.
- `syncing` semantics: `true` iff the channel is present in the bootstrap hint but NOT in the hint-blind `materialize_now` (i.e. not yet confirmed by a real `ChannelCreate` CRDT event). A confirmed channel always wins and shadows any hint entry for the same id (no duplicate row).
- Only the **read** path `list_channel_messages_impl` changes to `Ok(vec![])`. The sibling write/fetch IPCs that share the `"no engine for {c}/{ch}"` string — `post_channel_message` (`lib.rs:32155`, `:32266`), `download_channel_artifact` (`:32782`), `request_channel_backfill`-class (`:33761`) — MUST keep erroring (you cannot post to / download from an unspawned channel).
- Out of scope: reducing backfill latency (eager kick / reconcile), DM/profile-card convergence.

---

## File Structure

- `src-tauri/src/lib.rs` — `ChannelInfoDto` (add `syncing`); `channel_info_dto()` (add param); new `merge_channel_rows()` pure fn; new `resolve_confirmed_and_hint()` async helper; `list_channels_impl()` (rewrite read); `list_channel_messages_impl()` (add known-channel branch); inline `#[cfg(test)]` tests incl. extending `seeded_node_state`.
- `src-tauri/src/community_state_crdt.rs` — new `bootstrap_hint_channels()` accessor on `CommunityState`; inline unit test.
- `src/lib/community-service.ts` — `ChannelInfo` TS interface (add `syncing?`).
- `src/lib/community-service.test.ts` — service passthrough test.
- `src/lib/components/ChannelMessageFeed.svelte` — `channelSyncing?` prop + "still syncing" banner.
- `src/lib/components/CommunityView.svelte`, `src/lib/components/TownHallView.svelte` — thread `channelSyncing` to the feed.
- `src/lib/components/__tests__/ChannelMessageFeed.test.ts` — banner render test.
- `docs/superpowers/...` / harness notes — polling-contract doc note.

---

## Task 1: `ChannelInfoDto.syncing` field + `channel_info_dto` parameter

**Files:**
- Modify: `src-tauri/src/lib.rs:50636` (`ChannelInfoDto` struct), `src-tauri/src/lib.rs:30798-30814` (`channel_info_dto` fn), and its call sites `:31797` (prod) + `:73862-73864` (test).
- Test: `src-tauri/src/lib.rs` inline (`channel_info_dto_maps_kind` at `:73832`).

**Interfaces:**
- Produces: `pub struct ChannelInfoDto { …, pub syncing: bool }`; `fn channel_info_dto(channel_id: &ChannelId, info: &ChannelInfo, syncing: bool) -> ChannelInfoDto`.

- [ ] **Step 1: Extend the mapping test to assert `syncing` maps through**

In `channel_info_dto_maps_kind` (`lib.rs:73832`), the three existing calls become three-arg. Add assertions:

```rust
let dto_voice = channel_info_dto(&ChannelId([0x42; 16]), &voice, false);
let dto_text = channel_info_dto(&ChannelId([0x43; 16]), &text, true);
let dto_townhall = channel_info_dto(&ChannelId([0x44; 16]), &townhall, false);
assert!(!dto_voice.syncing);
assert!(dto_text.syncing, "syncing arg must map onto the DTO field");
assert!(!dto_townhall.syncing);
```

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(channel_info_dto_maps_kind)'`
Expected: FAIL — compile error (`channel_info_dto` takes 2 args, `ChannelInfoDto` has no field `syncing`).

- [ ] **Step 3: Add the field and the parameter**

In `ChannelInfoDto` (`lib.rs:50636`), after `deleted_at`:

```rust
    /// ZEB-776: true when this channel is known only from the invite's
    /// epoch_snapshot bootstrap hint and has not yet been confirmed by a real
    /// ChannelCreate CRDT event (the community root-fetch hasn't landed the
    /// admin's channel config yet). Flips to false on convergence. Always
    /// emitted so JS and scripted (RPC/api) callers get an unambiguous
    /// "still converging" signal instead of inferring it from an empty list
    /// plus a raw "no engine" error.
    pub syncing: bool,
```

In `channel_info_dto` (`lib.rs:30798`), add the parameter and field:

```rust
fn channel_info_dto(
    channel_id: &crate::community_membership::ChannelId,
    info: &crate::community_membership::ChannelInfo,
    syncing: bool,
) -> ChannelInfoDto {
    ChannelInfoDto {
        channel_id: hex::encode(channel_id.0),
        name: info.name.clone(),
        write_power: info.write_power,
        kind: match info.kind {
            crate::community_membership::ChannelKind::Text => "text".to_string(),
            crate::community_membership::ChannelKind::Voice => "voice".to_string(),
            crate::community_membership::ChannelKind::Townhall => "townhall".to_string(),
        },
        created_at: info.created_at.clone(),
        deleted_at: info.deleted_at.clone(),
        syncing,
    }
}
```

- [ ] **Step 4: Update the production call site to pass `false` (temporary — Task 3 replaces it)**

In `list_channels_impl` (`lib.rs:31797`), change the map closure to `channel_info_dto(channel_id, info, false)` so the crate compiles. (Task 3 rewrites this block entirely.)

- [ ] **Step 5: Fix any other direct `ChannelInfoDto { … }` literal constructions**

Run: `cd src-tauri && grep -rn "ChannelInfoDto {" src` — add `syncing: false` to any struct-literal construction the compiler flags (there may be none beyond the mapper). Let `cargo build` be the backstop.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(channel_info_dto_maps_kind)'`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-776: add syncing flag to ChannelInfoDto + channel_info_dto param"
```

---

## Task 2: `bootstrap_hint_channels()` accessor on `CommunityState`

**Files:**
- Modify: `src-tauri/src/community_state_crdt.rs` (after `seed_bootstrap_hint` at `:483`).
- Test: `src-tauri/src/community_state_crdt.rs` inline `#[cfg(test)]`.

**Interfaces:**
- Consumes: `CommunityState::seed_bootstrap_hint(MaterializedMembership)` (`:483`); `MaterializedMembership.channels: BTreeMap<ChannelId, ChannelInfo>` (`community_membership.rs:1839`).
- Produces: `pub fn bootstrap_hint_channels(&self) -> Vec<(ChannelId, ChannelInfo)>`.

- [ ] **Step 1: Write the failing test**

Add to the `community_state_crdt.rs` test module (mirror how other tests build a `CommunityState` + `MaterializedMembership`; `ChannelId`/`ChannelInfo` come from `crate::community_membership`):

```rust
#[test]
fn bootstrap_hint_channels_returns_seeded_channels_and_empty_without_hint() {
    use crate::community_membership::{ChannelId, ChannelInfo, ChannelKind};
    use crate::owner_state_types::Hlc;
    let state = CommunityState::new(SpaceId([0x01; 16]));
    // No hint seeded yet.
    assert!(state.bootstrap_hint_channels().is_empty());

    let mut channels = std::collections::BTreeMap::new();
    channels.insert(
        ChannelId([0x11; 16]),
        ChannelInfo {
            name: "general".into(),
            write_power: 0,
            kind: ChannelKind::Text,
            created_at: Hlc { wall_ms: 1, logical: 0, device_id: "seed".into() },
            deleted_at: None,
        },
    );
    state.seed_bootstrap_hint(MaterializedMembership { channels: channels.clone(), ..Default::default() });

    let got = state.bootstrap_hint_channels();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, ChannelId([0x11; 16]));
    assert_eq!(got[0].1.name, "general");
}
```

(If `ChannelInfo` has additional required fields beyond those shown, copy them from the `text_channel`/`joined_member` test helpers in `lib.rs:~38190` or the `ChannelInfo` definition at `community_membership.rs:2213`.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(bootstrap_hint_channels_returns_seeded_channels_and_empty_without_hint)'`
Expected: FAIL — `no method named bootstrap_hint_channels`.

- [ ] **Step 3: Add the accessor**

Immediately after `seed_bootstrap_hint` (`community_state_crdt.rs:487`):

```rust
    /// ZEB-776: the channels the bootstrap hint knows about (from the invite's
    /// epoch_snapshot), regardless of the `materialized()` log-empty guard.
    /// Empty when no hint was seeded. Clones out under the brief hint mutex —
    /// the read path merges these with the hint-blind materialize to label
    /// still-converging channels `syncing`.
    pub fn bootstrap_hint_channels(
        &self,
    ) -> Vec<(crate::community_membership::ChannelId, crate::community_membership::ChannelInfo)> {
        self.bootstrap_hint
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|h| h.channels.into_iter().collect())
            .unwrap_or_default()
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(bootstrap_hint_channels_returns_seeded_channels_and_empty_without_hint)'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/community_state_crdt.rs
git commit -m "ZEB-776: add CommunityState::bootstrap_hint_channels accessor"
```

---

## Task 3: `merge_channel_rows` pure fn + `list_channels_impl` two-source merge

**Files:**
- Modify: `src-tauri/src/lib.rs` — add `merge_channel_rows()` + `resolve_confirmed_and_hint()`; rewrite `list_channels_impl` (`:31729-31810`).
- Test: `src-tauri/src/lib.rs` inline — new `merge_channel_rows_*` unit tests; extend `list_channels_and_members_see_bootstrap_hint_before_any_crdt_event` (`:38326`).

**Interfaces:**
- Consumes: `channel_info_dto(cid, info, syncing)` (Task 1); `CommunityState::bootstrap_hint_channels()` (Task 2); `CommunityState::materialize_now(admin_addr) -> MaterializedMembership` (`community_state_crdt.rs:689`); `MaterializedMembership.channels: BTreeMap<ChannelId, ChannelInfo>`.
- Produces: `fn merge_channel_rows(confirmed: &BTreeMap<ChannelId, ChannelInfo>, hint: &[(ChannelId, ChannelInfo)]) -> Vec<ChannelInfoDto>`; `async fn resolve_confirmed_and_hint(state: &Mutex<NodeState>, space_id: SpaceId) -> Result<(BTreeMap<ChannelId, ChannelInfo>, Vec<(ChannelId, ChannelInfo)>), String>`.

- [ ] **Step 1: Write the failing unit test for `merge_channel_rows`**

Add to the `lib.rs` test module (reuse the `text_channel`/`ChannelId` helpers already there):

```rust
#[test]
fn merge_channel_rows_labels_hint_only_syncing_confirmed_not_and_dedups() {
    use crate::community_membership::ChannelId;
    let c_general = ChannelId([0x11; 16]); // in both confirmed + hint
    let c_random = ChannelId([0x22; 16]);  // hint only
    let c_dev = ChannelId([0x33; 16]);     // confirmed only

    let mut confirmed = std::collections::BTreeMap::new();
    confirmed.insert(c_general, text_channel("general", 1));
    confirmed.insert(c_dev, text_channel("dev", 3));

    let hint = vec![
        (c_general, text_channel("general", 1)),
        (c_random, text_channel("random", 2)),
    ];

    let rows = merge_channel_rows(&confirmed, &hint);
    // One row per distinct channel — c_general is not duplicated.
    assert_eq!(rows.len(), 3);
    let by_name = |n: &str| rows.iter().find(|r| r.name == n).expect("row present");
    assert!(!by_name("general").syncing, "confirmed shadows hint → not syncing");
    assert!(!by_name("dev").syncing, "confirmed-only → not syncing");
    assert!(by_name("random").syncing, "hint-only → syncing");
    // Sorted by created_at.wall_ms then logical then channel_id (created_at is
    // equal here, so ordering falls to channel_id): general, random, dev.
    assert_eq!(
        rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        vec!["general", "random", "dev"],
    );
}
```

(`text_channel(name, wall_ms)` is the existing helper near `lib.rs:38190`; it builds a `ChannelInfo` with `created_at.wall_ms = wall_ms`. Adjust the expected order if that helper's `created_at` differs — the invariant to assert is: confirmed-not-syncing, hint-only-syncing, no duplicate.)

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(merge_channel_rows_labels_hint_only_syncing_confirmed_not_and_dedups)'`
Expected: FAIL — `cannot find function merge_channel_rows`.

- [ ] **Step 3: Implement `merge_channel_rows`**

Add near `channel_info_dto` (`lib.rs:~30815`):

```rust
/// ZEB-776: build channel rows from the two sources. Confirmed channels (real
/// CRDT, from `materialize_now`) are `syncing:false` and always win; a hint
/// channel not among them is `syncing:true`. Sorted by created_at.wall_ms,
/// then logical, then channel_id (same order list_channels always used).
fn merge_channel_rows(
    confirmed: &std::collections::BTreeMap<
        crate::community_membership::ChannelId,
        crate::community_membership::ChannelInfo,
    >,
    hint: &[(
        crate::community_membership::ChannelId,
        crate::community_membership::ChannelInfo,
    )],
) -> Vec<ChannelInfoDto> {
    let mut rows: Vec<ChannelInfoDto> = confirmed
        .iter()
        .map(|(cid, info)| channel_info_dto(cid, info, false))
        .collect();
    for (cid, info) in hint {
        if !confirmed.contains_key(cid) {
            rows.push(channel_info_dto(cid, info, true));
        }
    }
    rows.sort_by(|a, b| {
        a.created_at
            .wall_ms
            .cmp(&b.created_at.wall_ms)
            .then_with(|| a.created_at.logical.cmp(&b.created_at.logical))
            .then_with(|| a.channel_id.cmp(&b.channel_id))
    });
    rows
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(merge_channel_rows_labels_hint_only_syncing_confirmed_not_and_dedups)'`
Expected: PASS.

- [ ] **Step 5: Add the shared `resolve_confirmed_and_hint` helper and rewrite `list_channels_impl`**

Add the helper (near `list_channels_impl`), preserving the existing error strings:

```rust
/// ZEB-776: shared resolution for the two channel read paths. From a joined
/// community's owner-state + community engine, returns (confirmed channels from
/// the hint-blind materialize, epoch-snapshot hint channels). Both
/// list_channels_impl and list_channel_messages_impl use it so they agree on
/// which channels a community "knows". Errors mirror the prior
/// list_channels_impl resolution.
async fn resolve_confirmed_and_hint(
    state: &std::sync::Mutex<NodeState>,
    space_id: crate::owner_state_types::SpaceId,
) -> Result<
    (
        std::collections::BTreeMap<
            crate::community_membership::ChannelId,
            crate::community_membership::ChannelInfo,
        >,
        Vec<(
            crate::community_membership::ChannelId,
            crate::community_membership::ChannelInfo,
        )>,
    ),
    String,
> {
    let (crdt_state, registry) = {
        let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
        (
            g.crdt_state.clone().ok_or_else(|| g.owner_not_loaded_msg())?,
            g.community_registry.clone().ok_or_else(|| g.owner_not_loaded_msg())?,
        )
    };
    let admin_addr = {
        let s = crdt_state.lock().await;
        let space = s.spaces.get(&space_id).cloned();
        drop(s);
        let space = space.ok_or_else(|| {
            format!("no Space for community {} in owner-state", hex::encode(space_id.0))
        })?;
        if space.kind != crate::owner_state_types::SpaceKind::Community {
            return Err(format!(
                "Space {} exists but is kind {:?}, not Community",
                hex::encode(space_id.0),
                space.kind
            ));
        }
        space
            .admin_addr
            .ok_or("community Space missing admin_addr (corrupt row?)")?
    };
    let engine_state = registry.state_for(&space_id).await.ok_or_else(|| {
        format!(
            "no engine for community {} — not joined or not yet started",
            hex::encode(space_id.0)
        )
    })?;
    let g = engine_state.lock().await;
    Ok((g.materialize_now(admin_addr).channels, g.bootstrap_hint_channels()))
}
```

Rewrite `list_channels_impl` body (keep its signature + the hex-parse block at `:31733-31738`) to:

```rust
    let space_id = crate::owner_state_types::SpaceId(id_bytes);
    let (confirmed, hint) = resolve_confirmed_and_hint(state, space_id).await?;
    Ok(merge_channel_rows(&confirmed, &hint))
```

Delete the now-superseded inline resolution + `materialized()` read + row-build + sort (`:31740-31809`).

- [ ] **Step 6: Extend the harness integration test to assert the `syncing` flag**

In `list_channels_and_members_see_bootstrap_hint_before_any_crdt_event` (`:38352`), after the existing name assertion, add:

```rust
        // ZEB-776: hint-only channels (no confirmed CRDT event yet) are syncing.
        assert!(
            listed_channels.iter().all(|c| c.syncing),
            "hint-only channels must be flagged syncing:true"
        );
```

- [ ] **Step 7: Run both tests + the existing list_channels tests**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_channels) + test(merge_channel_rows)'`
Expected: PASS (existing `list_channels` tests still green — same channels, now labeled).

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-776: list_channels surfaces hint channels with syncing flag via shared resolver"
```

---

## Task 4: `list_channel_messages_impl` returns `Ok(vec![])` for a known-but-unspawned channel

**Files:**
- Modify: `src-tauri/src/lib.rs` — `list_channel_messages_impl` (`:33320-33323`, the engine-miss arm); extend `seeded_node_state` (`:38219`) to wire an empty `ChannelLogRegistry`.
- Test: `src-tauri/src/lib.rs` inline — new test.

**Interfaces:**
- Consumes: `resolve_confirmed_and_hint(state, space_id)` (Task 3); `ChannelLogRegistry::new(config)` (`community_channel_log_engine.rs:2347`, construction mirrored from `:5873`).
- Produces: `list_channel_messages_impl` behavior — known channel + no engine → `Ok(vec![])`; unknown channel → existing `Err`.

- [ ] **Step 1: Extend `seeded_node_state` to accept/wire an empty `ChannelLogRegistry`**

The current harness leaves `channel_log_registry: None`, which would make `list_channel_messages_impl` fail early with `"channel_log_registry missing"`. Add an empty registry so the miss path is reachable. Construct `ChannelLogRegistryConfig` mirroring `community_channel_log_engine.rs:5873` (read that site for the exact fields), then in the `NodeState` literal (`:38314-38318`) add `channel_log_registry: Some(Arc::clone(&channel_log_registry))`. Return it too if a test needs it. No engine is spawned into it, so `engine(cid, chid)` returns `None` — exactly the pre-convergence state.

- [ ] **Step 2: Write the failing test**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_channel_messages_known_channel_syncing_returns_empty_unknown_errors() {
    let community = SpaceId([0xc0; 16]);
    let admin = OwnerAddr([0xad; 16]);
    let known = ChannelId([0x11; 16]);
    let unknown = ChannelId([0x99; 16]);

    let mut channels = BTreeMap::new();
    channels.insert(known, text_channel("general", 1));
    let hint = MaterializedMembership { channels, ..Default::default() };

    let (node_state, community_hex, registry, dir) =
        seeded_node_state(community, admin, hint).await;

    // Known channel, engine not spawned yet → Ok(empty), NOT "no engine".
    let msgs = crate::list_channel_messages_impl(
        &node_state, community_hex.clone(), hex::encode(known.0), None, 100, None,
    )
    .await
    .expect("known-but-syncing channel must return Ok, not the 'no engine' error");
    assert!(msgs.is_empty());

    // Unknown channel → still the existing error.
    let err = crate::list_channel_messages_impl(
        &node_state, community_hex, hex::encode(unknown.0), None, 100, None,
    )
    .await
    .expect_err("unknown channel must still error");
    assert!(err.contains("no engine for"), "got: {err}");

    registry.shutdown_all().await;
    drop(dir);
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_channel_messages_known_channel_syncing_returns_empty_unknown_errors)'`
Expected: FAIL — the known channel currently returns `Err("no engine for …")`.

- [ ] **Step 4: Implement the known-channel branch**

Replace the engine-miss `ok_or_else` (`lib.rs:33320-33323`) with an explicit match:

```rust
    let engine = match registry.engine(&cid, &chid).await {
        Some(engine) => engine,
        None => {
            // ZEB-776: distinguish "channel known to this community but its log
            // engine hasn't spawned yet" (still converging → Ok(empty)) from
            // "genuinely unknown channel" (not joined / bad id → the error).
            let (confirmed, hint) = resolve_confirmed_and_hint(state, cid).await?;
            let known = confirmed.contains_key(&chid)
                || hint.iter().any(|(c, _)| *c == chid);
            if known {
                return Ok(Vec::new());
            }
            return Err(format!("no engine for {community_id}/{channel_id}"));
        }
    };
```

(`resolve_confirmed_and_hint` re-locks `NodeState` briefly; the earlier `channel_log_registry` lock was already released. If `resolve_confirmed_and_hint` itself errors — e.g. no owner state — that error propagates via `?`, which is correct: no community context means the channel is not known.)

- [ ] **Step 5: Run to verify it passes**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_channel_messages_known_channel_syncing_returns_empty_unknown_errors)'`
Expected: PASS.

- [ ] **Step 6: Run the broader channel-message + list_channels suite for regressions**

Run: `cd src-tauri && cargo nextest run --locked --features test-fixtures -E 'test(list_channel_messages) + test(list_channels) + test(channel_log)'`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "ZEB-776: list_channel_messages returns Ok([]) for a known-but-unspawned channel"
```

---

## Task 5: Frontend `ChannelInfo.syncing` type + service passthrough + polling-contract doc

**Files:**
- Modify: `src/lib/community-service.ts:102-112` (`ChannelInfo` interface).
- Test: `src/lib/community-service.test.ts`.
- Modify: harness/testing notes doc (add polling contract).

**Interfaces:**
- Produces: `ChannelInfo.syncing?: boolean` (TS).

- [ ] **Step 1: Write the failing service test**

In `src/lib/community-service.test.ts`, add a case asserting `listChannels` passes `syncing` through from the IPC payload:

```ts
it('listChannels surfaces the syncing flag from the backend', async () => {
  const svc = makeService({
    list_channels: async () => [
      { channelId: 'ch1', name: 'general', writePower: 0, kind: 'text', createdAt: HLC, syncing: true },
    ],
  });
  const channels = await svc.listChannels('community1');
  expect(channels[0].syncing).toBe(true);
});
```

(Match the existing test's service-construction + `invoke` mocking pattern in this file — reuse its `HLC` fixture and however it stubs `invoke`/`list_channels`.)

- [ ] **Step 2: Run to verify it fails**

Run (repo root): `npx tsc --noEmit`
Expected: FAIL — `syncing` is not a property of `ChannelInfo`.

- [ ] **Step 3: Add the field to the type**

In `src/lib/community-service.ts`, inside `interface ChannelInfo` (after `deletedAt?`):

```ts
  /** ZEB-776: true while this channel is known only from the invite's
   *  epoch_snapshot (not yet confirmed by a real ChannelCreate). The Rust
   *  ChannelInfoDto always emits it; optional here so pre-existing test
   *  fixtures that omit it still type-check (absence is treated as false). */
  syncing?: boolean;
```

- [ ] **Step 4: Run type-check + the service test**

Run (repo root): `npx tsc --noEmit && npx vitest run src/lib/community-service.test.ts`
Expected: PASS.

- [ ] **Step 5: Add the polling-contract doc note**

Append to the ZEB-770 traps note / the agent-testing harness doc (search `docs/` for the ZEB-770 traps list or the harness README that documents join flows; if none is obviously the home, add a short subsection to `docs/superpowers/specs/2026-08-04-zeb-776-channel-syncing-state-design.md` under a new "Operator note" heading):

> After redeeming an invite, a channel may briefly report `syncing: true` from `list_channels` while the community root-fetch is still landing the admin's channel config. Scripted/agent flows must poll `list_channels` until the target channel's `syncing` is `false` before asserting on `list_channel_messages` (which returns `Ok([])`, not an error, for a still-syncing channel) — do not assert once. Note `syncing:false` proves config-convergence, not message-history readiness: `list_channel_messages` can still return `Ok([])` briefly after `syncing` flips false while the log engine backfills, so also wait for the expected message(s) (or tolerate an empty result).

- [ ] **Step 6: Commit**

```bash
git add src/lib/community-service.ts src/lib/community-service.test.ts docs/
git commit -m "ZEB-776: frontend ChannelInfo.syncing type + service passthrough + polling doc"
```

---

## Task 6: `ChannelMessageFeed` "still syncing" banner (threaded prop)

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (props `:74-81` + template).
- Modify: `src/lib/components/CommunityView.svelte`, `src/lib/components/TownHallView.svelte` (thread the prop to the feed).
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`.

**Interfaces:**
- Consumes: `ChannelInfo.syncing?` (Task 5).
- Produces: `ChannelMessageFeed` prop `channelSyncing?: boolean`; a `[data-testid="channel-syncing-banner"]` element rendered when it is true.

- [ ] **Step 1: Write the failing component test**

In `src/lib/components/__tests__/ChannelMessageFeed.test.ts`, following the file's existing render/props pattern:

```ts
it('shows the syncing banner when channelSyncing is true, and not otherwise', async () => {
  const { rerender } = render(ChannelMessageFeed, {
    props: { ...baseProps, channelSyncing: true },
  });
  expect(screen.queryByTestId('channel-syncing-banner')).not.toBeNull();

  await rerender({ ...baseProps, channelSyncing: false });
  expect(screen.queryByTestId('channel-syncing-banner')).toBeNull();
});
```

(`baseProps` = the minimal prop set the other tests in this file already build for `ChannelMessageFeed` — reuse it; it supplies `communityId`, `channelId`, and the service deps.)

- [ ] **Step 2: Run to verify it fails**

Run (repo root): `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts`
Expected: FAIL — no `channel-syncing-banner` element.

- [ ] **Step 3: Add the prop and banner**

In `ChannelMessageFeed.svelte`, add to the `Props` interface (`:74-81`) and destructure (`:32`):

```ts
    /** ZEB-776: true while this channel is still converging after a fresh
     *  join (known from the invite hint, not yet confirmed). Shows a small
     *  "still syncing" banner so an empty feed doesn't read as broken. */
    channelSyncing?: boolean;
```

In the template, above the message list, add:

```svelte
{#if channelSyncing}
  <div class="syncing-banner" data-testid="channel-syncing-banner" role="status">
    This channel is still syncing — messages will appear shortly.
  </div>
{/if}
```

Add a minimal, theme-aware style (mirror an existing muted banner in the file, e.g. `.reaction-error`):

```svelte
<style>
  .syncing-banner {
    padding: 0.4rem 0.75rem;
    font-size: 0.85em;
    color: var(--text-muted);
    background: var(--surface-raised);
    border-bottom: 1px solid var(--border);
  }
</style>
```

- [ ] **Step 4: Thread the prop from the parents**

In `CommunityView.svelte`, where it renders `<ChannelMessageFeed … />` (the same site that passes `channelId`), pass `channelSyncing={...}` resolved from the selected channel's `syncing` in the channel list it already holds (find the `channels`/selected-channel state; compute `channels.find((c) => c.channelId === selectedChannelId)?.syncing ?? false`). Do the same in `TownHallView.svelte` for its nested `<ChannelMessageFeed />` (forward a `channelSyncing` prop it receives, mirroring how ZEB-774 forwarded `resolveRosterName`).

- [ ] **Step 5: Run the component test + type-check**

Run (repo root): `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts && npx tsc --noEmit`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/ChannelMessageFeed.svelte src/lib/components/CommunityView.svelte src/lib/components/TownHallView.svelte src/lib/components/__tests__/ChannelMessageFeed.test.ts
git commit -m "ZEB-776: show a 'still syncing' banner on a converging channel"
```

---

## Final verification (run before opening the PR)

- [ ] Rust full sweep: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- [ ] Rust lint: `cd src-tauri && cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] Rust format: `cd src-tauri && cargo fmt --all -- --check`
- [ ] Frontend: `npx tsc --noEmit && npx vitest run` (repo root)
- [ ] Manual sanity (optional, two-node): redeem an invite; `list_channels` shows channels with `syncing:true` immediately; `list_channel_messages` returns `Ok([])` (not "no engine"); both flip within the convergence window.

---

## Self-review notes (author)

- **Spec coverage:** §1 syncing field → Task 1; §2 list_channels merge → Task 3; §3 accessor → Task 2; §4/§4a helper + list_channel_messages → Tasks 3+4; §5 frontend type + affordance → Tasks 5+6; §6 polling doc → Task 5. All covered.
- **Refinement vs spec:** the spec's `channel_sources(engine_state, admin_addr)` is realized as the fuller `resolve_confirmed_and_hint(state, space_id)` (does the whole NodeState→engine resolution) so both IPCs share one path and cannot drift — same intent, stronger DRY. The merge is factored into the pure `merge_channel_rows` for isolated unit-testing.
- **Type consistency:** `channel_info_dto(cid, info, syncing)` (Task 1) is used identically in `merge_channel_rows` (Task 3). `resolve_confirmed_and_hint` return type is consumed identically in Tasks 3 and 4. `bootstrap_hint_channels()` (Task 2) return type matches its use in the resolver.
- **Behavior-change containment:** only `list_channel_messages_impl` returns `Ok([])`; the four sibling IPCs sharing the string are untouched (Global Constraints). The ZEB-573 OPEN-path integration test asserts on the registry directly, not the IPC string, so it stays green.
