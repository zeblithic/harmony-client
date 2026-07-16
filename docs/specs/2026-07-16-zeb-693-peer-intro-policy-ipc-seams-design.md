# ZEB-693: testable `_impl` seams for the peer-intro-policy + friend-auto-accept IPCs

**Ticket:** ZEB-693 (Gap 1 only — Gap 2 was closed by ZEB-694 / PR #475).
**Scope:** pure test-seam refactor in `src-tauri/src/lib.rs`. No behavior change, no wire-format change, no frontend change.

## Problem

`set_peer_intro_policy` / `get_peer_intro_policy` and their siblings
`set_friend_auto_accept` / `get_friend_auto_accept` are `#[tauri::command]`
wrappers whose logic is only covered by round-trip tests that **re-implement**
the load/save inline against `ConnectivitySettings` — they never call the
command's own code (no live `AppHandle`/`State` in a `#[test]`). The `None`
settings-path branches (setter error, getter spec-default) and the enum's
serde contract are unverified.

## Why not the literal `connectivity_set_identity_discoverable_impl` precedent

That precedent's `_impl` takes `&std::sync::Mutex<NodeState>` — and is itself
untested, because constructing a `NodeState` in a unit test is impractical.
It needs the state because it pulls two fields. Our four commands pull exactly
one: `connectivity_settings_path: Option<PathBuf>`. So the testable seam takes
**the path**, not the state (deliberate, documented deviation).

## Design

Four inner fns (private, `async`, beside their wrappers):

```rust
async fn set_peer_intro_policy_impl(
    settings_path: Option<std::path::PathBuf>,
    policy: crate::friend_graph::PeerIntroPolicy,
) -> Result<(), String>

async fn get_peer_intro_policy_impl(
    settings_path: Option<std::path::PathBuf>,
) -> Result<crate::friend_graph::PeerIntroPolicy, String>

async fn set_friend_auto_accept_impl(
    settings_path: Option<std::path::PathBuf>,
    enabled: bool,
) -> Result<(), String>

async fn get_friend_auto_accept_impl(
    settings_path: Option<std::path::PathBuf>,
) -> Result<bool, String>
```

Each `_impl` owns everything after the state lock, **including the `None`
branch**:

* setters: `None` → `Err("connectivity_settings_path missing")`; `Some` →
  RMW under `connectivity_settings_write_lock()` via `spawn_blocking`
  (moved verbatim from the wrappers).
* getters: `None` → spec default (`Ok(PeerIntroPolicy::FriendsOfFriends)` /
  `Ok(true)`); `Some` → `load_or_default(&path).<field>`.

The `#[tauri::command]` wrappers shrink to: lock `NodeState` → clone
`connectivity_settings_path` → delegate to `_impl` → (setters only)
`app.emit(...)` unchanged. Emit stays in the wrapper: it needs `AppHandle`,
and keeping it out of `_impl` is what makes the seam testable.

## Tests

In the existing `#[cfg(test)]` mod in `lib.rs`, as `#[tokio::test]`
(the `_impl`s are async):

1. `peer_intro_policy_impl_round_trips` — tempdir path; `set_impl` then
   `get_impl` for each of `Open`, `AskMe`, `Closed`, `FriendsOfFriends`;
   fresh-path `get_impl` returns the `FriendsOfFriends` default.
   **Replaces** `set_peer_intro_policy_persists_round_trips` (which
   re-implemented the logic inline).
2. `peer_intro_policy_impl_none_path` — `set_impl(None, …)` →
   `Err` containing `connectivity_settings_path missing`;
   `get_impl(None)` → `Ok(FriendsOfFriends)`.
3. `friend_auto_accept_impl_round_trips` — same shape; fresh-path default is
   `true`. **Replaces** `set_friend_auto_accept_persists_round_trips`.
4. `friend_auto_accept_impl_none_path` — `set_impl(None, …)` → `Err`;
   `get_impl(None)` → `Ok(true)`.
5. `peer_intro_policy_serde_tokens_pinned` — `serde_json` round-trip pins the
   wire tokens exactly: `Open`↔`"open"`, `FriendsOfFriends`↔`"fof"`,
   `AskMe`↔`"ask"`, `Closed`↔`"closed"`. This is the achievable form of the
   ticket's "serde boundary" concern — the Tauri macro consumes this exact
   serde representation, so a variant rename that would break the JS contract
   fails here.

## Constraints

* No behavior change: RMW-under-write-lock semantics, emit payloads, error
  strings, and spec defaults all preserved verbatim.
* Gates: `cargo fmt --check`, `clippy --locked --all-targets
  --features test-fixtures -D warnings`, nextest (scoped during dev, full
  CI-parity sweep before PR), `tsc` + `vitest` untouched-but-run-by-CI.
* Execution: inline TDD in-session (single file, one cohesive pattern) —
  approved by Jake in the design review; whole-branch review + bot pass at
  PR time as usual.
