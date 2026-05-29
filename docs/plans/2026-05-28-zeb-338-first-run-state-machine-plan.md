# ZEB-338 First-Run State Machine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the owner-identity onboarding deadlock so a fresh harmony-client install can go from launch → create identity → land in a working main UI (and optionally auto-redeem a queued invite), shipping as v0.1.0-alpha.1.

**Architecture:** A three-state hard gate — `start_node` reports `has_owner_identity`; the frontend renders a non-dismissible `WelcomeModal` until the user mints; `mint_owner_identity` becomes a self-lifecycle IPC (stop node → mint+persist → restart node) so the user never has to "stop the node" manually. A `require_owner_loaded` helper + a 144-site error-phrasing sweep replace the misleading "node not running?" message. A deep-link invite that arrives pre-mint is queued and drained post-mint.

**Tech Stack:** Rust (Tauri 2 backend, `cargo nextest`), Svelte 5 runes (frontend, Vitest), serde camelCase IPC boundary.

**Spec:** `docs/specs/2026-05-28-zeb-338-first-run-state-machine-design.md` (commit `69a91c0`). The spec governs intent + behavior; **this plan is authoritative on the concrete code** and corrects several spec pseudocode simplifications (see "Plan corrections" below).

**Branch:** `zeb-338-first-run-state-machine` (already exists at HEAD `69a91c0`, the spec commit, off `origin/main` `c97a8bf`). Per `feedback_no_worktrees`: work in the main repo on this branch — never create a worktree.

---

## Plan corrections to spec pseudocode

The spec's pseudocode was written before the implementer verified every anchor against current source. The following corrections are **authoritative** — implement these, not the literal spec pseudocode. Each preserves the spec's observable behavior (Flows 1–6 in spec §6 all still hold).

1. **`stop_node_inner` does NOT need extraction.** A synchronous `stop_inner(state: &Mutex<NodeState>, expected_gen: Option<u64>) -> bool` already exists at `src-tauri/src/lib.rs:1125`. It is explicitly **async-context-safe** — it drives its async shutdown work on an ephemeral runtime inside `std::thread::scope` precisely so it can be called from async contexts without a nested-runtime panic. The mint IPC calls `stop_inner(&state, None)` directly. (`None` = stop unconditionally, no generation check.)

2. **Only `start_node_inner` needs extraction.** `start_node` (async, `src-tauri/src/lib.rs:1678`) currently takes `state: tauri::State<'_, Mutex<NodeState>>`. Extract its body into `pub(crate) async fn start_node_inner(endpoint: Option<String>, app: &AppHandle, state: &Mutex<NodeState>) -> Result<StartNodeResponse, String>`. The `#[tauri::command] start_node` wrapper becomes a one-line forwarder. The mint IPC calls `start_node_inner(None, &app, &state).await` (a `tauri::State` derefs to `&Mutex<NodeState>`). This is the **risk task (Task 3)**.

3. **Backup save flow uses a path-token, not a raw path.** `export_owner_recovery_file_to_path` takes `path_token` (a UUID from `request_export_save_path`), not a filesystem path. The frontend reuses the existing `OwnerService` wrapper: `requestExportSavePath({...})` → `pathToken` → `exportRecoveryFile(recoveryToken, pathToken, passphrase, comment)`. WelcomeModal must NOT call `invoke('export_owner_recovery_file_to_path', { pathToken: rawPath })`. (Spec §5.1 pseudocode was wrong on this point.)

4. **`onMinted` opens the existing `RedeemInviteDialog`, not a bare `invoke('redeem_invite')`.** App.svelte already routes invites through `showRedeemInvite = true` + `redeemUrl = url` (a `RedeemInviteDialog` with its own error handling). The post-mint drain sets those, not a raw invoke.

5. **The queued invite is drained at TWO sites** to close a cold-launch race between the boot IIFE (`start_node`) and the deep-link `onMount`: (a) boot, immediately after `start_node` resolves with `hasOwnerIdentity === true` (returning user clicked an invite); (b) `onMinted` (fresh user just minted). See Task 9 for the precise race analysis.

6. **`BackupReminderBanner` mounts as a fixed-position overlay in `App.svelte`** (mirroring the existing `.help-overlay` for `HelpMenuButton`), NOT inside `Layout.svelte`. `Layout.svelte` is snippet-only with no top-bar/chrome slot (confirmed: `src/lib/components/Layout.svelte` is a pure `{@render ...}` layout). Behavior is identical (banner at top of main UI); the mount point is cleaner.

7. **Namespaced localStorage keys, distinct from the legacy welcome-ack key.** The existing boot code uses `harmony.onboarding.welcomeAcknowledged` to gate the OLD welcome. The new model gates Welcome on the backend `hasOwnerIdentity` signal instead. The banner uses NEW keys so it never false-nags existing users who minted via the DevicesPanel:
   - `harmony.onboarding.recoveryArtifactBackedUp` (localStorage) — set on a successful export.
   - `harmony.onboarding.backupSkipped` (localStorage) — set when the user clicks "I accept the risk".
   - `harmony.onboarding.backupBannerDismissed` (sessionStorage) — set on banner Dismiss.
   - Banner shows iff `backupSkipped === 'true' && recoveryArtifactBackedUp !== 'true' && backupBannerDismissed !== 'true'`. (It keys on `backupSkipped`, which is ONLY set by the new skip-confirm path — so a user who minted+backed-up via DevicesPanel never sees it.)
   - The legacy `harmony.onboarding.welcomeAcknowledged` key is no longer read for welcome-gating and is left untouched (harmless orphan; no migration).

8. **WelcomeModal removes the invite-paste input** (invites now arrive via the deep-link queue, per spec Q5) but **keeps the footer** (app version + "How to submit feedback" link) from the current implementation.

---

## File structure

### New files

| Path | Responsibility |
|---|---|
| `src-tauri/src/owner_loaded.rs` | `OwnerLoadedHandles` struct, `OwnerLoadError` enum, `require_owner_loaded()` helper, `From<OwnerLoadError> for String`. Unit tests inline. |
| `src-tauri/tests/mint_owner_lifecycle.rs` | Integration tests for the self-lifecycle mint IPC. |
| `src-tauri/tests/error_phrasing_regression.rs` | Grep-guard test that the misleading phrasing never returns to `lib.rs`. |
| `src/lib/components/BackupReminderBanner.svelte` | Persistent "back up your identity" reminder shown after a skipped backup. |
| `src/lib/components/__tests__/BackupReminderBanner.test.ts` | Banner component tests. |

> **CBOR fixture note:** spec §8.3 lists a `start_node_response_v2.cbor` fixture. `StartNodeResponse` is `Serialize`-only (no `Deserialize` derive — confirmed at `lib.rs:1003`), and the IPC boundary is JSON, not CBOR. A binary CBOR fixture would be testing a serializer the type never uses. **Task 2 implements the wire-format pinning as JSON-shape assertions instead** (asserting the exact `serde_json` output incl. `hasOwnerIdentity` in camelCase). This satisfies the spec's intent (pin the wire shape, prove forward-compat) without a misleading artifact. Documented as a deviation in Task 2.

### Modified files

| Path | Changes |
|---|---|
| `src-tauri/src/lib.rs` | `StartNodeResponse` gains `has_owner_identity`; capture `has_owner_identity` in `start_node`; thread it to all response-construction sites; extract `start_node_inner`; `pub mod owner_loaded;`; phrasing sweep (144 sites). |
| `src-tauri/src/owner_commands.rs` | `mint_owner_identity` becomes self-lifecycle; drop `require_node_stopped` fast-fail; keep idempotent "already exists" guard. |
| `src/lib/types/onboarding.ts` | `StartNodeResponse` adds `hasOwnerIdentity?: boolean`. |
| `src/lib/components/WelcomeModal.svelte` | Two-pane hard gate; `onMinted` prop replaces `onDismiss`/`onJoinWithInvite`; remove Esc/backdrop/skip handlers; remove invite input; keep footer. |
| `src/lib/components/__tests__/WelcomeModal.test.ts` | Rewrite for the new contract. |
| `src/lib/deep-link-router.ts` | Add `queueInviteForPostMint` / `consumeQueuedInvite`. |
| `src/lib/__tests__/deep-link-router.test.ts` | Add queue tests. (Create file if absent.) |
| `src/App.svelte` | Boot destructures `hasOwnerIdentity`; gate Welcome on it; `onMinted` handler; deep-link routing branches on owner presence + queues; mount `BackupReminderBanner`. |
| `docs/release-process.md` | First-run smoke checklist in §3. |

---

## HARD RULES (every task enforces these)

- **Backend gates** (run from `src-tauri/`, each foreground with `timeout 600`):
  - `cargo fmt --all -- --check`
  - `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
  - `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (or scoped subset during a task; full sweep in Task 11)
- **Frontend gates** (run from repo root, via `npx` NOT pnpm): `npx tsc --noEmit` && `npx vitest run`
- **Commit BEFORE running the long gate** (per `feedback_implementer_gate_time_budget`). If any gate exceeds the 10-min wall-clock (`timeout 600`), surface `DONE_WITH_CONCERNS` rather than silently stalling.
- **Pipe exit codes lie** (`feedback_pipe_exit_codes_lie`): never trust `cargo … | tail`. Use `set -o pipefail` or check `${PIPESTATUS[0]}`.
- **Tauri IPC naming:** Rust commands `snake_case`; DTOs `#[serde(rename_all = "camelCase")]`; JS callers `camelCase`.
- **Tauri error extraction (frontend):** `const msg = e instanceof Error ? e.message : String(e);`
- **No worktrees.** Branch `zeb-338-first-run-state-machine` already exists; `git checkout` it in the main repo.
- **Baseline discipline** (`feedback_test_drift_is_our_fault` + `feedback_unrelated_test_failures`): Task 0 captures the pre-existing orphan failures (`folder_ingest::tests`, `mint::tests`, `mint_sync::tests`, `rename_content_integration` port-4242 flake, occasional `zenoh_iroh_*` timeouts). Those are NOT blocking. Any NEW failure introduced by Tasks 1–10 IS blocking.
- **macOS XprotectService** is already mitigated on this dev machine (Koya) per CLAUDE.md. If cold `cargo nextest` hangs > 10 min reappear, document in `DONE_WITH_CONCERNS`.

---

## Task 0: Pre-flight baseline (no commit)

**Purpose:** capture the exact orphan-failure baseline so later tasks can distinguish pre-existing breakage from regressions. No code changes, no commit.

- [ ] **Step 1: Confirm branch + base**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git checkout zeb-338-first-run-state-machine
git log --oneline -1   # expect 69a91c0 docs(zeb-338): first-run state machine design spec
git merge-base --is-ancestor origin/main HEAD && echo "based on origin/main ✓"
```

- [ ] **Step 2: Capture the backend test baseline** (foreground, bounded)

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb338-baseline.txt | tail -40
echo "nextest exit: ${PIPESTATUS[0]}"
```

Expected: a SMALL number of failures confined to the known orphan set (`folder_ingest`, `mint`, `mint_sync`, `rename_content_integration`, occasional `zenoh_iroh_*`). Record the exact failing test names from `/tmp/zeb338-baseline.txt`. If a failure appears OUTSIDE that set, note it — it's a pre-existing issue to flag, not something this PR introduced, but later tasks must not be blamed for it.

- [ ] **Step 3: Capture the frontend baseline**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx tsc --noEmit ; echo "tsc exit: $?"
npx vitest run 2>&1 | tail -25 ; echo "vitest exit: ${PIPESTATUS[0]}"
```

Expected: green (frontend has no known orphan failures). If anything fails here, record it as baseline.

- [ ] **Step 4: Record the phrasing-sweep target count**

```bash
cd src-tauri
grep -c "node not running?" src/lib.rs
grep -cn "missing — no owner identity?" src/lib.rs
```

Record the counts (spec says ~144 for the "node not running?" string). Task 5 will replace all of them and Task 5's regression test asserts the count returns to 0.

**No commit for Task 0.**

---

## Task 1: `owner_loaded.rs` module (helper + error type + tests)

**Files:**
- Create: `src-tauri/src/owner_loaded.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod owner_loaded;` in the module block near line 58, alphabetically adjacent to `pub mod owner_commands;`)

**Context for the implementer:** `NodeState` (defined in `src-tauri/src/lib.rs`, fields around lines 409–460) holds the owner-derived handles as `Option<…>` — they're `None` before an owner identity loads and `Some` after. The exact field names and types are visible in `create_community`'s extraction block at `lib.rs:14052-14072`. **Before writing the struct, open `lib.rs` and confirm each field's exact type** (`crdt_state`, `hlc_tracker`, `dm_device_id`, `dm_self_owner`, `community_registry`, `community_adapter_request_tx`, `channel_log_registry`, `dm_outbox`, `generation`). The types below match the spec but MUST be verified against the live `NodeState` declaration — a generic-arg mismatch will fail to compile.

- [ ] **Step 1: Write the module with the helper + error type**

Create `src-tauri/src/owner_loaded.rs`:

```rust
//! ZEB-338: owner-identity-loaded precondition helper.
//!
//! `start_node` tolerates the absence of an owner identity (pre-mint),
//! leaving the owner-derived `NodeState` fields as `None`. Owner-touching
//! IPCs require those fields. This helper extracts all of them atomically
//! (all-or-`NotLoaded`) so new code gets one clear precondition check and
//! one honest error instead of nine ad-hoc `.ok_or("crdt_state missing …")`
//! sites with a misleading "node not running?" message.
//!
//! Migration policy (spec §4.3): this is the recommended pattern for NEW
//! owner-touching IPCs. The ~144 existing ad-hoc sites are NOT mass-migrated
//! here — they get a phrasing-only sweep in the same PR (Task 5). Existing
//! IPCs adopt this helper incrementally as they're touched for other reasons.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;

use crate::community_state_sync::{CommunityAdapterRequest, CommunitySyncRegistry};
// NOTE: verify these import paths against the actual NodeState field types in
// lib.rs before relying on them. ChannelLogRegistry is generic over the Tauri
// runtime in stop_inner (`ChannelLogRegistry<tauri::Wry>`) — match whatever
// NodeState.channel_log_registry actually stores.
use crate::community_channel_log_engine::ChannelLogRegistry;
use crate::dm_outbox::DmOutbox;
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_types::{Hlc, OwnerAddr};

/// All owner-derived handles, extracted atomically from `NodeState`.
/// Field names mirror `NodeState`'s, except `device_id`/`self_owner` which
/// drop the `dm_` prefix for readability at call sites.
pub struct OwnerLoadedHandles {
    pub crdt_state: Arc<TokioMutex<OwnerState>>,
    pub hlc_tracker: Arc<TokioMutex<BTreeMap<String, Hlc>>>,
    pub device_id: String,
    pub self_owner: OwnerAddr,
    pub community_registry: Arc<CommunitySyncRegistry>,
    pub community_adapter_request_tx: mpsc::Sender<CommunityAdapterRequest>,
    pub channel_log_registry: Arc<ChannelLogRegistry<tauri::Wry>>,
    pub dm_outbox: Arc<TokioMutex<DmOutbox>>,
    pub generation: u64,
}

#[derive(thiserror::Error, Debug)]
pub enum OwnerLoadError {
    #[error("Owner identity not loaded. The app may be restarting after a mint — try again in a moment.")]
    NotLoaded,
    #[error("NodeState lock poisoned: {0}")]
    LockPoisoned(String),
}

impl From<OwnerLoadError> for String {
    fn from(e: OwnerLoadError) -> String {
        e.to_string()
    }
}

/// Extract the owner-loaded handles, or `NotLoaded` if any is absent.
pub fn require_owner_loaded(
    state: &Mutex<crate::NodeState>,
) -> Result<OwnerLoadedHandles, OwnerLoadError> {
    let g = state
        .lock()
        .map_err(|e| OwnerLoadError::LockPoisoned(e.to_string()))?;
    Ok(OwnerLoadedHandles {
        crdt_state: g.crdt_state.clone().ok_or(OwnerLoadError::NotLoaded)?,
        hlc_tracker: g.hlc_tracker.clone().ok_or(OwnerLoadError::NotLoaded)?,
        device_id: g.dm_device_id.clone().ok_or(OwnerLoadError::NotLoaded)?,
        self_owner: g.dm_self_owner.ok_or(OwnerLoadError::NotLoaded)?,
        community_registry: g
            .community_registry
            .clone()
            .ok_or(OwnerLoadError::NotLoaded)?,
        community_adapter_request_tx: g
            .community_adapter_request_tx
            .clone()
            .ok_or(OwnerLoadError::NotLoaded)?,
        channel_log_registry: g
            .channel_log_registry
            .clone()
            .ok_or(OwnerLoadError::NotLoaded)?,
        dm_outbox: g.dm_outbox.clone().ok_or(OwnerLoadError::NotLoaded)?,
        generation: g.generation,
    })
}
```

> If `tauri::Wry` or any import path is wrong, fix it to match the live `NodeState` field declarations. The struct exists to mirror those fields exactly.

- [ ] **Step 2: Register the module**

In `src-tauri/src/lib.rs`, add to the module block (alphabetical, next to `pub mod owner_commands;` near line 58):

```rust
pub mod owner_loaded;
```

- [ ] **Step 3: Write the unit tests** (append to `owner_loaded.rs`)

The test must build a `NodeState` with all owner fields populated and assert the helper returns handles; then null each field in turn and assert `NotLoaded`. `NodeState` is large; check whether it implements `Default` or has a test constructor (`grep -n "impl Default for NodeState\|fn test_\|set_test_self_owner" src-tauri/src/lib.rs`). The existing `set_test_self_owner` (lib.rs:843) signals there's a test-construction pattern — reuse it. If there's no full `Default`, the test populates only the nine fields the helper reads, leaving the rest at their zero/None defaults via `NodeState { ..Default::default() }` or the existing test builder.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Build a NodeState with all nine owner-loaded fields populated.
    // Reuse whatever test-construction path NodeState already exposes
    // (see set_test_self_owner at lib.rs:843). The helper only reads nine
    // fields; the rest can stay at default/None.
    fn node_state_with_all_owner_fields() -> crate::NodeState {
        // IMPLEMENTER: construct via the existing test builder / Default.
        // Populate crdt_state, hlc_tracker, dm_device_id, dm_self_owner,
        // community_registry, community_adapter_request_tx,
        // channel_log_registry, dm_outbox, generation with minimal valid
        // values (Arc::new(TokioMutex::new(OwnerState::default())), an mpsc
        // channel sender from tokio::sync::mpsc::channel(1), etc.).
        unimplemented!("construct test NodeState — see set_test_self_owner pattern")
    }

    #[test]
    fn require_owner_loaded_returns_handles_when_all_some() {
        let state = Mutex::new(node_state_with_all_owner_fields());
        let handles = require_owner_loaded(&state).expect("should be loaded");
        assert_eq!(handles.generation, /* the value you set */ 0);
    }

    #[test]
    fn require_owner_loaded_returns_not_loaded_when_crdt_state_none() {
        let mut ns = node_state_with_all_owner_fields();
        ns.crdt_state = None;
        let state = Mutex::new(ns);
        assert!(matches!(
            require_owner_loaded(&state),
            Err(OwnerLoadError::NotLoaded)
        ));
    }

    // Parameterized: null each of the nine fields in turn → NotLoaded.
    // Write one test per field (crdt_state, hlc_tracker, dm_device_id,
    // dm_self_owner, community_registry, community_adapter_request_tx,
    // channel_log_registry, dm_outbox). `generation` is a plain u64 (no
    // None state) so it has no null variant.
    #[test]
    fn require_owner_loaded_not_loaded_when_dm_outbox_none() {
        let mut ns = node_state_with_all_owner_fields();
        ns.dm_outbox = None;
        let state = Mutex::new(ns);
        assert!(matches!(
            require_owner_loaded(&state),
            Err(OwnerLoadError::NotLoaded)
        ));
    }
    // … repeat for hlc_tracker, dm_device_id, dm_self_owner,
    // community_registry, community_adapter_request_tx, channel_log_registry.
}
```

> If building a full `NodeState` in a unit test proves infeasible (too many private fields, no `Default`), move these tests to `src-tauri/tests/owner_loaded_integration.rs` using whatever public construction path exists, OR gate the test builder behind `#[cfg(any(test, feature = "test-fixtures"))]`. Surface `DONE_WITH_CONCERNS` if neither works and ship the all-some + one-None case via a hand-built minimal `NodeState`.

- [ ] **Step 4: Verify compile + tests pass**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(owner_loaded)' 2>&1 | tail -20
echo "exit: ${PIPESTATUS[0]}"
```

Expected: the `owner_loaded` tests PASS.

- [ ] **Step 5: fmt + clippy (scoped) + commit**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "clippy exit: ${PIPESTATUS[0]}"
cd ..
git add src-tauri/src/owner_loaded.rs src-tauri/src/lib.rs
git commit -m "feat(zeb-338): require_owner_loaded helper + OwnerLoadError"
```

---

## Task 2: `StartNodeResponse.has_owner_identity` + wire-shape tests + TS type

**Files:**
- Modify: `src-tauri/src/lib.rs` (struct at line 1003-1008; capture at ~2150; three construction sites at ~4916, ~4931, ~5263)
- Modify: `src/lib/types/onboarding.ts`

**Context:** `StartNodeResponse` is `Serialize`-only with `#[serde(rename_all = "camelCase")]`. `owner_loaded` is computed at `lib.rs:2146-2149` (`load_owner_state(...)?`). There are three sites that construct a `StartNodeResponse` (the three early/late return paths in `start_node`); each must set the new field. Find them all: `grep -n "StartNodeResponse {" src-tauri/src/lib.rs`.

- [ ] **Step 1: Write the failing wire-shape test**

Append to the existing `#[cfg(test)] mod` in `lib.rs` that covers IPC DTOs, or create a small inline test module near the struct. (Use `serde_json` — already a dependency.)

```rust
#[cfg(test)]
mod start_node_response_wire_tests {
    use super::StartNodeResponse;

    #[test]
    fn start_node_response_serializes_has_owner_identity_in_camel_case() {
        let r = StartNodeResponse {
            node_addr: "iroh:abc".to_string(),
            freshly_created: true,
            has_owner_identity: false,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["nodeAddr"], "iroh:abc");
        assert_eq!(json["freshlyCreated"], true);
        assert_eq!(json["hasOwnerIdentity"], false);
        // Exactly three keys — no snake_case leakage, no extra fields.
        assert_eq!(json.as_object().unwrap().len(), 3);
    }

    #[test]
    fn start_node_response_has_owner_identity_true_shape() {
        let r = StartNodeResponse {
            node_addr: "iroh:xyz".to_string(),
            freshly_created: false,
            has_owner_identity: true,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["hasOwnerIdentity"], true);
    }
}
```

> **Deviation from spec §8.3 (documented):** the spec called for a binary `start_node_response_v2.cbor` fixture. `StartNodeResponse` is `Serialize`-only and crosses the IPC boundary as JSON, not CBOR — a CBOR fixture would test an unused path. These JSON-shape assertions pin the wire shape and the camelCase contract, satisfying the spec's intent (the integration tests `…_true_when_owner_loaded` / `…_false_when_no_owner` from spec §8.1 land in Task 4's `mint_owner_lifecycle.rs` where a real `start_node` is driven). No `.cbor` file is created.

- [ ] **Step 2: Run it to verify it fails**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(start_node_response)' 2>&1 | tail -20
echo "exit: ${PIPESTATUS[0]}"
```

Expected: FAIL to compile (`has_owner_identity` field doesn't exist yet).

- [ ] **Step 3: Add the field to the struct**

In `src-tauri/src/lib.rs` (struct at ~1003):

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartNodeResponse {
    pub node_addr: String,
    pub freshly_created: bool,
    /// ZEB-338: true iff an owner identity (owner_state.cbor) loaded during
    /// this start_node call. Frontend hard-gates the WelcomeModal on this:
    /// `showWelcomeModal = !hasOwnerIdentity`. Forward-compat: frontend
    /// treats a missing field as `false` (older backend → show onboarding).
    pub has_owner_identity: bool,
}
```

- [ ] **Step 4: Capture `has_owner_identity` and thread it to all three sites**

Near `lib.rs:2146` where `owner_loaded` is computed:

```rust
let owner_loaded = crate::owner_state::load_owner_state(
    &identity_dir,
    crate::identity::KeychainStore::new().ok(),
)?;
// ZEB-338: snapshot before owner_loaded is moved/destructured downstream.
let has_owner_identity = owner_loaded.is_some();
```

> **Verify `owner_loaded` isn't moved before this line.** If the existing code does `if let Some(seed) = owner_loaded { … }` immediately, compute `has_owner_identity` BEFORE that block. Then add `has_owner_identity` to each of the three `StartNodeResponse { … }` literals (found via grep in the Files note). Each currently has `node_addr` + `freshly_created`; add `has_owner_identity`.

- [ ] **Step 5: Update the TS type**

In `src/lib/types/onboarding.ts`, extend the interface:

```ts
/** Returned by `invoke('start_node', { endpoint })`. */
export interface StartNodeResponse {
  /** Self iroh node address (e.g. "iroh:..."). */
  nodeAddr: string;
  /**
   * True when the keychain *device* identity was minted during this
   * `start_node` call. Device-level freshness — NOT owner-identity state.
   * Forward-compat: treat missing/undefined as `false`.
   */
  freshlyCreated?: boolean;
  /**
   * ZEB-338: true iff an owner identity loaded during this start_node call.
   * The frontend hard-gates the WelcomeModal on this:
   * `showWelcomeModal = !hasOwnerIdentity`.
   * Forward-compat: treat missing/undefined as `false` (older backend mid-
   * deploy → show onboarding, the safe default).
   */
  hasOwnerIdentity?: boolean;
}
```

- [ ] **Step 6: Verify tests pass + tsc**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(start_node_response)' 2>&1 | tail -20
echo "rust exit: ${PIPESTATUS[0]}"
cd ..
npx tsc --noEmit ; echo "tsc exit: $?"
```

Expected: rust tests PASS; tsc clean.

- [ ] **Step 7: fmt + commit**

```bash
cd src-tauri && cargo fmt --all && cd ..
git add src-tauri/src/lib.rs src/lib/types/onboarding.ts
git commit -m "feat(zeb-338): StartNodeResponse.has_owner_identity + camelCase wire tests"
```

---

## Task 3: Extract `start_node_inner` (RISK TASK — read the whole task first)

**Files:**
- Modify: `src-tauri/src/lib.rs` (`start_node` at 1678–~5283)

**Why this is the risk task (spec §4.2):** `start_node` is ~3600 lines, threads state through closures, channel construction, runtime spawns, and error-path cleanup. The goal is to make its body callable from a second IPC (`mint_owner_identity`) without duplicating it. The extraction is mechanical IF the body only touches `state` via `.lock()` (works on `&Mutex<NodeState>`) and `app` via methods that take `&self`.

**Contingency (spec §4.2):** if the extraction can't be made to compile within ~200 LOC of mechanical change (lifetime/`Send` knots across `.await`), STOP, revert the extraction, and surface `DONE_WITH_CONCERNS` recommending the hot-load re-scope (populate owner-derived `NodeState` fields on the running node directly in the mint IPC, avoiding any `start_node` change). Do NOT spend hours fighting the borrow checker.

- [ ] **Step 1: Inspect the current signature + how `state`/`app` are used**

```bash
cd src-tauri
sed -n '1677,1700p' src/lib.rs
# Scan for any use of `state` as a tauri::State-specific API (not just .lock()):
grep -n "state\.\(manage\|try_state\|app_handle\)\|state\.inner()" src/lib.rs | sed -n '1,40p'
```

If `state` is only ever used as `state.lock()` (and maybe `&state` passed to helpers taking `&Mutex<NodeState>` like `stop_inner`), the extraction is safe. If it's used as a `tauri::State`-specific method, the inner fn must take `&Mutex<NodeState>` and the wrapper passes `&state`.

- [ ] **Step 2: Introduce the inner function (keep the wrapper)**

Rename the current `async fn start_node(...)` body into a new `pub(crate)` inner fn and make the command a forwarder:

```rust
#[tauri::command]
async fn start_node(
    endpoint: Option<String>,
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<StartNodeResponse, String> {
    // ZEB-338: thin forwarder. Real logic lives in start_node_inner so the
    // self-lifecycle mint IPC (owner_commands::mint_owner_identity) can
    // restart the node after writing owner_state.cbor without duplicating
    // ~3600 lines. `tauri::State` derefs to `&Mutex<NodeState>`.
    start_node_inner(endpoint, &app, &state).await
}

/// ZEB-338: extracted body of `start_node`. Callable from any IPC that holds
/// an `&AppHandle` and `&Mutex<NodeState>` (the command wrapper above, and
/// `mint_owner_identity`'s node-restart phase).
pub(crate) async fn start_node_inner(
    endpoint: Option<String>,
    app: &AppHandle,
    state: &Mutex<NodeState>,
) -> Result<StartNodeResponse, String> {
    // … the entire existing start_node body, verbatim …
}
```

Mechanical fixes the implementer will likely need inside the body:
- Replace any `app.clone()` — still valid on `&AppHandle` (returns an owned `AppHandle`).
- Any `let app = app;` shadowing or moves: clone from the `&AppHandle` where an owned handle is needed (e.g., moved into a spawned task closure: `let app_for_task = app.clone();`).
- `state` used as `&Mutex<NodeState>`: `state.lock()` works unchanged. If the body did `&state` to get `&Mutex`, that's now just `state`. If it called a helper `foo(&state)` expecting `&Mutex<NodeState>`, that's now `foo(state)`.

- [ ] **Step 3: Compile-check (this is where the risk materializes)**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -40
echo "check exit: ${PIPESTATUS[0]}"
```

Expected: clean. If there are < ~10 mechanical errors (moved `app`, `&state` vs `state`), fix them and re-check. **If errors are lifetime/`Send`-across-`.await` knots that don't yield to mechanical fixes within ~200 LOC, invoke the contingency:** `git checkout src-tauri/src/lib.rs` to revert, then report `DONE_WITH_CONCERNS` with the specific compiler errors and a recommendation to re-scope Task 4 to hot-load.

- [ ] **Step 4: Confirm existing start_node behavior is unchanged**

```bash
cd src-tauri
set -o pipefail
# Run any tests that exercise start_node + the broad suite for the touched file.
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(start_node) or test(node_addr)' 2>&1 | tail -20
echo "exit: ${PIPESTATUS[0]}"
```

Expected: no NEW failures vs Task 0 baseline.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "clippy exit: ${PIPESTATUS[0]}"
cd ..
git add src-tauri/src/lib.rs
git commit -m "refactor(zeb-338): extract start_node_inner for reuse by mint IPC"
```

---

## Task 4: `mint_owner_identity` self-lifecycle + integration tests

**Files:**
- Modify: `src-tauri/src/owner_commands.rs` (`mint_owner_identity` at 158–217; drop `require_node_stopped` use at 166)
- Create: `src-tauri/tests/mint_owner_lifecycle.rs`

**Context:** the current `mint_owner_identity` (owner_commands.rs:158) fast-fails via `require_node_stopped` (line 166), then mints inside `run_blocking`. The rewrite removes the fast-fail, stops the node first (`stop_inner`, async-safe — correction #1), mints+persists (existing `run_blocking` body, lines 169-215, unchanged), then restarts via `start_node_inner` (correction #2). `stop_inner` and `start_node_inner` live in `lib.rs` — reference them as `crate::stop_inner` / `crate::start_node_inner`. They may need `pub(crate)` visibility; `stop_inner` is currently private `fn stop_inner` (lib.rs:1125) — change to `pub(crate) fn stop_inner` as part of this task.

- [ ] **Step 1: Make `stop_inner` reachable from `owner_commands`**

In `src-tauri/src/lib.rs:1125`, change `fn stop_inner` → `pub(crate) fn stop_inner`.

- [ ] **Step 2: Rewrite `mint_owner_identity`**

Replace the body of `mint_owner_identity` (owner_commands.rs:158-217). Keep all existing imports; add `tauri::AppHandle` to the signature.

```rust
#[tauri::command]
pub async fn mint_owner_identity(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<MintIpcResult, String> {
    let identity_dir = resolve_identity_dir()?;

    // Idempotent failure if already minted — existing guard, kept. The hard
    // gate (frontend) means this is normally unreachable, but a race or a
    // direct DevicesPanel call could hit it.
    if identity_dir.join("owner_state.cbor").exists() {
        return Err(
            "Owner identity already exists on this device. Wipe via Settings to re-mint."
                .to_string(),
        );
    }

    // ── Phase 1: stop the node ──────────────────────────────────────────
    // ZEB-338: mint takes responsibility for the node lifecycle so the user
    // never has to "stop the node" by hand (the old require_node_stopped
    // dead-end). `stop_inner` is async-context-safe — it drives its async
    // shutdown on an ephemeral runtime inside std::thread::scope, so calling
    // it from this async fn does NOT panic with a nested runtime.
    // `None` = stop unconditionally (no generation check).
    crate::stop_inner(&state, None);

    // ── Phase 2: mint + persist ─────────────────────────────────────────
    // Held under OWNER_STATE_WRITE_LOCK to serialize concurrent mints.
    // metadata-before-irreversible-write note (feedback_metadata_before_
    // irreversible_write): the cbor + keychain write here IS the desired
    // irreversible write. If Phase 3 (restart) fails afterward we do NOT roll
    // it back — rolling back would lose the user's freshly minted identity
    // (spec §7.1). The cost of a failed restart is a manual relaunch, which
    // is strictly better than identity loss.
    let mint_result = run_blocking(move || {
        let _owner_write_guard =
            OWNER_STATE_WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Re-check under the lock (TOCTOU: another caller could have minted
        // between the outer check and acquiring the lock).
        if identity_dir.join("owner_state.cbor").exists() {
            return Err(
                "Owner identity already exists on this device. Wipe via Settings to re-mint."
                    .to_string(),
            );
        }
        let MintResult {
            state: owner_state,
            recovery_artifact,
            device_signing_key,
        } = mint_owner(now_unix()).map_err(|e| format!("mint_owner: {e}"))?;
        let master_seed: Zeroizing<[u8; 32]> = Zeroizing::new(*recovery_artifact.as_bytes());
        save_owner_state_atomic(
            &identity_dir,
            &owner_state,
            &device_signing_key,
            Some(&*master_seed),
            KeychainStore::new().ok(),
        )?;
        let token = insert_token(master_seed.clone());
        let loaded = LoadedOwnerState {
            state: owner_state,
            device_signing_key,
            master_seed: Some(master_seed),
        };
        Ok(MintIpcResult {
            state: build_owner_state_view(&loaded, "this device".to_string()),
            recovery_token: token.to_string(),
        })
    })
    .await?;

    // ── Phase 3: restart the node — now loads owner_state.cbor ──────────
    crate::start_node_inner(None, &app, &state)
        .await
        .map_err(|e| format!("Node restart failed after mint: {e}"))?;

    Ok(mint_result)
}
```

- [ ] **Step 3: Remove the now-dead `require_node_stopped` + `ERR_NODE_RUNNING` if unused**

```bash
cd src-tauri
grep -n "require_node_stopped\|ERR_NODE_RUNNING" src/*.rs
```

If `mint_owner_identity` was the ONLY caller of `require_node_stopped`, delete both `require_node_stopped` (owner_commands.rs:71-77) and the `ERR_NODE_RUNNING` const (45-46) to avoid `dead_code` clippy failures. If other callers remain, leave them. (Verify with the grep — there may be other mint paths.)

- [ ] **Step 4: Write the integration tests**

Create `src-tauri/tests/mint_owner_lifecycle.rs`. These need to drive the real IPC against a tempdir identity dir. The identity dir is resolved from `$HOME`/`$USERPROFILE` (`identity.rs:705-710`), so the tests set `HOME` to a tempdir. Keychain may be unavailable in CI — `save_owner_state_atomic` is given `KeychainStore::new().ok()` (None-tolerant), so the cbor write is the assertable artifact.

```rust
//! ZEB-338: self-lifecycle mint IPC integration tests.
//!
//! These exercise mint_owner_identity end-to-end against a tempdir identity
//! directory (HOME override). Keychain may be absent in CI; the cbor file is
//! the load-bearing assertion.

use std::sync::Mutex;
use tempfile::TempDir;

// NOTE: mint_owner_identity is a #[tauri::command] taking tauri::State +
// AppHandle. Driving it without a full Tauri runtime is awkward. The
// IMPLEMENTER chooses one of:
//   (a) If owner_commands exposes (or can expose) a testable inner fn that
//       takes &Mutex<NodeState> instead of tauri::State, call that. Prefer
//       adding `mint_owner_identity_inner` mirroring start_node_inner, and
//       have the command forward to it — symmetric, testable.
//   (b) Use tauri::test::mock_builder / mock_app (the crate already enables
//       the tauri "test" feature in dev-deps — see Cargo.toml) to construct
//       an App with a managed NodeState, then invoke the command.
// Option (a) is cleaner and avoids a full mock app. RECOMMENDED: extract
// mint_owner_identity_inner(app: &AppHandle, state: &Mutex<NodeState>).

#[test]
fn mint_owner_identity_writes_cbor() {
    let home = TempDir::new().unwrap();
    // SAFETY/caveat: setting HOME affects the process; these tests must run
    // serially. Use a serial guard (the workspace already uses serial_test in
    // some places — check `grep -rn serial_test src-tauri/Cargo.toml`). If
    // present, annotate with #[serial]. If not, keep these in a dedicated
    // test binary (this file) so they don't interleave with HOME-sensitive
    // tests elsewhere.
    std::env::set_var("HOME", home.path());
    std::env::set_var("USERPROFILE", home.path());

    // … construct NodeState + AppHandle (or call the inner fn), invoke mint,
    // assert home.path().join(".harmony/owner_state.cbor").exists().
    // IMPLEMENTER fills in per the chosen option (a)/(b).
    unimplemented!("drive mint via inner fn or mock app; assert cbor exists")
}

#[test]
fn mint_owner_identity_idempotent_failure_when_already_exists() {
    // Mint once, then mint again; assert the second call returns the
    // "already exists" error and the first cbor is byte-identical before/after.
    unimplemented!()
}

#[test]
fn mint_owner_identity_restarts_node_with_owner_loaded() {
    // After mint, assert NodeState.crdt_state.is_some() && dm_outbox.is_some()
    // && community_registry.is_some() (the node restarted with owner loaded).
    // Requires the inner-fn path so the test holds the NodeState handle.
    unimplemented!()
}

#[test]
fn mint_owner_identity_node_restart_failure_preserves_minted_state() {
    // Inject a start_node_inner failure (e.g., a NodeState/env condition that
    // makes restart error) AFTER mint; assert the cbor IS written (no
    // rollback) and the IPC returns an error whose message starts with
    // "Node restart failed after mint:". If injecting a restart failure is
    // infeasible without invasive scaffolding, document as DONE_WITH_CONCERNS
    // and cover the no-rollback invariant via code review of the mint body
    // (the Ok(mint_result) is returned only after a successful restart, and
    // there is no rollback branch — visually verifiable).
    unimplemented!()
}
```

> **Implementer guidance:** strongly prefer adding `pub(crate) async fn mint_owner_identity_inner(app: &AppHandle, state: &Mutex<NodeState>) -> Result<MintIpcResult, String>` with the command forwarding to it (symmetric with `start_node_inner`). This makes all four tests straightforward without a mock Tauri app. If `tauri::test` mock-app is required instead, that's acceptable. Whichever path: the `restart_failure_preserves_minted_state` test is the most important (it locks the no-rollback invariant from `feedback_metadata_before_irreversible_write`) — if it can't be driven mechanically, cover it by review and note it.

- [ ] **Step 5: Run the new tests + the owner_commands suite**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures --test mint_owner_lifecycle 2>&1 | tail -25
echo "exit: ${PIPESTATUS[0]}"
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(mint)' 2>&1 | tail -25
echo "exit: ${PIPESTATUS[0]}"
```

Expected: new tests PASS; pre-existing `mint::tests` / `mint_sync::tests` orphan failures unchanged from Task 0 baseline (not introduced here).

- [ ] **Step 6: fmt + clippy + commit**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "clippy exit: ${PIPESTATUS[0]}"
cd ..
git add src-tauri/src/owner_commands.rs src-tauri/src/lib.rs src-tauri/tests/mint_owner_lifecycle.rs
git commit -m "feat(zeb-338): self-lifecycle mint_owner_identity (stop→mint→restart)"
```

---

## Task 5: Error-phrasing sweep + regression guard

**Files:**
- Modify: `src-tauri/src/lib.rs` (the 144 phrasing sites)
- Create: `src-tauri/tests/error_phrasing_regression.rs`

**Context (spec §4.3):** the misleading `"crdt_state missing — node not running?"` (and sibling `"X missing — node not running?"` / `"X missing — no owner identity?"`) strings are replaced with the honest `"Owner identity not loaded — please restart the app or recreate identity."`. Same `Result<_, String>` shape; no behavior change. Task 0 recorded the exact counts.

- [ ] **Step 1: Write the regression guard FIRST (TDD)**

Create `src-tauri/tests/error_phrasing_regression.rs`:

```rust
//! ZEB-338: guard against the misleading "node not running?" error phrasing
//! creeping back into lib.rs. The honest message is
//! "Owner identity not loaded — please restart the app or recreate identity."

#[test]
fn no_misleading_node_not_running_phrasing_in_lib_rs() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");
    let count = src.matches("node not running?").count();
    assert_eq!(
        count, 0,
        "phrasing regression: {count} site(s) still say 'node not running?' \
         in src/lib.rs — replace with 'Owner identity not loaded …'"
    );
}

#[test]
fn no_misleading_no_owner_identity_phrasing_in_lib_rs() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
        .expect("read src/lib.rs");
    let count = src.matches("missing — no owner identity?").count();
    assert_eq!(
        count, 0,
        "phrasing regression: {count} site(s) still say 'missing — no owner identity?'"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures --test error_phrasing_regression 2>&1 | tail -15
echo "exit: ${PIPESTATUS[0]}"
```

Expected: FAIL (count is ~144, not 0).

- [ ] **Step 3: Do the sweep**

The replacement must collapse three source phrasings into one. Because the prefixes vary (`crdt_state missing — node not running?`, `community_registry missing — node not running?`, `dm_outbox missing — no owner identity?`, etc.), do it with a careful scripted replace, then eyeball the diff:

```bash
cd src-tauri
# Replace the three families with the single honest message. Use perl for
# multi-pattern in-place edit. Run each, then review `git diff`.
perl -0pi -e 's/"[a-z_]+ missing — node not running\?"/"Owner identity not loaded — please restart the app or recreate identity."/g' src/lib.rs
perl -0pi -e 's/"crdt_state missing — node not running\?"/"Owner identity not loaded — please restart the app or recreate identity."/g' src/lib.rs
perl -0pi -e 's/"[a-z_]+ missing — no owner identity\?"/"Owner identity not loaded — please restart the app or recreate identity."/g' src/lib.rs
# Also catch the bare "crdt_state missing" (no suffix) sites from lib.rs:5582 etc.
perl -0pi -e 's/"crdt_state missing"/"Owner identity not loaded — please restart the app or recreate identity."/g' src/lib.rs
# Verify the target strings are gone:
grep -c "node not running?" src/lib.rs            # expect 0
grep -c "missing — no owner identity?" src/lib.rs  # expect 0
grep -c "crdt_state missing" src/lib.rs            # expect 0
```

> **Review the diff carefully** (`git diff src/lib.rs | head -200`): confirm only string literals changed, no code logic. The doc-comment lines that mention `crdt_state missing — node not running?` (e.g. lib.rs:10343, 11528, 11707) describe the IPC's error contract — update those doc comments to the new message too, so the regression grep (which scans the whole file incl. comments) passes AND the docs stay accurate.

- [ ] **Step 4: Verify the guard passes + full file still compiles**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures --test error_phrasing_regression 2>&1 | tail -15
echo "phrasing exit: ${PIPESTATUS[0]}"
timeout 600 cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -15
echo "check exit: ${PIPESTATUS[0]}"
```

Expected: both PASS / clean.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cd src-tauri
cargo fmt --all
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "clippy exit: ${PIPESTATUS[0]}"
cd ..
git add src-tauri/src/lib.rs src-tauri/tests/error_phrasing_regression.rs
git commit -m "refactor(zeb-338): honest owner-not-loaded error phrasing + regression guard"
```

---

## Task 6: deep-link-router queue + tests

**Files:**
- Modify: `src/lib/deep-link-router.ts`
- Create/Modify: `src/lib/__tests__/deep-link-router.test.ts`

**Context:** `deep-link-router.ts` currently only has `extractHarmonyInviteUrl`. Add a module-level single-slot queue (plain `let`, NOT `$state` — it's a `.ts` module, not a component, and is accessed only via functions per `feedback` on the ZEB-329 `latestRequest` lesson).

- [ ] **Step 1: Write the failing tests**

Create `src/lib/__tests__/deep-link-router.test.ts` (or extend if it exists):

```ts
import { describe, it, expect, beforeEach } from 'vitest';
import {
  extractHarmonyInviteUrl,
  queueInviteForPostMint,
  consumeQueuedInvite,
} from '../deep-link-router';

describe('post-mint invite queue', () => {
  beforeEach(() => {
    // Drain any residual queued value so tests don't bleed into each other.
    consumeQueuedInvite();
  });

  it('queueInviteForPostMint stores the url', () => {
    queueInviteForPostMint('harmony://invite/v1?x=1');
    expect(consumeQueuedInvite()).toBe('harmony://invite/v1?x=1');
  });

  it('consumeQueuedInvite returns and clears', () => {
    queueInviteForPostMint('harmony://invite/v1?x=2');
    expect(consumeQueuedInvite()).toBe('harmony://invite/v1?x=2');
    expect(consumeQueuedInvite()).toBeNull();
  });

  it('consumeQueuedInvite returns null when empty', () => {
    expect(consumeQueuedInvite()).toBeNull();
  });

  it('consumeQueuedInvite is idempotent on double call', () => {
    queueInviteForPostMint('harmony://invite/v1?x=3');
    consumeQueuedInvite();
    expect(consumeQueuedInvite()).toBeNull();
  });

  it('latest queue write wins', () => {
    queueInviteForPostMint('harmony://invite/v1?x=4');
    queueInviteForPostMint('harmony://invite/v1?x=5');
    expect(consumeQueuedInvite()).toBe('harmony://invite/v1?x=5');
  });
});
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx vitest run src/lib/__tests__/deep-link-router.test.ts 2>&1 | tail -20
echo "exit: ${PIPESTATUS[0]}"
```

Expected: FAIL (functions don't exist).

- [ ] **Step 3: Implement the queue**

Append to `src/lib/deep-link-router.ts`:

```ts
/**
 * ZEB-338: single-slot queue for a harmony:// invite that arrives before an
 * owner identity exists (fresh install + deep-link). The boot sequence /
 * WelcomeModal's onMinted drains it once the owner identity is present, then
 * routes it to the redeem dialog. Plain module-level `let` (not Svelte
 * $state) — this is a .ts module accessed only through the two functions
 * below, so reactivity would add nothing.
 *
 * "Consume once" semantics: consumeQueuedInvite clears the slot. If the
 * downstream redeem fails, the queue is NOT repopulated; the user retries via
 * the Help menu's paste-invite affordance (spec §5.3).
 */
let pendingInviteUrl: string | null = null;

export function queueInviteForPostMint(url: string): void {
  pendingInviteUrl = url;
}

export function consumeQueuedInvite(): string | null {
  const url = pendingInviteUrl;
  pendingInviteUrl = null;
  return url;
}
```

- [ ] **Step 4: Verify tests pass + tsc**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx vitest run src/lib/__tests__/deep-link-router.test.ts 2>&1 | tail -15
echo "vitest exit: ${PIPESTATUS[0]}"
npx tsc --noEmit ; echo "tsc exit: $?"
```

Expected: PASS / clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/deep-link-router.ts src/lib/__tests__/deep-link-router.test.ts
git commit -m "feat(zeb-338): post-mint invite queue in deep-link-router"
```

---

## Task 7: WelcomeModal two-pane hard gate (redaction test FIRST)

**Files:**
- Modify: `src/lib/components/WelcomeModal.svelte`
- Modify: `src/lib/components/__tests__/WelcomeModal.test.ts`

**Context:** rewrite the modal from the current invite-paste single-pane (which had `onDismiss`/`onJoinWithInvite` + Esc + backdrop) into a hard gate with stages `explain → minting → backup → skip-confirm`. Reuse `OwnerService` for mint + backup (correction #3). The redaction-invariant test is written FIRST per `feedback_second_order_correctness_review` (privacy is security-adjacent).

- [ ] **Step 1: Write the redaction-invariant test FIRST**

In `src/lib/components/__tests__/WelcomeModal.test.ts`, before any rendering code exists for pane 2, add (this drives the design — pane 2 must never put seed/token material in the DOM):

```ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import WelcomeModal from '../WelcomeModal.svelte';

// Mock the OwnerService so mint() returns a recoveryToken that looks like
// real hex seed material — the test asserts it NEVER reaches the DOM.
const mintMock = vi.fn();
const requestExportSavePathMock = vi.fn();
const exportRecoveryFileMock = vi.fn();
vi.mock('../owner-service', () => ({
  OwnerService: class {
    mint = mintMock;
    requestExportSavePath = requestExportSavePathMock;
    exportRecoveryFile = exportRecoveryFileMock;
  },
  extractError: (e: unknown) => (e instanceof Error ? e.message : String(e)),
}));

beforeEach(() => {
  mintMock.mockReset();
  requestExportSavePathMock.mockReset();
  exportRecoveryFileMock.mockReset();
  localStorage.clear();
  sessionStorage.clear();
});

describe('WelcomeModal recovery-artifact redaction invariant', () => {
  it('pane 2 DOM never contains hex seed/token material', async () => {
    // A recoveryToken that contains a long hex run — if it leaked into the
    // DOM, the regex below would catch it.
    mintMock.mockResolvedValue({
      state: { ownerId: 'x', ownerDisplayName: 'x', devices: [], canBackUp: true },
      recoveryToken: 'deadbeefdeadbeefdeadbeefdeadbeef0123456789abcdef0123456789abcdef',
    });
    const { getByTestId, container } = render(WelcomeModal, {
      props: { open: true, onMinted: vi.fn() },
    });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    // wait a tick for the mint promise + stage transition
    await Promise.resolve();
    await Promise.resolve();
    // Pane 2 ('backup') is now showing. Assert no 32+ hex-char run in the DOM.
    expect(container.innerHTML).not.toMatch(/[0-9a-f]{32,}/);
  });
});
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts 2>&1 | tail -25
echo "exit: ${PIPESTATUS[0]}"
```

Expected: FAIL (current modal has no `welcome-create-identity` testid / no mint flow).

- [ ] **Step 3: Rewrite WelcomeModal.svelte**

Replace the full file. Key points: hard gate (no Esc/backdrop/skip-to-dismiss), `onMinted` prop, stages, `OwnerService` reuse, footer retained, no invite input, no seed material in DOM.

```svelte
<script lang="ts">
  /**
   * ZEB-338 — First-run welcome modal as a HARD GATE.
   *
   * Mounts iff start_node returns hasOwnerIdentity=false. The only exit is a
   * successful mint (no skip-to-dismiss, no Esc, no backdrop). After mint,
   * pane 2 offers an (optional, severity-confirmed) recovery-file backup.
   *
   * Reuses OwnerService for mint + backup so the path-token flow
   * (requestExportSavePath → exportRecoveryFile) matches DevicesPanel.
   * The master_seed / recoveryToken are NEVER rendered (redaction invariant).
   */
  import { onMount } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';
  import { OwnerService, extractError, type MintIpcResult } from '../owner-service';
  import {
    MIN_RECOVERY_PASSPHRASE_LEN,
  } from '../recovery-policy';

  interface Props {
    open: boolean;
    onMinted: (mintResult: MintIpcResult) => void | Promise<void>;
  }
  const { open, onMinted }: Props = $props();

  type Stage = 'explain' | 'minting' | 'backup' | 'skip-confirm';
  let stage = $state<Stage>('explain');
  let mintResult = $state<MintIpcResult | null>(null);
  let mintError = $state<string | null>(null);
  let backupPassphrase = $state('');
  let backupError = $state<string | null>(null);
  let backupInFlight = $state(false);
  let appVersion = $state<string>('unknown');

  const svc = new OwnerService();

  onMount(async () => {
    try {
      appVersion = await getVersion();
    } catch (e) {
      console.debug('[zeb-338] WelcomeModal getVersion failed:', extractError(e));
    }
  });

  async function handleCreateIdentity() {
    stage = 'minting';
    mintError = null;
    try {
      const result = await svc.mint();
      mintResult = result;
      stage = 'backup';
    } catch (e) {
      mintError = extractError(e);
      stage = 'explain';
    }
  }

  async function handleSaveBackup() {
    if (mintResult === null) return;
    if ([...backupPassphrase].length < MIN_RECOVERY_PASSPHRASE_LEN) {
      backupError = `Passphrase must be at least ${MIN_RECOVERY_PASSPHRASE_LEN} characters.`;
      return;
    }
    if (backupInFlight) return;
    backupInFlight = true;
    backupError = null;
    try {
      const pathToken = await svc.requestExportSavePath({
        defaultFilename: 'owner-recovery.bin',
        filterName: 'Recovery file',
        filterExtensions: ['bin'],
      });
      if (pathToken === null) {
        // user cancelled the OS dialog — stay on pane 2
        backupInFlight = false;
        return;
      }
      await svc.exportRecoveryFile(mintResult.recoveryToken, pathToken, backupPassphrase, null);
      try {
        localStorage.setItem('harmony.onboarding.recoveryArtifactBackedUp', 'true');
      } catch (e) {
        console.debug('[zeb-338] backedUp flag write failed:', extractError(e));
      }
      backupPassphrase = '';
      await onMinted(mintResult);
    } catch (e) {
      backupError = extractError(e);
    } finally {
      backupInFlight = false;
    }
  }

  function handleSkipRequest() {
    stage = 'skip-confirm';
  }

  function handleSkipCancel() {
    stage = 'backup';
  }

  async function handleSkipConfirm() {
    if (mintResult === null) return;
    try {
      localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    } catch (e) {
      console.debug('[zeb-338] backupSkipped flag write failed:', extractError(e));
    }
    await onMinted(mintResult);
  }
</script>

{#if open}
  <div class="modal-backdrop" data-testid="welcome-modal-backdrop" role="presentation">
    <div
      class="modal-content"
      data-testid="welcome-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="welcome-title"
    >
      {#if stage === 'explain' || stage === 'minting'}
        <h2 id="welcome-title">Welcome to Harmony</h2>
        <p>
          Harmony is a federated, polycentric social fabric built on
          user-owned identity. Your identity lives <strong>only on this
          device</strong> — there's no central account, no server holding
          your data.
        </p>
        <p>
          When you create your identity you'll get a recovery artifact to back
          up. Save it somewhere safe — it's the only way to prove this identity
          is yours if you ever lose this device.
        </p>
        <p class="muted">
          Single-device only in v0.1.0-alpha — multi-device sync ships in a
          later release.
        </p>
        {#if mintError}
          <p class="error" data-testid="welcome-mint-error">{mintError}</p>
        {/if}
        <div class="actions">
          <button
            class="primary"
            data-testid="welcome-create-identity"
            onclick={handleCreateIdentity}
            disabled={stage === 'minting'}
          >
            {stage === 'minting' ? 'Creating your identity…' : 'Create my identity'}
          </button>
        </div>
      {:else if stage === 'backup'}
        <h2 id="welcome-title">Your identity is ready</h2>
        <p>
          Back up your recovery artifact now. Without it, you can't prove this
          identity is yours if this device is lost.
        </p>
        <p class="muted">
          The recovery file is encrypted with your passphrase. Save it
          somewhere safe (USB drive, password-manager attachment, etc.).
        </p>
        <label for="welcome-backup-pass">Passphrase (≥{MIN_RECOVERY_PASSPHRASE_LEN} chars)</label>
        <input
          id="welcome-backup-pass"
          data-testid="welcome-backup-passphrase"
          type="password"
          bind:value={backupPassphrase}
          oninput={() => { backupError = null; }}
        />
        {#if backupError}
          <p class="error" data-testid="welcome-backup-error">{backupError}</p>
        {/if}
        <div class="actions">
          <button
            class="primary"
            data-testid="welcome-save-backup"
            onclick={handleSaveBackup}
            disabled={[...backupPassphrase].length < MIN_RECOVERY_PASSPHRASE_LEN || backupInFlight}
          >
            {backupInFlight ? 'Saving…' : 'Save recovery file'}
          </button>
          <button data-testid="welcome-skip-backup" onclick={handleSkipRequest} disabled={backupInFlight}>
            Skip for now
          </button>
        </div>
      {:else if stage === 'skip-confirm'}
        <h2 id="welcome-title">Are you sure?</h2>
        <p>
          Without a backup, if you lose this device you lose this identity
          permanently. There's no central recovery — this is what
          "self-sovereign" means.
        </p>
        <div class="actions">
          <button data-testid="welcome-skip-cancel" onclick={handleSkipCancel}>
            Cancel
          </button>
          <button class="danger" data-testid="welcome-skip-confirm" onclick={handleSkipConfirm}>
            I accept the risk
          </button>
        </div>
      {/if}

      <footer>
        <span class="version" data-testid="welcome-version">v{appVersion}</span>
        <a
          data-testid="welcome-feedback-link"
          href="https://github.com/zeblithic/harmony-client/blob/main/docs/feedback.md"
          target="_blank"
          rel="noopener noreferrer"
        >
          How to submit feedback →
        </a>
      </footer>
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 520px;
    width: 90%;
  }
  .modal-content h2 { margin: 0 0 1rem; font-size: 1.25rem; }
  .modal-content p { margin: 0 0 1rem; line-height: 1.5; }
  .muted { color: var(--text-secondary, #aaa); font-size: 0.9rem; }
  label { display: block; margin-bottom: 0.4rem; font-size: 0.9rem; }
  input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem;
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    margin-bottom: 0.5rem;
  }
  .actions { display: flex; gap: 0.5rem; margin-top: 0.5rem; }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
  .actions button.primary { background: var(--accent, #5865f2); border-color: var(--accent, #5865f2); }
  .actions button.danger { background: var(--danger, #d9534f); border-color: var(--danger, #d9534f); }
  .actions button:disabled { opacity: 0.5; cursor: default; }
  .error { color: crimson; font-size: 0.85rem; margin: 0 0 0.5rem; }
  footer { margin-top: 1rem; font-size: 0.85rem; }
  .version { display: inline-block; margin-right: 1rem; color: var(--text-secondary, #aaa); opacity: 0.7; }
  footer a { color: var(--accent, #5865f2); text-decoration: none; }
  footer a:hover { text-decoration: underline; }
</style>
```

> **Verify `MIN_RECOVERY_PASSPHRASE_LEN` is exported from `../recovery-policy`** (`grep -n "MIN_RECOVERY_PASSPHRASE_LEN" src/lib/recovery-policy.ts`) — DevicesPanel imports it from there. If the path differs, fix the import.

- [ ] **Step 4: Write the rest of the component tests**

Add to `WelcomeModal.test.ts` (the redaction test from Step 1 stays). Cover the spec §8.2 list:

```ts
describe('WelcomeModal hard gate + flow', () => {
  it('renders explain pane when open and no mint yet', () => {
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    expect(getByTestId('welcome-create-identity')).toBeTruthy();
  });

  it('clicks create-my-identity invokes mint with no args', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    expect(mintMock).toHaveBeenCalledWith();
  });

  it('transitions to backup pane on mint success', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    expect(getByTestId('welcome-save-backup')).toBeTruthy();
  });

  it('stays on explain pane with inline error on mint failure', async () => {
    mintMock.mockRejectedValue('mint blew up');
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    expect(getByTestId('welcome-mint-error').textContent).toContain('mint blew up');
    expect(getByTestId('welcome-create-identity')).toBeTruthy();
  });

  it('save recovery file calls export with pathToken + passphrase', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    requestExportSavePathMock.mockResolvedValue('path-token-uuid');
    exportRecoveryFileMock.mockResolvedValue({ identityHash: 'h', byteLen: 1, path: '/x' });
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    await fireEvent.input(getByTestId('welcome-backup-passphrase'), { target: { value: 'longenoughpass' } });
    await fireEvent.click(getByTestId('welcome-save-backup'));
    await Promise.resolve(); await Promise.resolve();
    expect(exportRecoveryFileMock).toHaveBeenCalledWith('tok', 'path-token-uuid', 'longenoughpass', null);
    expect(localStorage.getItem('harmony.onboarding.recoveryArtifactBackedUp')).toBe('true');
    expect(onMinted).toHaveBeenCalled();
  });

  it('passphrase under 8 chars disables save button', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted: vi.fn() } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    await fireEvent.input(getByTestId('welcome-backup-passphrase'), { target: { value: 'short' } });
    expect((getByTestId('welcome-save-backup') as HTMLButtonElement).disabled).toBe(true);
  });

  it('skip → confirm sets backupSkipped and calls onMinted', async () => {
    mintMock.mockResolvedValue({ state: {}, recoveryToken: 'tok' });
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.click(getByTestId('welcome-create-identity'));
    await Promise.resolve(); await Promise.resolve();
    await fireEvent.click(getByTestId('welcome-skip-backup'));
    await fireEvent.click(getByTestId('welcome-skip-confirm'));
    await Promise.resolve();
    expect(localStorage.getItem('harmony.onboarding.backupSkipped')).toBe('true');
    expect(onMinted).toHaveBeenCalled();
  });

  it('hard gate ignores Escape keypress', async () => {
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.keyDown(window, { key: 'Escape' });
    // modal still rendered, onMinted never called
    expect(getByTestId('welcome-modal')).toBeTruthy();
    expect(onMinted).not.toHaveBeenCalled();
  });

  it('hard gate ignores backdrop click', async () => {
    const onMinted = vi.fn();
    const { getByTestId } = render(WelcomeModal, { props: { open: true, onMinted } });
    await fireEvent.click(getByTestId('welcome-modal-backdrop'));
    expect(getByTestId('welcome-modal')).toBeTruthy();
    expect(onMinted).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 5: Run tests + tsc**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts 2>&1 | tail -30
echo "vitest exit: ${PIPESTATUS[0]}"
npx tsc --noEmit ; echo "tsc exit: $?"
```

Expected: all PASS / clean.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/WelcomeModal.svelte src/lib/components/__tests__/WelcomeModal.test.ts
git commit -m "feat(zeb-338): WelcomeModal two-pane hard gate (mint + backup)"
```

---

## Task 8: BackupReminderBanner + tests

**Files:**
- Create: `src/lib/components/BackupReminderBanner.svelte`
- Create: `src/lib/components/__tests__/BackupReminderBanner.test.ts`

**Context:** persistent reminder shown after a skipped backup. Keys per correction #7. The component reads its own visibility from storage `onMount`; it exposes a `onBackedUp` callback so the parent can react if needed (optional). It reuses `OwnerService` for the backup flow.

- [ ] **Step 1: Write the failing tests**

Create `src/lib/components/__tests__/BackupReminderBanner.test.ts`:

```ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import BackupReminderBanner from '../BackupReminderBanner.svelte';

const requestExportSavePathMock = vi.fn();
const exportRecoveryFileMock = vi.fn();
const issueRecoveryTokenMock = vi.fn();
vi.mock('../owner-service', () => ({
  OwnerService: class {
    requestExportSavePath = requestExportSavePathMock;
    exportRecoveryFile = exportRecoveryFileMock;
    issueRecoveryToken = issueRecoveryTokenMock;
  },
  extractError: (e: unknown) => (e instanceof Error ? e.message : String(e)),
}));

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  requestExportSavePathMock.mockReset();
  exportRecoveryFileMock.mockReset();
  issueRecoveryTokenMock.mockReset();
});

describe('BackupReminderBanner visibility', () => {
  it('mounts when backupSkipped set and no backup flag', () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    const { queryByTestId } = render(BackupReminderBanner);
    expect(queryByTestId('backup-reminder-banner')).toBeTruthy();
  });

  it('does not mount when backup flag set', () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    localStorage.setItem('harmony.onboarding.recoveryArtifactBackedUp', 'true');
    const { queryByTestId } = render(BackupReminderBanner);
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('does not mount when backup was never skipped', () => {
    const { queryByTestId } = render(BackupReminderBanner);
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('dismiss hides for session', async () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    const { queryByTestId, getByTestId } = render(BackupReminderBanner);
    await fireEvent.click(getByTestId('backup-reminder-dismiss'));
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
    expect(sessionStorage.getItem('harmony.onboarding.backupBannerDismissed')).toBe('true');
  });

  it('does not mount when dismissed this session', () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    sessionStorage.setItem('harmony.onboarding.backupBannerDismissed', 'true');
    const { queryByTestId } = render(BackupReminderBanner);
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });

  it('back up now runs export flow and hides on success', async () => {
    localStorage.setItem('harmony.onboarding.backupSkipped', 'true');
    issueRecoveryTokenMock.mockResolvedValue('tok');
    requestExportSavePathMock.mockResolvedValue('path-token');
    exportRecoveryFileMock.mockResolvedValue({ identityHash: 'h', byteLen: 1, path: '/x' });
    const { queryByTestId, getByTestId } = render(BackupReminderBanner);
    await fireEvent.click(getByTestId('backup-reminder-backup-now'));
    // passphrase prompt appears inline; fill + submit
    await fireEvent.input(getByTestId('backup-reminder-passphrase'), { target: { value: 'longenoughpass' } });
    await fireEvent.click(getByTestId('backup-reminder-save'));
    await Promise.resolve(); await Promise.resolve();
    expect(localStorage.getItem('harmony.onboarding.recoveryArtifactBackedUp')).toBe('true');
    expect(queryByTestId('backup-reminder-banner')).toBeNull();
  });
});
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx vitest run src/lib/components/__tests__/BackupReminderBanner.test.ts 2>&1 | tail -20
echo "exit: ${PIPESTATUS[0]}"
```

Expected: FAIL (component doesn't exist).

- [ ] **Step 3: Implement the component**

Create `src/lib/components/BackupReminderBanner.svelte`. Note: a returning user (Flow 4) skipped backup and has no live recovery token in memory, so this banner must issue a fresh token via `issueRecoveryToken()` before exporting (the WelcomeModal had a token from mint; this banner doesn't).

```svelte
<script lang="ts">
  /**
   * ZEB-338 — persistent reminder shown after the user skipped the recovery
   * backup during onboarding. Sticky across launches (localStorage) until the
   * user backs up; dismissable for the current session only (sessionStorage).
   *
   * Visibility (correction #7): backupSkipped === 'true'
   *   && recoveryArtifactBackedUp !== 'true'
   *   && backupBannerDismissed !== 'true' (session)
   *
   * Keys on backupSkipped — set ONLY by WelcomeModal's skip-confirm path — so
   * users who minted + backed up via the DevicesPanel never see this.
   *
   * Unlike WelcomeModal (which holds a fresh mint token), this banner issues a
   * recovery token on demand via issueRecoveryToken() before exporting.
   */
  import { onMount } from 'svelte';
  import { OwnerService, extractError } from '../owner-service';
  import { MIN_RECOVERY_PASSPHRASE_LEN } from '../recovery-policy';

  let visible = $state(false);
  let showPassphrase = $state(false);
  let passphrase = $state('');
  let error = $state<string | null>(null);
  let inFlight = $state(false);

  const svc = new OwnerService();

  const KEY_SKIPPED = 'harmony.onboarding.backupSkipped';
  const KEY_BACKED_UP = 'harmony.onboarding.recoveryArtifactBackedUp';
  const KEY_DISMISSED = 'harmony.onboarding.backupBannerDismissed';

  onMount(() => {
    try {
      const skipped = localStorage.getItem(KEY_SKIPPED) === 'true';
      const backedUp = localStorage.getItem(KEY_BACKED_UP) === 'true';
      const dismissed = sessionStorage.getItem(KEY_DISMISSED) === 'true';
      visible = skipped && !backedUp && !dismissed;
    } catch (e) {
      // storage unavailable → safest is to NOT nag (avoids a stuck banner)
      console.debug('[zeb-338] BackupReminderBanner storage read failed:', extractError(e));
      visible = false;
    }
  });

  function dismiss() {
    try {
      sessionStorage.setItem(KEY_DISMISSED, 'true');
    } catch (e) {
      console.debug('[zeb-338] dismiss flag write failed:', extractError(e));
    }
    visible = false;
  }

  function startBackup() {
    showPassphrase = true;
    error = null;
  }

  async function save() {
    if ([...passphrase].length < MIN_RECOVERY_PASSPHRASE_LEN) {
      error = `Passphrase must be at least ${MIN_RECOVERY_PASSPHRASE_LEN} characters.`;
      return;
    }
    if (inFlight) return;
    inFlight = true;
    error = null;
    try {
      const token = await svc.issueRecoveryToken();
      const pathToken = await svc.requestExportSavePath({
        defaultFilename: 'owner-recovery.bin',
        filterName: 'Recovery file',
        filterExtensions: ['bin'],
      });
      if (pathToken === null) {
        inFlight = false;
        return; // user cancelled
      }
      await svc.exportRecoveryFile(token, pathToken, passphrase, null);
      try {
        localStorage.setItem(KEY_BACKED_UP, 'true');
      } catch (e) {
        console.debug('[zeb-338] backedUp flag write failed:', extractError(e));
      }
      passphrase = '';
      visible = false;
    } catch (e) {
      error = extractError(e);
    } finally {
      inFlight = false;
    }
  }
</script>

{#if visible}
  <div class="backup-banner" data-testid="backup-reminder-banner" role="status">
    <span class="warn">⚠ Your identity hasn't been backed up.</span>
    {#if !showPassphrase}
      <button data-testid="backup-reminder-backup-now" onclick={startBackup}>Back up now</button>
      <button class="ghost" data-testid="backup-reminder-dismiss" onclick={dismiss}>Dismiss</button>
    {:else}
      <input
        data-testid="backup-reminder-passphrase"
        type="password"
        placeholder="Passphrase (≥{MIN_RECOVERY_PASSPHRASE_LEN})"
        bind:value={passphrase}
        oninput={() => { error = null; }}
      />
      <button
        data-testid="backup-reminder-save"
        onclick={save}
        disabled={[...passphrase].length < MIN_RECOVERY_PASSPHRASE_LEN || inFlight}
      >
        {inFlight ? 'Saving…' : 'Save'}
      </button>
    {/if}
    {#if error}
      <span class="error" data-testid="backup-reminder-error">{error}</span>
    {/if}
  </div>
{/if}

<style>
  .backup-banner {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    background: var(--warn-bg, #4a3a1a);
    color: var(--text-primary, #fff);
    font-size: 0.85rem;
    border-bottom: 1px solid var(--border, #444);
  }
  .warn { flex: 0 0 auto; }
  .backup-banner button {
    padding: 0.25rem 0.6rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
  .backup-banner button.ghost { background: transparent; }
  .backup-banner input {
    padding: 0.25rem 0.5rem;
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
  }
  .error { color: crimson; }
</style>
```

- [ ] **Step 4: Run tests + tsc**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx vitest run src/lib/components/__tests__/BackupReminderBanner.test.ts 2>&1 | tail -25
echo "vitest exit: ${PIPESTATUS[0]}"
npx tsc --noEmit ; echo "tsc exit: $?"
```

Expected: PASS / clean.

- [ ] **Step 5: Commit**

```bash
git add src/lib/components/BackupReminderBanner.svelte src/lib/components/__tests__/BackupReminderBanner.test.ts
git commit -m "feat(zeb-338): BackupReminderBanner for skipped-backup reminder"
```

---

## Task 9: App.svelte wiring (boot gate + onMinted drain + deep-link queue + banner mount)

**Files:**
- Modify: `src/App.svelte`

**Context + the race analysis (correction #5):** the boot IIFE calls `start_node` and the deep-link `onMount` (lib.rs:854) registers the listener + drains `getCurrent()`. These are separate `onMount`s; their await-interleaving isn't ordered. The queue + dual-drain makes it race-free:

- `hasOwnerIdentityState` starts `false`; set to the real value when `start_node` resolves; set `true` in `onMinted`.
- Deep-link routing: `if (!hasOwnerIdentityState) queueInviteForPostMint(url)` else open the redeem dialog directly. The check-and-queue is synchronous (no `await` between), so it's atomic w.r.t. JS's single thread.
- Boot, after `start_node` resolves: if `hasOwnerIdentity === true`, drain the queue (returning user who clicked an invite); else show Welcome.
- `onMinted`: drain the queue (fresh user who clicked an invite).

The only way an invite is queued is when `hasOwnerIdentityState` is still `false`, which means boot hasn't set it true yet, which means boot's drain hasn't run yet → boot WILL drain it. If boot already set it true, the deep-link handler routes straight to the dialog (never queues). Race closed.

- [ ] **Step 1: Import the queue functions + BackupReminderBanner**

Near the other imports (line ~60):

```ts
import { extractHarmonyInviteUrl, queueInviteForPostMint, consumeQueuedInvite } from './lib/deep-link-router';
import BackupReminderBanner from './lib/components/BackupReminderBanner.svelte';
import type { MintIpcResult } from './lib/owner-service';
import type { StartNodeResponse } from './lib/types/onboarding';
```

> Adjust the existing `extractHarmonyInviteUrl` import (line 60) to add the two new names rather than duplicating the import.

- [ ] **Step 2: Add `hasOwnerIdentityState` + a shared invite-routing helper**

Near `showWelcomeModal` (line 237):

```ts
let showWelcomeModal = $state(false);
// ZEB-338: backend-authoritative owner-identity presence. Starts false; set
// from start_node's response; flipped true by onMinted. Gates the welcome
// hard-gate and the deep-link routing branch.
let hasOwnerIdentityState = $state(false);
```

Add a helper used by BOTH deep-link entry points (listener + getCurrent) — place it near the deep-link logic:

```ts
// ZEB-338: route an incoming harmony:// invite. Pre-mint (no owner identity)
// → queue for the post-mint drain. Post-mint → open the redeem dialog.
function routeInviteUrl(url: string): void {
  if (!hasOwnerIdentityState) {
    queueInviteForPostMint(url);
    return;
  }
  redeemUrl = url;
  redeemError = null;
  showRedeemInvite = true;
}

// ZEB-338: drain a queued invite into the redeem dialog (called post-mint and
// post-boot-when-owner-already-present).
function drainQueuedInvite(): void {
  const queued = consumeQueuedInvite();
  if (queued !== null) {
    redeemUrl = queued;
    redeemError = null;
    showRedeemInvite = true;
  }
}
```

- [ ] **Step 3: Replace the boot welcome-gate (lib.rs frontend boot, lines ~691-727)**

Change the `start_node` call to capture the response, and replace the localStorage-based welcome gate with the hard gate:

```ts
// Boot the harmony node in standalone mode.
let startResp: StartNodeResponse | null = null;
try {
  startResp = await invoke<StartNodeResponse>('start_node', { endpoint: null });
} catch (err) {
  console.warn('[harmony-client] auto-start_node failed:', err);
}

// ZEB-338: hard gate on backend owner-identity presence. Forward-compat:
// treat missing hasOwnerIdentity as false (older backend → show onboarding).
hasOwnerIdentityState = startResp?.hasOwnerIdentity === true;
if (hasOwnerIdentityState) {
  showWelcomeModal = false;
  // Returning user who clicked an invite before start_node resolved: drain it.
  drainQueuedInvite();
} else {
  // No owner identity → hard gate. (A deep-link that already arrived was
  // queued by routeInviteUrl, not shown over the welcome.)
  showWelcomeModal = true;
}
```

> Delete the old `harmony.onboarding.welcomeAcknowledged` read-gate block (lines ~712-727) entirely — the backend signal replaces it. Leave `acknowledgeWelcome()` definition in place ONLY if other code still calls it; otherwise delete it too (grep first: `grep -n acknowledgeWelcome src/App.svelte`). The deep-link handler currently calls `acknowledgeWelcome()` — those calls are removed in Step 4, so after Step 4 `acknowledgeWelcome` is likely dead and should be deleted to avoid an unused-function lint.

- [ ] **Step 4: Rewrite the deep-link handler (lines ~865-895) to use `routeInviteUrl`**

```ts
unlistenDeepLink = await listen<string[]>('deep-link-received', (event) => {
  const url = extractHarmonyInviteUrl(event.payload);
  if (url) {
    routeInviteUrl(url);
  }
});

// Drain URLs queued by the deep-link plugin before the listener registered.
try {
  const queued = await getCurrentDeepLink();
  if (queued) {
    const url = extractHarmonyInviteUrl(queued);
    if (url) {
      routeInviteUrl(url);
    }
  }
} catch (e) {
  const msg = e instanceof Error ? e.message : String(e);
  console.warn(`[harmony-client] deep-link getCurrent() failed: ${msg}`);
}
```

> This removes the old `showWelcomeModal = false` + `acknowledgeWelcome()` from both branches — the hard gate now owns welcome visibility, and `routeInviteUrl` queues (pre-mint) or opens the dialog (post-mint) without touching the welcome flag.

- [ ] **Step 5: Define `onMinted` + rewrite the WelcomeModal render site (lines ~2030-2043)**

Add the handler near the other modal handlers:

```ts
// ZEB-338: WelcomeModal hard-gate completion. Flip owner-present, close the
// gate, and drain any invite that was queued pre-mint (Flow 3).
async function onMinted(_result: MintIpcResult): Promise<void> {
  hasOwnerIdentityState = true;
  showWelcomeModal = false;
  drainQueuedInvite();
}
```

Replace the `<WelcomeModal .../>` block:

```svelte
<WelcomeModal open={showWelcomeModal} {onMinted} />
```

- [ ] **Step 6: Mount BackupReminderBanner as a fixed overlay**

Near the `.help-overlay` block (line ~2045), add a sibling overlay. The banner self-gates visibility, but also suppress it while the welcome hard-gate is up (don't stack a backup nag behind the modal):

```svelte
{#if !showWelcomeModal}
  <div class="backup-banner-overlay">
    <BackupReminderBanner />
  </div>
{/if}
```

Add minimal CSS in the `<style>` block:

```css
.backup-banner-overlay {
  position: fixed;
  top: 0;
  left: 0;
  right: 0;
  z-index: 40; /* below modal (1000) + help overlay; above app chrome */
}
```

- [ ] **Step 7: tsc + targeted vitest (App integration) + run the full frontend suite**

Add App integration tests if an App test harness exists (`ls src/lib/__tests__/ | grep -i app`); otherwise cover the wiring via the component tests already written (Tasks 6-8) and note that App.svelte boot wiring is verified by the manual smoke test (Task 10 §3). If an App test file exists, add:
- `boot_with_hasOwnerIdentity_false_mounts_WelcomeModal`
- `boot_with_hasOwnerIdentity_true_skips_WelcomeModal`
- `deep_link_during_no_owner_queues_invite_does_not_open_redeem`
- `onMinted_drains_queued_invite_opens_redeem`

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx tsc --noEmit ; echo "tsc exit: $?"
npx vitest run 2>&1 | tail -30
echo "vitest exit: ${PIPESTATUS[0]}"
```

Expected: tsc clean; full vitest suite green (no regressions in existing App/WelcomeModal-dependent tests — there may be existing tests asserting the OLD WelcomeModal props; update or remove those as part of this task).

> **Likely breakage:** existing tests that mount `WelcomeModal` with `onDismiss`/`onJoinWithInvite` props, or that assert the old localStorage welcome-ack gate, will fail. Find them (`grep -rln "onJoinWithInvite\|onDismiss.*[Ww]elcome\|welcomeAcknowledged" src/`) and update to the new contract. This is expected scope, not regression.

- [ ] **Step 8: Commit**

```bash
git add src/App.svelte
git commit -m "feat(zeb-338): wire hard-gate boot + onMinted drain + invite queue + backup banner"
```

---

## Task 10: Release-process smoke checklist + docs

**Files:**
- Modify: `docs/release-process.md` (§3 smoke test)

- [ ] **Step 1: Locate the §3 smoke-test section**

```bash
grep -n "smoke\|## 3\|### 3\|Smoke" docs/release-process.md | head
```

- [ ] **Step 2: Add the first-run flow checklist**

Insert into the §3 smoke-test list (adapt heading depth to the file's existing structure):

```markdown
#### First-run onboarding (ZEB-338) — required on every release

Run on a machine with NO existing Harmony identity (or wipe first):

1. Wipe `~/.harmony/` and the `harmony.client` keychain entry
   (macOS: `security delete-generic-password -s harmony.client` then check
   Keychain Access; Windows: Credential Manager; Linux: `secret-tool clear
   service harmony.client` or the libsecret store).
2. Launch the installed build.
3. WelcomeModal appears at the "Create my identity" pane and is NOT
   dismissable (Esc / clicking outside do nothing).
4. Click **Create my identity** — a "Creating your identity…" state shows for
   ~3 s — the pane transitions to the backup step.
5. Enter a passphrase (≥8 chars), click **Save recovery file**, choose a temp
   path. Export succeeds.
6. Modal closes; main UI loads; **+ Create community** succeeds (no
   "crdt_state missing" / "node not running" error).
7. Quit + relaunch — main UI loads directly (no Welcome), no backup banner.
8. Wipe again, relaunch, this time click **Skip for now → I accept the risk**.
   Main UI loads with a persistent backup-reminder banner. Relaunch → banner
   persists. Click **Back up now**, save → banner disappears and stays gone on
   next launch.
9. (If a Zeblithic invite URL is available) Wipe, then open the
   `harmony://invite/...` URL to launch the app. Welcome still hard-gates;
   after mint+backup, the redeem dialog opens automatically with the invite
   pre-filled.
```

- [ ] **Step 3: Commit**

```bash
git add docs/release-process.md
git commit -m "docs(zeb-338): first-run onboarding smoke checklist in release-process"
```

---

## Task 11: Final gate sweep + push + PR

**Files:** none (verification + ship)

- [ ] **Step 1: Full backend gate sweep (foreground, bounded)**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
cargo fmt --all -- --check ; echo "fmt exit: $?"
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "clippy exit: ${PIPESTATUS[0]}"
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tee /tmp/zeb338-final.txt | tail -40
echo "nextest exit: ${PIPESTATUS[0]}"
```

Expected: fmt clean, clippy clean. nextest: compare `/tmp/zeb338-final.txt` failures against the Task 0 baseline — the ONLY failures permitted are the pre-recorded orphans (`folder_ingest`, `mint`, `mint_sync`, `rename_content_integration`, occasional `zenoh_iroh_*`). Any NEW failing test is blocking — fix before proceeding. If a gate exceeds `timeout 600`, surface `DONE_WITH_CONCERNS`.

- [ ] **Step 2: MSRV check**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
set -o pipefail
timeout 600 cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -15
echo "msrv-shape check exit: ${PIPESTATUS[0]}"
```

(CI runs this against the declared MSRV toolchain; locally this confirms the code compiles under the same flags.)

- [ ] **Step 3: Full frontend gate sweep**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
set -o pipefail
npx tsc --noEmit ; echo "tsc exit: $?"
npx vitest run 2>&1 | tail -30
echo "vitest exit: ${PIPESTATUS[0]}"
```

Expected: both green.

- [ ] **Step 4: Push the branch**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git push -u origin zeb-338-first-run-state-machine
```

- [ ] **Step 5: Open the PR**

```bash
gh pr create --repo zeblithic/harmony-client \
  --base main --head zeb-338-first-run-state-machine \
  --title "ZEB-338: first-run state machine — owner-identity hard gate + self-lifecycle mint" \
  --body "$(cat <<'EOF'
## Summary

Closes the owner-identity onboarding deadlock surfaced during the Koya↔KRILE
alpha bring-up (2026-05-28): a fresh install landed in a main UI where every
action failed with `crdt_state missing — node not running?`, and the only path
to an owner identity required a "stop the node" step with no UI affordance.

This makes owner-identity presence a **hard gate**:

- `start_node` now returns `hasOwnerIdentity`; the frontend renders a
  non-dismissible WelcomeModal until the user mints.
- `mint_owner_identity` is now **self-lifecycle**: it stops the node, mints +
  persists `owner_state.cbor` + keychain, and restarts the node — so the user
  never has to stop the node by hand. (Reuses the existing async-safe
  `stop_inner` + a newly-extracted `start_node_inner`.)
- New `require_owner_loaded` helper + `OwnerLoadError`; the 144 misleading
  "node not running?" sites are swept to an honest "Owner identity not loaded
  — please restart the app or recreate identity." message (regression-guarded).
- Two-pane WelcomeModal (explain → mint → backup, with a severity-confirmed
  skip) + a persistent `BackupReminderBanner` for skipped backups.
- A harmony:// invite arriving pre-mint is queued and auto-opened in the redeem
  dialog after mint.

Design: `docs/specs/2026-05-28-zeb-338-first-run-state-machine-design.md`
(commit 69a91c0).
Plan: `docs/plans/2026-05-28-zeb-338-first-run-state-machine-plan.md`.

Implements [ZEB-338](https://linear.app/zeblith/issue/ZEB-338). Subsumes
[ZEB-335](https://linear.app/zeblith/issue/ZEB-335) (the "stop the node to mint"
dead-end dissolves — mint owns the lifecycle now).

Ships as **v0.1.0-alpha.1**; the auto-updater pushes it to existing installs.

## Test plan

Backend (from `src-tauri/`):
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (only pre-existing orphan failures remain)
- [ ] `cargo check --locked --all-targets --features test-fixtures` (MSRV shape)
- [ ] New: `mint_owner_lifecycle.rs`, `error_phrasing_regression.rs`, `owner_loaded` unit tests, `StartNodeResponse` wire-shape tests

Frontend (from repo root):
- [ ] `npx tsc --noEmit`
- [ ] `npx vitest run` — incl. new WelcomeModal hard-gate + redaction-invariant, BackupReminderBanner, deep-link queue tests

Manual (per `docs/release-process.md` §3 first-run checklist):
- [ ] Wipe identity → Welcome hard-gates → Create identity → Save backup → main UI → Create community works
- [ ] Skip backup → reminder banner persists across launches → Back up now clears it
- [ ] CI: fmt+clippy / nextest / large-tests / MSRV / frontend all green

## CI jobs expected
`Rust — fmt + clippy`, `Rust — test (nextest)`, `Rust — large tests`, `MSRV`, `Frontend — tsc, vitest`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 6: Report PR URL + hand off to the autonomous bot-review loop**

Print the PR URL. The controller then enters the autonomous PR-monitoring loop (per `feedback_autonomous_pr_monitoring_loop` + `feedback_human_in_loop_window`): watch CI + bot reviewers (CodeRabbit / Cursor / CodeAnt / Qodo), address each review round as a single bundled batch of commits + one push, converge, and pushover when there's no actionable feedback left and the PR is ready to merge. Do NOT trigger Greptile (`reference_greptile_manual_trigger`).

---

## Self-review checklist (run before handing to subagent-driven-development)

- **Spec coverage:** §4.1 → T2; §4.2 mint → T3+T4; §4.3 helper+sweep → T1+T5; §5.1 WelcomeModal → T7; §5.2 App boot+onMinted → T9; §5.3 deep-link queue → T6+T9; §5.4 banner → T8+T9; §6 flows → T9 wiring + T7/T8 components; §7 errors → T4 (mint), T7 (backup/mint UI), T9 (deep-link); §8 tests → distributed across T1/T2/T4/T5/T6/T7/T8/T9; §8.4 smoke → T10; §9 files → all tasks; release → T11.
- **Corrections documented:** the 8 plan-corrections section makes the plan authoritative where spec pseudocode was wrong (path-token flow, stop_inner already exists, redeem dialog vs bare invoke, dual-drain race, banner mount point, namespaced keys, CBOR→JSON pinning, invite-input removal).
- **Type consistency:** `MintIpcResult` (owner-service.ts) used in WelcomeModal + App; `StartNodeResponse` (onboarding.ts) used in App boot; `OwnerLoadedHandles`/`OwnerLoadError` (owner_loaded.rs) self-contained; localStorage keys are the SAME strings across WelcomeModal (writes) + BackupReminderBanner (reads): `harmony.onboarding.recoveryArtifactBackedUp`, `harmony.onboarding.backupSkipped`, `harmony.onboarding.backupBannerDismissed`.
- **Risk gate:** T3 (start_node_inner extraction) has an explicit contingency (revert + DONE_WITH_CONCERNS → hot-load re-scope) so it can't silently consume hours.
```
