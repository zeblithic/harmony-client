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
    BACKUP_FLAGS_CHANGED_EVENT,
    daysSinceBackupSkipped,
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

  // ZEB-650: localStorage isn't reactive, so another surface completing a
  // backup (DevicesPanel's modal) can't invalidate our $derived on its own.
  // The flags module fires BACKUP_FLAGS_CHANGED_EVENT on every marker write;
  // bumping this tick re-derives visibility/day-count without a remount
  // (CodeRabbit, PR #436).
  let flagsTick = $state(0);
  $effect(() => {
    const bump = () => { flagsTick++; };
    window.addEventListener(BACKUP_FLAGS_CHANGED_EVENT, bump);
    return () => window.removeEventListener(BACKUP_FLAGS_CHANGED_EVENT, bump);
  });

  // $derived (not $effect) so visibility is available synchronously on first
  // render and recomputes when the owner identity resolves (null → id). Reads
  // the owner-scoped skip/backed-up/dismissed flags for THIS owner (ZEB-587).
  const visible = $derived.by(() => {
    void flagsTick; // re-derive when the flags module signals a write
    return isBackupReminderVisible(ownerId) && !dismissedThisSession && !backedUpThisSession;
  });

  // ZEB-650: day count derives from the owner-scoped skippedAt stamp; null
  // (legacy owner) or 0 (skipped today) keeps the base copy unchanged.
  const skippedDays = $derived.by(() => {
    void flagsTick;
    return ownerId ? daysSinceBackupSkipped(ownerId) : null;
  });

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
      error = `Recovery passphrase must be at least ${MIN_RECOVERY_PASSPHRASE_LEN} characters.`;
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
    <span class="warn"><span class="icon" aria-hidden="true">🔑</span> Your identity hasn't been backed up{#if skippedDays !== null && skippedDays >= 1}<span data-testid="backup-reminder-days"> — you skipped backup {skippedDays} {skippedDays === 1 ? 'day' : 'days'} ago</span>{/if}.</span>
    {#if !showPassphrase}
      <button data-testid="backup-reminder-backup-now" onclick={startBackup}>Back up now</button>
      <button
        class="ghost"
        data-testid="backup-reminder-dismiss"
        onclick={dismiss}
        aria-label="Dismiss"
        title="Dismiss">✕</button>
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
  /* Commons "clay" banner — one of the two sanctioned clay surfaces. */
  .backup-banner {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    background: var(--gov-clay-soft);
    color: var(--gov-clay-deep);
    font-size: 0.85rem;
    border-bottom: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
  }
  .warn { flex: 0 0 auto; }
  .icon { margin-right: 0.15rem; }
  /* Primary actions ("Back up now", "Save") are clay-filled. */
  .backup-banner button {
    padding: 0.25rem 0.6rem;
    border: 1px solid var(--gov-clay);
    background: var(--gov-clay);
    color: var(--text-bright);
    border-radius: 4px;
    cursor: pointer;
  }
  .backup-banner button:disabled {
    opacity: 0.6;
    cursor: default;
  }
  /* Dismiss is a low-emphasis clay "✕". */
  .backup-banner button.ghost {
    background: transparent;
    border-color: transparent;
    color: var(--gov-clay-deep);
    font-size: 1rem;
    line-height: 1;
    padding: 0.25rem 0.4rem;
  }
  .backup-banner input {
    padding: 0.25rem 0.5rem;
    background: var(--surface-raised);
    color: var(--gov-clay-deep);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    border-radius: 4px;
  }
  .error { color: var(--fg-error); }
</style>
