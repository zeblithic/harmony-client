# Owner-state persist-only durability fence — design

**Status:** approved (Jake, 2026-06-21). Closes ZEB-509.

## Problem

A node that joins a community via `redeem_invite` can permanently lose the
community's epoch key if the process dies shortly after joining. On the next
boot the owner-state CRDT reloads with `spaces: {}`, so `live_epoch_key`
(`community_state_sync.rs:2506-2532`) returns `LiveEpochKeyMissing` forever. The
community-state serve path then silently withholds every reply
(`event_loop.rs:6688`), which the fetch driver reports as
"`startup root query: no responder`" — a permanent convergence deadlock that
blocks the async-DM deposit→recover loop, 3-node communities, and cross-WAN.

Empirically confirmed from surviving e2e `s6` artifacts: the founder's
persisted `owner_state_crdt.cbor` carries the Space with `ce`/`ek` (epoch fields),
while both redeemers' `spaces` maps are empty — even though their per-community
membership CRDT (`crdt.cbor`) survived and they accept relay deposits.

## Root cause

The owner-state durability fence is gated behind a network publish.

`redeem_invite_impl` already fences owner-state durability via
`fence_owner_state_flush` (`lib.rs:23762`, helper at `lib.rs:39486`), which calls
`engine.flush_now()` inside a 5-second `tokio::time::timeout`. But `flush_now`'s
engine arm (`fleet_sync.rs:438-461`) is **publish-before-persist**:

```rust
let pub_result = publish_root_now(&ctx).await;   // :451 — network publish FIRST
...
let persist_result = persist_now(&ctx).await;    // :458 — disk persist SECOND
```

`publish_root_now` does a `content_store.put` and a bounded(64) `publisher_tx.send`.
When there is no zenoh responder (the exact condition during async-DM bring-up),
the publish leg back-pressures past 5 seconds, the fence's `timeout` cancels the
whole future **mid-publish, so `persist_now` never runs**, and the fence only logs
a warning and re-arms the debounce (`notify_dirty`). The re-armed debounce lives
in the *same* engine task that is still stuck awaiting the publish, so it makes no
progress either. A SIGKILL then loses the epoch Space.

The asymmetry that made membership survive but the epoch vanish: the community
membership fence `fence_community_crdt_persist` (`lib.rs:39528`) calls the
community engine's **persist-only** `persist_now()` — no publish leg to stall —
so `crdt.cbor` always lands. The owner-state fence calls `flush_now`
(publish-then-persist), so its persist is starved.

This is a real durability bug on any platform (a crash in the window between
`redeem_invite` returning and the flush completing), not a co-located-harness
artifact. It is the same class as prior fixes (channel-log shutdown guard,
ZEB-460/462): a write whose durability isn't fenced-before-acknowledge.

`crdt_state` and the owner-state `SyncEngine` share one `Arc<Mutex<OwnerState>>`
(`lib.rs:4035`) and `persist_now` snapshots it (`fleet_sync.rs:516`), so the
flush already targets the right object — it just never reaches the persist step.

## Design

Mirror the community engine's proven persist-only path on the generic
`FleetSyncEngine<S>`, and point the owner-state durability fence at it.

### 1. `FleetSyncEngine<S>` gains a persist-only path

Add a `persist_now()` method that routes a oneshot through a dedicated
`persist_now_rx` arm in the engine task, calling the existing persist-only
`persist_now(&ctx)` free fn (`fleet_sync.rs:512`) — **persist, no publish**. This
mirrors `flush_now`'s plumbing (struct field + `mpsc::channel(8)` + `Ctx` field +
public method + `select!` arm) and the community engine's `persist_now` arm
(`community_state_sync.rs:2276`). The arm deliberately does **not** touch
`has_pending_dirty`: any pending state-root publish still fires on the next
debounce, exactly as the community arm documents.

### 2. `owner_state_sync::SyncEngine` exposes it

Add `pub async fn persist_now(&self) -> Result<(), SyncError>` delegating to
`self.inner.persist_now()`, alongside the existing `flush_now`/`notify_dirty`
delegates (`owner_state_sync.rs:167-184`).

### 3. `fence_owner_state_flush` becomes persist-only + best-effort publish

Change the fence (`lib.rs:39486`) to call `engine.persist_now()` instead of
`engine.flush_now()`, then `engine.notify_dirty()` so the owner-state root still
publishes to the user's other devices via the normal debounce — **best-effort,
never blocking durability**. The 5s timeout is retained as a guard against a
wedged engine task; a local persist is fast and will not normally approach it. On
persist error/timeout the existing `notify_dirty` re-arm is kept. This single
helper fixes every owner-state durability site at once (`redeem_invite`,
`create_community`, leave-adjacent writes) since they all route through it.

## Why mirror the community engine

`fence_community_crdt_persist` + the community `persist_now` arm are the proven
template — they already make membership durable independent of publish, which is
exactly why `crdt.cbor` survived while the epoch did not. Reusing the pattern
keeps the two fences symmetric and minimizes novel surface.

## Testing

- **Unit (deterministic regression guard):** mirror
  `persist_now_fences_crdt_without_publishing_zeb462`
  (`community_state_sync.rs:5453`) for `FleetSyncEngine`/owner-state: after a
  local mutation, `persist_now()` returns `Ok` and persists state to the backend
  while the publisher channel receives **no** publish — and it completes even
  when the publisher channel is saturated (the precise property `flush_now`
  lacks). This encodes the fix invariant: durability never waits on publish.
- A graceful-restart e2e is intentionally **not** added as a regression test: a
  graceful shutdown flushes via the shutdown arm, so it would pass with or
  without the fix (a false guard). Broader redeemer-restart e2e coverage (the
  gap that `s4_restart_durability` leaves — it covers only the creator) is noted
  as a follow-up; the deterministic guard lives at the unit level.

## Non-goals

- **The §10.6 backward-secrecy serve guard is not touched.** Refusing to serve
  under an incomplete epoch is correct; the bug is purely that the epoch was lost
  before it could be served.
- **The publish-gate-before-`verify_event` deadlock (ZEB-526) is out of scope** —
  a distinct root cause in the same subsystem, with its own fix.

## Files

- `src-tauri/src/fleet_sync.rs` — persist-only path on `FleetSyncEngine` + unit test.
- `src-tauri/src/owner_state_sync.rs` — `persist_now` delegate.
- `src-tauri/src/lib.rs` — `fence_owner_state_flush` → persist-only + `notify_dirty`.

## Gate

From `src-tauri/`: `cargo fmt --all -- --check` + `cargo clippy --locked
--all-targets --features test-fixtures --no-deps -- -D warnings` + `cargo nextest
run --locked --workspace --all-targets --features test-fixtures`. Per-task,
lib-scoped runs (`-p harmony-app --lib`) during dev; full `--all-targets` for the
final sweep.
