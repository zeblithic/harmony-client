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

  let percent = $derived(Math.min(100, Math.round((usedBytes / totalBytes) * 100)));
  let warning = $derived(percent >= 85);
</script>

<button class="quota-bar" onclick={onCleanupClick} aria-label="Storage quota — click to manage">
  <div class="quota-track">
    <div
      class="quota-fill"
      class:warning
      role="progressbar"
      aria-valuenow={percent}
      aria-valuemin={0}
      aria-valuemax={100}
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
    background: var(--bg-secondary, #2b2d31);
    border-top: 1px solid var(--border, #3f4147);
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
    background: var(--bg-tertiary, #232428);
  }

  .quota-track {
    height: 6px;
    background: var(--bg-tertiary, #232428);
    border-radius: 3px;
    overflow: hidden;
  }

  .quota-fill {
    height: 100%;
    background: var(--accent, #5865f2);
    border-radius: 3px;
    transition: width 0.3s;
  }

  .quota-fill.warning {
    background: #d83c3e;
  }

  .quota-text {
    font-size: 0.75rem;
    color: var(--text-muted, #949ba4);
  }
</style>
