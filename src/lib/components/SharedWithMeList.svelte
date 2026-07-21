<script lang="ts">
  import type { ReceivedFile } from '../types';
  import { formatBytes, relativeTime } from '../file-utils';

  let {
    files,
    onDownload,
  }: {
    /** Files shared with this user by other owners. `null` until
     *  `list_received_grants` resolves — render a neutral placeholder, NOT
     *  the proven-empty message (null-until-resolved honesty; mirrors
     *  ShareList's `grants`). A load FAILURE must also leave this `null`,
     *  never `[]`. */
    files: ReceivedFile[] | null;
    onDownload: (file: ReceivedFile) => void;
  } = $props();
</script>

<section class="shared-with-me" aria-label="Shared with me">
  {#if files === null}
    <p class="swm-placeholder" aria-busy="true">Loading…</p>
  {:else if files.length === 0}
    <p class="swm-empty">Nothing has been shared with you yet.</p>
  {:else}
    <ul class="swm-list">
      {#each files as file (file.cid)}
        <li class="swm-row">
          <div class="swm-meta">
            <span class="swm-name">{file.fileName}</span>
            <span class="swm-sub">Shared by {file.granterDisplay} · {formatBytes(file.fileSize)} · {relativeTime(file.receivedAt)}</span>
          </div>
          <button type="button" class="swm-download" onclick={() => onDownload(file)}>
            Download
          </button>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .shared-with-me {
    padding: 4px 0;
  }

  .swm-placeholder,
  .swm-empty {
    font-size: 0.8rem;
    color: var(--text-muted);
    margin: 4px 0;
    font-style: italic;
  }

  .swm-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .swm-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 4px 8px;
    border-radius: 4px;
    background: var(--share-bg);
  }

  .swm-row:hover {
    background: var(--share-bg-hover);
  }

  .swm-meta {
    display: flex;
    flex-direction: column;
    min-width: 0;
    flex: 1;
  }

  .swm-name {
    font-size: 0.85rem;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .swm-sub {
    font-size: 0.75rem;
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .swm-download {
    padding: 2px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: none;
    color: var(--text-secondary);
    font-size: 0.7rem;
    cursor: pointer;
    flex-shrink: 0;
  }

  .swm-download:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .swm-download:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
