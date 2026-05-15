<script lang="ts">
  import { onMount } from 'svelte';
  import { getBackupStaleness, dismissForDays } from '../backup-service';

  interface Props {
    onExportRequested?: () => void;
  }
  let { onExportRequested }: Props = $props();

  let isStale = $state(false);
  let daysSince = $state(0);

  onMount(async () => {
    try {
      const r = await getBackupStaleness();
      isStale = r.isStale;
      daysSince = r.daysSince;
    } catch {
      // Best-effort — never block UI on staleness check failure.
      isStale = false;
    }
  });

  function dismiss() {
    dismissForDays(7);
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
