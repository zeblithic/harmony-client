<script lang="ts">
  /**
   * ZEB-835 / ZEB-836 — self-serve escape from a *terminal* owner-boot failure.
   *
   * When `start_node` fails permanently the app is stuck on the "Couldn't start
   * Harmony" modal, whose only action is Retry (`location.reload()`) — which
   * re-runs the identical failing boot. Both known failure modes land here:
   *   - ZEB-835: device signing key in neither store (`load_owner_state` → Err).
   *   - ZEB-836: loaded device key not in `owner_state.cbor`'s enrollments.
   * Retry stays the primary action (most `error`-state causes are transient),
   * so this lives behind a quiet "Still stuck?" disclosure and offers the two
   * remedies that actually recover a permanent failure:
   *
   *   1. **Restore from recovery phrase** — reuse {@link OwnerRestoreWizard}.
   *      We first read the on-disk owner-id (`owner_id_on_disk`, key-free) and
   *      hand it to the wizard so a same-owner phrase classifies as a
   *      re-adoption (→ `force` overwrite of the broken `owner_state.cbor`)
   *      rather than a fresh restore the overwrite-guard would refuse.
   *   2. **Reset this device & start fresh** — gated behind an explicit confirm
   *      with honest copy, then `reset_local_identity` (snapshot-then-wipe +
   *      keychain clear) → reload into first-run onboarding.
   *
   * `invoke`/`reload` are injectable so the flow is unit-testable without Tauri
   * or a real page reload (mirrors the owner-restore-logic split).
   */
  import { invoke as tauriInvoke } from '@tauri-apps/api/core';
  import OwnerRestoreWizard from './OwnerRestoreWizard.svelte';

  let {
    invoke = tauriInvoke,
    reload = () => window.location.reload(),
    startExpanded = false,
  }: {
    invoke?: <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
    reload?: () => void;
    /**
     * Skip the "Still stuck?" disclosure and show the remedies directly. Used by
     * the ZEB-836 enrollment-missing screen, where recovery *is* the point (no
     * transient Retry to prefer first) — unlike the startup-error modal, where
     * Retry stays primary and this stays collapsed.
     */
    startExpanded?: boolean;
  } = $props();

  type Mode =
    | 'collapsed'
    | 'options'
    | 'restore'
    | 'reset-confirm'
    | 'resetting'
    | 'erase-confirm'
    | 'erasing';
  // Starts 'collapsed'; when `startExpanded` the template shows the options panel
  // directly (see the `collapsed && !startExpanded` gate below) without needing
  // the prop in this initializer.
  let mode = $state<Mode>('collapsed');
  let currentOwnerId = $state<string | null>(null);
  let confirmChecked = $state(false);
  let resetError = $state<string | null>(null);
  let restoreBlocked = $state<string | null>(null);
  // ZEB-842: erase-all clean-slate (typed-confirm). Distinct from the recovery
  // reset above — this hard-deletes identity AND every app-data cache.
  let eraseText = $state('');
  let eraseError = $state<string | null>(null);

  async function openRestore() {
    restoreBlocked = null;
    try {
      // null → no marker on disk (a true fresh, non-destructive restore);
      // hex → the broken owner's id, so a same-owner phrase force-overwrites it.
      currentOwnerId = await invoke<string | null>('owner_id_on_disk');
    } catch {
      // Present-but-corrupt/unreadable marker: a force=false restore would be
      // refused by the overwrite guard, and we can't derive an owner-id to force
      // it. Steer to Reset (which handles a corrupt marker) rather than open a
      // wizard whose restore will fail.
      restoreBlocked =
        "This device's identity file couldn't be read, so restoring in place isn't possible here. Use “Reset this device & start fresh” below — your recovery phrase still works after a reset.";
      return;
    }
    mode = 'restore';
  }

  async function doReset() {
    if (!confirmChecked || mode === 'resetting') return;
    mode = 'resetting';
    resetError = null;
    try {
      await invoke('reset_local_identity');
      // owner_state.cbor is gone → next boot classifies as `missing` → onboarding.
      reload();
    } catch (e) {
      resetError = e instanceof Error ? e.message : String(e);
      mode = 'reset-confirm';
    }
  }

  async function doErase() {
    // Typed-confirm gate: the fixed literal ERASE, matching IdentityPanel's
    // erase action. Irreversible for non-re-fetchable content (mail), so no
    // checkbox — the user must type it.
    if (eraseText !== 'ERASE' || mode === 'erasing') return;
    mode = 'erasing';
    eraseError = null;
    try {
      await invoke('erase_all_local_data');
      // Identity + caches gone → next boot classifies as `missing` → onboarding.
      reload();
    } catch (e) {
      eraseError = e instanceof Error ? e.message : String(e);
      mode = 'erase-confirm';
    }
  }
</script>

{#if mode === 'restore'}
  <OwnerRestoreWizard
    {currentOwnerId}
    onRestored={() => reload()}
    onCancel={() => {
      mode = 'options';
    }}
  />
{/if}

{#if mode !== 'restore'}
  {#if mode === 'collapsed' && !startExpanded}
    <button
      type="button"
      class="still-stuck-link"
      data-testid="startup-still-stuck"
      onclick={() => (mode = 'options')}
    >
      Still stuck?
    </button>
  {:else}
    <div class="recovery-options" data-testid="startup-recovery-options">
      {#if !startExpanded}
        <p class="recovery-lead">
          If retrying keeps failing, this identity can't be opened on this device.
          You can recover it:
        </p>
      {/if}

      <button
        type="button"
        class="recovery-btn restore"
        data-testid="startup-restore"
        onclick={openRestore}
      >
        Restore from recovery phrase
      </button>
      {#if restoreBlocked}
        <p class="reset-error" data-testid="startup-restore-blocked">{restoreBlocked}</p>
      {/if}

      {#if mode === 'reset-confirm' || mode === 'resetting'}
        <div class="reset-confirm-block">
          <label class="reset-confirm">
            <input
              type="checkbox"
              bind:checked={confirmChecked}
              data-testid="startup-reset-confirm"
              disabled={mode === 'resetting'}
            />
            <span data-testid="startup-reset-confirm-copy">
              Recover this device's onboarding. Your current identity is backed up
              to a folder on this device first, then set aside so the app starts
              fresh. Cached content (messages, avatars) stays on this device — use
              “Erase all local data” below to remove everything. You'll lose access
              to communities you joined here unless you have your recovery phrase.
              This can't be undone from the app.
            </span>
          </label>
          {#if resetError}
            <p class="reset-error" data-testid="startup-reset-error">{resetError}</p>
          {/if}
          <button
            type="button"
            class="recovery-btn reset-go"
            data-testid="startup-reset-go"
            disabled={!confirmChecked || mode === 'resetting'}
            onclick={doReset}
          >
            {mode === 'resetting' ? 'Resetting…' : 'Reset this device'}
          </button>
        </div>
      {:else}
        <button
          type="button"
          class="recovery-btn reset"
          data-testid="startup-reset"
          onclick={() => (mode = 'reset-confirm')}
        >
          Reset this device &amp; start fresh
        </button>
      {/if}

      {#if mode === 'erase-confirm' || mode === 'erasing'}
        <div class="erase-confirm-block">
          <p class="erase-lead">
            This permanently erases this device's identity <strong
              >and all cached data</strong
            >
            for this profile — messages, avatars, and more. Your recovery phrase still
            restores your identity, but the cached content can't be recovered.
          </p>
          <label class="erase-confirm">
            <span>Type <strong>ERASE</strong> to confirm</span>
            <input
              type="text"
              bind:value={eraseText}
              data-testid="startup-erase-input"
              disabled={mode === 'erasing'}
              autocomplete="off"
              autocapitalize="characters"
              spellcheck="false"
            />
          </label>
          {#if eraseError}
            <p class="reset-error" data-testid="startup-erase-error">{eraseError}</p>
          {/if}
          <button
            type="button"
            class="recovery-btn erase-go"
            data-testid="startup-erase-go"
            disabled={eraseText !== 'ERASE' || mode === 'erasing'}
            onclick={doErase}
          >
            {mode === 'erasing' ? 'Erasing…' : 'Erase all local data'}
          </button>
        </div>
      {:else}
        <button
          type="button"
          class="recovery-btn erase"
          data-testid="startup-erase"
          onclick={() => (mode = 'erase-confirm')}
        >
          Erase all local data
        </button>
      {/if}
    </div>
  {/if}
{/if}

<style>
  /* Colors come only from design tokens (var(--…)) — no raw literals; see
     src/style-token-guard.test.ts / ZEB-605. */
  .still-stuck-link {
    margin-top: 0.75rem;
    background: none;
    border: none;
    padding: 0;
    color: var(--text-muted);
    font: inherit;
    text-decoration: underline;
    cursor: pointer;
  }
  .still-stuck-link:hover {
    color: var(--text-primary);
  }

  .recovery-options {
    margin-top: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
    text-align: left;
  }
  .recovery-lead {
    margin: 0;
    font-size: 0.9rem;
    color: var(--text-muted);
  }

  .recovery-btn {
    font: inherit;
    padding: 0.5rem 0.9rem;
    border-radius: 8px;
    cursor: pointer;
    border: 1px solid var(--border-default);
    background: var(--surface-raised);
    color: var(--text-primary);
  }
  .recovery-btn:hover:not(:disabled) {
    border-color: var(--accent);
  }
  .recovery-btn.reset,
  .recovery-btn.reset-go,
  .recovery-btn.erase,
  .recovery-btn.erase-go {
    border-color: var(--danger);
    color: var(--danger);
  }
  .recovery-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .reset-confirm-block {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
  }
  .reset-confirm {
    display: flex;
    gap: 0.55rem;
    align-items: flex-start;
    font-size: 0.85rem;
    color: var(--text-primary);
    line-height: 1.35;
  }
  .reset-confirm input {
    margin-top: 0.2rem;
    flex-shrink: 0;
  }
  .reset-error {
    margin: 0;
    font-size: 0.85rem;
    color: var(--danger);
  }

  .erase-confirm-block {
    display: flex;
    flex-direction: column;
    gap: 0.55rem;
  }
  .erase-lead {
    margin: 0;
    font-size: 0.85rem;
    color: var(--text-primary);
    line-height: 1.35;
  }
  .erase-confirm {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    font-size: 0.85rem;
    color: var(--text-primary);
  }
  .erase-confirm input {
    font: inherit;
    padding: 0.4rem 0.55rem;
    border-radius: 6px;
    border: 1px solid var(--border-default);
    background: var(--surface-raised);
    color: var(--text-primary);
  }
</style>
