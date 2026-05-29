# ZEB-338: harmony-client first-run state machine — owner-identity hard gate + self-lifecycle mint IPC

**Status:** Approved 2026-05-28 (Jake-approved via brainstorm Q1–Q6 + sections 1–6 walkthrough)
**Linear:** [ZEB-338](https://linear.app/zeblith/issue/ZEB-338)
**Parent:** [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) (harmony-client v0.1.0-alpha umbrella)
**Subsumes:** [ZEB-335](https://linear.app/zeblith/issue/ZEB-335) (Urgent — mint-blocked-by-running-node deadlock)
**Related (out of scope):** [ZEB-333](https://linear.app/zeblith/issue/ZEB-333) (nav overflow), [ZEB-334](https://linear.app/zeblith/issue/ZEB-334) (self-chat default), [ZEB-336](https://linear.app/zeblith/issue/ZEB-336) (profile model), [ZEB-337](https://linear.app/zeblith/issue/ZEB-337) ("You" label), [ZEB-173](https://linear.app/zeblith/issue/ZEB-173) (owner→device binding — Done at protocol level)
**Release vehicle:** v0.1.0-alpha.1 (auto-updater pushes to existing installs)
**Brainstorm transcript:** 2026-05-28 session with Jake on Koya, surfaced during Koya↔KRILE alpha bring-up

---

## 1. Goal

Close the owner-identity onboarding deadlock that blocks every fresh install of harmony-client v0.1.0-alpha. After this spec ships, a tester who downloads the alpha for the first time can complete the path from "click install" to "land in a community feed with messages flowing" without hitting a dead-end dialog or needing developer assistance.

## 2. Background — the deadlock

Surfaced during 2026-05-28 alpha bring-up:

1. **`start_node`** (`src-tauri/src/lib.rs:1678`) tolerates the absence of an owner identity. It runs `load_owner_state` (`src-tauri/src/lib.rs:2146`), proceeds successfully if the result is `None`, and returns `StartNodeResponse { node_addr, freshly_created }` with `freshly_created` reflecting **device-identity** freshness (the iroh secret key), **not owner-identity** state. Frontend has no signal that owner identity is missing.
2. **`WelcomeModal`** ([ZEB-331](https://linear.app/zeblith/issue/ZEB-331)) mounts iff `freshly_created == true` and offers only two paths: `[Skip]` and `[Join with invite]`. Neither mints an owner identity. `Skip` lands the user in the main UI in a half-broken state. `[Join with invite]` calls `redeem_invite` (`src-tauri/src/lib.rs:15991`) which extracts `crdt_state.clone().ok_or("crdt_state missing — node not running?")` (`src-tauri/src/lib.rs:16017`) — fails the same way as `create_community`.
3. **`create_community`** (`src-tauri/src/lib.rs:14030`) requires `crdt_state` plus eight other owner-loaded fields. With no owner identity, every owner-touching IPC returns the misleading error `"crdt_state missing — node not running?"`. Counted: 144 sites in `src-tauri/src/lib.rs` alone.
4. **`mint_owner_identity`** (`src-tauri/src/owner_commands.rs:158`) is the legitimate path to acquire an owner identity. It calls `require_node_stopped` (`src-tauri/src/owner_commands.rs:71`) which returns `Err("Stop the node before minting an owner identity ...")` whenever the event-loop thread is running. The `stop_node` IPC (`src-tauri/src/lib.rs:5285`) exists but has no UI affordance — `grep -rn "stop_node" src/` returns zero hits. The user has no in-app path to stop the node.

Result: the only successful exit from a fresh install in v0.1.0-alpha is to quit the app, manually edit `~/.harmony/owner_state.cbor`, restart. Not viable for testers.

## 3. Architecture — four-state hard-gated state machine

A single state machine governs the app from launch to main-UI-reachable, with the owner-identity hard gate as the architectural backbone:

```
              ┌──────────────────────────────────────────────┐
              │ STATE 0: app launches, start_node runs       │
              │ start_node detects owner_state.cbor absent   │
              │ → returns { hasOwnerIdentity: false, ... }   │
              └─────────────────────┬────────────────────────┘
                                    ↓ frontend mounts
              ┌──────────────────────────────────────────────┐
              │ STATE 1: WelcomeModal (hard gate)            │
              │ - No backdrop dismiss, no Esc                │
              │ - Pane 1: explainer + [Create my identity]   │
              │ - Pane 2: backup + [Save file] / [Skip]      │
              └─────────────────────┬────────────────────────┘
                                    ↓ user clicks Create
              ┌──────────────────────────────────────────────┐
              │ STATE 2: minting in-flight                   │
              │ mint_owner_identity self-lifecycle IPC:      │
              │   1. stop_node_inner                         │
              │   2. mint + write cbor + keychain            │
              │   3. start_node_inner (loads owner)          │
              │   4. return MintIpcResult                    │
              │ Frontend shows "Creating your identity…"     │
              │ spinner; ~3-second wall clock.               │
              └─────────────────────┬────────────────────────┘
                                    ↓ mint returns
              ┌──────────────────────────────────────────────┐
              │ STATE 3: owner-loaded, main UI accessible    │
              │ crdt_state, hlc_tracker, dm_outbox,          │
              │ community_registry all Some.                 │
              │ Welcome closes; deep-link router drains      │
              │ any pending invite → redeem_invite fires.    │
              │ All 144 IPC sites' owner-required paths      │
              │ now succeed.                                 │
              └──────────────────────────────────────────────┘
```

**Invariants this enforces:**

1. **State 3 is the only state from which user-facing IPCs are reachable.** Hard gate guarantees `crdt_state == None` is never observable to the user.
2. **`mint_owner_identity` is the only transition out of State 1.** No backdoor "skip" path; WelcomeModal cannot be dismissed without minting.
3. **State 2 is bounded.** Self-lifecycle mint takes the node down and back up atomically; no half-states leak.
4. **Backup is post-mint, not in-mint.** Mint succeeds before backup is offered; "Skip backup" is reversible (user can back up later from Settings).

## 4. Backend changes

### 4.1 `StartNodeResponse` gains `has_owner_identity: bool`

```rust
// src-tauri/src/lib.rs (current shape at line 1005)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartNodeResponse {
    pub node_addr: String,
    pub freshly_created: bool,    // device identity freshness — unchanged
    // NEW:
    pub has_owner_identity: bool, // true iff owner_state.cbor loaded
}
```

Captured at line 2150 alongside `owner_loaded`:

```rust
let has_owner_identity = owner_loaded.is_some();
// ... existing code populates owner-derived fields conditional on owner_loaded ...
```

Threaded through to all three `StartNodeResponse` construction sites in `start_node` (`lib.rs:4916`, `4931`, `5263`). Forward-compat: frontend `StartNodeResponse` interface declares `hasOwnerIdentity?: boolean` with a `false` default for missing field (old clients are treated as needing onboarding — safe default).

### 4.2 `mint_owner_identity` becomes self-lifecycle

Remove the `require_node_stopped` fast-fail (`owner_commands.rs:166`). The IPC takes responsibility for the full transition:

```rust
#[tauri::command]
pub async fn mint_owner_identity(
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<crate::NodeState>>,
) -> Result<MintIpcResult, String> {
    let identity_dir = resolve_identity_dir()?;

    // Idempotent failure if already minted — existing guard, kept.
    if identity_dir.join("owner_state.cbor").exists() {
        return Err("Owner identity already exists on this device. Wipe via Settings to re-mint.".into());
    }

    // Phase 1: stop the node (idempotent — no-op if already stopped).
    // Extracted from the existing stop_node IPC body.
    stop_node_inner(&app, &state).await?;

    // Phase 2: mint + persist (existing logic from owner_commands.rs:192-214).
    // Held under OWNER_STATE_WRITE_LOCK to serialize concurrent attempts.
    let mint_result = run_blocking(move || {
        let _owner_write_guard = OWNER_STATE_WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        if identity_dir.join("owner_state.cbor").exists() {
            return Err("Owner identity already exists on this device. Wipe via Settings to re-mint.".to_string());
        }
        let MintResult { state: owner_state, recovery_artifact, device_signing_key } =
            mint_owner(now_unix()).map_err(|e| format!("mint_owner: {e}"))?;
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

    // Phase 3: restart the node — picks up the freshly-written owner_state.cbor.
    // Extracted from the existing start_node IPC body. Same arguments as the
    // boot-time call (endpoint = None).
    start_node_inner(&app, &state, None).await
        .map_err(|e| format!("Node restart failed after mint: {e}"))?;

    Ok(mint_result)
}
```

**Extraction note:** `stop_node_inner` and `start_node_inner` are extracted as `pub(crate)` helpers from the existing `stop_node` (`src-tauri/src/lib.rs:5285`) and `start_node` (`src-tauri/src/lib.rs:1678`) `#[tauri::command]` bodies. The Tauri-command wrappers stay (preserves the IPC surface for direct callers like the DevicesPanel "Create owner identity" button). The helpers contain the actual logic; the commands become one-line forwarders.

**Extraction risk (implementer flag):** `start_node` is large (lines 1678–5283 with intermediate sites) and threads state through closures, channel construction, runtime spawns, and error-path cleanup. If the extraction can't cleanly produce a callable-from-arbitrary-IPC inner function (e.g., the function signature can't lift cleanly because `tauri::State<'_, _>` lifetimes intertwine with `.await` boundaries), the implementer should:
1. Try the minimal extraction first — lift only the body, keep current parameter list.
2. If that hits lifetime / `Send` issues, refactor to a free function taking explicit owned handles.
3. If that requires structural changes beyond ~200 LOC of refactor, surface as DONE_WITH_CONCERNS and we re-scope to hot-load (which avoids the extraction entirely by populating the owner-derived fields on the running NodeState directly — riskier code path, but does NOT require touching `start_node` or `stop_node`).

The extraction approach is preferred because it reuses already-tested code; the hot-load fallback is the contingency.

**ZEB-335 disposition:** the DevicesPanel "Create owner identity" button (`src/lib/components/DevicesPanel.svelte:495`) calls the same `mint_owner_identity` IPC unchanged at the call site. The "Stop the node first" error message simply stops being reachable. ZEB-335 closes on merge.

### 4.3 `require_owner_loaded` helper for new code; phrasing sweep for old sites

New file `src-tauri/src/owner_loaded.rs`:

```rust
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;
use std::collections::BTreeMap;
use ed25519_dalek::SigningKey;

use crate::community_state_sync::{CommunitySyncRegistry, CommunityAdapterRequest};
use crate::channel_log_registry::ChannelLogRegistry;
use crate::dm_outbox::DmOutbox;
use crate::owner_state_crdt::OwnerState;
use crate::owner_state_types::{Hlc, OwnerAddr};

pub struct OwnerLoadedHandles {
    pub crdt_state: Arc<TokioMutex<OwnerState>>,
    pub hlc_tracker: Arc<TokioMutex<BTreeMap<String, Hlc>>>,
    pub device_id: String,
    pub self_owner: OwnerAddr,
    pub community_registry: Arc<CommunitySyncRegistry>,
    pub community_adapter_request_tx: mpsc::Sender<CommunityAdapterRequest>,
    pub channel_log_registry: Arc<ChannelLogRegistry>,
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

pub fn require_owner_loaded(state: &Mutex<crate::NodeState>) -> Result<OwnerLoadedHandles, OwnerLoadError> {
    let g = state.lock().map_err(|e| OwnerLoadError::LockPoisoned(e.to_string()))?;
    Ok(OwnerLoadedHandles {
        crdt_state: g.crdt_state.clone().ok_or(OwnerLoadError::NotLoaded)?,
        hlc_tracker: g.hlc_tracker.clone().ok_or(OwnerLoadError::NotLoaded)?,
        device_id: g.dm_device_id.clone().ok_or(OwnerLoadError::NotLoaded)?,
        self_owner: g.dm_self_owner.ok_or(OwnerLoadError::NotLoaded)?,
        community_registry: g.community_registry.clone().ok_or(OwnerLoadError::NotLoaded)?,
        community_adapter_request_tx: g.community_adapter_request_tx.clone().ok_or(OwnerLoadError::NotLoaded)?,
        channel_log_registry: g.channel_log_registry.clone().ok_or(OwnerLoadError::NotLoaded)?,
        dm_outbox: g.dm_outbox.clone().ok_or(OwnerLoadError::NotLoaded)?,
        generation: g.generation,
    })
}
```

**Old-site sweep:** every `"crdt_state missing — node not running?"` / `"X missing — node not running?"` / `"X missing — no owner identity?"` string in `src-tauri/src/lib.rs` (144 sites per `grep -c`) gets a single bulk replace to:

```
"Owner identity not loaded — please restart the app or recreate identity."
```

Same `Result<_, String>` shape; no semantic change. New mint-flow code (the helper-based IPCs) emits the typed `OwnerLoadError` and surfaces via `String::from(OwnerLoadError)` for IPC return.

**Migration policy:** the helper is the recommended pattern for new IPCs that need the owner-loaded handles. Existing IPCs migrate incrementally as they're touched for other reasons. No mass-migration of the 144 sites in this spec.

## 5. Frontend changes

### 5.1 WelcomeModal as a two-pane hard gate

**File:** `src/lib/components/WelcomeModal.svelte`

**Props change:**

```ts
interface Props {
  open: boolean;
  onMinted: (mintResult: MintIpcResult) => void | Promise<void>;
}
```

`onDismiss` and `onJoinWithInvite` go away. The modal is a hard gate — no skip path.

**State:**

```ts
type Stage = 'explain' | 'minting' | 'backup' | 'skip-confirm';
let stage = $state<Stage>('explain');
let mintResult = $state<MintIpcResult | null>(null);
let mintError = $state<string | null>(null);
let backupPassphrase = $state('');
let backupError = $state<string | null>(null);
```

**Pane 1 (`stage === 'explain'`):**

```
  Welcome to Harmony

  Harmony is a federated, polycentric social fabric built on
  user-owned identity. Your identity lives ONLY on this device —
  there's no central account, no server holding your data.

  When you create your identity, you'll get a recovery artifact
  to back up. Save it somewhere safe — it's the only way to
  prove this identity is yours if you ever lose this device.

  Single-device only in v0.1.0-alpha — multi-device sync ships
  in a later release.

  [Create my identity]   ← single primary action
```

On `[Create my identity]` click: `stage = 'minting'` → `mintError = null` → `await invoke('mint_owner_identity')` → on success: `mintResult = result; stage = 'backup'` → on error: `mintError = e instanceof Error ? e.message : String(e); stage = 'explain'`. Error rendered inline.

**Pane 2 (`stage === 'backup'`):**

```
  Your identity is ready

  Back up your recovery artifact NOW. Without it, you can't
  prove this identity is yours if this device is lost.

  The recovery file is encrypted with your passphrase. Save it
  somewhere safe (USB drive, password manager attachment, etc.).

  Passphrase:        [______________________________]  (≥8 chars)

  [Save recovery file]   [Skip for now]
```

The master_seed itself is never rendered in the UI — it lives only on disk (encrypted via the passphrase into the saved file) and in the OS keychain. `MintIpcResult.recoveryToken` returned by the mint IPC is a UUID reference to the in-memory seed; the frontend passes it through to `export_owner_recovery_file_to_path` and never displays it. The pane shows only the prompts and buttons — no seed material in any form.

**Privacy-invariant (defensive):** pane 2 default rendering does NOT contain any string matching `/[0-9a-f]{32,}/` — this guards against future bugs where a developer accidentally surfaces the recovery token or seed material into the DOM. Tested via the regex check in §8.2.

`[Save recovery file]` (primary, disabled when `backupPassphrase.length < 8`):
- `import { save } from '@tauri-apps/plugin-dialog';`
- `const path = await save({ defaultPath: 'harmony-recovery.bin', filters: [{ name: 'Recovery file', extensions: ['bin'] }] });`
- `if (path === null) return;` (user cancelled dialog)
- `await invoke('export_owner_recovery_file_to_path', { recoveryToken: mintResult.recoveryToken, pathToken: path, passphrase: backupPassphrase, comment: null });`
- On success: `localStorage.setItem('recoveryArtifactBackedUp', 'true');` → `onMinted(mintResult);` → modal closes.
- On error: `backupError = e instanceof Error ? e.message : String(e);` (inline error; pane stays).

`[Skip for now]` → `stage = 'skip-confirm'`.

**Pane 3 (`stage === 'skip-confirm'`):**

```
  Are you sure?

  Without a backup, if you lose this device you lose this
  identity permanently. There's no central recovery — this
  is what "self-sovereign" means.

  [Cancel]                          [I accept the risk]
```

`[Cancel]` → `stage = 'backup'`.

`[I accept the risk]` → `localStorage.setItem('welcomeAcknowledged', 'true');` → `onMinted(mintResult);` → modal closes (`recoveryArtifactBackedUp` stays unset; banner will show post-close).

**Hard-gate enforcement:**
- No backdrop click handler.
- No Esc key handler.
- No close button (`×`) in the modal chrome.
- `open` prop wired only to `!hasOwnerIdentity` in App.svelte — backend is the source of truth.

### 5.2 App.svelte boot integration

**At boot (existing line ~668 area):**

```ts
const { nodeAddr, freshlyCreated, hasOwnerIdentity }
  = await invoke<StartNodeResponse>('start_node', { endpoint: null });

let showWelcomeModal = $state(!hasOwnerIdentity);
let hasOwnerIdentityState = $state(hasOwnerIdentity);  // mutated by onMinted
```

`freshlyCreated` is retained for any device-identity-freshness-aware behavior elsewhere but no longer gates Welcome.

**`onMinted` handler:**

```ts
async function onMinted(result: MintIpcResult) {
  hasOwnerIdentityState = true;
  showWelcomeModal = false;
  const queued = consumeQueuedInvite();
  if (queued !== null) {
    try {
      await invoke('redeem_invite', { url: queued });
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      toast.error(`Couldn't redeem your invite right now (${msg}). Try pasting it again from the Help menu.`);
    }
  }
}
```

### 5.3 Deep-link queue + post-mint drain

**File:** `src/lib/deep-link-router.ts` (extended)

Add a module-level queue field:

```ts
let pendingInviteUrl: string | null = null;  // plain `let`, NOT $state — accessed only via fns

export function queueInviteForPostMint(url: string): void {
  pendingInviteUrl = url;
}

export function consumeQueuedInvite(): string | null {
  const url = pendingInviteUrl;
  pendingInviteUrl = null;
  return url;
}
```

**Deep-link handler update (App.svelte ~line 809):**

```ts
async function handleDeepLink(url: string) {
  const validated = extractHarmonyInviteUrl([url]);
  if (validated === null) {
    toast.error("That doesn't look like a harmony:// invite.");
    return;
  }
  if (!hasOwnerIdentityState) {
    queueInviteForPostMint(validated);
    return;  // WelcomeModal will drain after mint
  }
  try {
    await invoke('redeem_invite', { url: validated });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    toast.error(`Couldn't redeem invite: ${msg}`);
  }
}
```

**"Consume once" semantics:** if auto-redeem fails (network, peer offline, etc.), the queue is NOT repopulated. User retries via the Help menu's "Paste invite URL" affordance, same as a normal post-mint user.

### 5.4 BackupReminderBanner (new component)

**File:** `src/lib/components/BackupReminderBanner.svelte`

Mount in `Layout.svelte` near the top of the main app chrome. Single-line banner with:

```
⚠ Your identity hasn't been backed up.   [Back up now]   [Dismiss]
```

**Show condition:**

```ts
let visible = $state(false);

onMount(() => {
  const acknowledged = localStorage.getItem('welcomeAcknowledged') === 'true';
  const backedUp = localStorage.getItem('recoveryArtifactBackedUp') === 'true';
  const dismissedThisSession = sessionStorage.getItem('backupBannerDismissed') === 'true';
  visible = acknowledged && !backedUp && !dismissedThisSession;
});
```

`[Back up now]` opens the same `save()` dialog + `export_owner_recovery_file_to_path` flow as Welcome pane 2. On success: `localStorage.setItem('recoveryArtifactBackedUp', 'true');` → `visible = false`.

`[Dismiss]` → `sessionStorage.setItem('backupBannerDismissed', 'true');` → `visible = false`. Banner returns on the next session — sticky reminder, not permanent dismiss.

**localStorage / sessionStorage contract:**

| Key | Storage | Set when | Used by |
|---|---|---|---|
| `recoveryArtifactBackedUp` | localStorage | Successful `export_owner_recovery_file_to_path` | BackupReminderBanner (hides banner) |
| `welcomeAcknowledged` | localStorage | User clicks `[I accept the risk]` in Welcome pane 3 | BackupReminderBanner (gates "show iff acknowledged but not backed up") |
| `backupBannerDismissed` | sessionStorage | User clicks `[Dismiss]` on banner | BackupReminderBanner (hides for current session) |

## 6. Data flow (six scenarios)

### Flow 1 — cold start, no owner identity, no deep-link (the bug Jake hit)

```
App launch
  → start_node IPC
  → returns { nodeAddr, freshlyCreated: true, hasOwnerIdentity: false }
  → showWelcomeModal = true
  → WelcomeModal stage='explain'
  → user clicks [Create my identity]
  → stage='minting'; await invoke('mint_owner_identity')
    backend: stop_node_inner → mint → save cbor + keychain → start_node_inner
    returns MintIpcResult { state, recoveryToken }
  → stage='backup'
  → user clicks [Save recovery file]
    → save() dialog → passphrase prompt
    → invoke('export_owner_recovery_file_to_path', { recoveryToken, pathToken: path, passphrase, comment: null })
    → success: localStorage.recoveryArtifactBackedUp = 'true'
  → WelcomeModal closes, onMinted fires
  → consumeQueuedInvite returns null → no auto-redeem
  → main UI lands; crdt_state populated; create_community works
```

### Flow 2 — cold start, no owner, user clicks Skip

```
Same as Flow 1 until stage='backup'
  → user clicks [Skip for now]
  → stage='skip-confirm'
  → user clicks [I accept the risk]
    → localStorage.welcomeAcknowledged = 'true'
    → recoveryArtifactBackedUp stays unset
  → WelcomeModal closes, onMinted fires
  → main UI mounts BackupReminderBanner (visible == true)
  → reminder persists across relaunches until user backs up via banner [Back up now]
```

### Flow 3 — cold start with deep-link invite (KRILE clicks Koya's harmony://invite)

```
App launches with harmony://invite/... URL via OS deep-link handler
  → deep-link handler fires; awaits hasOwnerIdentityState
  → start_node returns { hasOwnerIdentity: false }
  → handleDeepLink: hasOwnerIdentityState === false → queueInviteForPostMint
  → showWelcomeModal = true
  → WelcomeModal hard-gate; user mints → backs up (or skips) → onMinted
  → consumeQueuedInvite returns the URL
  → auto-invoke('redeem_invite', { url }) — succeeds because crdt_state populated
  → user lands in the community feed
```

### Flow 4 — warm start, owner identity exists (returning user)

```
App launch
  → start_node returns { hasOwnerIdentity: true }
  → showWelcomeModal = false
  → main UI lands directly
  → BackupReminderBanner.visible = welcomeAcknowledged && !backedUp
    (true iff user previously skipped backup AND hasn't backed up since)
```

### Flow 5 — mint succeeds but auto-redeem fails (transient network)

```
Flow 1 or 3 succeeds through mint
  → consumeQueuedInvite returns URL
  → invoke('redeem_invite') rejects (network error, peer offline, etc.)
  → toast.error("Couldn't redeem your invite right now (${msg}). Try pasting it again from the Help menu.")
  → main UI lands; community not joined; backup banner behavior per Flow 1/2
  → user can retry via Help menu's "Paste invite URL" affordance
```

The invite is NOT re-queued on failure. Once consumed, it's consumed. Retry path is identical to a normal post-mint paste.

### Flow 6 — force-quit during Welcome, relaunch

```
User force-quits app mid-Welcome (stage='explain' or 'minting' pre-cbor-write)
  → owner_state.cbor never written
  → next launch: start_node returns { hasOwnerIdentity: false }
  → WelcomeModal mounts again from stage='explain'
  → no state loss; user proceeds normally
```

If the force-quit happened **after** the cbor write but before start_node_inner restart returned, the next launch sees `hasOwnerIdentity == true` and skips Welcome entirely; the user landed past the gate organically. Recovery banner still shows iff backup was skipped (the localStorage flag won't be set because the modal didn't reach `onMinted`).

## 7. Error handling

### 7.1 Mint IPC errors

| Error | Condition | Frontend response |
|---|---|---|
| `"Owner identity already exists on this device. Wipe via Settings to re-mint."` | Idempotent guard hit (owner_state.cbor exists). Should be unreachable from WelcomeModal because the hard gate is keyed on `hasOwnerIdentity == false`. | Toast + close modal (likely indicates race or test scaffolding hit prod) |
| `"Failed to write owner state: <io>"` | Disk full / permission denied during atomic write | Inline on pane 1; user retries |
| `"Failed to write to keychain: <kc>"` | Keychain unavailable (headless test env / locked keychain). `save_owner_state_atomic` rolls back the cbor write on keychain failure (existing behavior). | Inline on pane 1; user retries |
| `"Node restart failed after mint: <inner>"` | `start_node_inner` errored after mint succeeded. cbor + keychain ARE written; running node is half-broken. | Toast: `"Identity created, but the app needs to restart. Please quit and relaunch."`; modal closes (mint did succeed); next launch loads owner cleanly |

**No rollback on mint cbor write.** Even if `start_node_inner` fails afterward, the mint succeeded — rolling it back would lose the user's identity. The cost of a force-quit-and-relaunch is acceptable.

### 7.2 Backup-export errors

| Error | Condition | Frontend response |
|---|---|---|
| `"Recovery passphrase must be at least 8 characters."` | Existing validation in `export_owner_recovery_file_to_path` (`owner_commands.rs:231`) | Inline on pane 2; user fixes passphrase |
| `"Failed to write recovery file: <io>"` | Picked path is unwritable | Inline on pane 2; user picks different path |
| `"Token expired or invalid"` | In-memory recovery token consumed or timed out | Toast: `"Recovery export expired — please mint again or use Settings → Export Backup."`; modal closes |

Skip path remains available across all backup errors — backup is reversible from Settings later.

### 7.3 Deep-link / auto-redeem errors

| Error | Condition | Frontend response |
|---|---|---|
| `"That doesn't look like a harmony:// invite."` | `extractHarmonyInviteUrl` rejects | Toast; queue cleared; no auto-redeem |
| `"redeem_invite: already a member"` or similar | User clicked invite for a community they already joined | Toast: `"You're already in {community-name}."`; no state change |
| `"redeem_invite: <network error>"` | Transient connectivity (Flow 5) | Toast with retry suggestion; no re-queue |

### 7.4 IPC reentrancy during mint

Per architecture §3, the running node is **stopped** during the mint window. Any concurrent IPC call from the frontend during the mint window hits `crdt_state == None` and returns the existing error.

**Backend mitigation:** `OWNER_STATE_WRITE_LOCK` (`owner_commands.rs:43`) serializes mint calls. Concurrent mint attempts block on the lock; the second caller hits the "already exists" guard.

**Frontend mitigation:** WelcomeModal is a hard gate — main UI is not mounted while mint is in-flight, so no other component can invoke owner-required IPCs concurrently. Boot-time `start_node` call is sequential before WelcomeModal mounts.

**Defensive helper:** the new `require_owner_loaded` helper returns `OwnerLoadError::NotLoaded` distinctly. New mint-adjacent code uses this; old sites get the phrasing-only sweep.

### 7.5 `OwnerLoadError` taxonomy

```rust
#[derive(thiserror::Error, Debug)]
pub enum OwnerLoadError {
    #[error("Owner identity not loaded. The app may be restarting after a mint — try again in a moment.")]
    NotLoaded,
    #[error("NodeState lock poisoned: {0}")]
    LockPoisoned(String),
}
```

Frontend branches on error-string discrimination (Tauri rejection passes the string through). Bounded vocabulary; no leaking of internal field names. Frontend tests assert the exact strings; backend tests assert the variants.

### 7.6 Hard-gate enforcement — defense in depth

1. **Frontend (primary):** WelcomeModal renders without backdrop / Esc / close handlers; no skip button. Only path out of `stage='explain'` is a successful mint.
2. **Backend (defensive):** every owner-required IPC continues to return `OwnerLoadError::NotLoaded` (or the swept-phrasing `"Owner identity not loaded — please restart the app or recreate identity."`) if called pre-mint. Even if a future frontend change accidentally bypasses the modal, the backend refuses.

The combination guarantees that "no owner identity" is unreachable via supported UI paths, and degrades gracefully if any layer's invariant slips.

## 8. Testing

### 8.1 Backend unit tests (Rust, `cargo nextest`)

**New file `src-tauri/tests/mint_owner_lifecycle.rs`:**
- `mint_owner_identity_writes_cbor_and_keychain` — empty identity_dir + tempdir keychain, call mint IPC, assert owner_state.cbor exists + keychain entry present.
- `mint_owner_identity_restarts_node_with_owner_loaded` — running node with no owner, call mint, assert post-call `NodeState.crdt_state.is_some()` + `dm_outbox.is_some()` + `community_registry.is_some()`.
- `mint_owner_identity_idempotent_failure_when_already_exists` — call mint twice, assert second returns "already exists" error and the first identity is unchanged.
- `mint_owner_identity_node_restart_failure_preserves_minted_state` — inject `start_node_inner` failure post-mint (test scaffolding); assert cbor + keychain are written (no rollback) and IPC returns the "restart needed" error.

**New file `src-tauri/src/owner_loaded.rs` tests:**
- `require_owner_loaded_returns_handles_when_all_some` — populate every owner field, call helper, assert all handles returned.
- `require_owner_loaded_returns_not_loaded_when_crdt_state_none` — leave `crdt_state = None`, assert `OwnerLoadError::NotLoaded`.
- Parameterized variant: leave each of the 8 owner-loaded fields `None` in turn, assert all yield `NotLoaded`.

**Wire-format tests for `StartNodeResponse`:**
- `start_node_response_serializes_has_owner_identity_in_camel_case` — assert JSON has `hasOwnerIdentity: true/false`.
- `start_node_response_has_owner_identity_true_when_owner_loaded` — integration: pre-populate owner_state.cbor, call `start_node`, assert response.has_owner_identity == true.
- `start_node_response_has_owner_identity_false_when_no_owner` — empty identity_dir, assert false.

**Phrasing-sweep regression guard** (in `src-tauri/tests/error_phrasing_regression.rs`):

```rust
#[test]
fn no_misleading_node_not_running_phrasing_outside_helper() {
    let src = std::fs::read_to_string("src/lib.rs").unwrap();
    let count = src.matches("node not running?").count();
    assert_eq!(
        count, 0,
        "phrasing sweep regression: 'node not running?' leaked back into src/lib.rs"
    );
}
```

### 8.2 Frontend component tests (Vitest, Svelte 5 runes)

**`src/lib/components/__tests__/WelcomeModal.test.ts`:**
- `renders_explain_pane_when_open_and_no_mint_yet`
- `clicks_create_my_identity_invokes_mint_ipc_with_no_args`
- `transitions_to_backup_pane_on_mint_success`
- `stays_on_explain_pane_with_inline_error_on_mint_failure`
- `save_recovery_file_calls_export_ipc_with_passphrase_and_path`
- `skip_for_now_shows_confirmation_step_then_dismisses_via_onMinted`
- `passphrase_validation_under_8_chars_disables_save_button`
- `hard_gate_ignores_Escape_keypress` — fire Esc, assert `open` prop and stage unchanged.
- `hard_gate_ignores_backdrop_click` — click backdrop element, assert modal stays.
- `recovery_artifact_redaction_invariant` — pane 2 default rendering does NOT contain any string matching `/[0-9a-f]{32,}/`. Defensive guard: master_seed and recovery_token never appear in the DOM. Mirrors ZEB-329 R3 leak-test pattern.
- `accept_the_risk_sets_localStorage_welcomeAcknowledged` — assert flag set on skip confirm.
- `save_recovery_file_sets_localStorage_recoveryArtifactBackedUp` — assert flag set on successful export.

**`src/lib/components/__tests__/BackupReminderBanner.test.ts`** (new):
- `mounts_when_welcomeAcknowledged_set_and_no_backup_flag`
- `does_not_mount_when_backup_flag_set`
- `does_not_mount_when_welcome_not_acknowledged` (e.g., user backed up immediately — banner never appears)
- `back_up_now_button_opens_export_dialog`
- `dismiss_hides_for_session_but_returns_on_remount`

**`src/lib/__tests__/deep-link-router.test.ts`** (extended):
- `queueInviteForPostMint_stores_url`
- `consumeQueuedInvite_returns_and_clears`
- `consumeQueuedInvite_returns_null_when_empty`
- `consumeQueuedInvite_idempotent_on_double_call`

**App.svelte integration smoke** (in existing `src/lib/__tests__/App.integration.test.ts` or new file):
- `boot_with_hasOwnerIdentity_false_mounts_WelcomeModal`
- `boot_with_hasOwnerIdentity_true_skips_WelcomeModal`
- `deep_link_during_no_owner_queues_invite_does_not_invoke_redeem`
- `onMinted_drains_queued_invite_and_invokes_redeem`
- `onMinted_with_no_queued_invite_does_not_invoke_redeem`

### 8.3 Wire-format pinning

Add a CBOR fixture `src-tauri/tests/fixtures/start_node_response_v2.cbor` and a test asserting the new shape serializes identically across versions. Forward-compat assertion: deserializing the old shape (without `has_owner_identity`) defaults `has_owner_identity = false`. Mirrors the ZEB-331 wire-format-pinning pattern.

### 8.4 Manual smoke test (v0.1.0-alpha.1 release notes)

Added to `docs/release-process.md §3` smoke-test checklist:

1. Wipe `~/.harmony/` + the `harmony.client` keychain entry on the test machine.
2. Launch the installed alpha.
3. WelcomeModal mounts at `stage='explain'`.
4. Click `[Create my identity]` — spinner shows ~3s — pane transitions to `'backup'`.
5. Click `[Save recovery file]` — pick a temp path — passphrase prompt — succeeds.
6. Modal closes; main UI lands; `+ Create community` succeeds.
7. Quit + relaunch — main UI lands directly (no Welcome); no banner (backed up).
8. Wipe again; repeat; this time click `[Skip for now]` → confirm → main UI shows backup banner; relaunch → banner persists; click `[Back up now]` → save → banner disappears across next launch.

### 8.5 Not in scope for this PR's tests

- **Live deep-link auto-redeem.** Frontend unit tests cover the queue/drain logic; the live cross-device test (Flow 3) is part of the Koya↔KRILE bring-up resumption AFTER this PR ships, not part of this spec's CI gate.
- **Multi-device recovery / restore.** Out of scope per ZEB-173 not yet wired into client.

### 8.6 CI gates (unchanged per `feedback_ci_disabled`)

Backend (from `src-tauri/`):
- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- `HARMONY_LARGE_TESTS=1 cargo nextest run --locked --features test-fixtures --no-fail-fast --test folder_ingest_walker_integration -E 'test(nested_bundle_tree_round_trip)'`
- MSRV: `cargo check --locked --all-targets --features test-fixtures` with declared rust-version

Frontend (from repo root):
- `npx tsc --noEmit`
- `npx vitest run`

## 9. Files

### 9.1 New files

| Path | Purpose |
|---|---|
| `src-tauri/src/owner_loaded.rs` | `OwnerLoadedHandles` + `OwnerLoadError` + `require_owner_loaded` helper |
| `src-tauri/tests/mint_owner_lifecycle.rs` | Integration tests for self-lifecycle mint |
| `src-tauri/tests/error_phrasing_regression.rs` | Phrasing-sweep regression grep test |
| `src-tauri/tests/fixtures/start_node_response_v2.cbor` | Wire-format pinning fixture |
| `src/lib/components/BackupReminderBanner.svelte` | Persistent skipped-backup reminder |
| `src/lib/components/__tests__/BackupReminderBanner.test.ts` | Banner component tests |

### 9.2 Modified files

| Path | Changes |
|---|---|
| `src-tauri/src/lib.rs` | `StartNodeResponse` gains `has_owner_identity`; `start_node`'s 3 response-construction sites updated; phrasing sweep of 144 sites; extract `start_node_inner` / `stop_node_inner`; export new module |
| `src-tauri/src/owner_commands.rs` | `mint_owner_identity` becomes self-lifecycle (drops `require_node_stopped` fast-fail; adds stop+restart wrapping); helper kept for direct callers |
| `src/lib/types/onboarding.ts` | `StartNodeResponse` adds `hasOwnerIdentity?: boolean` |
| `src/lib/components/WelcomeModal.svelte` | Two-pane hard-gated implementation; props change (`onMinted` replaces `onDismiss`/`onJoinWithInvite`); no Esc/backdrop handlers |
| `src/lib/components/__tests__/WelcomeModal.test.ts` | Tests updated to new contract |
| `src/lib/deep-link-router.ts` | `queueInviteForPostMint` / `consumeQueuedInvite` exports |
| `src/lib/__tests__/deep-link-router.test.ts` | Queue tests added |
| `src/App.svelte` | `StartNodeResponse` destructure adds `hasOwnerIdentity`; `showWelcomeModal` gated on it; `onMinted` handler drains queued invite; deep-link handler queues on no-owner |
| `src/lib/components/Layout.svelte` | Mount `BackupReminderBanner` |
| `docs/release-process.md` | Smoke-test checklist §3 updated with first-run flow |

## 10. Out of scope (explicitly deferred)

- **Restore from existing recovery file.** Multi-device single-owner identity requires ZEB-173 binding wired into client. Alpha is single-device. UI for "I already have an identity, restore it here" is a follow-up ticket post-alpha.
- **Profile / display-name model.** ZEB-336 covers the per-device vs per-owner question and the pre-binding state question. Independent decision; this spec doesn't lock either way.
- **Self-chat / notepad default empty state.** ZEB-334. Orthogonal — that fix changes what the user sees when they have no community joined, regardless of whether they have an owner identity.
- **Nav-bar overflow on Windows.** ZEB-333. Pure UI bug; unrelated to onboarding state machine.
- **"You" label in feed.** ZEB-337. Polish; orthogonal.
- **Full migration of 144 IPC sites to `require_owner_loaded`.** Phrasing sweep only in this spec. Incremental migration as sites are touched for other reasons.
- **Multi-step onboarding wizard** (display name capture, etc.). Single-pane explainer + single-pane backup. Display name is a Settings affordance, not a Welcome step.
- **Hot-load owner identity into a running node.** Self-lifecycle stop+restart is the chosen architecture. Hot-load is left as a possible future optimization; not required for the alpha experience.

## 11. Release vehicle

**v0.1.0-alpha.1.** Re-runs `release.yml` against the new tag. Auto-updater pushes the fix to existing v0.1.0 installs (including KRILE).

**Definition of done** per ZEB-338:
1. This spec committed and Jake-approved.
2. Implementation plan at `docs/plans/2026-05-28-zeb-338-first-run-state-machine-plan.md` committed.
3. PR opened against `origin/main`, passes all 5 backend + 2 frontend CI gates.
4. Bot review loop converges (CodeRabbit / Cursor / CodeAnt / Qodo).
5. v0.1.0-alpha.1 release draft published with smoke-test checklist from §8.4.
6. Jake smoke-tests on Koya (wipe + reinstall + walk the flow); Koya↔KRILE bring-up resumes from this artifact.

## 12. Brainstorm decisions log

For traceability — the multiple-choice answers that locked this design:

| Q | Decision |
|---|---|
| Q1: Is "no owner identity" a reachable runtime state? | **No — hard gate.** |
| Q2: What does the Welcome→mint flow look like? | **Two-step: explain → mint + show backup. Display name deferred to Settings.** |
| Q3: How does mint transition the node? | **Self-lifecycle: stop_node_inner → mint → start_node_inner. ~3s spinner.** |
| Q4: How strict is the backup gate? | **Single-click skip with severity-confirm.** Persistent banner if skipped. |
| Q5: How does Welcome handle deep-link invites? | **Invite-agnostic Welcome + post-mint auto-redeem from queue.** |
| Q6: Error-phrasing overhaul scope? | **Helper for new code + phrasing sweep for the 144 old sites. No mass-migration.** |
