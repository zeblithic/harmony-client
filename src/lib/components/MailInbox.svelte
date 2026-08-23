<script lang="ts">
  import type { MailEntry, MailFolderKind, MailCounts } from '../types';
  // ZEB-946: the "today" mail time honors the owner's clock preference.
  // ZEB-952: recency is bucketed by local calendar day (shared seam), not by
  // elapsed 24h windows.
  import { formatMailRecency, type TimeFormatPrefs } from '../time-format';
  import { timeFormatPrefs } from '../time-format-service';
  import { dayClock } from '../day-clock';
  // ZEB-961: resolve the sender owner_id to its broadcast card name when
  // available, else the shared short-hex (first8…) — no local shortAddr copy.
  import { resolveMentionLabel } from '../mention-render';
  import PeerName from './PeerName.svelte';
  import { shortId } from '../short-addr';
  import type { ResolvedCard } from '../member-card-service';

  let {
    entries = [],
    activeFolder = 'inbox',
    counts = { inbox: { total: 0, unread: 0 }, sent: { total: 0, unread: 0 }, drafts: { total: 0, unread: 0 }, trash: { total: 0, unread: 0 } },
    selectedCid = null,
    syncState = 'idle',
    syncError = null,
    resolveCard,
    resolveNickname,
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
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    // ZEB-977: petname rung (see MailReader).
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    onRefresh?: () => void;
    onSelectEmail?: (cid: string) => void;
    onFolderChange?: (folder: MailFolderKind) => void;
    onCompose?: () => void;
    onMarkRead?: (cid: string) => void;
    onMoveTrash?: (cid: string) => void;
  } = $props();

  // ZEB-977: full ladder (petname ► card ► hex); this surface's established
  // hex form is the shared shortId (first8 + ellipsis), so swap it onto the
  // hex rung rather than the ladder's bare slice(0, 8).
  function senderName(address: string) {
    const resolved = resolveMentionLabel(address, resolveNickname, resolveCard);
    return resolved.source === 'hex' ? { ...resolved, label: shortId(address) } : resolved;
  }


  const folders: { kind: MailFolderKind; label: string }[] = [
    { kind: 'inbox', label: 'Inbox' },
    { kind: 'sent', label: 'Sent' },
    { kind: 'drafts', label: 'Drafts' },
    { kind: 'trash', label: 'Trash' },
  ];

  // `entry.timestamp` is in seconds; the seam works in ms. `now` is the shared
  // `$dayClock` (injected at the call site below) — the same reactive reference
  // every message surface uses. It re-emits at each local midnight, so a mounted
  // inbox reclassifies its rows across a day boundary instead of holding a
  // mount-time snapshot; a bare `Date.now()` would go stale until an unrelated
  // re-render (ZEB-952). Bucketing lives in the shared seam so its boundary
  // behavior is unit-tested deterministically — see time-format.test.ts.
  function formatTime(timestamp: number, now: number, prefs: TimeFormatPrefs): string {
    return formatMailRecency(timestamp * 1000, now, prefs);
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
          <span class="mail-sender"><PeerName name={senderName(entry.senderAddress)} ownerIdHex={entry.senderAddress} /></span>
          <span class="mail-subject">{entry.subjectSnippet || '(no subject)'}</span>
          <span class="mail-time">{formatTime(entry.timestamp, $dayClock, $timeFormatPrefs)}</span>
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
    padding: 8px 12px;
    border-bottom: 1px solid var(--border);
    flex-shrink: 0;
  }

  .folder-tabs {
    display: flex;
    gap: 4px;
  }

  .folder-tab {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 4px 8px;
    border: none;
    border-radius: 5px;
    background: transparent;
    color: var(--text-secondary);
    cursor: pointer;
    font-size: 0.8125rem;
  }

  .folder-tab.active {
    background: var(--surface-active);
    color: var(--text-primary);
  }

  /* Notification-count idiom, shared with NavNodeRow .unread-badge (ZEB-654 D2):
     accent pill beside the folder label — not the stacked CountChip data box. */
  .unread-badge {
    background: var(--accent);
    color: var(--on-accent);
    font-size: 0.6875rem;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 8px;
    flex-shrink: 0;
  }

  .compose-btn {
    padding: 6px 12px;
    border: none;
    border-radius: 5px;
    background: var(--accent);
    color: var(--on-accent);
    cursor: pointer;
    font-size: 0.8125rem;
  }

  .mail-list {
    flex: 1;
    overflow-y: auto;
  }

  .empty-state {
    padding: 32px;
    text-align: center;
    color: var(--text-secondary);
  }

  .mail-row {
    display: grid;
    grid-template-columns: 8rem 1fr auto auto;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 8px 12px;
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
    gap: 4px;
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
    padding: 0 4px;
  }

  .action-btn:hover {
    color: var(--text-primary);
  }

  .sync-controls {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    margin-left: 8px;
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
    border-radius: 5px;
    color: var(--text-secondary);
    cursor: pointer;
    padding: 2px 6px;
    font-size: 0.8125rem;
  }
  .sync-refresh-btn:hover {
    background: var(--hover-bg);
    color: var(--text-primary);
  }
</style>
