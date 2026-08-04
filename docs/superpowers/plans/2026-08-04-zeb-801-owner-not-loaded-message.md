# ZEB-801 owner-not-loaded message Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop every owner-derived-handle guard from advising the destructive "recreate identity" when the node is merely still starting, by replacing the single constant with a node-state classifier and sweeping all references.

**Architecture:** Retire `OWNER_NOT_LOADED_MSG`; add two non-destructive `&'static str` constants and classify between them on `node_is_running()` — via a `NodeState` method for guard-held sites and a companion re-locking free function for guard-released early-returns. Sweep all 175 production references + 5 test asserts + 4 comments. Single file (`src-tauri/src/lib.rs`) plus 3 test-string references in `src-tauri/src/api/rpc.rs`.

**Tech Stack:** Rust, `std::sync::Mutex<NodeState>`, Tauri IPC commands, `cargo nextest`.

## Global Constraints

- **Spec:** `docs/superpowers/specs/2026-08-04-zeb-801-owner-not-loaded-message-design.md`.
- **Message copy (verbatim, em-dash `—` U+2014):**
  - `OWNER_STILL_STARTING_MSG` = `Owner identity not loaded — the app is still starting. Try again in a moment.`
  - `OWNER_NO_IDENTITY_MSG` = `Owner identity not loaded — no identity is set up on this device yet.`
- **No message may contain "recreate" or "restart the app".** Pinned by a canary test.
- **Classification rule:** at a guard whose handle is `None` — `node_is_running()` true ⇒ `OWNER_NO_IDENTITY_MSG`; false ⇒ `OWNER_STILL_STARTING_MSG`.
- **Completeness check:** after Task 2, `grep -rn OWNER_NOT_LOADED_MSG src/` (run from `src-tauri/`) returns nothing.
- **Cargo commands run from `src-tauri/`.** `--locked` and `--features test-fixtures` are load-bearing.
- **`lib.rs` is the crate root** — a change relinks ~97 integration binaries (~50 min) on a full run. Iterate with `-p harmony-app --lib`; run the full `--workspace --all-targets` sweep exactly once, at the end of Task 2. Never use `scripts/test-select` for the final gate.
- **No behavior/signature/wire change** beyond the error strings.

---

## File Structure

- `src-tauri/src/lib.rs` (crate root) — owns `NodeState`, `node_is_running()` (`:1740`), the retired `OWNER_NOT_LOADED_MSG` (`:2400`), all 175 production guard sites, and the crate-root `#[cfg(test)] mod tests` (`:70916`). All new code and all but 3 references live here.
- `src-tauri/src/api/rpc.rs` — 3 pre-node parity test asserts (`:2584, :2645, :2670`) + 1 comment (`:2550`) referencing the constant by `crate::` path.

---

## Task 1: New constants + classifier method + tests + primary `.ok_or` sweep

Adds the two constants and the `NodeState::owner_not_loaded_msg` method, proves the classification with unit tests, and immediately sweeps the 163 `.ok_or(OWNER_NOT_LOADED_MSG)` sites so the method is used in production (no dead-code warning). The old constant stays defined (still referenced by the 12 remaining production sites + 5 asserts + 4 comments), so the crate still compiles.

**Files:**
- Modify: `src-tauri/src/lib.rs` — add constants near `:2400`; add method in the `impl NodeState` block near `node_is_running` (`:1740–1742`); add tests in `mod tests` (`:70916`); sweep the 163 `.ok_or` sites (throughout).

**Interfaces:**
- Produces: `pub(crate) const OWNER_STILL_STARTING_MSG: &str`, `pub(crate) const OWNER_NO_IDENTITY_MSG: &str`, and `fn owner_not_loaded_msg(&self) -> &'static str` on `NodeState` (private; crate-root guard sites and tests can call it).
- Consumes: `NodeState::node_is_running(&self) -> bool` (`:1740`), `NodeState::default()` (`:1982`), `NodeState.thread: Option<thread::JoinHandle<()>>` (`:772`).

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` at `src-tauri/src/lib.rs` (the module already does `use super::*;` — the existing `assert_eq!(err, OWNER_NOT_LOADED_MSG)` at `:74768` proves crate-root items are in scope):

```rust
#[test]
fn owner_not_loaded_msg_reports_still_starting_when_node_not_running() {
    // A default NodeState has `thread: None` → node not running → an absent
    // owner handle means boot hasn't reached the install point, so the
    // message must be the non-destructive still-starting one.
    let ns = NodeState::default();
    assert!(!ns.node_is_running());
    assert_eq!(ns.owner_not_loaded_msg(), OWNER_STILL_STARTING_MSG);
}

#[test]
fn owner_not_loaded_msg_reports_no_identity_when_node_running() {
    // A running node (thread present) with owner handles absent means the
    // node is up but no owner identity is loaded (pre-mint / absent). The
    // `|| {}` thread completes immediately; the handle drops with the
    // NodeState at end of test (NodeState has no Drop impl).
    let ns = NodeState {
        thread: Some(std::thread::spawn(|| {})),
        ..Default::default()
    };
    assert!(ns.node_is_running());
    assert_eq!(ns.owner_not_loaded_msg(), OWNER_NO_IDENTITY_MSG);
}

#[test]
fn owner_not_loaded_msgs_never_advise_recreate() {
    // ZEB-801 canary: the destructive "recreate identity" / "restart the app"
    // advice must never reappear at any owner-not-loaded guard.
    for msg in [OWNER_STILL_STARTING_MSG, OWNER_NO_IDENTITY_MSG] {
        let low = msg.to_lowercase();
        assert!(!low.contains("recreate"), "destructive advice in: {msg}");
        assert!(!low.contains("restart the app"), "destructive advice in: {msg}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail (compile error — undefined names)**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(owner_not_loaded_msg)'`
Expected: FAIL — `cannot find value OWNER_STILL_STARTING_MSG` / `no method named owner_not_loaded_msg` (does not compile).

- [ ] **Step 3: Add the two constants**

In `src-tauri/src/lib.rs`, directly **above** the existing `const OWNER_NOT_LOADED_MSG` (`:2400`, keep it for now), insert:

```rust
// ZEB-801: shown when an owner-derived handle is absent because the node has
// not finished starting (`!node_is_running()`) — the common case.
pub(crate) const OWNER_STILL_STARTING_MSG: &str =
    "Owner identity not loaded — the app is still starting. Try again in a moment.";
// ZEB-801: shown when the node IS running but no owner identity is loaded
// (pre-mint / absent). Non-destructive — never advises recreating identity.
pub(crate) const OWNER_NO_IDENTITY_MSG: &str =
    "Owner identity not loaded — no identity is set up on this device yet.";
```

- [ ] **Step 4: Add the classifier method**

In `src-tauri/src/lib.rs`, in the `impl NodeState` block, immediately after `node_is_running` (`:1740–1742`), insert:

```rust
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd src-tauri && cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(owner_not_loaded_msg)'`
Expected: PASS (3 tests).

- [ ] **Step 6: Sweep the 163 `.ok_or(OWNER_NOT_LOADED_MSG)` sites**

Every one uses the guard variable `g`. Global replace across `src-tauri/src/lib.rs` (Edit with `replace_all`, or the sed below):

Replace exact substring `.ok_or(OWNER_NOT_LOADED_MSG)` → `.ok_or_else(|| g.owner_not_loaded_msg())`

```bash
cd src-tauri && sed -i '' 's/\.ok_or(OWNER_NOT_LOADED_MSG)/.ok_or_else(|| g.owner_not_loaded_msg())/g' src/lib.rs
# Verify count: 163 replaced, and the ONLY remaining `.ok_or(OWNER_NOT_LOADED_MSG)` occurrences are zero
grep -c 'ok_or(OWNER_NOT_LOADED_MSG)' src/lib.rs   # expect 0
grep -c 'g.owner_not_loaded_msg()' src/lib.rs       # expect 163
```

- [ ] **Step 7: Verify compile + the swept sites + gates**

Run:
```bash
cd src-tauri && cargo fmt --all
cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(owner_not_loaded_msg)'
```
Expected: clippy clean (the method is now used at 163 production sites and by tests → no dead code; the old constant is still used by the remaining 12 sites + asserts → no dead-code warning); 3 tests PASS.

- [ ] **Step 8: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(ZEB-801): classify owner-not-loaded message; sweep .ok_or guard sites

Add OWNER_STILL_STARTING_MSG / OWNER_NO_IDENTITY_MSG and
NodeState::owner_not_loaded_msg(), classifying on node_is_running(). Sweep the
163 .ok_or(OWNER_NOT_LOADED_MSG) sites to .ok_or_else(|| g.owner_not_loaded_msg()).
Old constant retained until Task 2 finishes the remaining references.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
)"
```

---

## Task 2: Remaining sweep + delete old constant

> **Superseded during PR #603 convergence:** the re-locking free function
> `owner_not_loaded_msg_locked` described below was replaced by a **same-locked-
> snapshot capture** at each early-return (CodeRabbit flagged the re-lock as a
> `thread` TOCTOU). See the design spec §2b for the shipped approach; the steps
> below are kept as the original recipe. `set_friend_nickname` additionally
> splits its `crdt_state`/`path` check so a missing settings path is not reported
> as owner-not-loaded.

Finishes every remaining reference: the 1 `.ok_or_else(.to_string())` site, the 11 `return Err(...into())` early-returns, the 5 test asserts, the 4 comments, and deletes the old constant. Ends with `grep` returning nothing and the full suite green.

**Files:**
- Modify: `src-tauri/src/lib.rs` — add free fn; sweep the 12 remaining production sites; update 2 asserts + 3 comments; delete old const.
- Modify: `src-tauri/src/api/rpc.rs` — update 3 asserts + 1 comment.

**Interfaces:**
- Consumes: `NodeState::owner_not_loaded_msg` and `OWNER_STILL_STARTING_MSG` (from Task 1).
- Produces: `fn owner_not_loaded_msg_locked(state: &std::sync::Mutex<NodeState>) -> &'static str` (private, crate-root).

- [ ] **Step 1: Add the companion free function**

In `src-tauri/src/lib.rs`, immediately **below** the two new constants (added in Task 1, near `:2400`), insert:

```rust
/// ZEB-801: same classification as `NodeState::owner_not_loaded_msg`, for
/// error-path early-returns where the guard has already been released and only
/// the `Mutex` is in scope. Re-locks (cold path); a poisoned lock falls back
/// to the still-starting message (non-destructive).
fn owner_not_loaded_msg_locked(state: &std::sync::Mutex<NodeState>) -> &'static str {
    state
        .lock()
        .map(|g| g.owner_not_loaded_msg())
        .unwrap_or(OWNER_STILL_STARTING_MSG)
}
```

- [ ] **Step 2: Sweep the 1 `.ok_or_else(.to_string())` site (`:33025`, guard `g` held)**

Replace exact substring `.ok_or_else(|| OWNER_NOT_LOADED_MSG.to_string())` → `.ok_or_else(|| g.owner_not_loaded_msg().to_string())`

```bash
cd src-tauri && sed -i '' 's/\.ok_or_else(|| OWNER_NOT_LOADED_MSG\.to_string())/.ok_or_else(|| g.owner_not_loaded_msg().to_string())/g' src/lib.rs
grep -c 'OWNER_NOT_LOADED_MSG.to_string()' src/lib.rs   # expect 0
```

- [ ] **Step 3: Sweep the 11 `return Err(OWNER_NOT_LOADED_MSG.into())` early-returns**

All 11 sit in functions whose lock guard is already dropped and whose state local is named `state` (either `&std::sync::Mutex<NodeState>` or `tauri::State<'_, Mutex<NodeState>>`). Passing `&state` deref-coerces to `&Mutex<NodeState>` in **both** cases, so one uniform replacement works:

Replace exact substring `return Err(OWNER_NOT_LOADED_MSG.into())` → `return Err(owner_not_loaded_msg_locked(&state).into())`

```bash
cd src-tauri && sed -i '' 's/return Err(OWNER_NOT_LOADED_MSG\.into())/return Err(owner_not_loaded_msg_locked(\&state).into())/g' src/lib.rs
grep -c 'return Err(OWNER_NOT_LOADED_MSG.into())' src/lib.rs        # expect 0
grep -c 'owner_not_loaded_msg_locked(&state)' src/lib.rs           # expect 11
```

(The 11 sites: `redeem_friend_token_impl` :63411, `browse_friend_referrals` :63842, `request_introduction` :64135, `accept_friend_request_impl` :64946 + :65004, `accept_dm_invite_impl` :65238, `decline_friend_request_impl` :65090, `decline_dm_invite_impl` :65391, `set_friend_nickname` :65669, `add_friend_by_key_with_origin` :66648, `connectivity_set_identity_discoverable_impl` :62143. Site :65004 has a `friend-list-changed` emit on the preceding line — untouched.)

- [ ] **Step 4: Update the 2 lib.rs test asserts**

Both run against a non-running node (`mock_app_with_default_node_state()`, `thread: None`), so they now expect the still-starting message. Replace exact substring `assert_eq!(err, OWNER_NOT_LOADED_MSG);` → `assert_eq!(err, OWNER_STILL_STARTING_MSG);`

```bash
cd src-tauri && sed -i '' 's/assert_eq!(err, OWNER_NOT_LOADED_MSG);/assert_eq!(err, OWNER_STILL_STARTING_MSG);/g' src/lib.rs
grep -c 'assert_eq!(err, OWNER_NOT_LOADED_MSG)' src/lib.rs   # expect 0
```

- [ ] **Step 5: Update the 3 rpc.rs test asserts**

`test_state()` is a non-running node, so the pre-node owner IPCs classify to the still-starting message. Replace exact substring `crate::OWNER_NOT_LOADED_MSG` → `crate::OWNER_STILL_STARTING_MSG` in `src/api/rpc.rs` (hits the 3 code refs at :2584/:2645/:2670; the :2550 comment says `OWNER_NOT_LOADED_MSG` without `crate::` and is handled in Step 6):

```bash
cd src-tauri && sed -i '' 's/crate::OWNER_NOT_LOADED_MSG/crate::OWNER_STILL_STARTING_MSG/g' src/api/rpc.rs
grep -c 'crate::OWNER_NOT_LOADED_MSG' src/api/rpc.rs   # expect 0
```

- [ ] **Step 6: Update the 4 comments**

Edit each (distinct surrounding text) in `src-tauri/src/lib.rs` and `src-tauri/src/api/rpc.rs`:

- `lib.rs:1753`: `// the OWNER_NOT_LOADED_MSG-guarded handles; nulled on identity` → `// the owner-not-loaded-guarded handles; nulled on identity`
- `lib.rs:33406`: `/// - \`Err(OWNER_NOT_LOADED_MSG)\` / \`Err("channel_log_registry missing …")\`.` → `/// - \`Err(OWNER_STILL_STARTING_MSG)\` / \`Err("channel_log_registry missing …")\`.`
- `lib.rs:63216`: `/// Errors: \`OWNER_NOT_LOADED_MSG\` when the node isn't booted; the mint /` → `/// Errors: \`OWNER_STILL_STARTING_MSG\` when the node isn't booted; the mint /`
- `rpc.rs:2550`: `// OWNER_NOT_LOADED_MSG (proves the seam is shared, args parsed).` → `// OWNER_STILL_STARTING_MSG (proves the seam is shared, args parsed).`

- [ ] **Step 7: Delete the old constant + its ZEB-338 comment**

In `src-tauri/src/lib.rs`, delete the retired definition (`:2397–2401` — the 3 comment lines + the `const OWNER_NOT_LOADED_MSG` and its value line):

```rust
// ZEB-338: the single honest "owner identity not loaded" message. Use this at
// owner-derived-handle guards so the phrasing can't drift between call sites.
// (Incremental adoption — applied where edited, not a blanket sweep.)
const OWNER_NOT_LOADED_MSG: &str =
    "Owner identity not loaded — please restart the app or recreate identity.";
```

- [ ] **Step 8: Completeness check — no reference remains**

Run: `cd src-tauri && grep -rn OWNER_NOT_LOADED_MSG src/`
Expected: no output (exit 1). Every reference is now the new constants / classifier.

- [ ] **Step 9: Format + lint + full-workspace test sweep (final gate)**

Run:
```bash
cd src-tauri
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```
Expected: fmt clean; clippy clean (the free fn is used at 11 sites → no dead code; old constant gone → no unused-const); full suite green — including the retargeted `get_community_presence_errs_when_owner_not_loaded` / `subscribe_community_presence_errs_when_owner_not_loaded` (lib) and the rpc pre-node parity tests, all now asserting `OWNER_STILL_STARTING_MSG`.

- [ ] **Step 10: Commit**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/lib.rs src-tauri/src/api/rpc.rs
git commit -m "$(cat <<'EOF'
feat(ZEB-801): finish sweep — free fn, early-returns, asserts, retire constant

Add owner_not_loaded_msg_locked(&Mutex<NodeState>) for guard-released
early-returns; convert the 11 return-Err sites, the .ok_or_else(.to_string())
site, the 5 pre-node test asserts, and 4 comments; delete OWNER_NOT_LOADED_MSG.
grep -rn OWNER_NOT_LOADED_MSG src/ now returns nothing.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DUvg7gyEHqXrVU85Eg2D8D
EOF
)"
```

---

## Self-Review

**1. Spec coverage:**
- Two constants (§Components 1) → Task 1 Step 3. ✓
- Classifier method on `node_is_running()` (§Components 2) → Task 1 Step 4. ✓
- Companion free fn (§Components 2b) → Task 2 Step 1. ✓
- Sweep 163 `.ok_or` (§Components 3) → Task 1 Step 6. ✓
- Sweep 1 `.ok_or_else(.to_string())` (§inventory) → Task 2 Step 2. ✓
- Sweep 11 early-returns (§inventory) → Task 2 Step 3. ✓
- 5 test asserts retargeted (§Testing 4) → Task 2 Steps 4–5. ✓
- 4 comments (§inventory) → Task 2 Step 6. ✓
- Delete constant + grep-clean (§inventory) → Task 2 Steps 7–8. ✓
- Discrimination + canary tests (§Testing 1–3) → Task 1 Step 1. ✓
- Gates (§Testing) → Task 2 Step 9. ✓

**2. Placeholder scan:** No TBD/TODO; every code + command step is concrete.

**3. Type consistency:** `owner_not_loaded_msg(&self) -> &'static str` and `owner_not_loaded_msg_locked(&std::sync::Mutex<NodeState>) -> &'static str` are used consistently; both constants are `pub(crate) const … : &str`; `.ok_or_else(|| …)` preserves the `&'static str` Err type the guards already produced.
