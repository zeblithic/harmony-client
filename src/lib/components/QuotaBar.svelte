<script lang="ts">
  import { formatBytes } from '../file-utils';

  let {
    usedBytes,
    totalBytes,
    onCleanupClick,
  }: {
    usedBytes: number;
    totalBytes: number;
    onCleanupClick: () => void;
  } = $props();

  let percent = $derived(totalBytes > 0 ? Math.min(100, Math.round((usedBytes / totalBytes) * 100)) : 0);
  let warning = $derived(percent >= 85);
</script>

<button class="quota-bar" onclick={onCleanupClick} aria-label="Storage: {formatBytes(usedBytes)} of {formatBytes(totalBytes)} used ({percent}%) — click to manage">
  <div class="quota-track">
    <div
      class="quota-fill"
      class:warning
      style:width="{percent}%"
    ></div>
  </div>
  <span class="quota-text">{formatBytes(usedBytes)} of {formatBytes(totalBytes)} used ({percent}%)</span>
</button>

<style>
  .quota-bar {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px 12px;
    background: var(--bg-secondary);
    border-top: 1px solid var(--border);
    cursor: pointer;
    width: 100%;
    text-align: left;
    border-left: none;
    border-right: none;
    border-bottom: none;
    font: inherit;
    color: inherit;
  }

  .quota-bar:hover {
    background: var(--bg-tertiary);
  }

  .quota-bar:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .quota-track {
    height: 6px;
    background: var(--bg-tertiary);
    border-radius: 3px;
    overflow: hidden;
  }

  .quota-fill {
    height: 100%;
    background: var(--accent);
    border-radius: 3px;
    transition: width 0.3s;
  }

  .quota-fill.warning {
    background: var(--danger-muted);
  }

  .quota-text {
    font-size: 0.75rem;
    color: var(--text-muted);
  }
</style>
