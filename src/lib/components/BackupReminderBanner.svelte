<script lang="ts">
  /**
   * ZEB-338 — persistent reminder shown after the user skipped the recovery
   * backup during onboarding. Sticky across launches (localStorage) until the
   * user backs up; dismissable for the current session only (sessionStorage).
   *
   * Visibility: backupSkipped && !recoveryArtifactBackedUp && !dismissed,
   * all read for THIS owner via the owner-scoped flags (ZEB-587). backupSkipped
   * is set ONLY by WelcomeModal's skip-confirm path, so users who minted +
   * backed up via the DevicesPanel never see this.
   *
   * ZEB-587: the flags are owner-scoped (`<base>:owner-<id>`), so a fresh /
   * recreated identity that skipped is correctly reminded even when another
   * identity on the same (bundle-shared) localStorage already backed up.
   *
   * Unlike WelcomeModal (which holds a fresh mint token), this banner issues a
   * recovery token on demand via issueRecoveryToken() before exporting.
   */
  import { OwnerService, extractError } from '../owner-service';
  import { MIN_RECOVERY_PASSPHRASE_LEN } from '../recovery-policy';
  import {
    isBackupReminderVisible,
    markBannerDismissed,
    markRecoveryBackedUp,
  } from '../onboarding-backup-flags';

  interface Props {
    /** Current owner identity hex, or null until get_owner_state resolves.
     *  Visibility is owner-scoped; a null owner never shows the banner. */
    ownerId: string | null;
  }
  const { ownerId }: Props = $props();

  let showPassphrase = $state(false);
  let passphrase = $state('');
  let error = $state<string | null>(null);
  let inFlight = $state(false);
  // Per-session imperative overrides layered on top of the owner-scoped flags.
  // Set on dismiss / successful backup so the banner hides immediately; the
  // persisted flags also update, but localStorage/sessionStorage are not
  // reactive, so these drive the live recompute.
  let dismissedThisSession = $state(false);
  let backedUpThisSession = $state(false);

  const svc = new OwnerService();

  // $derived (not $effect) so visibility is available synchronously on first
  // render and recomputes when the owner identity resolves (null → id). Reads
  // the owner-scoped skip/backed-up/dismissed flags for THIS owner (ZEB-587).
  const visible = $derived(
    isBackupReminderVisible(ownerId) && !dismissedThisSession && !backedUpThisSession,
  );

  // Tie the per-session overrides to the owner that set them. If the owner prop
  // changes while this component stays mounted (e.g. the owner resolves after
  // mount), reset transient UI so one identity's dismiss/backup/half-typed
  // passphrase can't carry into another's reminder (ZEB-587 — CodeRabbit).
  let overrideOwnerId: string | null = ownerId;
  $effect(() => {
    if (overrideOwnerId === ownerId) return;
    overrideOwnerId = ownerId;
    dismissedThisSession = false;
    backedUpThisSession = false;
    showPassphrase = false;
    passphrase = '';
    error = null;
  });

  function dismiss() {
    if (ownerId) markBannerDismissed(ownerId);
    dismissedThisSession = true;
  }

  function startBackup() {
    showPassphrase = true;
    error = null;
  }

  async function save() {
    // Capture the initiating owner up front: the export below has several awaits
    // and `ownerId` could change underneath us, but the backed-up flag must name
    // the identity the user actually backed up.
    const backupOwnerId = ownerId;
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
        return; // user cancelled — finally resets inFlight
      }
      await svc.exportRecoveryFile(token, pathToken, passphrase, null);
      if (backupOwnerId) markRecoveryBackedUp(backupOwnerId);
      passphrase = '';
      backedUpThisSession = true;
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
