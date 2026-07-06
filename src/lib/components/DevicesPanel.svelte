<script lang="ts">
  import { onMount } from 'svelte';
  import { OwnerService, extractError, type OwnerStateView } from '../owner-service';
  import { loadProfile } from '../profile-service';
  import { loadDeviceLabel, saveDeviceLabel, resolveDefaultDeviceLabel } from '../device-label-service';
  import { setButlerPin, extractButlerPinError } from '../butler-pin-service';
  import {
    MAX_RECOVERY_COMMENT_BYTES,
    MIN_RECOVERY_PASSPHRASE_LEN,
  } from '../recovery-policy';
  import PairingInviter from './PairingInviter.svelte';
  import PairingJoiner from './PairingJoiner.svelte';
  import Modal from './Modal.svelte';
  import OwnerRestoreWizard from './OwnerRestoreWizard.svelte';

  let svc = new OwnerService();
  let state = $state<OwnerStateView | null>(null);
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let modalOpen = $state(false);
  let mintInFlight = $state(false);
  let mintError = $state<string | null>(null);
  let recoveryToken = $state<string | null>(null);

  // Per-device label (owner-private). Seeded from the store; defaulted to the
  // OS hostname on first run in onMount.
  let deviceLabel = $state<string | null>(loadDeviceLabel());

  /**
   * ZEB-336: the owner header and the this-device row are DISTINCT names.
   *
   * - Owner header ← `profile.displayName` (the owner's canonical name, also
   *   broadcast owner-keyed by profile_card_broadcast).
   * - This-device row ← the per-device LABEL store (`device-label-service`),
   *   which is owner-private and never overlaid from the owner name.
   *
   * The backend has no access to localStorage, so it returns placeholders for
   * both; we overlay each from its own local store. Defensive: a missing store
   * value leaves the backend value in place rather than blanking the field.
   */
  function applyLocalOverlay(view: OwnerStateView | null): OwnerStateView | null {
    if (!view) return null;
    const ownerName = loadProfile(view.ownerId)?.displayName;
    return {
      ...view,
      ...(ownerName ? { ownerDisplayName: ownerName } : {}),
      devices: view.devices.map((d) =>
        d.isThisDevice && deviceLabel ? { ...d, displayName: deviceLabel } : d,
      ),
    };
  }

  svc.onChange = () => { state = applyLocalOverlay(svc.state); };

  onMount(async () => {
    try {
      await svc.refresh();
    } catch (e) {
      loadError = extractError(e);
    } finally {
      loading = false;
    }
    // ZEB-336: when this device has no user-set label, default the DISPLAY to
    // the OS hostname, resolved FRESH each launch. NOT persisted — only a user
    // rename (saveRename) writes the store — so a one-off hostname-resolution
    // hiccup can never lock in the "This device" fallback. (CodeRabbit, PR #180.)
    // Isolated from the refresh try/catch above so a hostname hiccup can't blank
    // the already-loaded devices list via the loadError banner.
    if (!deviceLabel) {
      try {
        deviceLabel = await resolveDefaultDeviceLabel();
        state = applyLocalOverlay(svc.state);
      } catch {
        // Non-fatal: keep the backend device label until the user sets one.
      }
    }
  });

  let inviterOpen = $state(false);
  let joinerOpen = $state(false);

  let backupOpen = $state(false);
  // ZEB-454: owner-mnemonic restore wizard open/closed.
  let restoreOpen = $state(false);
  let backupPassphrase = $state('');
  let backupPassphraseConfirm = $state('');
  let backupComment = $state('');
  // Two flags so the button label can distinguish phases (the export IPC
  // is the only "Encrypting…" phase; the OS save dialog is "Choosing
  // location…"). Both flags gate the disabled predicate; only the
  // export-phase flag gates the "Encrypting…" label.
  // (Cursor Bugbot, PR #66 review.)
  let backupDialogInFlight = $state(false);
  let backupInFlight = $state(false);
  let backupError = $state<string | null>(null);
  let backupSavedPath = $state<string | null>(null);

  let renamingDeviceId = $state<string | null>(null);
  let renameDraft = $state('');

  function startRename(device: { deviceId: string; displayName: string; isThisDevice: boolean }) {
    renamingDeviceId = device.deviceId;
    renameDraft = device.displayName;
  }

  function saveRename(deviceId: string) {
    const trimmed = renameDraft.trim();
    if (trimmed.length === 0) return;
    // ZEB-336: a device rename writes the per-device LABEL, never the owner
    // profile. (Pre-split this wrote profile.displayName, renaming the owner.)
    saveDeviceLabel(trimmed);
    deviceLabel = trimmed;
    if (state) {
      state = {
        ...state,
        devices: state.devices.map((d) =>
          d.deviceId === deviceId ? { ...d, displayName: trimmed } : d,
        ),
      };
    }
    renamingDeviceId = null;
  }

  function cancelRename() {
    renamingDeviceId = null;
  }

  // ZEB-418 P2 D17: butler pin toggle. Single-select: toggling ON pins the
  // device; toggling the already-pinned device OFF clears the pin. Low-risk
  // action (advisory ordering, always reversible) — no confirmation tier.
  let butlerPinError = $state<string | null>(null);
  let butlerPinInFlight = $state(false);

  async function handleButlerPinToggle(device: { deviceVkHex: string; butlerPinned: boolean }) {
    if (butlerPinInFlight) return;
    butlerPinError = null;
    butlerPinInFlight = true;
    try {
      // If already pinned, toggle OFF (clear); otherwise pin this device.
      // Round-2 Greptile P1: the backend validates against the enrolled set
      // of 64-hex VERIFY-KEY ids (SP1 form) — `deviceVkHex`, never
      // `deviceId` (the identity-hash form, which is always rejected).
      const newPin = device.butlerPinned ? null : device.deviceVkHex;
      await setButlerPin(newPin);
      // Re-fetch the device list so butler_pinned reflects the new state.
      await svc.refresh();
    } catch (e) {
      butlerPinError = extractButlerPinError(e);
    } finally {
      butlerPinInFlight = false;
    }
  }

  async function openBackup() {
    backupOpen = true;
    backupPassphrase = '';
    backupPassphraseConfirm = '';
    backupComment = '';
    backupError = null;
    backupSavedPath = null;
    // Token-reuse policy: if a token is already in hand (e.g., handleConfirmMint
    // just minted and stashed one), use it. Otherwise issue a fresh one. This
    // preserves the happy mint→backup path (one token, one consume) while still
    // healing post-failure paths because commitBackup nulls the token in finally
    // (whether export succeeded or failed) — so a second openBackup always
    // re-issues. Tokens ARE TTL-bounded (5min) and LRU-evictable, so a stale
    // post-mint token will surface as "expired or invalid" on commit, which is
    // recoverable via the inline Retry button.
    if (recoveryToken !== null) return;
    try {
      recoveryToken = await svc.issueRecoveryToken();
    } catch (e) {
      backupError = extractError(e);
    }
  }

  async function retryIssueToken() {
    backupError = null;
    try {
      recoveryToken = await svc.issueRecoveryToken();
    } catch (e) {
      backupError = extractError(e);
    }
  }

  async function commitBackup() {
    // Function-level reentrancy guard: even with the Save backup button
    // gated on the in-flight flags, two click handlers can be queued in
    // the same event-loop turn before Svelte's reactivity propagates
    // `disabled` to the DOM. Symmetric with IdentityPanel's
    // advanceFromFileEntry guard. (CodeRabbit, PR #66 review.)
    if (backupDialogInFlight || backupInFlight) return;
    // Clear any error from a prior commit attempt in this same modal session
    // BEFORE re-validating. Without this, a stale error string from the
    // previous click can render alongside (or instead of) the current
    // validation outcome — e.g., user fixes a passphrase mismatch but the
    // "Passphrases do not match" string lingers because no early-return path
    // resets backupError. The token/dialog/export path below has its own
    // backupError = null (line above the export try block); this guards the
    // pre-validation early-return paths.
    backupError = null;
    if (recoveryToken === null) {
      backupError = 'No recovery token available.';
      return;
    }
    if (backupPassphrase !== backupPassphraseConfirm) {
      backupError = 'Passphrases do not match.';
      return;
    }
    // Count Unicode codepoints, not UTF-16 code units, so the check matches
    // the Rust backend's `passphrase.chars().count()` for multibyte input
    // (emoji, CJK). Spreading a string yields one element per codepoint.
    if ([...backupPassphrase].length < MIN_RECOVERY_PASSPHRASE_LEN) {
      backupError = `Passphrase must be at least ${MIN_RECOVERY_PASSPHRASE_LEN} characters.`;
      return;
    }
    // Comment cap is BYTES (matches harmony-owner's hard 256-byte limit on
    // the underlying field). The maxlength={256} attribute on the input is
    // a UI hint counting characters, which over-permits for multibyte input;
    // enforce the byte cap explicitly here so the backend never rejects.
    //
    // Validate the SAME string we send to the backend — backupComment.trim().
    // Validating the raw (untrimmed) string falsely rejects comments where
    // leading/trailing whitespace pushes the byte count past 256 even though
    // the trimmed form fits.
    const trimmedComment = backupComment.trim();
    const commentBytes = new TextEncoder().encode(trimmedComment).length;
    if (commentBytes > MAX_RECOVERY_COMMENT_BYTES) {
      backupError = `Comment must be at most ${MAX_RECOVERY_COMMENT_BYTES} bytes (currently ${commentBytes}).`;
      return;
    }
    // Mark dialog-in-flight BEFORE the save dialog opens so a fast double-
    // click on Save backup cannot queue a second dialog and consume the
    // single-use recovery token twice (CodeRabbit, PR #66 review).
    // The two flags are deliberately separate: backupDialogInFlight gates
    // disable; only backupInFlight (the export-phase flag) drives the
    // "Encrypting…" label, which would otherwise lie during the dialog
    // phase (Cursor Bugbot, PR #66 review).
    backupDialogInFlight = true;
    let pathToken: string | null;
    try {
      pathToken = await svc.requestExportSavePath({
        defaultFilename: 'owner-recovery.bin',
        filterName: 'Recovery file',
        filterExtensions: ['bin'],
      });
    } catch (e) {
      backupError = extractError(e);
      return;
    } finally {
      backupDialogInFlight = false;
    }
    if (pathToken === null) return;  // user cancelled
    backupInFlight = true;
    try {
      const info = await svc.exportRecoveryFile(
        recoveryToken,
        pathToken,
        backupPassphrase,
        trimmedComment ? trimmedComment : null,
      );
      // Wipe passphrase fields immediately on success — shortens secret
      // retention in the renderer between this point and closeBackup
      // (CodeRabbit, PR #66 review). closeBackup also wipes them; this
      // is belt-and-braces.
      backupPassphrase = '';
      backupPassphraseConfirm = '';
      backupSavedPath = info.path;
    } catch (e) {
      backupError = extractError(e);
    } finally {
      // Token is single-use server-side: take_token consumes it on any path
      // past validation, including disk-write failures. Always null it so
      // the next openBackup() call issues a fresh token instead of replaying
      // a stale one (which would error "expired or invalid").
      recoveryToken = null;
      backupInFlight = false;
    }
  }

  function closeBackup() {
    backupOpen = false;
    // Tokens are single-use server-side; don't carry across opens.
    recoveryToken = null;
    // Wipe sensitive passphrase material from component state instead of
    // letting it linger between modal sessions. JS strings are immutable so
    // we can't actually zero the underlying buffer (the original allocations
    // remain in V8's heap until GC), but dropping our references at least
    // makes them eligible for collection rather than holding them indefinitely
    // across the panel's lifetime.
    backupPassphrase = '';
    backupPassphraseConfirm = '';
    backupComment = '';
    backupError = null;
  }

  function formatOwnerFingerprint(hex: string): string {
    // 32 hex chars (16 bytes) → eight groups of 4 hex chars separated by ·.
    // Renders the FULL 16-byte owner identity hash so users can disambiguate
    // their own owner identity from another's (truncating to 8 of 16 bytes
    // would only cover half the entropy and weaken visual disambiguation).
    if (hex.length < 32) return hex;
    return hex.match(/.{4}/g)!.slice(0, 8).join('·');
  }

  function deviceInitial(name: string): string {
    return name.trim().charAt(0).toUpperCase() || '?';
  }

  function formatEnrolledAt(ts: number): string {
    const ms = ts * 1000;
    const now = Date.now();
    const ageDays = Math.floor((now - ms) / (1000 * 60 * 60 * 24));
    if (ageDays < 1) return 'today';
    if (ageDays < 2) return 'yesterday';
    if (ageDays < 30) return `${ageDays}d ago`;
    return new Date(ms).toLocaleDateString();
  }

  async function handleConfirmMint() {
    mintInFlight = true;
    mintError = null;
    try {
      const result = await svc.mint();
      recoveryToken = result.recoveryToken;
      modalOpen = false;
    } catch (e) {
      mintError = extractError(e);
    } finally {
      mintInFlight = false;
    }
  }
</script>

<section class="devices-panel" aria-labelledby="devices-heading">
  <h2 id="devices-heading">Devices</h2>

  {#if loading}
    <p class="loading">Loading…</p>
  {:else if loadError}
    <p class="error" role="alert">Failed to load: {loadError}</p>
  {:else if state === null}
    <div class="empty">
      <p class="explainer">
        You haven't created an owner identity yet. Either start a new one for this
        device, or join an existing one already running on another of your devices.
      </p>
      <div class="empty-actions">
        <button class="primary" onclick={() => { modalOpen = true; }}>
          Bind this device to a new owner identity →
        </button>
        <button class="secondary" onclick={() => { joinerOpen = true; }}>
          Join existing identity →
        </button>
      </div>
    </div>
  {:else}
    <div class="populated">
      <!-- ① Owner identity header -->
      <div class="owner-header">
        <div class="label">OWNER IDENTITY</div>
        <div class="owner-row">
          <div>
            <div class="owner-name">{state.ownerDisplayName}</div>
            <div class="owner-fingerprint">{formatOwnerFingerprint(state.ownerId)}</div>
          </div>
          <button
            class="primary"
            disabled={!state.canBackUp}
            title={state.canBackUp ? '' : 'Master seed not on this device — backup is no longer possible.'}
            onclick={openBackup}
          >
            Back up owner identity →
          </button>
        </div>
        <!-- ZEB-454: re-adopt this owner identity from its 24-word recovery
             phrase (the GUI analog of `restore owner-mnemonic`). -->
        <button
          class="restore-link"
          data-testid="devices-restore-mnemonic"
          onclick={() => { restoreOpen = true; }}
        >
          Restore from recovery phrase…
        </button>
      </div>
      {#if restoreOpen}
        <!-- On success, reload so a fresh start_node loads the re-minted
             owner_state (mirrors the pairing-join completion path). -->
        <OwnerRestoreWizard
          currentOwnerId={state.ownerId}
          onRestored={() => location.reload()}
          onCancel={() => { restoreOpen = false; }}
        />
      {/if}

      <!-- ② Devices list -->
      <div class="devices-list">
        <div class="label">MY DEVICES ({state.devices.length})</div>
        {#each state.devices as device (device.deviceId)}
          <div class="device-row">
            <div class="device-icon">{deviceInitial(device.displayName)}</div>
            <div class="device-meta">
              <div class="device-name-row">
                {#if renamingDeviceId === device.deviceId}
                  <input
                    type="text"
                    bind:value={renameDraft}
                    aria-label="Device name"
                    onkeydown={(e) => {
                      if (e.key === 'Enter') saveRename(device.deviceId);
                      if (e.key === 'Escape') cancelRename();
                    }}
                  />
                  <button class="secondary" onclick={() => saveRename(device.deviceId)}>Save</button>
                  <button class="secondary" onclick={cancelRename}>Cancel</button>
                {:else}
                  <span class="device-name">{device.displayName}</span>
                  {#if device.isThisDevice}
                    <span class="this-device-marker">this device</span>
                    <button class="rename-btn" onclick={() => startRename(device)}>Rename</button>
                  {/if}
                {/if}
              </div>
              <div class="device-secondary">
                {#if device.trustDecision.kind === 'full'}
                  <span class="trust-badge full">● trusted</span>
                {:else if device.trustDecision.kind === 'provisional'}
                  <span class="trust-badge provisional">● provisional</span>
                {:else}
                  <span class="trust-badge refused">● refused</span>
                {/if}
                <span class="separator">·</span>
                <span>added {formatEnrolledAt(device.enrolledAt)}</span>
                <span class="separator">·</span>
                <span class="fingerprint">{device.fingerprint}</span>
              </div>
              <!-- ZEB-418 P2 D17: always-on butler toggle -->
              <div class="butler-row">
                <label class="butler-label">
                  <input
                    type="checkbox"
                    class="butler-checkbox"
                    checked={device.butlerPinned}
                    disabled={butlerPinInFlight}
                    onchange={() => handleButlerPinToggle(device)}
                    aria-label={device.butlerPinned
                      ? `Remove always-on butler from ${device.displayName}`
                      : `Set ${device.displayName} as always-on butler`}
                  />
                  <span class="butler-label-text">Always-on butler</span>
                </label>
              </div>
            </div>
          </div>
        {/each}
        {#if butlerPinError}
          <p class="error" role="alert">{butlerPinError}</p>
        {/if}
      </div>

      <!-- ③ Add-another-device footer -->
      <div class="add-another-footer">
        <div class="label">ADD ANOTHER DEVICE</div>
        {#if state.canBackUp}
          <button class="primary" onclick={() => { inviterOpen = true; }}>
            Add another device →
          </button>
          <p class="explainer">
            Both devices need to be on the same Wi-Fi network and in pairing mode.
            The new device will join under your existing owner identity.
          </p>
        {:else}
          <p class="explainer">
            This device cannot enroll others — its master seed has been wiped.
            Use a device that holds the master seed to add new devices.
          </p>
        {/if}
      </div>
    </div>
  {/if}

  {#if backupOpen}
    <Modal
      onCancel={closeBackup}
      canCancel={!backupDialogInFlight && !backupInFlight}
      ariaLabelledby="backup-modal-heading"
    >
      <h3 id="backup-modal-heading">Back up owner identity</h3>
      {#if backupSavedPath}
        <p>Recovery file written to <code>{backupSavedPath}</code>. Keep it somewhere safe.</p>
        <button class="primary" onclick={closeBackup}>Done</button>
      {:else}
        <p>
          Choose a strong passphrase. The encrypted file alone cannot be opened
          without it.
        </p>
        <label>
          Passphrase
          <input type="password" bind:value={backupPassphrase} aria-label="Passphrase" />
        </label>
        <label>
          Confirm passphrase
          <input type="password" bind:value={backupPassphraseConfirm} aria-label="Confirm passphrase" />
        </label>
        <label>
          Comment (optional)
          <!--
            No `maxlength` attribute — that counts UTF-16 code units, but
            the harmony-owner backend cap is 256 bytes. commitBackup
            validates byte length explicitly via TextEncoder before submit.
          --><input type="text" bind:value={backupComment} aria-label="Comment" />
        </label>
        {#if backupError}
          <p class="error" role="alert">{backupError}</p>
        {/if}
        <div class="modal-actions">
          <button class="secondary" onclick={closeBackup} disabled={backupDialogInFlight || backupInFlight}>Cancel</button>
          {#if recoveryToken === null && backupError}
            <!--
              Token-issuance failed (e.g., locked keychain). Inline retry
              avoids forcing the user to close + reopen the modal.
            -->
            <button class="secondary" onclick={retryIssueToken} disabled={backupDialogInFlight || backupInFlight}>Retry</button>
          {/if}
          <!--
            Disable Save backup when no token is available (e.g., issue_owner_recovery_token
            failed during openBackup). Otherwise the user clicks Save and gets a confusing
            "No recovery token available" inline error instead of the disabled-state hint.
          -->
          <button class="primary" onclick={commitBackup} disabled={backupDialogInFlight || backupInFlight || recoveryToken === null}>
            {#if backupInFlight}Encrypting…{:else if backupDialogInFlight}Choose location…{:else}Save backup{/if}
          </button>
        </div>
      {/if}
    </Modal>
  {/if}

  {#if joinerOpen}
    <PairingJoiner onClose={async () => {
      joinerOpen = false;
      await svc.refresh();
    }} />
  {/if}
  {#if inviterOpen}
    <PairingInviter hostname={state?.ownerDisplayName ?? 'this device'} onClose={async () => {
      inviterOpen = false;
      await svc.refresh();
    }} />
  {/if}

  {#if modalOpen}
    <Modal
      onCancel={() => { modalOpen = false; }}
      canCancel={!mintInFlight}
      ariaLabelledby="modal-heading"
    >
      <h3 id="modal-heading">Create your owner identity</h3>
      <p>
        This will create your owner identity. This device will be bound as the first device.
        You'll receive a recovery file to back up — you can do this immediately or later.
      </p>
      {#if mintError}
        <p class="error" role="alert">{mintError}</p>
      {/if}
      <div class="modal-actions">
        <button class="secondary" onclick={() => { modalOpen = false; }} disabled={mintInFlight}>
          Cancel
        </button>
        <button class="primary" onclick={handleConfirmMint} disabled={mintInFlight}>
          {mintInFlight ? 'Creating…' : 'Create owner identity'}
        </button>
      </div>
    </Modal>
  {/if}
</section>

<style>
  .devices-panel {
    padding: 16px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    margin-bottom: 16px;
  }
  .devices-panel h2 {
    margin: 0 0 12px;
    font-size: 14px;
    color: var(--text-primary);
  }
  .empty .explainer {
    color: var(--text-secondary);
    font-size: 13px;
    margin-bottom: 12px;
  }
  .empty-actions { display: flex; flex-direction: column; gap: 8px; }
  .primary, .secondary {
    padding: 6px 12px;
    border-radius: 4px;
    border: 1px solid var(--border);
    cursor: pointer;
    font-size: 13px;
  }
  .primary {
    background: var(--accent);
    color: var(--text-bright);
    border-color: var(--accent);
  }
  .secondary {
    background: var(--bg-primary);
    color: var(--text-primary);
  }
  .primary:disabled, .secondary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
    margin: 8px 0;
  }
  .loading {
    color: var(--text-muted);
    font-size: 13px;
  }
  .label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }
  .owner-header {
    padding-bottom: 14px;
    border-bottom: 1px solid var(--border);
    margin-bottom: 14px;
  }
  .owner-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
  }
  .owner-name {
    font-weight: 600;
    color: var(--text-primary);
  }
  .owner-fingerprint {
    font-size: 12px;
    color: var(--text-muted);
    font-family: var(--font-mono);
  }
  .restore-link {
    margin-top: 8px;
    padding: 0;
    background: none;
    border: none;
    color: var(--accent);
    font-size: 0.8rem;
    cursor: pointer;
    text-decoration: underline;
    text-align: left;
  }
  .restore-link:hover {
    opacity: 0.85;
  }
  .devices-list {
    padding-bottom: 14px;
    border-bottom: 1px solid var(--border);
    margin-bottom: 14px;
  }
  .device-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px;
    background: var(--bg-primary);
    border-radius: 4px;
  }
  .device-icon {
    width: 32px;
    height: 32px;
    border-radius: 6px;
    background: var(--accent);
    color: var(--text-bright);
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 14px;
    font-weight: 600;
    flex-shrink: 0;
  }
  .device-meta {
    flex: 1;
    min-width: 0;
  }
  .device-name-row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 8px;
  }
  .device-name {
    font-weight: 600;
    color: var(--text-primary);
  }
  .this-device-marker {
    font-size: 11px;
    color: var(--text-muted);
  }
  .device-secondary {
    font-size: 12px;
    color: var(--text-secondary);
    margin-top: 2px;
  }
  .trust-badge.full { color: var(--success); }
  .trust-badge.provisional { color: var(--warning-bright); }
  .trust-badge.refused { color: var(--danger); }
  .separator { margin: 0 6px; color: var(--text-muted); }
  .fingerprint { font-family: var(--font-mono); }
  .add-another-footer .explainer {
    font-size: 12px;
    color: var(--text-secondary);
    line-height: 1.5;
    margin: 0;
  }
  .rename-btn {
    font-size: 11px;
    padding: 4px 8px;
    border: 1px solid var(--border);
    background: var(--bg-primary);
    color: var(--text-primary);
    border-radius: 4px;
    cursor: pointer;
  }
  .rename-btn:hover {
    background: var(--bg-tertiary);
  }
  /* ZEB-418 P2 D17: butler pin toggle */
  .butler-row {
    margin-top: 6px;
  }
  .butler-label {
    display: flex;
    align-items: center;
    gap: 6px;
    cursor: pointer;
    user-select: none;
  }
  .butler-checkbox {
    cursor: pointer;
    accent-color: var(--accent);
  }
  .butler-checkbox:disabled {
    cursor: not-allowed;
    opacity: 0.5;
  }
  .butler-label-text {
    font-size: 12px;
    color: var(--text-secondary);
  }
</style>
