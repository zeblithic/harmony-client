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
      // the pre-walk failed (rare: unreadable root) — in that case we
      // can't report a denominator at all: `totalFilesSeen` counts only
      // leaves the walker actually touched, so substituting it would
      // silently change the meaning of the headline depending on whether
      // the pre-walk succeeded.
      if (result.preWalkTotal > 0) {
        return `Cancelled — added ${result.succeeded} of ${result.preWalkTotal} files`;
      }
      return `Cancelled — added ${result.succeeded} files (total unknown)`;
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
    result
      ? result.skipped.hidden + result.skipped.symlink + result.skipped.other
      : 0,
  );
  let hasSkipped = $derived(skippedTotal > 0);
  let hasFailed = $derived(
    !!result && (result.failed.length > 0 || result.failedOverflow > 0),
  );
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
            {#if result.skipped.other > 0}
              <li>{result.skipped.other} other special files (FIFOs/sockets/devices)</li>
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
    color: var(--text-primary);
    font-size: 1.1rem;
    margin: 0 0 16px;
  }

  .body {
    margin-bottom: 16px;
  }

  .section {
    margin-bottom: 8px;
    color: var(--text-secondary);
    font-size: 0.9rem;
  }

  .section summary {
    cursor: pointer;
    padding: 4px 0;
  }

  .section summary:focus-visible {
    outline: 2px solid var(--accent);
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
    font-family: var(--font-mono);
    word-break: break-all;
  }

  .section .overflow {
    color: var(--text-muted);
    font-style: italic;
    opacity: 0.85;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .done-btn {
    background: var(--accent);
    color: var(--text-primary);
    border: 1px solid var(--border);
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .done-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
