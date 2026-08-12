<script lang="ts">
  /**
   * ZEB-904/905 — non-blocking local-only-mode notice. Shown when start_node
   * reports `fleetCryptoMissing`: the owner loaded from disk but this device
   * holds no master seed and no fleet-KeyTree material, so the node booted
   * local-only. Communities/channels/profile work normally; device-to-device
   * sync, friend features, and encrypted file shares are paused until the
   * user restores the recovery phrase (Account → Devices → Restore from
   * recovery phrase) or re-pairs from a device that holds the seed.
   *
   * Deliberately NOT a blocking modal (contrast ZEB-836's enrollment-missing
   * screen): the device is operational, this is a capability notice.
   * Dismissable for the current session only — the state persists across
   * launches until keys are restored, so it should resurface next boot.
   */
  interface Props {
    /** `fleetCryptoMissing` from the start_node response (false when absent). */
    fleetCryptoMissing: boolean;
  }
  const { fleetCryptoMissing }: Props = $props();

  let dismissedThisSession = $state(false);

  const visible = $derived(fleetCryptoMissing && !dismissedThisSession);
</script>

{#if visible}
  <div class="fleet-sync-banner" data-testid="fleet-sync-disabled-banner" role="status">
    <span class="warn">
      <span class="icon" aria-hidden="true">🔒</span>
      This device's sync keys are missing — device sync, friends, and encrypted
      file shares are paused. Communities work normally.
    </span>
    <span class="hint" data-testid="fleet-sync-disabled-hint">
      Restore your recovery phrase (Account&nbsp;→&nbsp;Devices) to re-enable.
    </span>
    <button
      class="ghost"
      data-testid="fleet-sync-disabled-dismiss"
      onclick={() => {
        dismissedThisSession = true;
      }}
      aria-label="Dismiss"
      title="Dismiss">✕</button>
  </div>
{/if}

<style>
  /* Same "clay" banner surface as BackupReminderBanner — this is the same
     class of persistent, actionable-but-not-blocking notice. */
  .fleet-sync-banner {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    background: var(--gov-clay-soft);
    border-bottom: 1px solid var(--gov-clay-border);
    font-size: 0.85rem;
    flex-wrap: wrap;
  }
  .warn {
    color: var(--gov-clay-text);
  }
  .icon {
    margin-right: 0.25rem;
  }
  .hint {
    color: var(--gov-clay-text);
    opacity: 0.8;
  }
  .ghost {
    margin-left: auto;
    background: none;
    border: none;
    color: var(--gov-clay-text);
    cursor: pointer;
    padding: 0.15rem 0.4rem;
    border-radius: 4px;
  }
  .ghost:hover {
    background: var(--gov-clay-border);
  }
</style>
