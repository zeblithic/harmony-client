<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { open } from '@tauri-apps/plugin-dialog';
  import { onMount } from 'svelte';
  import { OwnerService } from '../owner-service';

  const svc = new OwnerService();

  function assertNever(x: never): never {
    throw new Error(`unhandled wizard variant: ${JSON.stringify(x)}`);
  }

  // `fullHash` is `null` until `current_identity_hash` resolves on mount.
  // Anything that reads/decides on the current identity (the typed-prefix
  // confirm gate, the Backup/Restore buttons, copy-to-clipboard) must check
  // for null first — otherwise an empty-string slice can silently bypass the
  // confirm gate before the hash arrives.
  let fullHash = $state<string | null>(null);
  let hashLoaded = $derived(fullHash !== null && fullHash.length === 32);
  let displayHash = $derived(fullHash ? `0x${fullHash.slice(0, 8)}…` : '…');
  // Empty when the hash is unloaded — paired with `hashLoaded` to keep the
  // typed-prefix gate uncomparable until the real hash is in hand.
  let currentPrefix = $derived(fullHash ? fullHash.slice(0, 8) : '');
  let loadError = $state<string | null>(null);
  // True while a backup save dialog or export IPC call is in flight. Blocks
  // the Continue button so a fast double-click cannot queue parallel save
  // dialogs and parallel export_recovery_file_to_path writes (CodeRabbit,
  // PR #66 review). Cleared in advanceFromFileEntry's outer finally.
  let backupInFlight = $state(false);
  // True while a restore IPC call is in flight. Blocks the Replace button so
  // double-click cannot issue duplicate restore_* requests against the store.
  let restoreInFlight = $state(false);

  // Wire-format from Rust `RestoreInfo` / `PreviewedRecovery` (camelCase per
  // project convention — see `src-tauri/src/identity_commands.rs` and every
  // other IPC payload struct in `lib.rs`).
  //
  // `previewToken` is set only for file-based restore. It identifies the
  // server-side cached seed from `preview_recovery_file` — `commitRestore`
  // passes it back to `restore_recovery_from_preview_token` so the backend
  // restores the EXACT seed the user just saw the hash for, even if the
  // recovery file on disk has changed in the meantime.
  // The mnemonic-based restore re-derives from the words at commit time, so
  // it has no token.
  interface RestoreCandidate {
    identityHash: string;
    mintedAt?: number;
    comment?: string;
    previewToken?: string;
  }

  type BackupStep =
    | { phase: 'pickType' }                                                                                               // step 1
    | { phase: 'mnemonicReveal'; words: string[]; revealed: boolean; storedSafely: boolean; loadError: string | null }   // step 2a
    | { phase: 'fileEntry'; passphrase: string; passphraseConfirm: string; comment: string; showPass: boolean }          // step 2b
    | { phase: 'fileSaved'; savedPath: string }                                                                           // step 3b success
    | { phase: 'fileSaveError'; error: string; passphrase: string; passphraseConfirm: string; comment: string };          // step 3b error — carries prior input so Back can restore the form

  type RestoreStep =
    | { phase: 'pickSource' }                                                                                             // step 1
    | { phase: 'mnemonicEntry'; input: string; validationError: string | null }                                          // step 2a
    | { phase: 'fileEntry'; pendingFilePath: string; passphrase: string; showPass: boolean; restoreError: string | null }  // step 2b before decrypt
    | { phase: 'fileDecrypted'; pendingFilePath: string; restoreCandidate: RestoreCandidate }                              // step 2b after decrypt — no passphrase: token replaces it
    | { phase: 'confirm'; restoreSource: 'mnemonic' | 'file'; pendingWords: string[]; pendingFilePath?: string; restoreCandidate: RestoreCandidate; typedPrefix: string }  // step 3 — no passphrase: commit goes through previewToken
    | { phase: 'commitError'; error: string }                                                                             // step 3 error
    | { phase: 'done'; postRestoreHash: string };                                                                         // step 4

  type WizardState =
    | { kind: 'idle' }
    | { kind: 'backup'; step: BackupStep }
    | { kind: 'restore'; step: RestoreStep };

  let wizardState = $state<WizardState>({ kind: 'idle' });

  // Compile-time exhaustiveness check: the compiler proves this switch is
  // exhaustive over BackupStep / RestoreStep. If a new variant is added in
  // Tasks 6-9 without a matching case here, tsc will error at the
  // assertNever(step) call — catching forgotten-variant bugs before runtime.
  function checkExhaustive(state: WizardState): void {
    if (state.kind === 'backup') {
      const step = state.step;
      switch (step.phase) {
        case 'pickType':
        case 'mnemonicReveal':
        case 'fileEntry':
        case 'fileSaved':
        case 'fileSaveError':
          break;
        default:
          assertNever(step);
      }
    } else if (state.kind === 'restore') {
      const step = state.step;
      switch (step.phase) {
        case 'pickSource':
        case 'mnemonicEntry':
        case 'fileEntry':
        case 'fileDecrypted':
        case 'confirm':
        case 'commitError':
        case 'done':
          break;
        default:
          assertNever(step);
      }
    }
  }
  // Silence the unused-variable warning — this function is intentionally
  // called only for its compile-time type narrowing.
  void checkExhaustive;

  // Transient UI state for pickType step (not yet committed to wizardState)
  let selectedBackupType = $state<'mnemonic' | 'file' | null>(null);

  onMount(async () => {
    try {
      fullHash = await invoke<string>('current_identity_hash');
    } catch (e) {
      loadError = `Could not read identity store: ${e}. The wizard cannot continue.`;
    }
  });

  async function copyText(s: string): Promise<void> {
    if (!s || !navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(s);
    } catch {
      // Some browsers reject when document is unfocused. User can retry.
    }
  }

  async function copyHash() {
    if (fullHash) await copyText(fullHash);
  }

  function resetToIdle() {
    wizardState = { kind: 'idle' };
    selectedBackupType = null;
    selectedRestoreSource = null;
  }

  async function advanceFromPickType() {
    if (!selectedBackupType) return;
    if (selectedBackupType === 'mnemonic') {
      // Capture the current state so we can detect if the user cancelled
      // (or otherwise transitioned away) while the invoke was pending.
      const epoch = wizardState;
      let words: string[];
      try {
        words = await invoke<string[]>('export_mnemonic_words');
      } catch (e) {
        // Same guard for the error path: don't resurrect a cancelled wizard.
        if (wizardState !== epoch) return;
        wizardState = {
          kind: 'backup',
          step: { phase: 'mnemonicReveal', words: [], revealed: false, storedSafely: false, loadError: `Could not load recovery phrase: ${e}` },
        };
        return;
      }
      if (wizardState !== epoch) return;
      wizardState = {
        kind: 'backup',
        step: { phase: 'mnemonicReveal', words, revealed: false, storedSafely: false, loadError: null },
      };
    } else {
      // No await on this path — direct transition is safe.
      wizardState = {
        kind: 'backup',
        step: { phase: 'fileEntry', passphrase: '', passphraseConfirm: '', comment: '', showPass: false },
      };
    }
  }

  async function advanceFromFileEntry() {
    if (wizardState.kind !== 'backup' || wizardState.step.phase !== 'fileEntry') return;
    const { passphrase, passphraseConfirm, comment } = wizardState.step;
    if (!passphrase || passphrase !== passphraseConfirm) return;

    // Set busy flag BEFORE the save dialog opens so a fast double-click on
    // Continue cannot queue parallel dialogs and parallel exports
    // (CodeRabbit, PR #66 review). Outer try/finally guarantees the flag
    // is cleared on every exit path — including the epoch-cancel returns
    // where wizardState changed under us during a pending await.
    backupInFlight = true;
    try {
      // Capture epoch before first await (save dialog).
      const epoch = wizardState;

      let pathToken: string | null;
      try {
        pathToken = await svc.requestExportSavePath({
          title: 'Save recovery file',
          defaultFilename: 'identity.recovery',
          filterName: 'Recovery file',
          filterExtensions: ['recovery'],
        });
      } catch (e) {
        // Pre-refactor this swallowed silently — but the new wider IPC
        // surface (request_export_save_path) can fail for reasons that
        // user-cancel doesn't cover (dispatch error, plugin error, etc.),
        // so genuine failures need to be surfaced. `null` still signals
        // user-cancel (Ok(None) over Tauri serde) — that branch below
        // returns silently. (CodeRabbit, PR #66 review.)
        if (wizardState !== epoch) return;
        wizardState = {
          kind: 'backup',
          step: {
            phase: 'fileSaveError',
            error: `Could not open the save dialog: ${e}. Try again.`,
            passphrase,
            passphraseConfirm,
            comment,
          },
        };
        return;
      }

      // Guard: did the user cancel the wizard while the dialog was open?
      if (wizardState !== epoch) return;

      // User cancelled the dialog — silent return per spec.
      if (pathToken === null) return;

      // Capture epoch again before the second await (file write).
      const epoch2 = wizardState;

      try {
        const savedPath = await invoke<string>('export_recovery_file_to_path', {
          pathToken,
          passphrase,
          comment: comment || null,
        });

        if (wizardState !== epoch2) return;
        wizardState = { kind: 'backup', step: { phase: 'fileSaved', savedPath } };
      } catch (e) {
        if (wizardState !== epoch2) return;
        // Carry the form input forward so Back returns the user to a populated
        // form rather than blank fields (Cursor review feedback).
        wizardState = {
          kind: 'backup',
          step: {
            phase: 'fileSaveError',
            error: `Could not save to the chosen location: ${e}. Try a different location.`,
            passphrase,
            passphraseConfirm,
            comment,
          },
        };
      }
    } finally {
      backupInFlight = false;
    }
  }

  // Transient UI state for pickSource step (not yet committed to wizardState)
  let selectedRestoreSource = $state<'mnemonic' | 'file' | null>(null);

  function advanceFromPickSource() {
    if (!selectedRestoreSource) return;
    if (selectedRestoreSource === 'mnemonic') {
      wizardState = {
        kind: 'restore',
        step: { phase: 'mnemonicEntry', input: '', validationError: null },
      };
    } else {
      wizardState = {
        kind: 'restore',
        step: { phase: 'fileEntry', pendingFilePath: '', passphrase: '', showPass: false, restoreError: null },
      };
    }
    selectedRestoreSource = null;
  }

  // Token-count rule for the mnemonic textarea — used in the live word-count
  // display, the Continue gate, and the Continue handler. Single source of truth
  // so the three sites can't drift apart (e.g. if the rule ever changes to
  // strip non-word characters).
  function countWords(s: string): number {
    return s.split(/\s+/).filter(w => w.length > 0).length;
  }

  async function advanceFromMnemonicEntry() {
    if (wizardState.kind !== 'restore' || wizardState.step.phase !== 'mnemonicEntry') return;
    const { input } = wizardState.step;
    const words = input.split(/\s+/).filter(w => w.length > 0);
    if (words.length !== 24) return;  // gate matches countWords() in the template

    // Clear any prior validation error before the invoke.
    wizardState = { kind: 'restore', step: { phase: 'mnemonicEntry', input, validationError: null } };

    // Race-pattern epoch guard.
    const epoch = wizardState;

    let candidateHash: string;
    try {
      candidateHash = await invoke<string>('preview_mnemonic_identity', { words });
    } catch (e) {
      if (wizardState !== epoch) return;
      const msg = String(e);
      let friendly: string;
      if (/checksum/i.test(msg)) {
        friendly = "These 24 words don't form a valid recovery phrase. Double-check your transcription.";
      } else if (/wordlist|not.*word/i.test(msg)) {
        friendly = "One or more words isn't a recognized recovery word.";
      } else {
        friendly = `Could not parse recovery phrase: ${msg}`;
      }
      wizardState = { kind: 'restore', step: { phase: 'mnemonicEntry', input, validationError: friendly } };
      return;
    }

    if (wizardState !== epoch) return;
    wizardState = {
      kind: 'restore',
      step: {
        phase: 'confirm',
        restoreSource: 'mnemonic',
        pendingWords: words,
        restoreCandidate: { identityHash: candidateHash },
        typedPrefix: '',
      },
    };
  }

  async function advanceFromRestoreFileEntry() {
    if (wizardState.kind !== 'restore' || wizardState.step.phase !== 'fileEntry') return;

    // Capture epoch before the first await (open dialog).
    const epoch = wizardState;

    let pickedPath: string | string[] | null;
    try {
      pickedPath = await open({
        title: 'Open recovery file',
        filters: [{ name: 'Recovery file', extensions: ['recovery'] }],
        multiple: false,
      });
    } catch {
      // Treat dialog error the same as cancel — silent return.
      return;
    }

    // Guard: did the wizard change while the dialog was open?
    if (wizardState !== epoch) return;

    // User cancelled the dialog — silent return per spec.
    if (!pickedPath) return;

    // open() with multiple: false returns string | null (not string[]).
    const filePath = Array.isArray(pickedPath) ? pickedPath[0] : pickedPath;

    // Reflect the picked path into wizardState. Clear any prior decrypt/IO
    // error: it referred to the OLD file and would otherwise persist next to
    // the new file path (Cursor + CodeRabbit round 3) until the user touched
    // the passphrase field.
    wizardState = {
      kind: 'restore',
      step: { ...wizardState.step, pendingFilePath: filePath, restoreError: null },
    };
  }

  async function decryptRestoreFile() {
    if (wizardState.kind !== 'restore' || wizardState.step.phase !== 'fileEntry') return;
    const step = wizardState.step;
    if (!step.pendingFilePath || !step.passphrase) return;

    // Capture epoch before the await (invoke).
    const epoch = wizardState;

    let candidate: RestoreCandidate;
    try {
      candidate = await invoke<RestoreCandidate>('preview_recovery_file', {
        inPath: step.pendingFilePath,
        passphrase: step.passphrase,
      });
    } catch (e) {
      if (wizardState !== epoch) return;
      // The backend prefixes IO failures with "failed to read <path>:". We
      // surface those verbatim so the user sees the actual filesystem error
      // (missing file, permissions, file too large) rather than a misleading
      // "passphrase incorrect" message. Genuine decrypt/AEAD failures keep
      // the spec's ambiguous text — we don't leak which input was wrong.
      // Tauri rejects with a string in production, but tests use Error;
      // unwrap to .message in both paths so the prefix-detection works.
      const msg = e instanceof Error ? e.message : String(e);
      const isIoError =
        msg.startsWith('failed to read ') ||
        msg.includes(' is too large to be a recovery file');
      wizardState = {
        kind: 'restore',
        step: {
          ...wizardState.step,
          restoreError: isIoError
            ? msg
            : 'Could not decrypt — passphrase incorrect or file corrupted.',
        },
      };
      return;
    }

    if (wizardState !== epoch) return;
    // The passphrase is intentionally NOT carried into fileDecrypted: once
    // preview returns a token, the commit IPC takes only the token. Keeping
    // the passphrase in reactive state would prolong secret retention with
    // no upside (CodeRabbit round 5).
    wizardState = {
      kind: 'restore',
      step: {
        phase: 'fileDecrypted',
        pendingFilePath: step.pendingFilePath,
        restoreCandidate: candidate,
      },
    };
  }

  function advanceFromFileDecrypted() {
    if (wizardState.kind !== 'restore' || wizardState.step.phase !== 'fileDecrypted') return;
    const step = wizardState.step;
    wizardState = {
      kind: 'restore',
      step: {
        phase: 'confirm',
        restoreSource: 'file',
        pendingWords: [],
        pendingFilePath: step.pendingFilePath,
        restoreCandidate: step.restoreCandidate,
        typedPrefix: '',
      },
    };
  }

  async function commitRestore() {
    if (wizardState.kind !== 'restore' || wizardState.step.phase !== 'confirm') return;
    // Reentrancy guard: the button is disabled while in-flight, but a fast
    // double-click can still queue two synchronous callbacks before Svelte's
    // disabled state propagates. Returning here (and below) makes the
    // function effectively idempotent at the function-call boundary.
    if (restoreInFlight) return;
    // Defensive: the button is also gated on hashLoaded, but a non-UI caller
    // (or a future code path) shouldn't be able to commit a restore against
    // an unloaded current hash — the typed-prefix gate would have compared
    // against an empty string.
    if (!hashLoaded) return;
    const step = wizardState.step;

    // Runtime guard: file-restore commits on the previewToken from
    // restoreCandidate, NOT pendingFilePath / passphrase. The token was
    // populated by decryptRestoreFile from the preview IPC response.
    // Defends against a future refactor that loses the token.
    if (step.restoreSource === 'file' && !step.restoreCandidate.previewToken) {
      wizardState = {
        kind: 'restore',
        step: { phase: 'commitError', error: 'Internal error: preview token missing — re-pick the recovery file.' },
      };
      return;
    }

    const epoch = wizardState;
    restoreInFlight = true;

    try {
      let postRestoreHash: string;
      if (step.restoreSource === 'mnemonic') {
        postRestoreHash = await invoke<string>('restore_mnemonic_from_words', { words: step.pendingWords });
      } else {
        const info = await invoke<{ identityHash: string }>('restore_recovery_from_preview_token', {
          previewToken: step.restoreCandidate.previewToken,
        });
        postRestoreHash = info.identityHash;
      }
      if (wizardState !== epoch) return;
      wizardState = { kind: 'restore', step: { phase: 'done', postRestoreHash } };
    } catch (e) {
      if (wizardState !== epoch) return;
      wizardState = { kind: 'restore', step: { phase: 'commitError', error: String(e) } };
    } finally {
      restoreInFlight = false;
    }
  }

  async function finishRestore() {
    // Capture the hash we know succeeded — the commit returned it. If the
    // refresh below fails (network blip, IPC error), we still have a correct
    // value to display rather than the stale pre-restore one.
    const knownPostRestoreHash =
      wizardState.kind === 'restore' && wizardState.step.phase === 'done'
        ? wizardState.step.postRestoreHash
        : null;
    const epoch = wizardState;
    try {
      const refreshed = await invoke<string>('current_identity_hash');
      if (wizardState !== epoch) return;
      fullHash = refreshed;
    } catch {
      // Refresh failed — fall back to the value commitRestore returned, which
      // we know matches what's on disk because commit succeeded.
      if (wizardState !== epoch) return;
      if (knownPostRestoreHash) fullHash = knownPostRestoreHash;
    }
    if (wizardState !== epoch) return;
    wizardState = { kind: 'idle' };
    selectedRestoreSource = null;
  }
</script>

{#if loadError}
  <section class="identity-panel" aria-label="Identity">
    <h3 class="section-title">Your Identity</h3>
    <p class="error">{loadError}</p>
  </section>
{:else if wizardState.kind === 'idle'}
  <section class="identity-panel" aria-label="Identity">
    <h3 class="section-title">Your Identity</h3>
    <div class="hash-row">
      <span class="label">Identity hash</span>
      <button
        class="hash-display"
        title="Click to copy full identity hash"
        onclick={copyHash}
      >
        {displayHash}
      </button>
    </div>
    <div class="actions">
      <button
        disabled={!hashLoaded}
        onclick={() => (wizardState = { kind: 'backup', step: { phase: 'pickType' } })}
      >Backup…</button>
      <button
        disabled={!hashLoaded}
        onclick={() => (wizardState = { kind: 'restore', step: { phase: 'pickSource' } })}
      >Restore…</button>
    </div>
    <p class="explainer">
      Back up your identity to a 24-word phrase or an encrypted file.
      Restore replaces your current identity — the current one becomes unrecoverable.
    </p>
  </section>
{:else if wizardState.kind === 'backup'}
  {#if wizardState.step.phase === 'pickType'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Choose backup type</h3>
      <fieldset class="backup-type-picker">
        <legend class="visually-hidden">Backup type</legend>
        <label>
          <input type="radio" bind:group={selectedBackupType} value="mnemonic" />
          24-word recovery phrase
        </label>
        <label>
          <input type="radio" bind:group={selectedBackupType} value="file" />
          Encrypted recovery file
        </label>
      </fieldset>
      <div class="actions">
        <button onclick={resetToIdle}>Cancel</button>
        <button disabled={!selectedBackupType} onclick={advanceFromPickType}>Continue</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'mnemonicReveal'}
    <section class="identity-panel" aria-label="Identity">
      {#if wizardState.step.loadError}
        <h3 class="section-title">Backup identity</h3>
        <p class="error">{wizardState.step.loadError}</p>
        <div class="actions">
          <button onclick={resetToIdle}>Back to settings</button>
        </div>
      {:else}
        <h3 class="section-title">Your recovery phrase</h3>
        <p class="hash-anchor">Backing up identity {displayHash}</p>
        <p class="explainer">
          Write these 24 words down. Anyone with them can recover your identity.
          There is no way to retrieve them later.
        </p>
        <ol
          data-testid="mnemonic-grid"
          class="mnemonic-grid"
          class:blurred={!wizardState.step.revealed}
        >
          {#each wizardState.step.words as w, i (i)}
            <li class="word">{w}</li>
          {/each}
        </ol>
        {#if !wizardState.step.revealed}
          <button onclick={() => {
            if (wizardState.kind === 'backup' && wizardState.step.phase === 'mnemonicReveal') {
              wizardState = { kind: 'backup', step: { ...wizardState.step, revealed: true } };
            }
          }}>Reveal</button>
        {:else}
          <label class="confirm-label">
            <input
              type="checkbox"
              checked={wizardState.step.storedSafely}
              onchange={() => {
                if (wizardState.kind === 'backup' && wizardState.step.phase === 'mnemonicReveal') {
                  wizardState = { kind: 'backup', step: { ...wizardState.step, storedSafely: !wizardState.step.storedSafely } };
                }
              }}
            />
            I've stored this safely
          </label>
        {/if}
        <div class="actions">
          <button onclick={resetToIdle}>Cancel</button>
          <button
            disabled={!wizardState.step.revealed || !wizardState.step.storedSafely}
            onclick={resetToIdle}
          >Done</button>
        </div>
      {/if}
    </section>
  {:else if wizardState.step.phase === 'fileEntry'}
    <section class="identity-panel" aria-label="Identity">
      <p class="hash-anchor">Backing up identity {displayHash}</p>
      <h3 class="section-title">Recovery file passphrase</h3>
      <p class="explainer">
        This passphrase encrypts your recovery file. You'll need it to
        restore later. Don't reuse your account password — pick something
        you can remember or store in a password manager.
      </p>
      <label class="field-label">
        Passphrase
        <div class="passphrase-row">
          <input
            type={wizardState.step.showPass ? 'text' : 'password'}
            aria-label="Passphrase"
            value={wizardState.step.passphrase}
            oninput={(e) => {
              if (wizardState.kind === 'backup' && wizardState.step.phase === 'fileEntry') {
                wizardState = { kind: 'backup', step: { ...wizardState.step, passphrase: (e.target as HTMLInputElement).value } };
              }
            }}
            autocomplete="new-password"
          />
          <button
            type="button"
            aria-label={wizardState.step.showPass ? 'Hide passphrase' : 'Show passphrase'}
            onclick={() => {
              if (wizardState.kind === 'backup' && wizardState.step.phase === 'fileEntry') {
                wizardState = { kind: 'backup', step: { ...wizardState.step, showPass: !wizardState.step.showPass } };
              }
            }}
          >{wizardState.step.showPass ? '🙈' : '👁'}</button>
        </div>
      </label>
      <label class="field-label">
        Confirm passphrase
        <input
          type={wizardState.step.showPass ? 'text' : 'password'}
          aria-label="Confirm passphrase"
          value={wizardState.step.passphraseConfirm}
          oninput={(e) => {
            if (wizardState.kind === 'backup' && wizardState.step.phase === 'fileEntry') {
              wizardState = { kind: 'backup', step: { ...wizardState.step, passphraseConfirm: (e.target as HTMLInputElement).value } };
            }
          }}
          autocomplete="new-password"
        />
      </label>
      <label class="field-label">
        Comment (optional)
        <input
          type="text"
          aria-label="Comment (optional)"
          value={wizardState.step.comment}
          oninput={(e) => {
            if (wizardState.kind === 'backup' && wizardState.step.phase === 'fileEntry') {
              wizardState = { kind: 'backup', step: { ...wizardState.step, comment: (e.target as HTMLInputElement).value } };
            }
          }}
          placeholder="laptop-2026-04-15"
        />
      </label>
      <div class="actions">
        <button onclick={resetToIdle}>Cancel</button>
        <button
          disabled={backupInFlight || !wizardState.step.passphrase || wizardState.step.passphrase !== wizardState.step.passphraseConfirm}
          onclick={advanceFromFileEntry}
        >Continue</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'fileSaved'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Recovery file saved</h3>
      <p class="hash-anchor">Backing up identity {displayHash}</p>
      <p>✓ Wrote recovery file to <code>{wizardState.step.savedPath}</code></p>
      <div class="actions">
        <button onclick={resetToIdle}>Done</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'fileSaveError'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Recovery file save failed</h3>
      <p class="error">{wizardState.step.error}</p>
      <div class="actions">
        <button onclick={() => {
          if (wizardState.kind === 'backup' && wizardState.step.phase === 'fileSaveError') {
            const { passphrase, passphraseConfirm, comment } = wizardState.step;
            wizardState = {
              kind: 'backup',
              step: { phase: 'fileEntry', passphrase, passphraseConfirm, comment, showPass: false },
            };
          }
        }}>Back</button>
        <button onclick={resetToIdle}>Cancel</button>
      </div>
    </section>
  {/if}
{:else if wizardState.kind === 'restore'}
  {#if wizardState.step.phase === 'pickSource'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Restore identity</h3>
      <p class="explainer">Choose how to restore your identity. The current identity will be permanently replaced.</p>
      <fieldset class="backup-type-picker">
        <legend class="visually-hidden">Restore source</legend>
        <label>
          <input type="radio" bind:group={selectedRestoreSource} value="mnemonic" />
          24-word recovery phrase
        </label>
        <label>
          <input type="radio" bind:group={selectedRestoreSource} value="file" />
          Recovery file
        </label>
      </fieldset>
      <div class="actions">
        <button onclick={resetToIdle}>Cancel</button>
        <button disabled={!selectedRestoreSource} onclick={advanceFromPickSource}>Continue</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'mnemonicEntry'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Restore from recovery phrase</h3>
      <label class="field-label">
        Recovery phrase
        <textarea
          class="mnemonic-textarea"
          aria-label="Recovery phrase"
          rows={6}
          placeholder="Paste your 24 words here. Spaces or newlines are fine."
          value={wizardState.step.input}
          oninput={(e) => {
            if (wizardState.kind === 'restore' && wizardState.step.phase === 'mnemonicEntry') {
              wizardState = {
                kind: 'restore',
                step: { phase: 'mnemonicEntry', input: (e.target as HTMLTextAreaElement).value, validationError: null },
              };
            }
          }}
        ></textarea>
      </label>
      <p class="word-count">
        {countWords(wizardState.step.input)} / 24 words
      </p>
      {#if wizardState.step.validationError !== null}
        <p class="inline-error" role="alert">{wizardState.step.validationError}</p>
      {/if}
      <div class="actions">
        <button onclick={resetToIdle}>Cancel</button>
        <button
          disabled={countWords(wizardState.step.input) !== 24}
          onclick={advanceFromMnemonicEntry}
        >Continue</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'fileEntry'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Restore from recovery file</h3>
      <div class="actions">
        <button onclick={advanceFromRestoreFileEntry}>Pick recovery file…</button>
      </div>
      {#if wizardState.step.pendingFilePath}
        <p class="file-path-display">File: <code>{wizardState.step.pendingFilePath}</code></p>
        <label class="field-label">
          Passphrase
          <div class="passphrase-row">
            <input
              type={wizardState.step.showPass ? 'text' : 'password'}
              aria-label="Passphrase"
              value={wizardState.step.passphrase}
              oninput={(e) => {
                if (wizardState.kind === 'restore' && wizardState.step.phase === 'fileEntry') {
                  wizardState = { kind: 'restore', step: { ...wizardState.step, passphrase: (e.target as HTMLInputElement).value, restoreError: null } };
                }
              }}
              autocomplete="current-password"
            />
            <button
              type="button"
              aria-label={wizardState.step.showPass ? 'Hide passphrase' : 'Show passphrase'}
              onclick={() => {
                if (wizardState.kind === 'restore' && wizardState.step.phase === 'fileEntry') {
                  wizardState = { kind: 'restore', step: { ...wizardState.step, showPass: !wizardState.step.showPass } };
                }
              }}
            >{wizardState.step.showPass ? '🙈' : '👁'}</button>
          </div>
        </label>
        {#if wizardState.step.restoreError !== null}
          <p class="inline-error" role="alert">{wizardState.step.restoreError}</p>
        {/if}
      {/if}
      <div class="actions">
        <button onclick={resetToIdle}>Cancel</button>
        <button
          disabled={!wizardState.step.pendingFilePath || !wizardState.step.passphrase}
          onclick={decryptRestoreFile}
        >Decrypt</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'fileDecrypted'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Restore from recovery file</h3>
      <p class="success-text">✓ Decrypted <code>{wizardState.step.pendingFilePath}</code></p>
      <p class="field-label">
        Restored identity hash:
        <button
          class="hash-display"
          title="Click to copy full identity hash"
          onclick={() => copyText(wizardState.kind === 'restore' && wizardState.step.phase === 'fileDecrypted' ? wizardState.step.restoreCandidate.identityHash : '')}
        >0x{wizardState.step.restoreCandidate.identityHash.slice(0, 8)}…</button>
      </p>
      <div class="backup-meta">
        <p class="field-label">Backup metadata:</p>
        {#if wizardState.step.restoreCandidate.mintedAt != null}
          <p class="meta-row">Minted: {new Date(wizardState.step.restoreCandidate.mintedAt * 1000).toISOString()}</p>
        {/if}
        {#if wizardState.step.restoreCandidate.comment}
          <p class="meta-row">Comment: {wizardState.step.restoreCandidate.comment}</p>
        {/if}
      </div>
      <div class="actions">
        <button onclick={resetToIdle}>Cancel</button>
        <button onclick={advanceFromFileDecrypted}>Continue</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'confirm'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Confirm overwrite</h3>
      <p class="warning-text">
        Warning: replacing your identity is unrecoverable. Your current identity will be permanently lost.
        Make sure you have a backup of your current identity before proceeding.
      </p>
      <div class="hash-diff">
        <span class="hash-diff-label">Current</span>
        <span class="hash-diff-value hash-diff-current">0x{currentPrefix}…</span>
        <span class="hash-diff-arrow">→</span>
        <span class="hash-diff-label">Restored</span>
        <span class="hash-diff-value hash-diff-new">0x{wizardState.step.restoreCandidate.identityHash.slice(0, 8)}…</span>
      </div>
      <label class="field-label">
        Type the first 8 chars of your CURRENT identity hash to proceed: ({currentPrefix})
        <input
          type="text"
          aria-label="Confirm current identity hash prefix"
          placeholder={currentPrefix}
          value={wizardState.step.typedPrefix}
          oninput={(e) => {
            if (wizardState.kind === 'restore' && wizardState.step.phase === 'confirm') {
              wizardState = { kind: 'restore', step: { ...wizardState.step, typedPrefix: (e.target as HTMLInputElement).value } };
            }
          }}
          autocomplete="off"
          spellcheck={false}
        />
      </label>
      {#if wizardState.step.typedPrefix.length > 0 && wizardState.step.typedPrefix !== currentPrefix}
        <p class="inline-error" role="alert">That doesn't match your current identity hash.</p>
      {/if}
      <div class="actions">
        <button onclick={resetToIdle} disabled={restoreInFlight}>Cancel</button>
        <button
          disabled={!hashLoaded || restoreInFlight || wizardState.step.typedPrefix !== currentPrefix}
          onclick={commitRestore}
        >{restoreInFlight ? 'Replacing…' : 'Replace identity'}</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'commitError'}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">Restore failed</h3>
      <p class="error">{wizardState.step.error}</p>
      <div class="actions">
        <button onclick={() => {
          if (wizardState.kind === 'restore' && wizardState.step.phase === 'commitError') {
            // Go back to source picker so user can retry from scratch
            wizardState = { kind: 'restore', step: { phase: 'pickSource' } };
          }
        }}>Back</button>
        <button onclick={resetToIdle}>Cancel</button>
      </div>
    </section>
  {:else if wizardState.step.phase === 'done'}
    {@const restoredHash = wizardState.step.postRestoreHash}
    <section class="identity-panel" aria-label="Identity">
      <h3 class="section-title">✓ Identity restored</h3>
      <p class="hash-anchor">New identity hash:</p>
      <button
        class="hash-display"
        title="Click to copy full identity hash"
        onclick={() => copyText(restoredHash)}
      >0x{restoredHash.slice(0, 8)}…</button>
      <p class="explainer">
        Verify this matches what you expected. If it does not match your backup's expected hash,
        restore again from the correct backup before performing any other action.
      </p>
      <div class="actions">
        <button onclick={finishRestore}>Done</button>
      </div>
    </section>
  {/if}
{/if}

<style>
  .identity-panel { padding: 16px; }
  .section-title {
    margin: 0 0 12px;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .hash-row { display: flex; align-items: center; gap: 8px; margin: 8px 0; }
  .hash-display {
    font-family: ui-monospace, monospace;
    background: var(--bg-tertiary);
    padding: 6px 10px;
    border-radius: 4px;
    border: none;
    color: inherit;
    cursor: pointer;
  }
  .hash-display:hover { background: var(--border); }
  .actions { display: flex; gap: 8px; margin: 16px 0; }
  .explainer { color: var(--text-secondary); font-size: 0.85em; margin-top: 14px; }
  .error { color: var(--danger); }
  .visually-hidden {
    position: absolute; width: 1px; height: 1px; padding: 0;
    margin: -1px; overflow: hidden; clip: rect(0, 0, 0, 0);
    white-space: nowrap; border: 0;
  }
  .backup-type-picker {
    border: none; padding: 0; margin: 8px 0 16px;
    display: flex; flex-direction: column; gap: 8px;
  }
  .backup-type-picker label {
    display: flex; align-items: center; gap: 8px;
    cursor: pointer; color: var(--text-primary);
  }
  .mnemonic-grid {
    display: grid;
    grid-template-columns: repeat(4, 1fr);
    gap: 6px;
    background: var(--bg-tertiary);
    border-radius: 6px;
    padding: 12px 12px 12px 36px; /* left padding leaves room for the list marker */
    font-family: ui-monospace, monospace;
    font-size: 0.85em;
    margin: 12px 0;
    list-style: decimal;
  }
  .mnemonic-grid.blurred { filter: blur(6px); user-select: none; }
  .word { padding: 2px 0; }
  .confirm-label {
    display: flex; align-items: center; gap: 8px;
    margin: 12px 0; cursor: pointer; color: var(--text-primary);
  }
  .hash-anchor { color: var(--text-secondary); font-size: 0.85em; margin: 4px 0 8px; }
  .warning-text {
    color: var(--text-secondary);
    font-size: 0.85em;
    margin: 8px 0 12px;
    padding: 8px 10px;
    background: var(--bg-tertiary);
    border-radius: 4px;
    border-left: 3px solid var(--border);
  }
  .hash-diff {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin: 12px 0;
    font-family: ui-monospace, monospace;
    font-size: 0.85em;
  }
  .hash-diff-label { color: var(--text-muted); }
  .hash-diff-value { color: var(--text-primary); }
  .hash-diff-current { opacity: 0.65; }
  .hash-diff-new { color: var(--accent); }
  .hash-diff-arrow { color: var(--text-muted); }
  .inline-error { color: var(--danger); font-size: 0.85em; margin: 4px 0; }
  .field-label {
    display: flex; flex-direction: column; gap: 4px;
    margin: 8px 0; color: var(--text-primary); font-size: 0.9em;
  }
  .field-label input {
    flex: 1;
    padding: 6px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: inherit;
    font-size: inherit;
  }
  .passphrase-row {
    display: flex; gap: 6px; align-items: center;
  }
  .passphrase-row input { flex: 1; }
  .mnemonic-textarea {
    font-family: ui-monospace, monospace;
    padding: 6px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: inherit;
    font-size: inherit;
    resize: vertical;
    width: 100%;
    box-sizing: border-box;
  }
  .word-count {
    color: var(--text-secondary);
    font-size: 0.85em;
    margin: 4px 0;
  }
  .file-path-display {
    color: var(--text-secondary);
    font-size: 0.85em;
    margin: 8px 0;
  }
  .success-text {
    color: var(--text-secondary);
    font-size: 0.9em;
    margin: 8px 0;
  }
  .backup-meta {
    margin: 8px 0 12px;
  }
  .meta-row {
    color: var(--text-secondary);
    font-size: 0.85em;
    margin: 2px 0;
    padding-left: 8px;
  }
</style>
