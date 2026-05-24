# ZEB-328 Sub-project A: Build & Release Pipeline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce v0.1.0-alpha desktop artifacts (macOS/Windows/Linux) that hand-picked alpha testers can download, install, and auto-update. Wire harmony:// deep-links so click-to-join works post-install.

**Architecture:** GitHub Actions workflow (`workflow_dispatch`-triggered, matrix-built, fan-in-released) + Tauri 2 updater plugin (ed25519-signed bundles, GitHub-Pages-hosted manifest) + Tauri 2 deep-link plugin (OS-level `harmony://` registration). All artifacts OS-unsigned; testers walk past Gatekeeper/SmartScreen using documented workarounds.

**Tech Stack:** Tauri 2.10.x, Rust (workspace at `src-tauri/`), Svelte 5 + Vite, GitHub Actions, ed25519 signing keys via Tauri CLI.

**Spec:** `docs/specs/2026-05-24-zeb-328-build-release-pipeline-design.md` (commit `ecaaa00` on branch `zeb-328-build-release-pipeline-spec`).

**Parent epic:** [ZEB-327](https://linear.app/zeblith/issue/ZEB-327).

**Branch:** `zeb-328-build-release-pipeline-spec` (already exists, off latest origin/main `c5c4da9`). This plan + all implementation commits land on the same branch; final task pushes and opens the PR.

---

## Operator-only prerequisites (NOT subagent-doable)

These actions can happen AFTER the PR merges, BEFORE the first real release. They're documented here for context — the PR ships the workflow file + plugin wiring that REQUIRE these prerequisites to be done by the operator (zeblith) before the workflow can complete a real release end-to-end. Subagent implementers do NOT execute these steps.

- **OP-1:** Generate Tauri signing keypair locally:
  ```bash
  npx @tauri-apps/cli signer generate -w ~/.tauri/harmony-updater.key
  ```
  Output: `~/.tauri/harmony-updater.key` (private) + `~/.tauri/harmony-updater.key.pub` (public).
- **OP-2:** Set GitHub Actions repository secrets:
  - `TAURI_SIGNING_PRIVATE_KEY` = contents of `~/.tauri/harmony-updater.key`
  - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` = passphrase used during generation
- **OP-3:** Embed the public key in `tauri.conf.json` (Task 3 leaves a placeholder; replace with real value from `~/.tauri/harmony-updater.key.pub` before triggering first release).
- **OP-4:** Back up the signing keypair + passphrase to 1Password + physical lockbox (per spec §6.3).
- **OP-5:** Create `gh-pages` branch from an empty commit (per spec §6.5):
  ```bash
  git checkout --orphan gh-pages
  git rm -rf .
  git commit --allow-empty -m "init gh-pages"
  git push origin gh-pages
  git checkout main
  ```
- **OP-6:** In GitHub repo Settings → Pages, set source = `gh-pages` branch, root.
- **OP-7:** Verify the repo's visibility:
  ```bash
  gh repo view zeblithic/harmony-client --json visibility
  ```
  GitHub Actions minutes free-tier applies only to public repos (spec §10.3 hedge).

After OP-1 through OP-7 are complete, `gh workflow run release.yml -f version=0.1.0-alpha.1` will produce the first release end-to-end.

---

## Task 0: Pre-flight + baseline

No commit. Confirm starting state.

**Files:** none (read-only checks).

- [ ] **Step 1: Confirm branch + latest main**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  git branch --show-current
  # Expected: zeb-328-build-release-pipeline-spec
  git log --oneline -3
  # Expected top commit: ecaaa00 docs(zeb-328): amend spec §5.2 + §11.1 …
  git fetch origin
  git log origin/main..HEAD --oneline
  # Expected: 2 commits (339285e spec, ecaaa00 amendment)
  ```

- [ ] **Step 2: Capture baseline gate status**

  ```bash
  cd src-tauri
  cargo fmt --all -- --check; echo "FMT=$?"
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5; echo "CLIPPY=$?"
  ```
  Record exit codes. These must stay green after every task.

- [ ] **Step 3: Confirm Tauri version**

  ```bash
  grep '"@tauri-apps/cli"' /Users/zeblith/work/zeblithic/harmony-client/package.json
  # Expected: "@tauri-apps/cli": "^2.10.1"
  grep '^tauri ' src-tauri/Cargo.toml
  # Expected: tauri = ... matching 2.x
  ```
  All plugin versions added in subsequent tasks must be compatible with this Tauri version. If Tauri version is not 2.10.x, halt and report.

- [ ] **Step 4: Confirm no pre-existing rust-toolchain.toml**

  ```bash
  ls /Users/zeblith/work/zeblithic/harmony-client/rust-toolchain.toml 2>&1 || echo "absent (good)"
  ls /Users/zeblith/work/zeblithic/harmony-client/src-tauri/rust-toolchain.toml 2>&1 || echo "absent (good)"
  ```
  If a `rust-toolchain.toml` already exists, Task 1 becomes a no-op verification rather than a creation step.

---

## Task 1: Pin Rust toolchain

**Files:**
- Create: `rust-toolchain.toml` (repo root)

- [ ] **Step 1: Identify current Rust version**

  ```bash
  rustc --version
  # Expected output: rustc 1.X.Y (commit-hash YYYY-MM-DD)
  ```
  Use whatever version is reported; pin to it for stability.

- [ ] **Step 2: Create `rust-toolchain.toml`**

  ```toml
  [toolchain]
  channel = "1.X.Y"   # ← replace with actual version from Step 1
  components = ["clippy", "rustfmt"]
  ```
  Place at the repo root (NOT in `src-tauri/`) so it applies to the workspace.

- [ ] **Step 3: Verify rust-toolchain.toml is honored**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  rustup show active-toolchain
  # Expected: 1.X.Y-...
  ```

- [ ] **Step 4: Run baseline gates with the pinned toolchain**

  ```bash
  cd src-tauri
  cargo fmt --all -- --check && echo "FMT OK"
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -5
  ```
  Both must pass.

- [ ] **Step 5: Commit**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  git add rust-toolchain.toml
  git commit -m "build(zeb-328): pin Rust toolchain via rust-toolchain.toml"
  ```

---

## Task 2: Add Tauri updater + deep-link plugin deps

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `package.json`

- [ ] **Step 1: Determine the latest 2.x versions of both plugins compatible with Tauri 2.10**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
  cargo search tauri-plugin-updater --limit 1
  cargo search tauri-plugin-deep-link --limit 1
  ```
  Pin to the major version reported (e.g., `^2.0` — Tauri 2 plugins follow Tauri's 2.x major).

- [ ] **Step 2: Add Rust deps**

  Add these lines to `src-tauri/Cargo.toml` under `[dependencies]` (alphabetical placement next to `tauri-plugin-dialog`):

  ```toml
  tauri-plugin-deep-link = "2"
  tauri-plugin-updater = "2"
  ```

- [ ] **Step 3: Add JS deps**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  npm install @tauri-apps/plugin-deep-link@^2 @tauri-apps/plugin-updater@^2
  ```
  This updates `package.json` + `package-lock.json` together.

- [ ] **Step 4: Verify the workspace still compiles**

  ```bash
  cd src-tauri
  cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -10
  ```
  Must exit 0. (No plugin INIT code yet — just adding deps shouldn't break anything.)

- [ ] **Step 5: Commit**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  git add src-tauri/Cargo.toml src-tauri/Cargo.lock package.json package-lock.json
  git commit -m "build(zeb-328): add tauri-plugin-{updater,deep-link} deps"
  ```

---

## Task 3: Configure `tauri.conf.json`

**Files:**
- Modify: `src-tauri/tauri.conf.json`

The file today is minimal (productName, version, identifier, build, app). Add `bundle` and `plugins` sections per spec §5, §6, §8.

- [ ] **Step 1: Read current contents**

  ```bash
  cat /Users/zeblith/work/zeblithic/harmony-client/src-tauri/tauri.conf.json
  ```
  Confirm the exact current structure before editing.

- [ ] **Step 2: Add `bundle` section with deep-link registration**

  Insert this as a new top-level key (after `"app"`):

  ```json
  "bundle": {
    "active": true,
    "category": "SocialNetworking",
    "targets": ["dmg", "msi", "appimage"],
    "shortDescription": "Harmony — federated, polycentric social fabric",
    "longDescription": "Harmony is a federated, polycentric social fabric built on user-owned identity and self-hosted infrastructure. This is the desktop client.",
    "deepLinks": [
      {
        "name": "harmony",
        "domains": [],
        "schemes": ["harmony"]
      }
    ]
  }
  ```

  Notes:
  - `targets`: limits the bundler to the three formats we ship per spec §4. Tauri builds only the targets matching the current OS.
  - `category`: macOS app category; "SocialNetworking" is the conventional pick.
  - `deepLinks.schemes`: the `harmony` scheme means clicking `harmony://invite/...` launches the app.

- [ ] **Step 3: Add `plugins.updater` section**

  Insert this as a new top-level key (after `"bundle"`):

  ```json
  "plugins": {
    "updater": {
      "active": true,
      "endpoints": [
        "https://zeblithic.github.io/harmony-client/latest.json"
      ],
      "pubkey": "PLACEHOLDER_REPLACE_WITH_OP_3_OUTPUT"
    }
  }
  ```

  The `pubkey` placeholder MUST be replaced by the operator (per OP-3) with the actual ed25519 public key from `~/.tauri/harmony-updater.key.pub` before triggering the first release. The plan ships the placeholder so the workflow file + Rust init can be committed and merged; release-time the operator swaps it in.

- [ ] **Step 4: Verify the file parses as JSON**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
  python3 -c "import json; json.load(open('tauri.conf.json'))" && echo "JSON OK"
  ```

- [ ] **Step 5: Run cargo check to ensure tauri-build accepts the schema**

  ```bash
  cd src-tauri
  cargo check --locked --features test-fixtures 2>&1 | tail -10
  ```
  Must exit 0. Tauri's build script reads `tauri.conf.json` and fails loudly on schema errors.

- [ ] **Step 6: Commit**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  git add src-tauri/tauri.conf.json
  git commit -m "build(zeb-328): tauri.conf.json — bundle + deep-link + updater config

placeholder updater pubkey replaced at release time per OP-3"
  ```

---

## Task 4: Wire Rust-side updater + deep-link plugin init

**Files:**
- Modify: `src-tauri/src/lib.rs` (the Tauri builder section around line 30743, where `tauri_plugin_dialog::init()` is registered).

- [ ] **Step 1: Locate the builder**

  ```bash
  grep -n "tauri_plugin_dialog::init\|tauri::Builder::default\|.plugin(" /Users/zeblith/work/zeblithic/harmony-client/src-tauri/src/lib.rs | head -10
  ```
  Confirms the existing builder shape — the plugins get registered with `.plugin(...)` calls chained off `Builder::default()`.

- [ ] **Step 2: Add plugin imports**

  At the top of `src-tauri/src/lib.rs`, near other `use` statements, add:

  ```rust
  use tauri_plugin_deep_link::DeepLinkExt;
  ```

  (`tauri_plugin_updater` does NOT require a `use` for builder-side registration; only frontend consumes the JS API.)

- [ ] **Step 3: Register both plugins on the builder**

  Find the existing `.plugin(tauri_plugin_dialog::init())` line. Immediately after it, add:

  ```rust
  .plugin(tauri_plugin_updater::Builder::new().build())
  .plugin(tauri_plugin_deep_link::init())
  ```

- [ ] **Step 4: Add a setup hook that registers deep-link URL handler**

  Inside the builder's `.setup(|app| { ... })` block (find existing setup or add one), append:

  ```rust
  // ZEB-328: deep-link handler — forward harmony:// URLs to the frontend.
  // Frontend's deep-link listener (in App.svelte, Task 6) opens
  // RedeemInviteDialog with the URL.
  {
      let app_handle = app.handle().clone();
      app.deep_link().on_open_url(move |event| {
          let urls: Vec<String> = event.urls().iter().map(|u| u.to_string()).collect();
          if let Err(e) = app_handle.emit("deep-link-received", &urls) {
              tracing::warn!(error = %e, "deep-link emit failed");
          }
      });
  }
  ```

  Notes:
  - `app.deep_link()` is the plugin's app-handle extension method (provided by the `DeepLinkExt` trait imported in Step 2).
  - The plugin handles OS-level URL receipt; we forward to the frontend via a Tauri event called `deep-link-received` so the existing RedeemInviteDialog flow can take over.
  - On macOS, deep links may arrive BEFORE the frontend has subscribed to the event. The Tauri deep-link plugin queues these; the frontend reads them via `getCurrent()` on mount (handled in Task 6).

- [ ] **Step 5: Verify build**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
  cargo check --locked --all-targets --features test-fixtures 2>&1 | tail -15
  ```
  Must exit 0. Compilation errors here usually mean the plugin's API surface differs from drafted; consult `cargo doc -p tauri-plugin-updater --open` and `cargo doc -p tauri-plugin-deep-link --open` to adjust.

- [ ] **Step 6: Run clippy**

  ```bash
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
  ```
  Must exit 0.

- [ ] **Step 7: Commit**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  git add src-tauri/src/lib.rs
  git commit -m "feat(zeb-328): wire tauri-plugin-{updater,deep-link} init

Updater plugin registers without setup; the frontend toast (Task 5)
drives the check + apply flow. Deep-link plugin emits 'deep-link-received'
events on harmony:// URL arrival so the frontend can route them to
RedeemInviteDialog (Task 6)."
  ```

---

## Task 5: Ship in-app update notification (toast + adapter + startup hook)

Three coordinated frontend changes that together produce: app starts → checks `latest.json` → if newer version available + not previously dismissed → shows toast with "Restart to update" / "Later" / "Skip this version" buttons.

**Files:**
- Create: `src/lib/components/UpdateAvailableToast.svelte`
- Create: `src/lib/components/__tests__/UpdateAvailableToast.test.ts`
- Create: `src/lib/updater-adapter.ts`
- Create: `src/lib/__tests__/updater-adapter.test.ts`
- Modify: `src/App.svelte` (root component — add startup check + mount toast)

- [ ] **Step 1: Write failing test for the updater-adapter**

  Create `src/lib/__tests__/updater-adapter.test.ts`:

  ```typescript
  import { describe, it, expect, vi, beforeEach } from "vitest";

  // Mock the Tauri plugin module BEFORE importing the adapter.
  vi.mock("@tauri-apps/plugin-updater", () => ({
    check: vi.fn(),
  }));

  import { checkForUpdate } from "../updater-adapter";
  import { check } from "@tauri-apps/plugin-updater";

  describe("checkForUpdate", () => {
    beforeEach(() => {
      vi.clearAllMocks();
      localStorage.clear();
    });

    it("returns the Update object when one is available", async () => {
      const fakeUpdate = {
        version: "0.1.0-alpha.2",
        available: true,
        downloadAndInstall: vi.fn(),
      };
      (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
      const result = await checkForUpdate();
      expect(result).toBe(fakeUpdate);
    });

    it("returns null when no update is available", async () => {
      (check as ReturnType<typeof vi.fn>).mockResolvedValue({ available: false });
      const result = await checkForUpdate();
      expect(result).toBeNull();
    });

    it("returns null and logs on network failure", async () => {
      (check as ReturnType<typeof vi.fn>).mockRejectedValue(new Error("network"));
      const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
      const result = await checkForUpdate();
      expect(result).toBeNull();
      expect(warnSpy).toHaveBeenCalled();
      warnSpy.mockRestore();
    });

    it("respects dismissed_version localStorage entry", async () => {
      localStorage.setItem("harmony.updater.dismissed_version", "0.1.0-alpha.5");
      const fakeUpdate = { version: "0.1.0-alpha.2", available: true };
      (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
      const result = await checkForUpdate();
      expect(result).toBeNull();   // dismissed version is HIGHER → suppress
    });

    it("does NOT suppress when available version is higher than dismissed", async () => {
      localStorage.setItem("harmony.updater.dismissed_version", "0.1.0-alpha.2");
      const fakeUpdate = { version: "0.1.0-alpha.5", available: true };
      (check as ReturnType<typeof vi.fn>).mockResolvedValue(fakeUpdate);
      const result = await checkForUpdate();
      expect(result).toBe(fakeUpdate);
    });
  });
  ```

- [ ] **Step 2: Run test (must fail — adapter doesn't exist yet)**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  npx vitest run src/lib/__tests__/updater-adapter.test.ts 2>&1 | tail -20
  ```
  Expected: failure (`Cannot find module '../updater-adapter'`).

- [ ] **Step 3: Implement `updater-adapter.ts`**

  Create `src/lib/updater-adapter.ts`:

  ```typescript
  import { check, type Update } from "@tauri-apps/plugin-updater";

  const DISMISSED_VERSION_KEY = "harmony.updater.dismissed_version";

  /** SemVer-style compare. Returns >0 if a > b, <0 if a < b, 0 if equal.
   * Sufficient for the alpha.N pre-release counter we use; uses
   * Intl.Collator's "numeric" mode so alpha.10 > alpha.2 correctly. */
  function semverCompare(a: string, b: string): number {
    const collator = new Intl.Collator(undefined, { numeric: true });
    return collator.compare(a, b);
  }

  /** Check the configured updater endpoint. Returns the Update object
   * when a newer-than-current AND newer-than-dismissed version is
   * available; null otherwise. Logs (does not throw) on network / parse
   * failure so app startup is never blocked by updater issues. */
  export async function checkForUpdate(): Promise<Update | null> {
    let update: Update | null = null;
    try {
      update = await check();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.warn(`[updater] check failed: ${msg}`);
      return null;
    }

    if (!update || !update.available) {
      return null;
    }

    const dismissed = localStorage.getItem(DISMISSED_VERSION_KEY);
    if (dismissed && semverCompare(update.version, dismissed) <= 0) {
      return null;
    }

    return update;
  }

  /** Persist a "don't bother me about this version" decision. */
  export function dismissVersion(version: string): void {
    localStorage.setItem(DISMISSED_VERSION_KEY, version);
  }
  ```

- [ ] **Step 4: Re-run test (must pass)**

  ```bash
  npx vitest run src/lib/__tests__/updater-adapter.test.ts 2>&1 | tail -10
  ```
  Expected: 5 passed.

- [ ] **Step 5: Write failing test for the toast component**

  Create `src/lib/components/__tests__/UpdateAvailableToast.test.ts`:

  ```typescript
  import { describe, it, expect, vi, beforeEach } from "vitest";
  import { render, fireEvent } from "@testing-library/svelte";
  import UpdateAvailableToast from "../UpdateAvailableToast.svelte";

  const baseUpdate = {
    version: "0.1.0-alpha.2",
    downloadAndInstall: vi.fn().mockResolvedValue(undefined),
  };

  describe("UpdateAvailableToast", () => {
    beforeEach(() => {
      vi.clearAllMocks();
      localStorage.clear();
    });

    it("renders the available version", () => {
      const { getByText } = render(UpdateAvailableToast, {
        update: baseUpdate,
        onDismiss: () => {},
      });
      expect(getByText(/0\.1\.0-alpha\.2/)).toBeInTheDocument();
    });

    it("calls update.downloadAndInstall on Restart click", async () => {
      const { getByText } = render(UpdateAvailableToast, {
        update: baseUpdate,
        onDismiss: () => {},
      });
      await fireEvent.click(getByText(/restart to update/i));
      expect(baseUpdate.downloadAndInstall).toHaveBeenCalledOnce();
    });

    it("calls onDismiss on Later click", async () => {
      const onDismiss = vi.fn();
      const { getByText } = render(UpdateAvailableToast, {
        update: baseUpdate,
        onDismiss,
      });
      await fireEvent.click(getByText(/later/i));
      expect(onDismiss).toHaveBeenCalledOnce();
    });

    it("persists dismissed_version on Skip click", async () => {
      const onDismiss = vi.fn();
      const { getByText } = render(UpdateAvailableToast, {
        update: baseUpdate,
        onDismiss,
      });
      await fireEvent.click(getByText(/skip this version/i));
      expect(localStorage.getItem("harmony.updater.dismissed_version")).toBe(
        "0.1.0-alpha.2",
      );
      expect(onDismiss).toHaveBeenCalledOnce();
    });
  });
  ```

- [ ] **Step 6: Run test (must fail — component doesn't exist)**

  ```bash
  npx vitest run src/lib/components/__tests__/UpdateAvailableToast.test.ts 2>&1 | tail -20
  ```

- [ ] **Step 7: Implement `UpdateAvailableToast.svelte`**

  Create `src/lib/components/UpdateAvailableToast.svelte`. Mirror the existing toast/notification patterns in the codebase (e.g., `ConfirmDialog.svelte`, `ConfirmationModal.svelte` — read one first to match styling conventions).

  Minimal viable structure (Svelte 5 runes syntax):

  ```svelte
  <script lang="ts">
    import type { Update } from "@tauri-apps/plugin-updater";
    import { dismissVersion } from "../updater-adapter";

    interface Props {
      update: Pick<Update, "version" | "downloadAndInstall">;
      onDismiss: () => void;
    }

    const { update, onDismiss }: Props = $props();
    let applying = $state(false);
    let applyError = $state<string | null>(null);

    async function restart() {
      applying = true;
      applyError = null;
      try {
        await update.downloadAndInstall();
        // downloadAndInstall restarts the app; we never reach here on success.
      } catch (e) {
        applying = false;
        applyError = e instanceof Error ? e.message : String(e);
      }
    }

    function later() {
      onDismiss();
    }

    function skip() {
      dismissVersion(update.version);
      onDismiss();
    }
  </script>

  <div class="update-toast" role="status" aria-live="polite">
    <div class="title">Harmony v{update.version} is available.</div>
    {#if applyError}
      <div class="error">Update could not be applied: {applyError}</div>
    {/if}
    <div class="actions">
      <button onclick={restart} disabled={applying}>
        {applying ? "Applying…" : "Restart to update"}
      </button>
      <button onclick={later} disabled={applying}>Later</button>
      <button onclick={skip} disabled={applying}>Skip this version</button>
    </div>
  </div>

  <style>
    .update-toast {
      position: fixed;
      bottom: 1rem;
      right: 1rem;
      max-width: 380px;
      padding: 0.75rem 1rem;
      background: var(--bg-secondary, #2a2a2a);
      color: var(--fg-primary, #fff);
      border-radius: 6px;
      box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
      z-index: 1000;
    }
    .title { font-weight: 600; margin-bottom: 0.5rem; }
    .error {
      color: var(--fg-error, #ff6b6b);
      font-size: 0.875rem;
      margin-bottom: 0.5rem;
    }
    .actions { display: flex; gap: 0.5rem; flex-wrap: wrap; }
    .actions button {
      padding: 0.375rem 0.75rem;
      border-radius: 4px;
      border: 1px solid var(--border-default, #555);
      background: transparent;
      color: inherit;
      cursor: pointer;
    }
    .actions button:hover:not(:disabled) {
      background: var(--bg-hover, rgba(255, 255, 255, 0.08));
    }
    .actions button:disabled { opacity: 0.5; cursor: not-allowed; }
  </style>
  ```

  **Adapt the styling block** to match patterns used by other components in `src/lib/components/` — likely CSS variables differ.

- [ ] **Step 8: Re-run toast test (must pass)**

  ```bash
  npx vitest run src/lib/components/__tests__/UpdateAvailableToast.test.ts 2>&1 | tail -10
  ```
  Expected: 4 passed.

- [ ] **Step 9: Modify `src/App.svelte` to wire the startup check + toast**

  Read current App.svelte first:

  ```bash
  cat /Users/zeblith/work/zeblithic/harmony-client/src/App.svelte
  ```

  Add:
  - Import `checkForUpdate` from `./lib/updater-adapter`
  - Import `UpdateAvailableToast` from `./lib/components/UpdateAvailableToast.svelte`
  - State variable `availableUpdate: Update | null` (Svelte 5 rune syntax `$state(null)`)
  - In an `onMount` (or equivalent Svelte 5 startup hook): call `checkForUpdate()`, assign to `availableUpdate`
  - In the template: `{#if availableUpdate}<UpdateAvailableToast update={availableUpdate} onDismiss={() => availableUpdate = null} />{/if}`

  Exact diff varies by current App.svelte shape; implementer reads + edits.

- [ ] **Step 10: Run all frontend gates**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  npx tsc --noEmit 2>&1 | tail -10
  npx vitest run 2>&1 | tail -20
  ```
  Both must exit 0. Previous tests should still pass; the two new test files add 9 tests.

- [ ] **Step 11: Commit**

  ```bash
  git add src/App.svelte src/lib/updater-adapter.ts src/lib/components/UpdateAvailableToast.svelte src/lib/__tests__/updater-adapter.test.ts src/lib/components/__tests__/UpdateAvailableToast.test.ts
  git commit -m "feat(zeb-328): in-app update notification (toast + adapter + startup check)

App-launch silent check of the Tauri-updater endpoint. When a newer
version is available AND the user hasn't skipped it, a non-blocking
toast appears with Restart / Later / Skip-this-version. Restart drives
the plugin's downloadAndInstall (signature-verified). Skip persists
to localStorage; future checks suppress until a version higher than
the skipped one is available."
  ```

---

## Task 6: Wire harmony:// deep-link end-to-end (frontend handler)

Backend already emits `deep-link-received` events (Task 4). Frontend listens, filters for `harmony://invite/...` URLs, and opens RedeemInviteDialog with the URL.

**Files:**
- Modify: `src/App.svelte` (add deep-link listener)
- Possibly modify: existing component that owns RedeemInviteDialog state — verify with grep

- [ ] **Step 1: Locate how RedeemInviteDialog is currently shown**

  ```bash
  grep -rn "RedeemInviteDialog\|redeem-invite-url" /Users/zeblith/work/zeblithic/harmony-client/src/ | head -20
  ```
  Find the component that owns the "open redeem dialog with URL X" state. Likely a top-level App.svelte or a sidebar action. Note the API for triggering it programmatically.

- [ ] **Step 2: Test the URL filter (write a small unit test first)**

  Create `src/lib/__tests__/deep-link-router.test.ts`:

  ```typescript
  import { describe, it, expect } from "vitest";
  import { extractHarmonyInviteUrl } from "../deep-link-router";

  describe("extractHarmonyInviteUrl", () => {
    it("returns the URL when it matches harmony://invite/", () => {
      const urls = ["harmony://invite/abc123"];
      expect(extractHarmonyInviteUrl(urls)).toBe("harmony://invite/abc123");
    });

    it("returns the first matching URL when multiple given", () => {
      const urls = ["harmony://other/x", "harmony://invite/abc123", "harmony://invite/def"];
      expect(extractHarmonyInviteUrl(urls)).toBe("harmony://invite/abc123");
    });

    it("returns null when no harmony://invite/ URL", () => {
      expect(extractHarmonyInviteUrl(["harmony://other/x"])).toBeNull();
      expect(extractHarmonyInviteUrl(["https://example.com"])).toBeNull();
      expect(extractHarmonyInviteUrl([])).toBeNull();
    });
  });
  ```

- [ ] **Step 3: Run test (must fail)**

  ```bash
  npx vitest run src/lib/__tests__/deep-link-router.test.ts 2>&1 | tail -10
  ```

- [ ] **Step 4: Implement `deep-link-router.ts`**

  Create `src/lib/deep-link-router.ts`:

  ```typescript
  const HARMONY_INVITE_PREFIX = "harmony://invite/";

  /** Pick the first harmony://invite/... URL from a list (the deep-link
   * plugin can deliver multiple URLs at once on first launch). Returns
   * null when none match. */
  export function extractHarmonyInviteUrl(urls: string[]): string | null {
    return urls.find((u) => u.startsWith(HARMONY_INVITE_PREFIX)) ?? null;
  }
  ```

- [ ] **Step 5: Re-run test (must pass)**

  ```bash
  npx vitest run src/lib/__tests__/deep-link-router.test.ts 2>&1 | tail -10
  ```

- [ ] **Step 6: Wire the listener in `App.svelte`**

  In the same startup hook used by Task 5 (or a sibling effect):

  ```typescript
  import { listen } from "@tauri-apps/api/event";
  import { getCurrent } from "@tauri-apps/plugin-deep-link";
  import { extractHarmonyInviteUrl } from "./lib/deep-link-router";

  // ... inside onMount or $effect:

  // Handle deep links that arrived BEFORE the listener subscribed
  // (e.g., the user opened harmony://invite/... and the app cold-launched).
  const queued = await getCurrent();
  if (queued) {
    const url = extractHarmonyInviteUrl(queued);
    if (url) {
      openRedeemInviteDialog(url);   // function from Step 1's component
    }
  }

  // Handle deep links that arrive while the app is running.
  const unlisten = await listen<string[]>("deep-link-received", (event) => {
    const url = extractHarmonyInviteUrl(event.payload);
    if (url) {
      openRedeemInviteDialog(url);
    }
  });

  // (Optional, depending on Svelte version): return unlisten from
  // onMount/onDestroy so the listener cleans up on unmount.
  ```

  The `openRedeemInviteDialog(url)` function reuses whatever mechanism is identified in Step 1. If no existing imperative API, the simplest pattern is a `$state` variable `pendingInviteUrl` watched by a `{#if pendingInviteUrl}<RedeemInviteDialog url={pendingInviteUrl} ... />{/if}`.

- [ ] **Step 7: Run all frontend gates**

  ```bash
  npx tsc --noEmit 2>&1 | tail -10
  npx vitest run 2>&1 | tail -10
  ```

- [ ] **Step 8: Run backend gates (cargo check + clippy + fmt)**

  ```bash
  cd src-tauri
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
  cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -10
  ```
  Backend should be unaffected by frontend-only changes; this confirms no regression. Pre-existing orphan failures (folder_ingest, mint, etc.) are acceptable per `feedback_test_drift_is_our_fault`; NEW failures are blocking.

- [ ] **Step 9: Commit**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  git add src/App.svelte src/lib/deep-link-router.ts src/lib/__tests__/deep-link-router.test.ts
  git commit -m "feat(zeb-328): wire harmony:// deep-link to RedeemInviteDialog

Frontend listens for the 'deep-link-received' event (emitted by the
Rust-side handler from Task 4) AND drains any URL queued before the
listener subscribed (via plugin-deep-link's getCurrent()). harmony://invite/...
URLs route to the existing redeem-invite dialog flow."
  ```

---

## Task 7: Write `.github/workflows/release.yml`

**Files:**
- Create: `.github/workflows/release.yml`

- [ ] **Step 1: Create the workflow file with precheck + matrix + release jobs**

  Create `.github/workflows/release.yml`:

  ```yaml
  name: Release

  on:
    workflow_dispatch:
      inputs:
        version:
          description: 'SemVer + pre-release (e.g., 0.1.0-alpha.1)'
          required: true
          type: string

  permissions:
    contents: write    # for gh release create + gh-pages push

  jobs:
    precheck:
      name: Precheck (version match + gates + tag clear)
      runs-on: ubuntu-22.04
      steps:
        - uses: actions/checkout@v4

        - name: Confirm version input matches tauri.conf.json
          run: |
            CONFIG_VERSION=$(jq -r .version src-tauri/tauri.conf.json)
            if [ "$CONFIG_VERSION" != "${{ inputs.version }}" ]; then
              echo "::error::version mismatch: input=${{ inputs.version }} config=$CONFIG_VERSION"
              echo "Bump tauri.conf.json version + commit before triggering."
              exit 1
            fi

        - name: Confirm tag does not already exist
          env:
            GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          run: |
            if gh release view "v${{ inputs.version }}" --repo "${{ github.repository }}" >/dev/null 2>&1; then
              echo "::error::release v${{ inputs.version }} already exists"
              exit 1
            fi

        - name: Install Rust toolchain (honors rust-toolchain.toml)
          uses: dtolnay/rust-toolchain@stable
          with:
            components: clippy, rustfmt

        - name: Setup Node
          uses: actions/setup-node@v4
          with:
            node-version: '20'
            cache: 'npm'

        - name: npm ci
          run: npm ci

        - name: cargo fmt --check
          working-directory: src-tauri
          run: cargo fmt --all -- --check

        - name: cargo clippy
          working-directory: src-tauri
          run: cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings

        - name: cargo nextest
          working-directory: src-tauri
          run: |
            cargo install cargo-nextest --locked
            cargo nextest run --locked --workspace --all-targets --features test-fixtures

        - name: tsc --noEmit
          run: npx tsc --noEmit

        - name: vitest
          run: npx vitest run

    build:
      name: Build (${{ matrix.os }})
      needs: precheck
      runs-on: ${{ matrix.os }}
      strategy:
        fail-fast: false
        matrix:
          include:
            - os: macos-14
              target: aarch64-apple-darwin
            - os: macos-13
              target: x86_64-apple-darwin
            - os: windows-latest
              target: x86_64-pc-windows-msvc
            - os: ubuntu-22.04
              target: x86_64-unknown-linux-gnu

      steps:
        - uses: actions/checkout@v4

        - name: Install Rust toolchain
          uses: dtolnay/rust-toolchain@stable
          with:
            targets: ${{ matrix.target }}

        - name: Install Linux build deps
          if: matrix.os == 'ubuntu-22.04'
          run: |
            sudo apt-get update
            sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev libsoup-3.0-dev javascriptcoregtk-4.1

        - name: Setup Node
          uses: actions/setup-node@v4
          with:
            node-version: '20'
            cache: 'npm'

        - name: npm ci
          run: npm ci

        - name: tauri build
          env:
            TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
            TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
          run: npm run tauri build -- --target ${{ matrix.target }}

        - name: Sanity-check artifact presence
          shell: bash
          run: |
            set -euo pipefail
            ls -la src-tauri/target/${{ matrix.target }}/release/bundle/ || true
            # Verify Tauri produced at least one signed .sig file
            find src-tauri/target/${{ matrix.target }}/release/bundle/ -name "*.sig" | head -1

        - name: Upload artifacts
          uses: actions/upload-artifact@v4
          with:
            name: bundle-${{ matrix.target }}
            path: |
              src-tauri/target/${{ matrix.target }}/release/bundle/**/*.dmg
              src-tauri/target/${{ matrix.target }}/release/bundle/**/*.msi
              src-tauri/target/${{ matrix.target }}/release/bundle/**/*.AppImage
              src-tauri/target/${{ matrix.target }}/release/bundle/**/*.tar.gz
              src-tauri/target/${{ matrix.target }}/release/bundle/**/*.sig
              src-tauri/target/${{ matrix.target }}/release/bundle/**/*.zip
            if-no-files-found: warn

    release:
      name: Create GitHub Release + update manifest
      needs: build
      runs-on: ubuntu-22.04
      steps:
        - uses: actions/checkout@v4

        - name: Download all artifacts
          uses: actions/download-artifact@v4
          with:
            path: artifacts/

        - name: Flatten artifact tree
          run: |
            mkdir -p release-files
            find artifacts/ -type f \( -name "*.dmg" -o -name "*.msi" -o -name "*.AppImage" -o -name "*.tar.gz" -o -name "*.sig" -o -name "*.zip" \) -exec cp {} release-files/ \;
            ls -la release-files/

        - name: Create draft GitHub Release
          env:
            GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          run: |
            gh release create "v${{ inputs.version }}" \
              --title "v${{ inputs.version }}" \
              --generate-notes \
              --draft \
              release-files/*

        - name: Regenerate latest.json + push to gh-pages
          env:
            GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          run: |
            set -euo pipefail
            VERSION="${{ inputs.version }}"
            REPO="${{ github.repository }}"
            REPO_URL="https://github.com/${REPO}"
            PUB_DATE=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

            # Build per-platform entries by reading uploaded .sig files
            sig_for() {
              local pattern="$1"
              local sig_file
              sig_file=$(find release-files/ -name "${pattern}" -name "*.sig" | head -1)
              [ -n "$sig_file" ] && cat "$sig_file" || echo ""
            }
            url_for() {
              local pattern="$1"
              local file
              file=$(find release-files/ -name "${pattern}" ! -name "*.sig" | head -1)
              if [ -n "$file" ]; then
                echo "${REPO_URL}/releases/download/v${VERSION}/$(basename "$file")"
              else
                echo ""
              fi
            }

            cat > latest.json <<EOF
            {
              "version": "${VERSION}",
              "notes": "See ${REPO_URL}/releases/tag/v${VERSION}",
              "pub_date": "${PUB_DATE}",
              "platforms": {
                "darwin-aarch64": {
                  "signature": "$(sig_for '*aarch64*.app.tar.gz*')",
                  "url": "$(url_for '*aarch64*.app.tar.gz')"
                },
                "darwin-x86_64": {
                  "signature": "$(sig_for '*x64*.app.tar.gz*')",
                  "url": "$(url_for '*x64*.app.tar.gz')"
                },
                "windows-x86_64": {
                  "signature": "$(sig_for '*nsis.zip*')",
                  "url": "$(url_for '*nsis.zip')"
                },
                "linux-x86_64": {
                  "signature": "$(sig_for '*amd64.AppImage.tar.gz*')",
                  "url": "$(url_for '*amd64.AppImage.tar.gz')"
                }
              }
            }
            EOF
            cat latest.json

            # Push to gh-pages
            git config user.name "github-actions[bot]"
            git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
            git fetch origin gh-pages
            git worktree add /tmp/gh-pages gh-pages
            cp latest.json /tmp/gh-pages/latest.json
            cd /tmp/gh-pages
            git add latest.json
            git commit -m "release: v${VERSION} manifest" || echo "no changes"
            git push origin gh-pages

        - name: Publish release (un-draft)
          env:
            GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          run: gh release edit "v${{ inputs.version }}" --draft=false
  ```

  Notes:
  - The `sig_for` / `url_for` shell helpers are best-effort glob matches against the artifact tree; the actual filename patterns produced by Tauri 2 may differ slightly per platform. The smoke-test step (OP-equivalent, post-workflow) catches mismatches.
  - `dtolnay/rust-toolchain@stable` honors `rust-toolchain.toml` if present (added in Task 1).
  - Linux deps list is the canonical Tauri 2 Linux dep set for Ubuntu 22.04.

- [ ] **Step 2: Validate workflow YAML syntax**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))" && echo "YAML OK"
  ```

- [ ] **Step 3: (Optional) actionlint validation**

  If `actionlint` is installed locally:
  ```bash
  actionlint .github/workflows/release.yml
  ```
  Skip if not installed. The first real workflow trigger will catch any remaining issues.

- [ ] **Step 4: Commit**

  ```bash
  git add .github/workflows/release.yml
  git commit -m "ci(zeb-328): release.yml — workflow_dispatch + matrix + fan-in

Precheck gates: version match against tauri.conf.json, tag not
already published, full quality-gate sweep (fmt/clippy/nextest/
tsc/vitest) inline per amended spec §5.2. Matrix builds across
macos-14 / macos-13 / windows-latest / ubuntu-22.04. Release job
creates draft, uploads artifacts, regenerates gh-pages/latest.json,
un-drafts."
  ```

---

## Task 8: Write per-OS install docs

**Files:**
- Create: `docs/install-macos.md`
- Create: `docs/install-windows.md`
- Create: `docs/install-linux.md`

- [ ] **Step 1: Write `docs/install-macos.md`**

  Follow spec §9.1 structure. Sections: Download → First launch (Gatekeeper) → xattr fallback → Optional permissions → Updating → Uninstalling. ~1 page. Plain English, no jargon.

- [ ] **Step 2: Write `docs/install-windows.md`**

  Follow spec §9.2 structure. Sections: Download → Run installer (SmartScreen) → Default location → Firewall prompt → Updating → Uninstalling. ~1 page.

- [ ] **Step 3: Write `docs/install-linux.md`**

  Follow spec §9.3 structure. Sections: Download → chmod +x → Launch → AppImageLauncher link → libsecret requirement → Updating → Uninstalling. ~1 page.

- [ ] **Step 4: Cross-link from README**

  Read `/Users/zeblith/work/zeblithic/harmony-client/README.md` (currently one line). Add a "Install" section linking to the three platform docs. Keep README short (under 30 lines total).

- [ ] **Step 5: Commit**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client
  git add docs/install-macos.md docs/install-windows.md docs/install-linux.md README.md
  git commit -m "docs(zeb-328): per-OS install docs + README cross-link

Tester-facing install + Gatekeeper/SmartScreen workaround docs
per spec §9. README now points testers at the right platform doc."
  ```

---

## Task 9: Write `docs/release-process.md`

**Files:**
- Create: `docs/release-process.md`

- [ ] **Step 1: Write the operator playbook**

  Follow spec §9.4 + §10 structure. Sections:

  1. **One-time setup (OP-1 through OP-7)** — exact commands for keypair generation, secret upload, gh-pages bootstrap, Pages source config, visibility verification. Document the keypair backup procedure (1Password + lockbox). Document the `pubkey` placeholder swap in `tauri.conf.json`.
  2. **Per-release operator flow** — preflight → bump version → trigger workflow → watch → smoke-test → publish draft.
  3. **Smoke test playbook** (per spec §11.3) — exact steps to validate each platform's artifact.
  4. **Severe-incident playbooks** — keypair LEAK (rotate + ship new pubkey + accept stranded clients); keypair LOSS (same recovery, more comms work); release-published-broken (revoke + hotfix N+1).

  Keep it ~2-3 pages — operator-focused, no narrative, copy-pasteable commands.

- [ ] **Step 2: Cross-link from spec**

  Add a note to the bottom of `docs/specs/2026-05-24-zeb-328-build-release-pipeline-design.md`:

  ```markdown
  ## Operator playbook

  See [`docs/release-process.md`](../release-process.md) for the operator-only release-cutting procedure, one-time bootstrap (OP-1 through OP-7), and severe-incident playbooks.
  ```

- [ ] **Step 3: Commit**

  ```bash
  git add docs/release-process.md docs/specs/2026-05-24-zeb-328-build-release-pipeline-design.md
  git commit -m "docs(zeb-328): release-process.md — operator playbook + bootstrap

One-time setup steps (OP-1..OP-7), per-release flow, smoke-test
playbook, severe-incident response per spec §10."
  ```

---

## Task 10: Final 5-gate sweep + push + PR creation

- [ ] **Step 1: Run all 5 gates locally one more time**

  ```bash
  cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
  cargo fmt --all -- --check && echo "FMT OK"
  cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -10
  cargo nextest run --locked --workspace --all-targets --features test-fixtures 2>&1 | tail -15

  cd /Users/zeblith/work/zeblithic/harmony-client
  npx tsc --noEmit 2>&1 | tail -10
  npx vitest run 2>&1 | tail -10
  ```

  All five must exit 0 (modulo pre-existing orphan failures per `feedback_test_drift_is_our_fault`).

- [ ] **Step 2: Confirm commit log on branch**

  ```bash
  git log origin/main..HEAD --oneline
  # Expected ~12 commits: 2 spec + ~10 implementation
  ```

- [ ] **Step 3: Push branch**

  ```bash
  git push origin zeb-328-build-release-pipeline-spec
  ```

- [ ] **Step 4: Create PR**

  ```bash
  gh pr create \
    --base main \
    --head zeb-328-build-release-pipeline-spec \
    --title "ZEB-328 Sub-project A: build & release pipeline + harmony:// deep-link wiring" \
    --body "$(cat <<'EOF'
  ## Summary

  Sub-project A of [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) — the build & release pipeline that lets us cut v0.1.0-alpha desktop builds for macOS / Windows / Linux that hand-picked testers can download, install, and auto-update.

  Implements [ZEB-328](https://linear.app/zeblith/issue/ZEB-328). Spec at `docs/specs/2026-05-24-zeb-328-build-release-pipeline-design.md` (commits 339285e + ecaaa00).

  ### What ships

  - `.github/workflows/release.yml` — workflow_dispatch + version input, precheck (version-match + 5-gate sweep + tag-clear), matrix build across macos-14 / macos-13 / windows-latest / ubuntu-22.04, fan-in to GitHub Release + gh-pages manifest update.
  - Tauri 2 updater plugin wired: silent check on launch, non-blocking toast ("Restart to update" / "Later" / "Skip this version"), per-version dismissal persisted to localStorage.
  - Tauri 2 deep-link plugin wired: `harmony://invite/...` URLs route to the existing RedeemInviteDialog flow.
  - `tauri.conf.json` updates: `bundle` config (deepLinks for harmony scheme, three install targets), `plugins.updater` config (placeholder pubkey — operator swaps per OP-3 before first release).
  - Pinned Rust toolchain (`rust-toolchain.toml`) for build determinism.
  - Per-OS install docs (`docs/install-macos.md` / `install-windows.md` / `install-linux.md`) with Gatekeeper / SmartScreen workaround instructions.
  - Operator playbook (`docs/release-process.md`) including one-time bootstrap, per-release flow, smoke-test steps, and severe-incident response.
  - README updated to cross-link the install docs.

  ### What does NOT ship in this PR

  - Operator-only bootstrap (OP-1..OP-7 in the plan: keypair generation, secrets upload, gh-pages branch creation, Pages enablement, pubkey embed). Documented in `docs/release-process.md`; required before the first real release-workflow run can succeed end-to-end.
  - Cross-WAN validation surface (Sub-project B), first-run UX walkthrough (Sub-project C), Zeblithic community bootstrap (Sub-project D). Separate PRs.
  - OS code-signing / notarization (deferred to public beta).

  ### Test plan

  - [x] cargo fmt / clippy / nextest green locally
  - [x] tsc / vitest green locally (9 new tests across updater-adapter + UpdateAvailableToast + deep-link-router)
  - [ ] **Operator post-merge:** complete OP-1..OP-7 per docs/release-process.md
  - [ ] **Operator post-merge:** trigger release.yml with version=0.1.0-alpha.1, validate workflow exits green within ~25min, smoke-test artifact on each platform per docs/release-process.md
  - [ ] **Operator post-merge:** confirm auto-updater round-trip from a v0.1.0-alpha.1 install → cut v0.1.0-alpha.2 → toast appears → restart-apply succeeds

  ### References

  - Parent epic: [ZEB-327](https://linear.app/zeblith/issue/ZEB-327)
  - Spec: \`docs/specs/2026-05-24-zeb-328-build-release-pipeline-design.md\`
  - Plan: \`docs/plans/2026-05-24-zeb-328-build-release-pipeline-plan.md\`
  - Sibling sub-projects (sequenced): B (validation surface), C (onboarding UX), D (Zeblithic community bootstrap) — to be filed after this lands.

  🤖 Generated with [Claude Code](https://claude.com/claude-code)
  EOF
  )"
  ```

  Important per `feedback_linear_pr_auto_close`:
  - PR title does NOT contain ZEB-327 (parent epic) — it contains only ZEB-328 (child).
  - Parent epic is referenced as a markdown link, not a bare close trigger.
  - This PR completes ZEB-328 (the sub-project); ZEB-327 (umbrella) stays open after merge.

- [ ] **Step 5: Note the PR URL + return**

  Print the PR URL. Subsequent autonomous bot-review monitoring loop takes over from here.

---

## Risks during implementation

- **Tauri plugin API drift** between minor versions — if Task 4's draft init code doesn't compile against the installed plugin versions, consult `cargo doc -p tauri-plugin-{updater,deep-link} --open` and adapt the calls. Don't guess; verify against the docs.
- **App.svelte shape unknown** — Tasks 5 + 6 modify App.svelte without showing its current contents. Implementer MUST read it before editing and adapt the wiring to its actual structure (Svelte 5 runes vs. older syntax, existing onMount hooks, etc.).
- **rust-toolchain.toml channel choice** — pin to the version the operator's machine reports in Task 1 Step 1, not an arbitrary number. CI's `dtolnay/rust-toolchain@stable` honors this file.
- **Workflow YAML helper functions** in Task 7's release job (`sig_for` / `url_for`) use best-effort glob patterns; actual Tauri 2 filename conventions per platform may differ slightly. Iterate post-merge once the workflow runs once and we see real filenames.
- **package.json/package-lock.json conflicts** — Task 2's `npm install` modifies both files. If lockfile drift appears in the diff for non-updater/non-deep-link entries, halt; this means the lockfile was stale on main.

## Out of scope for this PR (filed as follow-ups if discovered)

- Multi-region CDN for update manifest (spec §12.2)
- `.deb` / `.rpm` / `.flatpak` Linux packages
- macOS Universal binary (we ship per-arch DMGs)
- Custom domain for update endpoint (`updates.harmony.zeblith.net`)
- sccache / build-time optimization
