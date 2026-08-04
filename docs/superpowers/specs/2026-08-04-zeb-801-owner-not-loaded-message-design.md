# ZEB-801 — non-destructive, state-classified "owner not loaded" message Design

**Ticket:** ZEB-801 (follow-up to ZEB-338's owner-precondition sweep)
**File under change:** `src-tauri/src/lib.rs` (plus `src-tauri/src/api/rpc.rs` test-string references)
**Date:** 2026-08-04

## Goal

Stop the owner-derived-handle guards from telling a user to **"recreate
identity"** — an unrecoverable, friendship-destroying action on a file-store
identity — when the true cause is almost always that the node is *still
starting*. Replace the single destructive constant with a classifier that
returns a non-destructive message chosen by the node's actual state, and sweep
every guard site to use it.

## Background — what ZEB-338 shipped, and how it landed wrong

`OWNER_NOT_LOADED_MSG` (`lib.rs:2400`) is the string
`"Owner identity not loaded — please restart the app or recreate identity."`,
applied at ~175 owner-derived-handle guard sites (see the Reference inventory
below), the dominant shape being:

```rust
g.crdt_state.clone().ok_or(OWNER_NOT_LOADED_MSG)?
g.community_registry.clone().ok_or(OWNER_NOT_LOADED_MSG)?
g.dm_outbox.clone().ok_or(OWNER_NOT_LOADED_MSG)?
```

ZEB-338 introduced this constant to replace an earlier *misleading* message
(`crdt_state missing — node not running?`) that appeared when the true cause
was a missing owner identity. But the sweep then applied the honest-identity
wording at guards whose **dominant** cause is *node-not-started*, inverting the
original bug. The constant's own comment is candid: *"Incremental adoption —
applied where edited, not a blanket sweep."*

### The harm (why this is more than cosmetic)

The suggested remedy — "recreate identity" — is the single most destructive
action available. On a file-store identity (`~/.harmony/identity.enc` +
`master_seed.enc`) recreating is unrecoverable, and because friendships key on
owner id it silently destroys every friend leg. The error is self-fulfilling:
it claims the identity is gone, and following its advice is what makes that
true. Waiting a few seconds fixes it completely. (Observed on Ildwyn during a
fleet rebuild, 2026-07-26.)

## Verified-source correction to the ticket's proposed mechanism

The ticket proposes distinguishing the two cases with `dm_self_owner`: *"if
`dm_self_owner.is_some()` but the derived handle is `None` → node still
starting; if `dm_self_owner.is_none()` → genuine identity problem."*

**That discriminator cannot fire.** Traced against source (2026-08-04):

- `dm_self_owner`, `crdt_state`, `dm_outbox`, `community_registry`,
  `hlc_tracker`, `dm_device_id` are all installed in **one atomic guard block**
  in `start_node` (`lib.rs:12476–12552`) — `guard.thread = Some(...)` and
  `guard.generation += 1` land in that same block — and are nulled together on
  identity restore (`lib.rs:80588`, plus `stop_inner`). Their source vars
  (`self_owner_for_state`, `crdt_state_for_state`) are both set only on the
  owner-present boot branch (`lib.rs:11803–11805`), default `None`
  (`lib.rs:4548`, `:4554`).
- So `dm_self_owner` flips `None→Some` in **lockstep** with the guarded
  handles: it is never `Some` while a sibling handle is `None`. Testing
  `dm_self_owner.is_some()` at a site whose handle is `None` is vacuous.
- The ticket's evidence — `get_owner_state` returning the ownerId during the
  window — does **not** prove `dm_self_owner` is `Some`. `get_owner_state`
  reads the *persisted* identity from disk (`owner_commands::get_owner_state`),
  a different source than the in-memory `NodeState.dm_self_owner`.

### The discriminator that does work: `node_is_running()` (`self.thread`)

`self.thread: Option<thread::JoinHandle<()>>` (`lib.rs:772`);
`node_is_running(&self) -> bool { self.thread.is_some() }` (`lib.rs:1740`). It
is set in the *same* atomic install block, so at a guard whose handle is
`None`:

- `!node_is_running()` (thread `None`) ⇒ boot has not reached the install
  point → **still starting**. (This is the observed Ildwyn case, and the state
  the sibling `channel_log_registry missing — node not running` guard reports
  honestly.)
- `node_is_running()` (thread `Some`) but the handle is `None` ⇒ node up with
  no owner identity loaded → **no identity / pre-mint** (the no-owner boot
  branch installs `thread` but leaves the owner handles `None`).

Neither state warrants "recreate identity."

## Decision (settled with Jake 2026-08-04)

**Approach B — classify + full sweep, with a rename for clarity.** Chosen over
(A) retexting the one constant to a single message, and (C) B plus a
disk-existence check to split "never minted" from "present-but-failed-to-load."
C is declined as scope creep for a rare, UI-gated state (it also adds disk I/O
under the guard lock and re-introduces a careful pointer to reset). A is
rejected because a single "try again" message misleads a genuine no-identity
node.

## Reference inventory (verified 2026-08-04, `grep` across `src/`)

Every `OWNER_NOT_LOADED_MSG` reference the rename must touch (175 production
sites + 5 test asserts + 4 comments + the definition):

| Shape | Count | Location(s) | Becomes |
| -- | -- | -- | -- |
| `.ok_or(OWNER_NOT_LOADED_MSG)` (guard held, receiver `g`) | 163 | `lib.rs` | `.ok_or_else(\|\| g.owner_not_loaded_msg())` |
| `.ok_or_else(\|\| OWNER_NOT_LOADED_MSG.to_string())` (guard `g` held) | 1 | `lib.rs:33025` | `.ok_or_else(\|\| g.owner_not_loaded_msg().to_string())` |
| `return Err(OWNER_NOT_LOADED_MSG.into())` (guard **released**) | 11 | `lib.rs:62143, 63411, 63842, 64135, 64946, 65004, 65090, 65238, 65391, 65669, 66648` | capture `not_loaded_msg` in the extraction block; `return Err(not_loaded_msg.into())` (§2b) |
| `assert_eq!(err, OWNER_NOT_LOADED_MSG)` (default node ⇒ not running) | 2 | `lib.rs:74768, 74778` | `assert_eq!(err, OWNER_STILL_STARTING_MSG)` |
| `assert_eq!(msg, crate::OWNER_NOT_LOADED_MSG)` (pre-node ⇒ not running) | 3 | `rpc.rs:2584, 2645, 2670` | `crate::OWNER_STILL_STARTING_MSG` |
| Prose comment naming the constant | 4 | `lib.rs:1753, 33406, 63216`; `rpc.rs:2550` | reworded to the new names |
| `const` definition | 1 | `lib.rs:2400` | replaced by two constants (below) |

After the rename, no `OWNER_NOT_LOADED_MSG` identifier remains. A final
`grep -rn OWNER_NOT_LOADED_MSG src/` returning nothing is the completeness
check.

## Components (all in `lib.rs` unless noted)

### 1. Two non-destructive message constants (replacing `OWNER_NOT_LOADED_MSG`)

Retire `OWNER_NOT_LOADED_MSG`. Add, at the same crate-root location:

```rust
/// ZEB-801: shown when an owner-derived handle is absent because the node
/// has not finished starting (`!node_is_running()`). The common case.
pub(crate) const OWNER_STILL_STARTING_MSG: &str =
    "Owner identity not loaded — the app is still starting. Try again in a moment.";

/// ZEB-801: shown when the node IS running but no owner identity is loaded
/// (pre-mint / absent). Non-destructive — never advises recreating identity.
pub(crate) const OWNER_NO_IDENTITY_MSG: &str =
    "Owner identity not loaded — no identity is set up on this device yet.";
```

Both are `pub(crate)` (the existing constant is reachable as
`crate::OWNER_NOT_LOADED_MSG` from `api/rpc.rs` tests; keep that reachability).
Neither string contains "recreate" or "restart the app or recreate".

### 2. Classifier method on `NodeState`

```rust
impl NodeState {
    /// ZEB-801: classify why an owner-derived handle is absent, for a
    /// non-destructive user-facing error. Called from the `ok_or_else` at the
    /// owner-handle guard sites, i.e. only when a handle is already `None`.
    ///
    /// The owner handles (`crdt_state`, `dm_outbox`, `community_registry`,
    /// `dm_self_owner`, …) are installed in one atomic block alongside
    /// `self.thread` at `start_node` and nulled together, so `thread`
    /// faithfully separates the two absence causes. Neither warrants the
    /// destructive "recreate identity" advice ZEB-338 left here.
    fn owner_not_loaded_msg(&self) -> &'static str {
        if self.node_is_running() {
            OWNER_NO_IDENTITY_MSG
        } else {
            OWNER_STILL_STARTING_MSG
        }
    }
}
```

Private method (guard sites are in the crate-root module, so private is
sufficient and keeps the surface minimal). Returns `&'static str` — the same
Err type the guards produce today.

### 2b. Same-locked-snapshot capture for guard-released early-returns

The 11 `return Err(OWNER_NOT_LOADED_MSG.into())` sites extract their handles
inside a `{ let g = state.lock()…; (g.field.clone(), …) }` block and **drop the
guard** before the `else { return Err(..) }`. An earlier draft re-locked from a
small free function at the return — but `thread` can flip between the two locks
(node start/stop), so a handle observed as `None` under lock #1 could be
classified against a *different* `thread` under lock #2 (CodeRabbit, PR #603).

**Capture the classified message in the SAME locked snapshot as the handle
read.** In each extraction block, bind the message while the guard is held and
return it — no second lock, no TOCTOU:

```rust
let not_loaded_msg;
let (handle_a, handle_b, …) = {
    let g = state.lock().map_err(|e| format!("NodeState poisoned: {e}"))?;
    not_loaded_msg = g.owner_not_loaded_msg(); // same snapshot as the handles
    (g.handle_a.clone(), g.handle_b.clone(), …)
};
let (Some(a), …) = (handle_a, …) else {
    return Err(not_loaded_msg.into());
};
```

`not_loaded_msg` is definitely-assigned before its only use (the block runs to
completion, or `?` returns the poison error first). No free function; the method
is the single classification rule at every site.

### 3. Sweep the sites (per the reference inventory)

Two mechanical families plus the early-return family:

- **163 `.ok_or(OWNER_NOT_LOADED_MSG)` sites** — all use the same guard
  variable `g` (verified: 152 `g.<field>` occurrences, zero inline-`lock()`
  sites), covering both `g.<handle>.clone().ok_or(...)` and
  `g.dm_self_owner.ok_or(...)` (Copy, no clone). Replace with
  `.ok_or_else(|| g.owner_not_loaded_msg())`. `g` (a `MutexGuard<NodeState>`)
  auto-derefs to `NodeState`, so the method resolves at every site.
- **1 `.ok_or_else(|| OWNER_NOT_LOADED_MSG.to_string())` site** (`lib.rs:33025`,
  guard `g` held) → `.ok_or_else(|| g.owner_not_loaded_msg().to_string())`.
- **11 `return Err(OWNER_NOT_LOADED_MSG.into())` sites** (guard released) →
  capture `not_loaded_msg = g.owner_not_loaded_msg()` in the extraction block
  and `return Err(not_loaded_msg.into())` (§2b). Two of these are twins
  (`redeem_friend_token_impl` / `add_friend_by_key_with_origin`, identical
  extraction blocks); one (`accept_friend_request_impl`) has two returns off one
  block sharing a single capture; two use an inline `state.lock()?.field.clone()`
  temporary, rebound to a named guard so the message can be captured.
  `set_friend_nickname` additionally splits its combined `crdt_state`/`path`
  check so a missing `connectivity_settings_path` returns
  `"connectivity_settings_path missing"`, not the owner-not-loaded message
  (CodeRabbit).

## Data flow (unchanged shape)

Guard reads `g.<handle>`; when `None`, `ok_or_else` consults `g.thread`
(already under the held `NodeState` lock) and returns the classified
`&'static str`; `?` coerces `&'static str → String` for Tauri commands exactly
as `ok_or(OWNER_NOT_LOADED_MSG)` did. No new state, no disk I/O, no wire-format
or signature change, no async, and no extra lock — the early-return sites
classify from the message captured in their existing extraction lock (§2b), so
`thread` and the handle are always read from the same snapshot.

## Borrow / type notes

- `g.<handle>.clone().ok_or_else(|| g.owner_not_loaded_msg())`: `g.<handle>`
  and the closure both borrow `*g` **immutably**; the `.clone()` borrow ends
  before `ok_or_else` runs, and the closure is invoked synchronously within the
  same statement while `g` is alive. Multiple shared borrows — compiles.
- Err type stays `&'static str`; `?` behavior is byte-for-byte the same as the
  current constant-based sites.

## Error handling / fail-open semantics

- The classifier is a pure field read under the already-held guard lock; it
  cannot itself fail.
- **Both** messages are non-destructive: the worst outcome of ever misjudging
  the two states is telling the user to "try again" or that "no identity is set
  up" — never to destroy anything. There is no data-loss path introduced.

## Testing

Add to `lib.rs`'s existing `#[cfg(test)] mod tests`:

1. **Discrimination — still starting.** `NodeState::default()` (thread `None`)
   ⇒ `owner_not_loaded_msg() == OWNER_STILL_STARTING_MSG`.
2. **Discrimination — no identity.**
   `NodeState { thread: Some(std::thread::spawn(|| {})), ..Default::default() }`
   (thread `Some`) ⇒ `owner_not_loaded_msg() == OWNER_NO_IDENTITY_MSG`. The
   `|| {}` thread completes immediately; the handle drops with the `NodeState`
   at end of test (a harmless detach — `is_some()` reads the `Option`, not
   thread liveness). Struct-update literal accesses private fields, which the
   crate-root test module can do.
3. **Canary — no destructive verb.** Assert neither `OWNER_STILL_STARTING_MSG`
   nor `OWNER_NO_IDENTITY_MSG` contains `"recreate"` **or** `"restart the app"`
   (both banned phrases). Pins the fix so a future edit cannot silently
   reintroduce the destructive advice. A companion source-scan test
   (`no_destructive_recreate_identity_advice_in_lib_rs`) asserts the full
   pre-fix sentence is absent from `lib.rs` entirely.

4. **Guard-site behavior (existing tests, retargeted).** The node-state guards
   resolve to the still-starting message on a
   non-running node, so the existing pre-node assertions become the regression
   coverage for the sweep:
   - `lib.rs:74768`, `:74778` (`get_community_presence` /
     `subscribe_community_presence` against `mock_app_with_default_node_state()`,
     `thread` `None`) → `assert_eq!(err, OWNER_STILL_STARTING_MSG)`.
   - `rpc.rs:2584`, `:2645`, `:2670` (`test_state()` is a non-running node) →
     `crate::OWNER_STILL_STARTING_MSG`. The "all these verbs share one string"
     invariant still holds — they all classify to the still-starting message
     pre-node.

The early-return sites need no separate unit test: they capture the same
`owner_not_loaded_msg()` the method tests (1–2) already cover, from the same
locked snapshot as the handle they gate on.

Gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets
--features test-fixtures --no-deps -- -D warnings`; final validation is the
full-workspace `cd src-tauri && cargo nextest run --locked --workspace
--all-targets --features test-fixtures` (NOT `scripts/test-select`, per house
rules). `lib.rs` is the crate root, so a lib change relinks the integration
binaries — iterate with `-p harmony-app --lib`, full sweep once before PR.

## Out of scope

- Disk-based split of "never minted" vs "present-but-failed-to-load" in the
  no-identity branch (option C — declined).
- Naming the internal handle (`crdt_state`, …) in the user-facing text — that
  is operator-facing, and ZEB-855 owns uniform reject/guard-site observability.
- Any change to `owner_loaded.rs::require_owner_loaded` / `OwnerLoadError`
  (already non-destructive; new code uses it).
- Any change to `Hlc`, the atomic install block, or node lifecycle.
