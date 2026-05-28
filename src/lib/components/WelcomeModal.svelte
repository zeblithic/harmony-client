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
  // ZEB-338 / PR #169: defense-in-depth escape. App.svelte only opens this gate
  // when start_node reported NO owner identity, so mint hitting "already exists"
  // should be unreachable — but if owner_state.cbor exists yet start_node didn't
  // load it, staying on the explain pane would deadlock the hard gate. Surfacing
  // a reload (which re-runs start_node → reports present) guarantees an exit.
  let alreadyExists = $state(false);
  // ZEB-338 / PR #169: bound to the dialog element for the focus trap.
  let modalEl = $state<HTMLElement | null>(null);

  const svc = new OwnerService();

  onMount(async () => {
    try {
      appVersion = await getVersion();
    } catch (e) {
      console.debug('[zeb-338] WelcomeModal getVersion failed:', extractError(e));
    }
  });

  // ZEB-338 / PR #169: trap focus inside the hard gate. `aria-modal` alone does
  // not stop Tab from reaching the background (e.g. the help button), so we mark
  // sibling overlays inert, move initial focus into the dialog, cycle Tab within
  // it, and restore focus on close.
  $effect(() => {
    if (!open || modalEl === null) return;

    const dialog = modalEl;
    const backdrop = dialog.parentElement;
    const root = backdrop?.parentElement ?? null;
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const inerted: HTMLElement[] = [];

    if (root && backdrop) {
      for (const child of Array.from(root.children)) {
        if (child !== backdrop && child instanceof HTMLElement && !child.hasAttribute('inert')) {
          child.setAttribute('inert', '');
          inerted.push(child);
        }
      }
    }

    const focusables = (): HTMLElement[] =>
      Array.from(
        dialog.querySelectorAll<HTMLElement>(
          'a[href], button:not([disabled]), input:not([disabled]), [tabindex]:not([tabindex="-1"])',
        ),
      );

    (focusables()[0] ?? dialog).focus();

    function onKeydown(ev: KeyboardEvent) {
      if (ev.key !== 'Tab') return;
      const items = focusables();
      if (items.length === 0) {
        ev.preventDefault();
        return;
      }
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement;
      if (ev.shiftKey && active === first) {
        ev.preventDefault();
        last.focus();
      } else if (!ev.shiftKey && active === last) {
        ev.preventDefault();
        first.focus();
      }
    }

    dialog.addEventListener('keydown', onKeydown);

    return () => {
      dialog.removeEventListener('keydown', onKeydown);
      for (const el of inerted) el.removeAttribute('inert');
      previouslyFocused?.focus?.();
    };
  });

  async function handleCreateIdentity() {
    stage = 'minting';
    mintError = null;
    alreadyExists = false;
    try {
      const result = await svc.mint();
      mintResult = result;
      stage = 'backup';
    } catch (e) {
      const msg = extractError(e);
      mintError = msg;
      // An "already exists" mint failure means an identity is on disk but the
      // node didn't load it — reload is the only safe exit from the hard gate.
      alreadyExists = /already exist/i.test(msg);
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
      bind:this={modalEl}
      class="modal-content"
      data-testid="welcome-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="welcome-title"
      tabindex="-1"
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
        {#if alreadyExists}
          <p class="muted" data-testid="welcome-already-exists-hint">
            An identity already exists on this device but couldn't be loaded.
            Reload to try starting it again.
          </p>
        {/if}
        <div class="actions">
          {#if alreadyExists}
            <button
              class="primary"
              data-testid="welcome-already-exists-reload"
              onclick={() => location.reload()}
            >
              Reload Harmony
            </button>
          {:else}
            <button
              class="primary"
              data-testid="welcome-create-identity"
              onclick={handleCreateIdentity}
              disabled={stage === 'minting'}
            >
              {stage === 'minting' ? 'Creating your identity…' : 'Create my identity'}
            </button>
          {/if}
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
