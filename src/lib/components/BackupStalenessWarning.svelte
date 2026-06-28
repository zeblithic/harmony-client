<script lang="ts">
  import { getBackupStaleness, dismissForDays } from '../backup-service';

  interface Props {
    /** Current owner identity hex, or null until get_owner_state resolves.
     *  The staleness check + dismiss snooze are owner-scoped (ZEB-589); a null
     *  owner stays inert (this banner mounts before the owner resolves). */
    ownerId: string | null;
    onExportRequested?: () => void;
  }
  let { ownerId, onExportRequested }: Props = $props();

  let isStale = $state(false);
  let daysSince = $state(0);

  // Re-check whenever the owner identity resolves/changes (the banner mounts
  // outside the owner-present gate). Cancellation drops a superseded fetch so a
  // prior owner's result can't clobber the current owner's.
  $effect(() => {
    const owner = ownerId;
    if (!owner) {
      isStale = false;
      return;
    }
    let cancelled = false;
    void getBackupStaleness(owner)
      .then((r) => {
        if (cancelled) return;
        isStale = r.isStale;
        daysSince = r.daysSince;
      })
      .catch(() => {
        // Best-effort — never block UI on staleness check failure.
        if (!cancelled) isStale = false;
      });
    return () => {
      cancelled = true;
    };
  });

  function dismiss() {
    if (ownerId) dismissForDays(7, ownerId);
    isStale = false;
  }

  function exportNow() {
    onExportRequested?.();
  }
</script>

{#if isStale}
  <div class="backup-staleness-banner" data-testid="backup-staleness-banner" role="status">
    <strong>⚠ Your backup is {daysSince} days old</strong>
    <p>
      You've made changes since your last backup. Communities joined, DMs sent,
      and folder organization will be lost if you can't access this device.
    </p>
    <div class="actions">
      <button type="button" onclick={exportNow}>Export new backup</button>
      <button type="button" onclick={dismiss}>Dismiss for 7 days</button>
    </div>
  </div>
{/if}

<style>
  .backup-staleness-banner {
    background: var(--warn-bg, #fff8e1);
    border: 1px solid var(--warn-border, #f0c870);
    border-radius: 6px;
    padding: 0.75rem 1rem;
    margin: 0.5rem 1rem;
    color: var(--warn-fg, #5c4400);
  }
  .actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }
  button {
    padding: 0.25rem 0.75rem;
  }
</style>
