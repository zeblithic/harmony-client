<script lang="ts">
  import type { ContentDetail } from '../types';

  let {
    item,
    onPublish,
    onRelease,
    onBurn,
    onArchive,
    onPin,
    onUnpin,
    onExport,
  }: {
    item: ContentDetail;
    onPublish?: () => void;
    onRelease?: () => void;
    onBurn?: () => void;
    onArchive?: () => void;
    onPin?: () => void;
    onUnpin?: () => void;
    onExport?: () => void;
  } = $props();
</script>

<div class="file-actions">
  <button class="action-btn" onclick={() => onPublish?.()} aria-label="Publish">
    <span class="action-icon" aria-hidden="true">{'\u{1F310}'}</span>
    Publish
  </button>

  <button class="action-btn" onclick={() => onRelease?.()} aria-label="Release">
    <span class="action-icon" aria-hidden="true">{'\u{1F4A8}'}</span>
    Release
  </button>

  <button class="action-btn destructive" onclick={() => onBurn?.()} aria-label="Burn">
    <span class="action-icon" aria-hidden="true">{'\u{1F525}'}</span>
    Burn
  </button>

  <button class="action-btn" onclick={() => onArchive?.()} aria-label="Archive">
    <span class="action-icon" aria-hidden="true">{'\u{1F4E6}'}</span>
    Archive
  </button>

  {#if item.pinned}
    <button class="action-btn" onclick={() => onUnpin?.()} aria-label="Unpin">
      <span class="action-icon" aria-hidden="true">{'\u{1F4CC}'}</span>
      Unpin
    </button>
  {:else}
    <button class="action-btn" onclick={() => onPin?.()} aria-label="Pin">
      <span class="action-icon" aria-hidden="true">{'\u{1F4CC}'}</span>
      Pin
    </button>
  {/if}

  <button class="action-btn" onclick={() => onExport?.()} aria-label="Export">
    <span class="action-icon" aria-hidden="true">{'\u{1F4E4}'}</span>
    Export
  </button>
</div>

<style>
  .file-actions {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .action-btn {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 8px;
    border-radius: 4px;
    border: none;
    background: var(--bg-tertiary, #232428);
    color: var(--text-primary, #f2f3f5);
    font-size: 0.85rem;
    cursor: pointer;
    font: inherit;
    text-align: left;
  }

  .action-btn:hover {
    background: var(--bg-tertiary-hover, #35373c);
    color: var(--accent, #5865f2);
  }

  .action-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }

  .action-btn.destructive {
    background: #d83c3e;
    color: #ffffff;
  }

  .action-btn.destructive:hover {
    background: #c23234;
    color: #ffffff;
  }

  .action-icon {
    flex-shrink: 0;
    width: 1.2em;
    text-align: center;
  }
</style>
