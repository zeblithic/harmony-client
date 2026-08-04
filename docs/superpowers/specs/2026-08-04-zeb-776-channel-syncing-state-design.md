# ZEB-776 — Distinguishable "channels syncing" state after invite redemption

**Status:** design approved (read-path scope), 2026-08-04
**Ticket:** ZEB-776 (Medium, harmony-client) — "Channels don't materialize immediately after joining a community — `list_channels` returns `[]` and the log engine reports 'no engine' for minutes"
**Branch:** `zeblith/zeb-776-channels-materialize-after-join`

## Problem

Immediately after an ordinary (invite-URL) redemption, for a window of up to ~2 minutes:

- `list_channels` → `[]` (the community's channels are invisible)
- `list_channel_messages` → `Err("no engine for <community>/<channel>")` (reads like a missing-resource bug)

Then it self-heals with no user action. This is indistinguishable from a genuine regression (the ZEB-573/ZEB-584 channels-gap class), makes scripted join steps flaky, and shows the nav tree and the API disagreeing about whether the community has channels.

## Root cause (investigation, answers ticket ask #1)

It is a **missed eager-wake compounded by a dead bootstrap hint** — not inherent-only latency.

1. **Members appear but channels don't** because they have different local sources. Redeem inserts **local** membership events (self-Join / admin-bootstrap / PendingJoin) → members materialize immediately. There is **no local `ChannelCreate`** — those were authored by the admin before the invite and only arrive later via the community root-fetch. The joiner's **only local source of the channel set is the invite's `epoch_snapshot`**.

2. **Redeem already seeds that channel set** into `CommunityState.bootstrap_hint` (`community_state_crdt.rs:483 seed_bootstrap_hint`). `list_channels_impl` already reads hint-aware `materialized()` (ZEB-598). **But the hint is dead-on-arrival:** `materialized()` only returns the hint while `version == 0 && log.is_empty()` (`community_state_crdt.rs:566`), and redeem's own local membership inserts make the log non-empty *before the frontend's first `list_channels` call*. So the channel list the joiner is holding gets discarded.

3. **The ~2-min latency is the root-fetch backoff ladder floored by a deliberate anti-storm cooldown.** The community engine spawns a per-community root-fetch driver at redeem (`community_state_sync.rs:5894`), which fires at t=0 then backs off 30s→90s→210s on each unanswered reply (`channel_backfill.rs:773 run_root_fetch_driver`). Eager re-arm (transport-epoch bump / presence kick) exists but is gated by `EPOCH_REARM_COOLDOWN_MS = 60_000`. So the earliest effective re-fetch after the wasted t=0 attempt is ~t=60s; reply-ingest + per-channel message backfill stacks to ≈2 min. **This floor is intentional and is NOT the lever we pull.**

4. **The invite-only path has no eager reconcile.** ZEB-573 added `reconcile_community_channel_logs` at redeem but scoped it to the OPEN cross-WAN dial path (`open_join_iroh` set); the ordinary invite path is explicitly excluded (`lib.rs:40287-40288`).

## Approved scope

Fix the **read path** so the joiner surfaces the channels it already holds, and stop emitting the misleading "no engine" error for a channel that is merely still syncing. Do **not** chase the backfill latency (fighting the 60s cooldown, plus an unresolved "why is the t=0 query unanswered" transport question) — that is a separate follow-up.

## Design

### 1. `ChannelInfoDto` gains a `syncing` flag (Rust)

`lib.rs:50636 ChannelInfoDto` (derives `Serialize`, `#[serde(rename_all = "camelCase")]`) gains:

```rust
/// ZEB-776: true when this channel is known only from the invite's
/// epoch_snapshot bootstrap hint and has not yet been confirmed by a real
/// ChannelCreate CRDT event (i.e. the community root-fetch hasn't landed the
/// admin's channel config yet). Flips to false once the authoritative event
/// materializes. Always emitted (no skip) so the JS bridge and scripted
/// (RPC/api) callers get an unambiguous "still converging" signal instead of
/// having to infer it from an empty list plus a raw "no engine" error.
pub syncing: bool,
```

`channel_info_dto()` (`lib.rs:30798`) gains a `syncing: bool` parameter, threaded to the new field. All existing call sites pass `false` except the hint-only merge in `list_channels_impl` (below). (Call sites to update: `list_channels_impl` at `:31797`, and any others surfaced by grep — e.g. the `channel_info_dto_maps_kind` test at `:73832`.)

### 2. `list_channels_impl` merges confirmed + hint-only channels (Rust)

Replace the single hint-aware `materialized()` read (`lib.rs:31789-31798`) with an explicit two-source merge that preserves ZEB-598's "show hint channels" behavior *and* labels them. Both sources come from one shared helper (§4a) so the two read paths cannot drift:

```rust
// Shared helper: hint-blind confirmed materialize + the epoch-snapshot hint
// channels for this community. `admin_addr` already resolved above.
let (confirmed, hint_channels) = channel_sources(&engine_state, admin_addr).await;

let mut rows: Vec<ChannelInfoDto> = confirmed
    .channels
    .iter()
    .map(|(cid, info)| channel_info_dto(cid, info, /* syncing */ false))
    .collect();
for (cid, info) in &hint_channels {
    if !confirmed.channels.contains_key(cid) {
        rows.push(channel_info_dto(cid, info, /* syncing */ true));
    }
}
// existing sort by created_at.wall_ms, then logical, then channel_id
```

Semantics: a channel confirmed by a real `ChannelCreate` always wins (`syncing:false`) and shadows any hint entry for the same id (including a channel later `ChannelDelete`d in the real CRDT — `materialize_now` carries it with `deleted_at` set, and the dedup keeps that authoritative row). A channel present only in the hint is `syncing:true`. Once the root-fetch lands the admin's config, the channel migrates from the hint branch to the confirmed branch and `syncing` flips to `false` with no further change.

### 3. New `bootstrap_hint_channels()` accessor (Rust)

Beside `seed_bootstrap_hint` (`community_state_crdt.rs:483`), add:

```rust
/// ZEB-776: the channels the bootstrap hint knows about (from the invite's
/// epoch_snapshot), regardless of the `materialized()` log-empty guard.
/// Empty when no hint was seeded. Clones out under the brief hint mutex — the
/// read path (list_channels) merges these with the hint-blind materialize to
/// label still-converging channels `syncing`.
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

(`MaterializedMembership.channels` is the same map iterated at `list_channels_impl:31795`.)

### 4a. Shared `channel_sources` helper (Rust)

Both read paths need the same two things: the hint-blind confirmed materialize and the epoch-snapshot hint channels. Factor them into one private async helper so they cannot drift:

```rust
/// ZEB-776: the two channel sources for a joined community's read paths —
/// (hint-blind confirmed materialize, epoch-snapshot hint channels). Callers
/// resolve `admin_addr` from owner-state first (as `list_channels_impl` does).
async fn channel_sources(
    engine_state: &Mutex<CommunityState>,   // the `registry.state_for(&space_id)` handle
    admin_addr: OwnerAddr,
) -> (MaterializedMembership, Vec<(ChannelId, ChannelInfo)>) {
    let g = engine_state.lock().await;
    (g.materialize_now(admin_addr), g.bootstrap_hint_channels())
}
```

`list_channels_impl` (§2) builds `syncing:false` rows from `confirmed.channels` and `syncing:true` rows from hint channels not in `confirmed`. `list_channel_messages_impl` (§4) uses it to decide known-vs-unknown.

### 4. `list_channel_messages_impl` returns `Ok(vec![])` for a known-but-unspawned channel (Rust)

Currently (`lib.rs:33320-33323`) a missing per-channel `ChannelLogEngine` → `Err("no engine for {c}/{ch}")` unconditionally. Change: when the engine is missing **but the channel is known to the community engine** (confirmed ∪ hint), return `Ok(vec![])` (the channel exists; no messages have synced yet). A genuinely-unknown channel — not in the community's confirmed or hint channel set — keeps the existing `Err`.

Implementation: in the `None` arm of the `registry.engine(&cid, &chid)` lookup, resolve the community engine (mirroring `list_channels_impl`'s `community_registry` + `admin_addr` + `state_for` setup), call `channel_sources` (§4a), and test `confirmed.channels.contains_key(&chid) || hint_channels.iter().any(|(c, _)| *c == chid)`. Known → `Ok(vec![])`; unknown → the existing `Err`. Resolving the community engine here (a second registry lookup) is the one added cost; it is a cheap map read and only on the miss path.

Rationale for `Ok(vec![])` over a nicer error string: it fixes scripted/RPC flows (no error to special-case), renders as a normal empty channel, and composes with the `syncing` flag — the documented poll contract is "poll `list_channels` until the channel's `syncing` is false, then read messages." A live-but-genuinely-empty channel (`syncing:false`, `Ok([])`) and a still-syncing channel (`syncing:true`, `Ok([])`) are disambiguated by the channel flag, not by the messages call.

### 5. Frontend (TypeScript / Svelte)

- **Type:** `ChannelInfo` (`src/lib/community-service.ts:102`) gains `syncing?: boolean` (optional so existing test fixtures that omit it keep compiling; backend always emits it, absence treated as `false`).
- **Channels appear instantly for free:** channels flow to the nav through the existing `listChannels` pipeline (`App.svelte`, `channel-nav-sync`), so once the backend returns hint channels they render immediately — no nav rewiring.
- **No more "no engine" error:** with the backend returning `Ok([])`, the message pane shows an empty channel instead of surfacing the raw error.
- **Minimal visible affordance:** a small "syncing…" indicator on channel rows whose `syncing` is true, and/or a "Channels still syncing" empty-state in the message pane for a `syncing` channel. Exact surface finalized against the nav row / message-pane components during planning; if the nav-row integration proves non-trivial (per the NavPanel per-view re-sync complexity), fall back to the message-pane empty-state only. This affordance is polish on top of the two load-bearing wins (channels appear; no scary error).

### 6. Docs (ticket ask #3)

Add the polling contract to the harness/testing notes: after redeem, a channel may be `syncing:true` briefly; scripted flows must poll `list_channels` until the target channel's `syncing` is `false` before asserting on `list_channel_messages`, rather than asserting once.

## Testing

**Rust (unit, no two-node harness needed):**

- `bootstrap_hint_channels()` returns the seeded channels; empty when no hint.
- `list_channels_impl` after a seeded hint + local membership inserts (the production redeem sequence — the case `list_channels_and_members_see_bootstrap_hint_before_any_crdt_event` never covered): returns the hint channels with `syncing:true`.
- `list_channels_impl` once a real `ChannelCreate` is materialized: the same channel now `syncing:false`, not duplicated.
- `list_channel_messages_impl`: known-but-unspawned channel → `Ok(vec![])`; genuinely-unknown channel → the existing `Err("no engine …")`.
- `channel_info_dto` maps `syncing` through (extend `channel_info_dto_maps_kind`).

**Frontend (vitest):**

- `ChannelInfo` with `syncing:true` flows through `community-service.listChannels`.
- The syncing affordance renders for a `syncing` channel and not for a normal one (scoped to whichever component carries it).

## Out of scope (explicit)

- **Reducing the backfill latency** (eager kick / reconcile of the root-fetch + channel-log drivers on invite-only redeem). Fights the deliberate `EPOCH_REARM_COOLDOWN_MS` and carries the unresolved "why the t=0 root-fetch is unanswered on a healthy transport" question. Candidate follow-up ticket.
- **DM author / profile-card convergence** (ZEB-568 territory) — untouched.

## Edge cases

- **Deleted-after-invite channel:** confirmed row (with `deleted_at`) shadows the hint; behaves as today once converged. Brief pre-convergence window may show a since-deleted channel as `syncing` — acceptable for a Medium UX fix.
- **Confirmed-but-engine-not-yet-spawned** (transient, between `ChannelCreate` materialize and delta-consumer spawn): `syncing:false`, `list_channel_messages` → `Ok([])`. Resolves in moments.
- **No hint at all** (non-invite joins, e.g. community creator): `bootstrap_hint_channels()` empty; behavior identical to today.
