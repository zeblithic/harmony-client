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
  import { invoke } from '@tauri-apps/api/core';
  import { OwnerService, extractError, type MintIpcResult } from '../owner-service';
  import {
    identityKeyBackupNote,
    normalizeIdentityStoreBackend,
    type IdentityStoreBackend,
  } from '../identity-backup-copy';
  import {
    MIN_RECOVERY_PASSPHRASE_LEN,
  } from '../recovery-policy';
  import { markBackupSkipped, markRecoveryBackedUp } from '../onboarding-backup-flags';
  import { trapFocus } from '../focus-trap';
  import PairingJoiner from './PairingJoiner.svelte';
  import OwnerRestoreWizard from './OwnerRestoreWizard.svelte';
  import OwnerPhraseReveal from './OwnerPhraseReveal.svelte';
  import HarmonyMark from './HarmonyMark.svelte';
  import WizardProgress from './WizardProgress.svelte';

  interface Props {
    open: boolean;
    onMinted: (mintResult: MintIpcResult) => void | Promise<void>;
  }
  const { open, onMinted }: Props = $props();

  type Stage = 'explain' | 'minting' | 'backup' | 'skip-confirm' | 'joining' | 'restore';
  let stage = $state<Stage>('explain');

  // ZEB-610 (Commons G): the mint hard gate is a 3-step wizard — Welcome →
  // Create (both sage) → Back up (clay: the one stage where losing the file is
  // irreversible). The pip rail visualizes that arc; clay lives ONLY on the
  // backup step's pip + its warning callout.
  const WIZARD_STEPS = [
    { label: 'Welcome', accent: 'sage' as const },
    { label: 'Create', accent: 'sage' as const },
    { label: 'Back up', accent: 'clay' as const },
  ];
  const wizardIndex = $derived(stage === 'backup' ? 2 : stage === 'minting' ? 1 : 0);
  let mintResult = $state<MintIpcResult | null>(null);
  let mintError = $state<string | null>(null);
  let backupPassphrase = $state('');
  let backupError = $state<string | null>(null);
  let backupInFlight = $state(false);
  let appVersion = $state<string>('unknown');
  // ZEB-768: which backend actually holds the identity key, so the backup
  // note tells the truth instead of always asserting the OS keychain.
  // Defaults to backend-neutral wording until the IPC getter resolves.
  let identityBackend = $state<IdentityStoreBackend>('unknown');
  const keychainNote = $derived(identityKeyBackupNote(identityBackend));
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
    try {
      // ZEB-768: never leave this asserting the keychain — on failure the
      // 'unknown' default keeps the backend-neutral wording.
      identityBackend = normalizeIdentityStoreBackend(
        await invoke<string>('identity_store_backend'),
      );
    } catch (e) {
      console.debug('[zeb-768] WelcomeModal identity_store_backend failed:', extractError(e));
    }
  });

  // ZEB-338 / PR #169: trap focus inside the hard gate (shared trapFocus util,
  // also used by App.svelte's startup-error overlay).
  $effect(() => {
    if (!open || modalEl === null) return;
    return trapFocus(modalEl);
  });

  async function handleCreateIdentity() {
    stage = 'minting';
    mintError = null;
    alreadyExists = false;
    try {
      const result = await svc.mint();
      mintResult = result;
      // ZEB-830: onMount queried identity_store_backend before mint, when the
      // seed's location wasn't yet decided — and mint can fall through to the
      // encrypted file even with a keychain handle available. Re-query now so
      // the backup note reflects where the seed ACTUALLY landed.
      try {
        identityBackend = normalizeIdentityStoreBackend(
          await invoke<string>('identity_store_backend'),
        );
      } catch (e) {
        console.debug('[zeb-830] post-mint identity_store_backend failed:', extractError(e));
      }
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
      backupError = `Recovery passphrase must be at least ${MIN_RECOVERY_PASSPHRASE_LEN} characters.`;
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
        // user cancelled the OS dialog — stay on pane 2 (finally resets the flag)
        return;
      }
      await svc.exportRecoveryFile(mintResult.recoveryToken, pathToken, backupPassphrase, null);
      // ZEB-587: owner-scope the flag so it tracks THIS identity's backup state.
      markRecoveryBackedUp(mintResult.state.ownerId);
      backupPassphrase = '';
      await onMinted(mintResult);
    } catch (e) {
      backupError = extractError(e);
    } finally {
      backupInFlight = false;
    }
  }

  // ZEB-650 slice 2 (CodeRabbit PR #437): a phrase backup needs its own exit —
  // without one, the only way past this stage after writing the words down is
  // "Skip for now", which would record a successful backup as a skip.
  let phraseBackedUp = $state(false);

  async function handlePhraseContinue() {
    if (mintResult === null) return;
    await onMinted(mintResult);
  }

  function handleSkipRequest() {
    stage = 'skip-confirm';
  }

  function handleSkipCancel() {
    stage = 'backup';
  }

  async function handleSkipConfirm() {
    if (mintResult === null) return;
    // ZEB-587: owner-scope the flag so a fresh identity that skips is correctly
    // reminded later, instead of inheriting another identity's "skipped" state.
    markBackupSkipped(mintResult.state.ownerId);
    await onMinted(mintResult);
  }

  // ZEB-494: enter the "join an existing device" flow — pair this fresh device
  // into the user's existing owner identity instead of minting a new one. The
  // joiner pairing IPCs work pre-identity (the Zenoh transport + pairing state
  // machine are up at hasOwnerIdentity=false), so this reuses the existing
  // PairingJoiner unchanged.
  function handleJoinExisting() {
    mintError = null;
    stage = 'joining';
  }

  // ZEB-494: pairing completed — enrollment installed owner_state.cbor + the
  // distributed fleet KeyTree on disk. A cert-only device builds its fleet
  // engines from that KeyTree only on a fresh boot (ZEB-492 / s7), so reload to
  // re-run start_node: it loads the installed identity (hasOwnerIdentity=true →
  // this hard gate no longer mounts) and builds the engines. Cleaner than the
  // onMinted hot-flip, which only works because mint installs a seed-holder
  // in-place within the same boot.
  function handleJoinComplete() {
    location.reload();
  }

  // ZEB-454: restore the owner identity from its 24-word recovery phrase on a
  // fresh install (total-loss recovery on a new machine). Non-destructive —
  // the gate only mounts when no owner exists on this device, so there is
  // nothing to overwrite. On success, reload so start_node loads the restored
  // owner_state (same exit as a completed pairing-join).
  function handleRestoreExisting() {
    mintError = null;
    stage = 'restore';
  }
</script>

{#if open && stage === 'restore'}
  <!-- Rendered as the sole modal (gate backdrop suppressed below), mirroring
       the 'joining' branch. Cancel returns to the explain pane; the hard gate
       is not dismissed — only a successful restore (which reloads into the
       restored identity) or a mint leaves the gate. -->
  <OwnerRestoreWizard
    currentOwnerId={null}
    onRestored={() => location.reload()}
    onCancel={() => { stage = 'explain'; }}
  />
{/if}

{#if open && stage === 'joining'}
  <!-- ZEB-494: pair this device into an existing identity instead of minting.
       Rendered as the sole modal (the gate's own backdrop is suppressed below)
       so there's one modal on screen. Cancel/failure returns to the explain
       pane — the hard gate is NOT dismissed; only a completed enrollment (which
       reloads into the joined identity) or a successful mint leaves the gate. -->
  <PairingJoiner
    onComplete={handleJoinComplete}
    onClose={() => { stage = 'explain'; }}
  />
{/if}

{#if open && stage !== 'joining' && stage !== 'restore'}
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
        <div class="welcome-header">
          <HarmonyMark size={58} withDot={true} />
          <h2 id="welcome-title">Welcome to Harmony</h2>
          <p class="tagline">User-owned identity, only on this device.</p>
        </div>
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
          Already have Harmony on another device? You can add this one to your
          existing identity instead of starting fresh.
        </p>
        {#if mintError && !alreadyExists}
          <!-- Map the raw backend mint failure to friendly copy; keep the raw
               string available for bug reports inside a <details> disclosure.
               The "already exists" case is handled below by its own hint +
               Reload escape, so the raw line is suppressed there. -->
          <div class="error mint-error" data-testid="welcome-mint-error" role="alert">
            <p class="mint-error-summary">Couldn’t create your identity on this device. Please try again.</p>
            <details>
              <summary>Technical details</summary>
              <pre class="raw-error">{mintError}</pre>
            </details>
          </div>
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
            <button
              class="linklike"
              data-testid="welcome-join-existing"
              onclick={handleJoinExisting}
              disabled={stage === 'minting'}
            >
              Join another of my devices
            </button>
            <button
              class="linklike"
              data-testid="welcome-restore-mnemonic"
              onclick={handleRestoreExisting}
              disabled={stage === 'minting'}
            >
              Restore from recovery phrase
            </button>
          {/if}
        </div>
        <div class="wizard-rail">
          <WizardProgress
            steps={WIZARD_STEPS}
            activeIndex={wizardIndex}
            showCounter={stage !== 'explain'}
          />
        </div>
      {:else if stage === 'backup'}
        <h2 id="welcome-title">Your identity is ready</h2>
        <div class="backup-callout" role="note">
          <span class="backup-callout-glyph" aria-hidden="true">🔑</span>
          <p>
            You hold the <strong>only copy</strong> of this identity. Back up
            the encrypted recovery file now — without it, you can't prove this
            identity is yours if this device is lost. There's no central
            recovery.
          </p>
        </div>
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
        <p class="keychain-note">
          {keychainNote}
        </p>
        <div class="actions">
          <button
            class="primary"
            data-testid="welcome-save-backup"
            onclick={handleSaveBackup}
            disabled={[...backupPassphrase].length < MIN_RECOVERY_PASSPHRASE_LEN || backupInFlight}
          >
            {backupInFlight ? 'Saving…' : 'Save recovery file'}
          </button>
          <button class="linklike" data-testid="welcome-skip-backup" onclick={handleSkipRequest} disabled={backupInFlight}>
            Skip for now
          </button>
        </div>
        {#if mintResult !== null}
          <div class="phrase-alternative">
            <OwnerPhraseReveal
              ownerId={mintResult.state.ownerId}
              onBackedUp={() => {
                phraseBackedUp = true;
              }}
            />
            {#if phraseBackedUp}
              <div class="actions">
                <button
                  class="primary"
                  data-testid="welcome-phrase-continue"
                  onclick={handlePhraseContinue}
                  disabled={backupInFlight}
                >
                  Continue
                </button>
              </div>
            {/if}
          </div>
        {/if}
        <div class="wizard-rail">
          <WizardProgress
            steps={WIZARD_STEPS}
            activeIndex={wizardIndex}
            showCounter={stage !== 'explain'}
          />
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
    background: var(--overlay);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--surface-raised);
    color: var(--text-primary);
    font-family: var(--font-ui);
    padding: 1.75rem;
    border: 1px solid var(--border-default);
    border-radius: 10px;
    box-shadow: var(--shadow-e3);
    max-width: 520px;
    width: 90%;
  }
  .modal-content h2 {
    margin: 0 0 1rem;
    font-family: var(--font-display);
    font-size: 1.35rem;
    font-weight: 600;
    letter-spacing: -0.01em;
  }
  .welcome-header {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 1.25rem;
  }
  .welcome-header h2 { margin: 0; }
  p.tagline {
    margin: 0;
    font-family: var(--font-display);
    font-style: italic;
    font-size: 1rem;
    color: var(--accent);
  }
  .modal-content p { margin: 0 0 1rem; line-height: 1.5; }
  .muted { color: var(--text-secondary); font-size: 0.9rem; }

  /* Step 3 (backup) — the ONE clay stage. Warning callout: you hold the only
     copy. Clay tokens appear here and on the backup pip only; sage elsewhere. */
  .backup-callout {
    display: flex;
    gap: 0.6rem;
    align-items: flex-start;
    padding: 0.75rem 0.85rem;
    margin: 0 0 1rem;
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    border-radius: 8px;
  }
  .backup-callout-glyph { font-size: 1.15rem; line-height: 1.4; }
  .backup-callout p {
    margin: 0;
    color: var(--gov-clay-deep);
    font-size: 0.9rem;
    line-height: 1.5;
  }
  p.keychain-note {
    margin: 0 0 0.75rem;
    color: var(--accent);
    font-size: 0.82rem;
    line-height: 1.45;
  }

  label { display: block; margin-bottom: 0.4rem; font-size: 0.9rem; }
  input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem 0.6rem;
    background: var(--bg-primary);
    color: var(--text-primary);
    border: 1px solid var(--border-default);
    border-radius: 6px;
    margin-bottom: 0.5rem;
    transition: border-color 0.15s ease, box-shadow 0.15s ease;
  }
  input:focus {
    /* Keep the focus ring visible under forced-colors / High Contrast,
       where box-shadow is dropped (Qodo #412; see ForkConfirmDialog #411). */
    outline: 2px solid transparent;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent);
  }
  .actions { display: flex; gap: 0.5rem; margin-top: 0.5rem; align-items: center; }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border-default);
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border-radius: 6px;
    font-family: var(--font-ui);
    cursor: pointer;
  }
  .actions button.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--on-accent);
  }
  .actions button.danger {
    background: var(--danger);
    border-color: var(--danger);
    color: var(--text-bright);
  }
  /* Secondary affordances (join / restore / skip) read as sage links, not
     competing filled buttons. */
  .actions button.linklike {
    background: transparent;
    border-color: transparent;
    color: var(--accent);
    padding: 0.5rem 0.5rem;
  }
  .actions button.linklike:hover:not(:disabled) { text-decoration: underline; }
  .actions button:disabled { opacity: 0.5; cursor: default; }
  .error { color: var(--fg-error); font-size: 0.85rem; margin: 0 0 0.5rem; }
  .mint-error-summary { margin: 0 0 0.35rem; }
  .mint-error details { color: var(--text-secondary); }
  .mint-error summary { cursor: pointer; }
  .mint-error .raw-error {
    margin: 0.35rem 0 0;
    padding: 0.4rem 0.5rem;
    background: var(--bg-tertiary);
    border-radius: 4px;
    font-size: 0.78rem;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .phrase-alternative {
    margin-top: 12px;
    padding-top: 12px;
    border-top: 1px solid var(--border);
  }
  .wizard-rail {
    display: flex;
    justify-content: center;
    margin-top: 1.5rem;
  }
  footer { margin-top: 1rem; font-size: 0.85rem; }
  .version { display: inline-block; margin-right: 1rem; color: var(--text-secondary); opacity: 0.7; }
  footer a { color: var(--accent); text-decoration: none; }
  footer a:hover { text-decoration: underline; }
</style>
