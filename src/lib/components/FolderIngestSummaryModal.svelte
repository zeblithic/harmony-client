<script lang="ts">
  import Modal from './Modal.svelte';
  import type { IngestFolderTreeResult } from '../file-manager-service';

  let {
    open,
    result,
    onDismiss,
  }: {
    open: boolean;
    result: IngestFolderTreeResult | null;
    onDismiss: () => void;
  } = $props();

  const titleId = `folder-ingest-summary-title-${Math.random().toString(36).slice(2)}`;

  // Headline branches across four shapes of `result`:
  //   normal:    cancelled=false, rootSidecarId set, succeeded > 0
  //   empty:     cancelled=false, rootSidecarId set, succeeded == 0
  //   cancelled: cancelled=true (rootSidecarId may be null pre-root or set post-root)
  //   failed:    cancelled=false, rootSidecarId null (root manifest build itself failed)
  let headline = $derived.by(() => {
    if (!result) return '';
    if (result.cancelled) {
      // `preWalkTotal` is the count taken BEFORE the walker started, so a
      // mid-walk cancel still reports against the full tree. `-1` means
      // the pre-walk failed (rare: unreadable root) — fall back to
      // `totalFilesSeen` (counters of leaves the walker actually touched).
      const denom = result.preWalkTotal > 0 ? result.preWalkTotal : result.totalFilesSeen;
      return `Cancelled — added ${result.succeeded} of ${denom} files`;
    }
    if (result.rootSidecarId === null) {
      return 'Folder ingest failed before completing';
    }
    if (result.succeeded === 0) {
      return `Added folder ${result.rootName}`;
    }
    return `Added folder ${result.rootName} with ${result.succeeded} files`;
  });

  let skippedTotal = $derived(
    result ? result.skipped.hidden + result.skipped.symlink + result.skipped.oversized : 0,
  );
  let hasSkipped = $derived(skippedTotal > 0);
  let hasFailed = $derived(!!result && result.failed.length > 0);
  let failedTotal = $derived(
    result ? result.failed.length + result.failedOverflow : 0,
  );
</script>

{#if open && result}
  <Modal onCancel={onDismiss} ariaLabelledby={titleId}>
    <h2 id={titleId} class="title">{headline}</h2>

    <div class="body" aria-live="polite">
      {#if hasSkipped}
        <details class="section">
          <summary>Skipped: {skippedTotal} items</summary>
          <ul>
            {#if result.skipped.hidden > 0}
              <li>{result.skipped.hidden} hidden files</li>
            {/if}
            {#if result.skipped.symlink > 0}
              <li>{result.skipped.symlink} symlinks (not followed)</li>
            {/if}
            {#if result.skipped.oversized > 0}
              <li>{result.skipped.oversized} files too large (&gt;8 GiB)</li>
            {/if}
          </ul>
        </details>
      {/if}

      {#if hasFailed}
        <details class="section">
          <summary>Failed: {failedTotal} files</summary>
          <ul>
            {#each result.failed as entry (entry.path)}
              <li><span class="path">{entry.path}</span>: {entry.message}</li>
            {/each}
            {#if result.failedOverflow > 0}
              <li class="overflow">and {result.failedOverflow} more (errors not shown)</li>
            {/if}
          </ul>
        </details>
      {/if}
    </div>

    <div class="actions">
      <!-- svelte-ignore a11y_autofocus -->
      <button
        type="button"
        class="done-btn"
        autofocus
        onclick={onDismiss}
      >
        Done
      </button>
    </div>
  </Modal>
{/if}

<style>
  .title {
    color: var(--text-primary, #f2f3f5);
    font-size: 1.1rem;
    margin: 0 0 16px;
  }

  .body {
    margin-bottom: 16px;
  }

  .section {
    margin-bottom: 8px;
    color: var(--text-secondary, #b5bac1);
    font-size: 0.9rem;
  }

  .section summary {
    cursor: pointer;
    padding: 4px 0;
  }

  .section summary:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }

  .section ul {
    list-style: none;
    padding-left: 12px;
    margin: 4px 0 0;
  }

  .section li {
    padding: 2px 0;
    font-size: 0.85rem;
  }

  .section .path {
    font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    word-break: break-all;
  }

  .section .overflow {
    color: var(--text-muted, #b5bac1);
    font-style: italic;
    opacity: 0.85;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .done-btn {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
    border: 1px solid var(--border, transparent);
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .done-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
