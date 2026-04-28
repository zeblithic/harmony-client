<script lang="ts">
  import { onMount } from 'svelte';
  import { OwnerService, extractError, type OwnerStateView } from '../owner-service';
  import { loadProfile, saveProfile } from '../profile-service';
  import { save } from '@tauri-apps/plugin-dialog';

  let svc = new OwnerService();
  let state = $state<OwnerStateView | null>(null);
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let modalOpen = $state(false);
  let mintInFlight = $state(false);
  let mintError = $state<string | null>(null);
  let recoveryToken = $state<string | null>(null);

  /**
   * Backend always returns the placeholder name "this device" for the
   * isThisDevice row (it has no access to localStorage). Overlay the
   * locally-persisted profile.displayName so the rename survives refresh
   * and restart. Cross-device names will eventually come via gossip
   * (deferred); v1 only overlays the local entry.
   *
   * Defensive: if loadProfile returns no usable name (e.g., localStorage
   * unavailable in private-mode browsers), pass the view through unchanged
   * so the user still sees the backend placeholder rather than a crash.
   */
  function applyLocalProfileOverlay(view: OwnerStateView | null): OwnerStateView | null {
    if (!view) return null;
    const profile = loadProfile();
    const localName = profile?.displayName;
    if (!localName) return view;
    return {
      ...view,
      ownerDisplayName: localName,
      devices: view.devices.map((d) =>
        d.isThisDevice ? { ...d, displayName: localName } : d,
      ),
    };
  }

  svc.onChange = () => { state = applyLocalProfileOverlay(svc.state); };

  onMount(async () => {
    try {
      await svc.refresh();
    } catch (e) {
      loadError = extractError(e);
    } finally {
      loading = false;
    }
  });

  let backupOpen = $state(false);
  let backupPassphrase = $state('');
  let backupPassphraseConfirm = $state('');
  let backupComment = $state('');
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
    const profile = loadProfile();
    saveProfile({ ...profile, displayName: trimmed });
    if (state) {
      // Optimistic local update — refresh from backend on next mount.
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

  async function openBackup() {
    backupOpen = true;
    backupPassphrase = '';
    backupPassphraseConfirm = '';
    backupComment = '';
    backupError = null;
    backupSavedPath = null;
    if (recoveryToken === null) {
      try {
        recoveryToken = await svc.issueRecoveryToken();
      } catch (e) {
        backupError = extractError(e);
      }
    }
  }

  async function commitBackup() {
    if (recoveryToken === null) {
      backupError = 'No recovery token available.';
      return;
    }
    if (backupPassphrase !== backupPassphraseConfirm) {
      backupError = 'Passphrases do not match.';
      return;
    }
    if (backupPassphrase.length < 12) {
      backupError = 'Passphrase must be at least 12 characters.';
      return;
    }
    let out: string | null;
    try {
      out = await save({
        defaultPath: 'owner-recovery.bin',
        filters: [{ name: 'Recovery file', extensions: ['bin'] }],
      });
    } catch (e) {
      backupError = extractError(e);
      return;
    }
    if (!out) return;
    backupInFlight = true;
    backupError = null;
    try {
      await svc.exportRecoveryFile(
        recoveryToken,
        out,
        backupPassphrase,
        backupComment.trim() ? backupComment.trim() : null,
      );
      backupSavedPath = out;
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
  }

  function formatOwnerFingerprint(hex: string): string {
    // 32 hex chars → "xxxx·xxxx·xxxx·xxxx" for readability
    if (hex.length < 16) return hex;
    return `${hex.slice(0,4)}·${hex.slice(4,8)}·${hex.slice(8,12)}·${hex.slice(12,16)}`;
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
        You haven't created an owner identity yet. Once you do, this device will be
        bound to it, and any other devices you add later will appear here.
      </p>
      <button class="primary" onclick={() => { modalOpen = true; }}>
        Bind this device to a new owner identity →
      </button>
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
      </div>

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
            </div>
          </div>
        {/each}
      </div>

      <!-- ③ Educational footer -->
      <div class="add-another-footer">
        <div class="label">ADD ANOTHER DEVICE</div>
        <p class="explainer">
          Pairing UI is coming. For now, multi-device coexistence requires the
          <code>enroll_via_master</code> flow which ships in a follow-up. Currently
          only one device is bound under your owner identity.
        </p>
      </div>
    </div>
  {/if}

  {#if backupOpen}
    <div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="backup-modal-heading">
      <div class="modal">
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
            <input type="text" bind:value={backupComment} maxlength={256} aria-label="Comment" />
          </label>
          {#if backupError}
            <p class="error" role="alert">{backupError}</p>
          {/if}
          <div class="modal-actions">
            <button class="secondary" onclick={closeBackup} disabled={backupInFlight}>Cancel</button>
            <button class="primary" onclick={commitBackup} disabled={backupInFlight}>
              {backupInFlight ? 'Encrypting…' : 'Save backup'}
            </button>
          </div>
        {/if}
      </div>
    </div>
  {/if}

  {#if modalOpen}
    <div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="modal-heading">
      <div class="modal">
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
      </div>
    </div>
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
  .primary, .secondary {
    padding: 6px 12px;
    border-radius: 4px;
    border: 1px solid var(--border);
    cursor: pointer;
    font-size: 13px;
  }
  .primary {
    background: var(--accent);
    color: white;
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
  .modal-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0,0,0,0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--bg-secondary);
    padding: 24px;
    border-radius: 8px;
    max-width: 480px;
    border: 1px solid var(--border);
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
    font-family: monospace;
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
    color: white;
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
  .trust-badge.full { color: #4ade80; }
  .trust-badge.provisional { color: #fbbf24; }
  .trust-badge.refused { color: var(--danger); }
  .separator { margin: 0 6px; color: var(--text-muted); }
  .fingerprint { font-family: monospace; }
  .add-another-footer .explainer {
    font-size: 12px;
    color: var(--text-secondary);
    line-height: 1.5;
    margin: 0;
  }
  .add-another-footer code {
    font-family: monospace;
    font-size: 11px;
    background: var(--bg-tertiary);
    padding: 1px 4px;
    border-radius: 3px;
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
</style>
