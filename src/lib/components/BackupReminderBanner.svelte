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
