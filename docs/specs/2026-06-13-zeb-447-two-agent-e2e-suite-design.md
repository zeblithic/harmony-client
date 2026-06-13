# ZEB-447 — Two-agent E2E scenario suite: design

**Status:** approved (brainstorm with Jake, 2026-06-13)
**Parent:** [ZEB-451](https://linear.app/zeblith/issue/ZEB-451) (agent-testing ecosystem)
**Base:** `main` @ `92b75fe9`
**Supersedes ticket framing:** ZEB-447's original Phase-1 (CDP/Playwright) → Phase-2 (headless API) ordering is **inverted** — the headless API surface (ZEB-445/452) and side-by-side profile isolation (ZEB-446) are now all merged, so we go **API-first**. Playwright/CDP variants are deferred to *only* visually-asserted scenarios (none of the initial set need them).

## Goal

A Rust harness that drives **two real `harmony-app serve` processes** through the headless HTTP/WS API to prove end-to-end, two-sided behavior with **no human in the loop**. The single-machine (two-profile) form is the automatable, deterministic core, built and self-validated on one machine (Koya). The **same scenario logic** is then runnable across machines (Koya↔Ildwyn↔AVALON) for the DoD's live proof.

## Why this is distinct from the existing tests

The repo already has ~71 in-process two-engine integration tests, but they all use **hermetic `Minimal`-preset endpoints** (loopback QUIC, no real discovery). ZEB-447's worth is **two real `serve` processes discovering and talking over the actual transport stack** — catching the wiring and process-boundary bugs the in-process tests structurally cannot. The harness therefore spawns the **real built binary** (`CARGO_BIN_EXE_harmony-app`), never `start_server` in-process.

## Settled decisions

1. **Orchestration model: single-machine orchestrator core.** One driver process spawns two `serve` instances on one machine, drives both via the HTTP API, passes artifacts (invite URLs, ids) directly in-process, kills a PID to simulate "offline", and asserts via read-RPCs + WS events. This is the primary, fully-automatable deliverable. The cross-machine live run reuses the same scenario definitions and is the DoD's final proof, not the development substrate.

2. **Harness form: a Rust harness in a dedicated standalone crate** (`e2e-harness/`, at repo root). This repo is a **single-package repo** (`src-tauri` = the `harmony-app` package), **not** a Cargo workspace — and we deliberately do **not** convert it to one (that would disturb the Tauri `src-tauri` build layout and the `cd src-tauri && cargo …` CI invocations, well outside this ticket's scope). `e2e-harness` is therefore its own independent crate with its own `Cargo.toml`, **not** part of harmony-app's compile/relink graph — which is exactly the isolation we want: its deps (reqwest, tokio-tungstenite) and slow real-subprocess tests stay **off harmony-app's integration-test relink path** (the ~97-binary / ~25-min `--all-targets` relink cost). Scenarios are `#[tokio::test]` functions sharing a small driver library. Runs under `cargo nextest`; CI-gateable as its own job later.

3. **Keychain safety is mandatory — use named profiles only.** `serve` runs the *real* binary, so test-build keychain hermeticity does **not** apply. Named profiles (`--profile alice` / `--profile bob`) never touch the OS keychain — they always use the encrypted-file vault, unlocked via `HARMONY_PASSPHRASE`. The harness therefore **always** spawns named profiles with a passphrase, under a per-run temp `HOME` / `XDG_DATA_HOME`. Using the default profile (which tries the real OS keychain) is forbidden — it would clobber the developer's real keychain (ZEB-428 class).

4. **Transport isolation per node.** Each spawned node gets: `--api-port 0` (OS-assigned ephemeral HTTP/WS port, read back from the `api/port` discovery file); a **distinct Reticulum UDP port** via `HARMONY_RETICULUM_PORT` (two nodes cannot share 4242); and a distinct zenoh zid (derived from the distinct profile identity, ZEB-446). All three transports stay available so any delivery path (iroh-direct, zenoh, Reticulum) can be exercised.

5. **Convergence is asserted by polling read-RPCs, not by assuming events exist.** Several state changes have **no** live event (e.g. ZEB-404's root cause was a missing `community-members-changed` event). The driver's primary convergence primitive is `poll_until(read_rpc, predicate, timeout)`. WS events are used as supplementary/early signals where they genuinely exist (`channel-message-received`, `friend-list-changed`, `mint-changed`, etc.), never as the sole gate for state we can read directly.

6. **"Offline" = a real process kill.** Headless `serve` has no window, so the ZEB-433 "closing a window ≠ quit" pain disappears: offline is `kill()` (SIGKILL, hard) or `stop_node` (graceful); back-online is a relaunch against the **same profile/data-dir** (state rehydrates from disk).

7. **Cross-machine coordination dogfoods the coord instance.** For the live run, the two agents relay artifacts (invite URLs) and turn-taking signals through the already-running `serve --profile coord` Harmony instance (posted as coord-community messages), with a documented manual-relay fallback. This part lands after the single-machine core and rides ZEB-444 (AVALON bring-up).

## Architecture

### `NodeHandle` — one `serve` subprocess

Wraps a spawned `harmony-app --profile <p> serve --api-port 0` child:

- **Spawn:** sets per-run env (`HOME`, `XDG_DATA_HOME` → temp dir; `HARMONY_PASSPHRASE`; `HARMONY_RETICULUM_PORT` → distinct port), launches the child, then **waits for the `<data-dir>/api/{port,token}` discovery files** to appear (bounded poll) before returning. Because `e2e-harness` is a standalone crate it cannot use `CARGO_BIN_EXE_harmony-app` (that env exists only for harmony-app's own test targets); it resolves the binary via `HARMONY_APP_BIN` env override, else `../src-tauri/target/{release,debug}/harmony-app` relative to the crate; if the binary is absent it fails the suite loudly with a build hint (never a silent skip).
- **`rpc(cmd, args) -> Result<Value, ApiError>`:** `POST http://127.0.0.1:<port>/v1/rpc/<cmd>` with `Authorization: Bearer <token>` and a camelCase JSON body. Maps non-200 to a typed error carrying the server's error string (identical to the GUI's).
- **WS subscriber:** a background task connects `ws://127.0.0.1:<port>/v1/events`, deserializes frames (`{seq, event, payload}`), and forwards them to an `mpsc`. `await_event(pred, timeout)` consumes from a buffered tail so events aren't missed between calls. Handles the `_lagged` sentinel by surfacing a gap warning.
- **`status() -> StatusView`:** `GET /v1/status` → `{running, generation, ownerId, uptimeSecs, port, version}`.
- **`kill()`** (SIGKILL — hard offline) and **`shutdown()`** (`POST /v1/shutdown`, graceful). `Drop` does best-effort kill + flushes captured logs into the artifact dir.

### `driver` library — semantic helpers

Thin typed wrappers over raw RPC, encoding the camelCase arg contracts once so scenarios read declaratively:
`mint()`, `create_community(name, inviteOnly)`, `generate_invite(communityId)`, `redeem_invite(url)`, `list_community_members(communityId)`, `generate_friend_token()`, `redeem_friend_token(url)`, `accept_friend_request(ownerIdHex)`, `list_friends()`, `add_space(kind, name, members)`, `send_dm(spaceId, content, mime)`, `read_dm_thread(spaceId)`, `create_channel(communityId, name, writePower)`, `list_channels(communityId)`, `post_channel_message(communityId, channelId, body)`, `list_channel_messages(...)`, plus `poll_until(pred, timeout)`.

### Artifact collector

Each run writes `target/e2e-runs/<scenario>-<runid>/`:
- `alice.log` / `bob.log` — each node's rolling tracing log (copied from its data-dir on teardown),
- `alice.status.jsonl` / `bob.status.jsonl` — periodic `/v1/status` snapshots,
- `alice.events.jsonl` / `bob.events.jsonl` — the captured WS event stream,
- `trace.jsonl` — the orchestrator's step log (rpc in/out + assertions, timestamped).

Collected on **every** run; retained on failure (cleaned on success unless `HARMONY_E2E_KEEP=1`).

## Proposed file layout

```
e2e-harness/                       # new standalone crate (repo root; NOT a harmony-app member)
  Cargo.toml                       # own manifest; deps: tokio, reqwest, tokio-tungstenite, serde_json, tempfile
  src/
    lib.rs                         # NodeHandle, driver lib, artifact collector, binary resolver
  tests/
    e2e_two_node.rs                # one #[tokio::test] per scenario (nextest entrypoints)
docs/
  playbooks/e2e-two-agent-suite.md # cross-machine run protocol (the "agent pair" doc)
```

`e2e-harness` builds and runs independently (`cd e2e-harness && cargo nextest run`); it is not referenced by harmony-app's manifest, so editing it never relinks harmony-app's test set. The two-node scenario tests are gated behind a `--features e2e` (default-off) so a bare `cargo test` in the crate stays fast and the real-transport scenarios run only when deliberately invoked.

## Scenario set

Mapped to the recurring bug classes the suite exists to catch:

- **S1 — Invite → cross-node join → roster convergence.** Alice mints a community + invite; Bob `redeem_invite`s and joins; assert (poll) Bob appears in Alice's `list_community_members` and Alice in Bob's, with correct display names. *(foundational; ZEB-404 roster class)*
- **S2 — Friend-add → DM picker → DM exchange.** Alice & Bob exchange friend tokens and accept; assert each appears in the other's `list_friends` (the ZEB-431 picker class — the friend graph is what feeds the DM picker); `add_space` a DM; bidirectional `send_dm` / `read_dm_thread` round-trip. *(ZEB-431)*
- **S3 — Channel created while peer offline → reconnect catch-up.** Alice & Bob both in the community; **`kill()` Bob** (hard offline); Alice `create_channel`; relaunch Bob against the same profile/data-dir; assert the new channel appears in Bob's `list_channels` after reconnect. *(ZEB-434)*
- **S4 (stretch) — Restart durability.** Alice mints a community, then `kill()` shortly after the create returns (before the persistence debounce); relaunch; assert the community rehydrates from disk. *(ZEB-393)*

DoD requires ≥3 proven in a live cross-machine run; S1–S3 are the committed core, S4 is built if cheap.

## Error handling & determinism

- **Outer timeout per scenario** (`tokio::time::timeout`) so a hung node surfaces as a clear failure, not a stuck suite.
- **iroh warm-up** before timed assertions (the one-time global-init cost, ZEB-347).
- **First-contact-over-loopback risk:** whether two local `serve` instances complete first contact purely on loopback or still need pkarr/relay (internet) is **de-risked in build task 1** (a bare S1 smoke). The dev box has internet either way, so a network dependency is a hermeticity caveat, not a blocker; it will be documented.
- **No silent skips:** if the binary is missing or first-contact can't be established, the suite fails loudly (or skips with an explicit logged reason), never green-on-no-op.

## CI / gating

Real-subprocess + real-transport tests are slow and network-touching, so they are **excluded from the per-task `--lib` gate**. They run as their own nextest group (`--features e2e`) in the final sweep and as a dedicated, deliberately-invoked `e2e` CI job — not on every push.

## Cross-machine reuse (live DoD proof — AVALON-gated)

The scenario *logic* is shared; only the two endpoints differ (remote `serve` instances an agent started on each machine). `docs/playbooks/e2e-two-agent-suite.md` documents the run protocol: per-scenario setup, the explicit sync points, the artifact hand-offs, and pass/fail assertions. The default coordination channel dogfoods the running `serve --profile coord` instance (relay invite URLs + "ready" signals as coord-community messages); manual relay is the documented fallback. This section lands after the single-machine core and rides ZEB-444.

## Out of scope (YAGNI)

- Playwright/CDP GUI driving (deferred to future visually-asserted scenarios only).
- A declarative scenario interpreter / `scenarios.toml` registry (scenarios are plain Rust functions).
- SSH/remote-drive of both machines from one control point.
- Voice, voting, file-ingest, and other RPC surfaces not exercised by S1–S4.

## Definition of done

1. The `e2e-harness` crate builds and S1–S3 pass single-machine (two profiles) on Koya, with artifacts produced.
2. `docs/playbooks/e2e-two-agent-suite.md` documents the cross-machine run protocol.
3. ≥3 scenarios proven in a live Ildwyn↔AVALON run with artifacts attached (rides ZEB-444; tracked as the closing step).
