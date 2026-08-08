# ZEB-879 (residual): runtime discoverability enable — verify + observe

**Status:** design settled (2026-08-08). Scope confirmed with Jake: "Verify + observe."
**Branch:** `zeblith/zeb-879-runtime-enable-republish-stall`

## Background

ZEB-879's original symptom (misleading "no relays available" on a fresh mint) was
addressed earlier: the error-copy fix (879a) shipped in PR #634 and ZEB-881 flipped
the default discoverability posture to ON. The remaining, open residual is the
**"Update" section** of ZEB-879:

> On AVALON, enabling `connectivity_set_identity_discoverable {enabled:true}` at
> runtime did NOT trigger a case-B republish for ~14 min (zero routing-record /
> pkarr-identity activity); it only published after a `stop_node`→`start_node`
> bounce. On Ildwyn the same runtime enable republished on the next ~7.5-min
> cadence. "A user who flips the setting and waits may stay silently unreachable
> with no error at all."

## Root-cause investigation (systematic-debugging Phase 1–3)

Traced the full runtime-enable path and its safety net:

1. **Trigger path** — `connectivity_set_identity_discoverable_impl`
   → `set_identity_discoverable_detached` (persist-then-toggle, cancellation-surviving)
   → `PkarrIdentityPublisher::enable()`
   → `PkarrPublisher::register(HANDLE, …)` which sets `next_publish_at = now` and
   `wakeup.notify_one()`.
2. **Driver** — `PkarrPublisher::spawn()` is started **unconditionally** when the
   owner loads (`lib.rs:9799`), regardless of the discoverability setting. The
   identity publisher shares that exact `Arc<PkarrPublisher>` (`lib.rs:9991–10119`).
   So `register()` wakes a running loop, which drives a PUT within seconds (or a
   60-s retry on a transient relay error).
3. **Periodic self-heal** — the `routing_republish` closure fires every
   `BUTLER_SET_REFRESH_MS` = 7.5 min (`event_loop.rs:4591`), reads the **persisted**
   setting via `identity_republish_enabled()` and re-registers case-B whenever the
   user has opted in (ZEB-516 redundancy net).

**Findings:**

- **Both publish paths are code-correct.** The immediate path reaches a running
  driver; the periodic path self-heals on the persisted flag. No deterministic
  logic defect explains a 14-minute stall.
- **Relay cooldown is 30 s** (`harmony-pkarr` `relay.rs`), so cold/cooled relays
  cannot produce a multi-minute stall — that hypothesis is refuted by the core's
  own constant.
- **The immediate enable is completely silent.** `enable()` → `register()` logs
  nothing on success, and the driver's success path logs nothing either. The only
  loud identity-success log is the periodic tick's `re-publish completed
  identity=true`. So the field reporters could only ever *see* the periodic tick —
  "Ildwyn at 7.5 min / AVALON at 14 min" describes when the **periodic net** logged,
  and says nothing about whether the silent immediate publish worked. That silence
  **is** the ticket's "silently unreachable with no error at all."

**Conclusion:** the runtime-enable trigger is correct; the reported stall is an
environmental/timing artifact made *invisible* by a silent success path. The frozen
core (`harmony-pkarr`) exposes no per-handle PUT result — `network_health`'s
`identity_last_publish_ms` is itself *derived* from relay health (ZEB-511), not a
direct PUT observation. So the correct, in-scope handling (per systematic-debugging's
"process reveals no single root cause") is **observability + bounded verification**,
not a speculative rewrite of a correct, frozen-core-adjacent path.

## Design

Give the runtime discoverability toggle an explicit, logged, verified outcome.
Client-side only; no changes to the frozen `harmony-pkarr` crate.

### `src/pkarr_identity_publisher.rs`

- `pub async fn is_active(&self) -> bool` — true when this device's identity
  (case-B) publication is registered with the driver. Reads the driver's active
  handle set (`PkarrPublisher::active_handles()`) — the **same** "is it active?"
  source of truth the ZEB-385 Network Health self-test uses, not a duplicated flag.
- `pub enum ToggleOutcome { EnabledActive, EnabledInactive, Disabled }`.
- `pub async fn toggle_and_verify(&self, enabled: bool) -> ToggleOutcome` —
  applies the toggle and reports the observed outcome. On enable, verifies the
  publication registered. Registration completes **synchronously** inside
  `enable()` (`register()` inserts under the state lock before returning), so a
  single post-enable check is authoritative — no polling window is needed.

`enable()` / `disable()` stay public and unchanged: the boot enable (`lib.rs:10155`,
its own ZEB-794 log) and the periodic tick (`lib.rs:10452`, its own debug log) keep
their context-appropriate logging and do **not** route through `toggle_and_verify`.

### `src/lib.rs` — `set_identity_discoverable_detached`

Replace the bare `enable()`/`disable()` with `toggle_and_verify(enabled)` and log
the outcome (the toggle was previously fire-and-forget + silent):

- `EnabledActive` → `info!` — case-B registered; node resolvable by
  `add_friend_by_key` once the driver publishes.
- `EnabledInactive` → `warn!` — enabled but NOT registered afterwards; node may stay
  unreachable (wiring regression guard).
- `Disabled` → `info!` — case-B unregistered; `add_friend_by_key` now returns
  `unreachable`.

The verify is inline in the existing cancellation-surviving detached task; it adds a
sub-millisecond `active_handles()` read, not network latency. The toggle's return
value to the IPC caller is unchanged (a `warn` does not fail the toggle — the setting
persisted).

### What this does and does not prove

- **Does:** make the runtime enable/disable observable (never silent again), and
  positively confirm the publication is registered — catching any wiring regression
  that would leave the node silently unreachable.
- **Does not:** confirm the network PUT landed on a relay/DHT. That is not observable
  client-side without a change to the frozen core; `network_health` already derives a
  best-effort last-publish signal from relay health for the diagnostics surface.

## Test plan

Unit tests in `pkarr_identity_publisher.rs` (existing `MockPkarrRelay` + spawned
`PkarrPublisher` pattern):

1. `is_active` is false before enable, true after enable, false after disable.
2. `toggle_and_verify(true)` returns `EnabledActive` and leaves `is_active` true.
3. `toggle_and_verify(false)` (after an enable) returns `Disabled` and leaves
   `is_active` false.

The existing `identity_discoverable_toggle_pair_survives_future_cancellation`
(`lib.rs`) already pins that the detached toggle registers the handle end-to-end;
it continues to pass unchanged.

## Out of scope / follow-ups

- A concurrent-writer lost-update on `connectivity-settings.json` (if any writer
  RMWs the file without `connectivity_settings_write_lock()`) — not observed here,
  not chased; note only.
- End-to-end "did the PUT land" verification (self-resolve) — has DHT/relay
  propagation delay, prone to false negatives in a bounded window; deliberately not
  attempted.
