<script lang="ts">
  import type { MailEntry, MailFolderKind, MailCounts } from '../types';

  let {
    entries = [],
    activeFolder = 'inbox',
    counts = { inbox: { total: 0, unread: 0 }, sent: { total: 0, unread: 0 }, drafts: { total: 0, unread: 0 }, trash: { total: 0, unread: 0 } },
    selectedCid = null,
    syncState = 'idle',
    syncError = null,
    onRefresh,
    onSelectEmail,
    onFolderChange,
    onCompose,
    onMarkRead,
    onMoveTrash,
  }: {
    entries: MailEntry[];
    activeFolder: MailFolderKind;
    counts: MailCounts;
    selectedCid: string | null;
    syncState?: 'idle' | 'syncing' | 'error';
    syncError?: string | null;
    onRefresh?: () => void;
    onSelectEmail?: (cid: string) => void;
    onFolderChange?: (folder: MailFolderKind) => void;
    onCompose?: () => void;
    onMarkRead?: (cid: string) => void;
    onMoveTrash?: (cid: string) => void;
  } = $props();

  const folders: { kind: MailFolderKind; label: string }[] = [
    { kind: 'inbox', label: 'Inbox' },
    { kind: 'sent', label: 'Sent' },
    { kind: 'drafts', label: 'Drafts' },
    { kind: 'trash', label: 'Trash' },
  ];

  function formatTime(timestamp: number): string {
    const date = new Date(timestamp * 1000);
    const now = new Date();
    const diffDays = Math.floor((now.getTime() - date.getTime()) / 86400_000);
    if (diffDays === 0) return date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    if (diffDays < 7) return date.toLocaleDateString([], { weekday: 'short' });
    return date.toLocaleDateString([], { month: 'short', day: 'numeric' });
  }

  function shortAddr(addr: string): string {
    if (addr.length <= 8) return addr;
    return addr.slice(0, 8) + '...';
  }
</script>

<div class="mail-inbox">
  <div class="mail-toolbar">
    <div class="folder-tabs">
      {#each folders as folder}
        <button
          type="button"
          class="folder-tab"
          class:active={activeFolder === folder.kind}
          onclick={() => onFolderChange?.(folder.kind)}
        >
          {folder.label}
          {#if counts[folder.kind]?.unread > 0}
            <span class="unread-badge">{counts[folder.kind].unread}</span>
          {/if}
        </button>
      {/each}
    </div>
    <div class="sync-controls">
      {#if syncState === 'syncing'}
        <span class="sync-spinner" title="Syncing mailbox…" aria-label="Syncing">⟳</span>
      {:else if syncState === 'error'}
        <span
          class="sync-error-icon"
          title={syncError ?? 'Sync error'}
          role="img"
          aria-label={syncError ?? 'Sync error'}
        >
          ⚠
        </span>
      {/if}
      <button
        type="button"
        class="sync-refresh-btn"
        onclick={() => onRefresh?.()}
        title="Refresh mailbox"
        aria-label="Refresh mailbox"
      >
        ⟳
      </button>
    </div>
    <button type="button" class="compose-btn" onclick={() => onCompose?.()}>
      Compose
    </button>
  </div>

  <div class="mail-list">
    {#if entries.length === 0}
      <div class="empty-state">No messages in {activeFolder}</div>
    {:else}
      {#each entries as entry (entry.messageCid)}
        <div
          class="mail-row"
          class:unread={!entry.read}
          class:selected={selectedCid === entry.messageCid}
          role="button"
          tabindex="0"
          onclick={() => onSelectEmail?.(entry.messageCid)}
          onkeydown={(e) => {
            if (e.key !== 'Enter' && e.key !== ' ') return;
            if (e.target instanceof HTMLButtonElement) return;
            e.preventDefault();
            onSelectEmail?.(entry.messageCid);
          }}
        >
          <span class="mail-sender">{shortAddr(entry.senderAddress)}</span>
          <span class="mail-subject">{entry.subjectSnippet || '(no subject)'}</span>
          <span class="mail-time">{formatTime(entry.timestamp)}</span>
          <div class="mail-actions">
            {#if !entry.read}
              <button
                type="button"
                class="action-btn"
                title="Mark read"
                aria-label="Mark as read"
                onclick={(e) => { e.stopPropagation(); onMarkRead?.(entry.messageCid); }}
              >
                &bull;
              </button>
            {/if}
            <button
              type="button"
              class="action-btn"
              title="Move to Trash"
              aria-label="Move to trash"
              onclick={(e) => { e.stopPropagation(); onMoveTrash?.(entry.messageCid); }}
            >
              &times;
            </button>
          </div>
        </div>
      {/each}
    {/if}
  </div>
</div>

<style>
  .mail-inbox {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
  }

  .mail-toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.5rem 0.75rem;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .folder-tabs {
    display: flex;
    gap: 0.25rem;
  }

  .folder-tab {
    padding: 0.25rem 0.625rem;
    border: none;
    border-radius: 4px;
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 0.8125rem;
  }

  .folder-tab.active {
    background: var(--surface-active);
    color: var(--text-primary);
  }

  .unread-badge {
    display: inline-block;
    min-width: 1rem;
    padding: 0 0.25rem;
    margin-left: 0.25rem;
    border-radius: 8px;
    background: var(--accent);
    color: var(--text-bright);
    font-size: 0.6875rem;
    text-align: center;
  }

  .compose-btn {
    padding: 0.375rem 0.75rem;
    border: none;
    border-radius: 4px;
    background: var(--accent);
    color: var(--text-bright);
    cursor: pointer;
    font-size: 0.8125rem;
  }

  .mail-list {
    flex: 1;
    overflow-y: auto;
  }

  .empty-state {
    padding: 2rem;
    text-align: center;
    color: var(--text-secondary);
  }

  .mail-row {
    display: grid;
    grid-template-columns: 8rem 1fr auto auto;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.5rem 0.75rem;
    border: none;
    border-bottom: 1px solid var(--border);
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    text-align: left;
    font-size: 0.8125rem;
  }

  .mail-row:hover {
    background: var(--surface-hover);
  }

  .mail-row.selected {
    background: var(--surface-active);
  }

  .mail-row.unread {
    color: var(--text-primary);
    font-weight: 500;
  }

  .mail-sender {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-family: var(--font-mono);
    font-size: 0.75rem;
  }

  .mail-subject {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .mail-time {
    font-size: 0.75rem;
    color: var(--text-secondary);
    white-space: nowrap;
  }

  .mail-actions {
    display: flex;
    gap: 0.25rem;
    opacity: 0;
  }

  .mail-row:hover .mail-actions,
  .mail-row:focus-within .mail-actions {
    opacity: 1;
  }

  .action-btn {
    border: none;
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 1rem;
    padding: 0 0.25rem;
  }

  .action-btn:hover {
    color: var(--text-primary);
  }

  .sync-controls {
    display: inline-flex;
    align-items: center;
    gap: 0.25rem;
    margin-left: 0.5rem;
  }
  .sync-spinner {
    display: inline-block;
    animation: mail-sync-spin 1.5s linear infinite;
    color: var(--text-secondary);
    font-size: 0.9rem;
  }
  @keyframes mail-sync-spin {
    from { transform: rotate(0deg); }
    to { transform: rotate(360deg); }
  }
  .sync-error-icon {
    color: var(--mail-error-text);
    cursor: help;
    font-size: 0.95rem;
  }
  .sync-refresh-btn {
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-secondary);
    cursor: pointer;
    padding: 0.125rem 0.4rem;
    font-size: 0.8125rem;
  }
  .sync-refresh-btn:hover {
    background: var(--hover-bg);
    color: var(--text-primary);
  }
</style>
