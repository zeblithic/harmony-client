# ZEB-711 + ZEB-712 — monotonic limiter clock + community-engine closing guard

Bundle PR (one-PR-per-repo). Branch `zeb-711-712-limiter-clock-fence-toctou` off `main@b19ee07b`.

- **ZEB-711** — migrate both ALPN rate limiters (`IntroRateLimiter` on `harmony/friend-pex/v1`,
  `FriendRateLimiter` on `harmony/friend/v1`) off `wall_now_ms()` onto a monotonic timeline, in
  one move so the two acceptors' audited posture stays uniform.
- **ZEB-712** — close the registry-detach TOCTOU structurally at the engine (one fix, not
  per-site): a `closing` guard on `CommunitySyncEngine` inserts, mirroring the ZEB-248
  channel-log shutdown guard.

## Verified state (2026-07-18, main@b19ee07b)

| # | Fact | Where |
|---|------|-------|
| V1 | Both limiters live in `friend_intro.rs`; every admit method takes an explicit `now_ms: u64` (the clock seam) driven by callers with `wall_now_ms()` (`SystemTime`) | `friend_intro.rs:737-960` |
| V2 | Exactly 3 caller computations feed all 6 admit calls: pex `let now` ×2 (tier-1 + tier-2 reuse in each arm), friend `limiter_now_ms` (reused by `admit_owner`) | `iroh_pex_acceptor.rs:606→609/651, 746→749/795`; `iroh_friend_acceptor.rs:2069→2072/2133` |
| V3 | Other `wall_now_ms()` uses are legitimately wall-domain: HLC stamping (`next_hlc`), friend-token expiry (`token_gate_open`), pending-request recorded-at | `iroh_friend_acceptor.rs:1975,2019,2304` |
| V4 | `FriendRateLimiter` serve-path shed tests exist with `with_caps` tiny caps — a paused-clock window-expiry serve test slots in beside them | `iroh_friend_acceptor.rs:5467-5560` |
| V5 | ZEB-712 residual window confirmed: fence re-lock check releases the NodeState mutex, then `insert_local_event` proceeds through the stale snapshot Arc; `stop_inner` → `registry.shutdown_all()` → engine `shutdown_tx` final-flushes and the task exits — a later insert appends to `state` with no task listening (accepted, never persisted/published) | `lib.rs:2363` (stop take), `community_state_sync.rs:1723` (shutdown), `:2524` (shutdown arm) |
| V6 | **Option 1 (stop_inner bumps `generation`) FAILS its audit**: the only production bump is start_node's Ok(thread) arm with a documented ZEB-221 invariant — "bump generation ONLY here … Post-install checks (pairing-handle install, failure cleanup, stop_inner gating) compare `guard.generation` against `our_gen` and rely on this invariant." `stop_inner` itself gates on `generation != gen → stale stop` (`lib.rs:2332`); bumping on stop breaks that contract | `lib.rs:11218-11226, 2331-2335, 12188-12648` |
| V7 | All local-insert entry points funnel through two lock blocks: `insert_event_with_resolved_pubs` (shared body for `insert_local_event`, `insert_local_event_with_pubs`, ZEB-583 `insert_local_channel_create`) and `insert_local_event_pair` (own C5-atomic lock block) | `community_state_sync.rs:1307,1343,1369,1395,1542` |
| V8 | Prior art for the guard: ZEB-248 channel-log shutdown — appends check `closing` under the log lock; shutdown sets it before the final flush. Same semantics wanted here | `channel_log.rs` (ZEB-248, PR #248) |

## Design

### D1 — ZEB-711 monotonic limiter clock

- Add `epoch: tokio::time::Instant` to `IntroRateLimiter` and `FriendRateLimiter` (captured in
  `with_caps`, so `new()` inherits) + one accessor each:
  `pub fn monotonic_now_ms(&self) -> u64 { self.epoch.elapsed().as_millis() as u64 }`.
  `tokio::time::Instant` is monotonic AND honors the paused test clock (`start_paused` /
  `tokio::time::advance`), which is exactly the testability the ticket asks to keep.
- Replace the 3 caller computations (V2) with `self.<limiter>.monotonic_now_ms()`. The admit
  methods keep their `now_ms` params — every existing unit test keeps feeding relative ms
  unchanged; only the production feed source changes.
- Wall time stays for V3 sites (HLC, token expiry, recorded-at) — per the ticket, wall is for
  logging/expiry domains only.
- Semantics note: window state is per-limiter-instance and the epoch is too, so timelines are
  internally consistent; a process restart resets both together (windows are in-memory anyway).

### D2 — ZEB-712 engine closing guard (remediation option 2)

Option 1 is rejected on the V6 audit — exactly the audit the ticket called for. Option 2 closes
the window structurally at the single point every community lifecycle IPC funnels through,
covering all 12 fence sites plus `remove_space`'s cleanup gate and any FUTURE call site, with
zero per-site changes (the existing `is_none()` fences remain as fast-fail diagnostics).

- `closing: Arc<AtomicBool>` on `CommunitySyncEngine`, cloned into `InternalCtx`.
- Shutdown arm (`community_state_sync.rs:2524`) — FIRST action, before the final
  publish/persist: `{ let _g = ctx.state.lock().await; ctx.closing.store(true, SeqCst); }`.
  Because inserts append under that same `state` lock, this is atomic w.r.t. every insert:
  an insert that wins the lock race lands BEFORE the flag and is included in the shutdown
  arm's final flush (durable — correct success); one that loses sees the flag and errors
  (surfaced — correct failure). The silent middle ground is gone.
- Check in both V7 lock blocks (shared body + pair), inside the lock, before any mutation:
  `LocalInsertError::EngineShuttingDown` (new variant,
  `#[error("community engine is shutting down (node stopped?)")]`) — IPC sites already
  stringify `LocalInsertError` via Display, so the message reaches the frontend unchanged.
- DM paths are OUT of scope: `dm_outbox` already has its own structural guard (ZEB-234
  `dm_send_stopping`/`dm_send_inflight` fences taken under the NodeState lock at stop).

## Test plan (red-first)

- R1 (rust, ZEB-712): `insert_after_shutdown_errs_engine_shutting_down` — build engine via the
  existing fixtures, `shutdown().await`, insert a valid event → expect
  `Err(EngineShuttingDown)`. **Deterministically red pre-fix:** returns `Ok(Inserted)` — the
  exact silent-loss bug. Companion `insert_local_event_pair` variant pins the second entry
  point.
- R2 (rust, ZEB-712, pin): `insert_before_shutdown_included_in_final_flush` — insert, shutdown,
  reload persisted CRDT → event present (guards the flush-inclusion half of the semantics).
- R3 (rust, ZEB-711): paused-clock serve-level test beside V4: `with_caps(1, _, W)` shed on
  second dial, `tokio::time::advance(W+1)`, third dial admitted. **Red pre-fix:** wall clock
  ignores `advance`, third dial still shed. Plus one unit-level pin per limiter:
  `monotonic_now_ms()` advances with `tokio::time::advance` under `start_paused`.
- Gates per task (iterative): `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`,
  `scripts/test-select --context task` — paste the emitted `round=… bucket=…` line into the
  task report.
- Final (pre-PR, CI parity): `cargo nextest run --locked --workspace --all-targets
  --features test-fixtures` + the same fmt/clippy set + `npx tsc --noEmit` + `npx vitest run`
  (no frontend files touched; parity confirmation only).

## Task order

1. Commit plan. 2. ZEB-712: R1/R2 red → closing guard → green. 3. ZEB-711: R3 red → epoch +
feed-site migration → green. 4. Full gates → PR (`Closes ZEB-711. Closes ZEB-712.`) → converge.
