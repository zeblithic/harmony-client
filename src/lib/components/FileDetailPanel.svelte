<script lang="ts">
  import type { ContentDetail, ContentOriginInfo, FileGrant, ReplicationTier } from '../types';
  import FileMetadata from './FileMetadata.svelte';
  import SensitivityBadge from './SensitivityBadge.svelte';
  import ReplicationStatus from './ReplicationStatus.svelte';
  import FileActions from './FileActions.svelte';
  import ShareList from './ShareList.svelte';

  // ZEB-612 S3 removed the mock-backed ShareList/StorageBuddyList/origin;
  // ZEB-669 restores backup (S3) + origin (S4) against real backends.
  // ZEB-674 C5 restores ShareList as an honest per-file viewer ACL — it
  // only exists (and is only coherent) for files encrypted at ingest, so
  // it renders only when `detail.encrypted`.
  let {
    detail,
    usedByVines = 0,
    onTierChange,
    onBurn,
    onArchive,
    onPin,
    onUnpin,
    onExport,
    onSetBackup,
    fileGrants = null,
    availableGrantFriends = [],
    onGrantRead,
    onRevokeRead,
    resolveNickname,
    resolveCard,
  }: {
    detail: ContentDetail;
    /** Vines referencing this CID (client-computed, real descriptors). */
    usedByVines?: number;
    onTierChange: (tier: ReplicationTier) => void;
    onBurn: () => void;
    onArchive: () => void;
    onPin: () => void;
    onUnpin: () => void;
    onExport: () => void;
    /** ZEB-669 S3: toggle "back up with buddies". Rejections (stable
     *  `ineligible:` prefix) render inline; absent → section hidden. */
    onSetBackup?: (sidecarId: string, backup: boolean) => Promise<void>;
    /** ZEB-674 C5: the current file's grant list. `null` until `listGrants`
     *  resolves for `detail.cid` — see `ShareList`'s honesty contract. */
    fileGrants?: FileGrant[] | null;
    /** ZEB-674 C5: friends eligible for the "Share with…" picker. */
    availableGrantFriends?: { address: string; displayName: string | null }[];
    onGrantRead?: (address: string) => Promise<void>;
    onRevokeRead?: (address: string) => Promise<void>;
    /** ZEB-977: petname + live-card rungs, threaded through to ShareList. */
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    resolveCard?: (ownerIdHex: string) => import('../member-card-service').ResolvedCard | undefined;
  } = $props();

  // ZEB-669 S3: frontend proxy gate for backup eligibility — the backend
  // (CID-class check in set_backup_flag) stays the authority; this only
  // drives the disabled state + reason copy. Clearing an already-set flag
  // is always allowed (backend contract).
  let backupEligible = $derived(detail.sensitivity === 'public');
  let backupPending = $state(false);
  let backupError = $state<string | null>(null);
  /** Optimistic in-flight value: the checkbox is server-authoritative
   *  (`detail.backup`), so mid-await rerenders would snap it back to the
   *  old value (Qodo PR #450). Shown value = optimistic ?? server truth;
   *  cleared when server truth refreshes and on rejection (which reverts
   *  the box). */
  let optimisticBackup = $state<boolean | null>(null);
  /** Request identity: a file switch invalidates in-flight completions so
   *  a stale rejection/finally can't clobber the next file's toggle state
   *  (CodeRabbit PR #450). Plain counter — never read reactively. */
  let backupReq = 0;
  $effect(() => {
    // File switch: clear ALL transient toggle state for the new file.
    void detail.sidecarId;
    backupReq++;
    optimisticBackup = null;
    backupError = null;
    backupPending = false;
  });
  $effect(() => {
    // Same-file server refresh: authoritative value arrived, drop optimism.
    void detail.backup;
    optimisticBackup = null;
  });
  let backupChecked = $derived(optimisticBackup ?? detail.backup ?? false);

  async function handleBackupToggle(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const wanted = input.checked;
    if (!onSetBackup || backupPending) return;
    const req = ++backupReq;
    backupPending = true;
    backupError = null;
    optimisticBackup = wanted;
    try {
      await onSetBackup(detail.sidecarId, wanted);
    } catch (err) {
      if (req !== backupReq) return; // stale completion — a newer file/request owns the state
      const msg = err instanceof Error ? err.message : String(err);
      // Strip the stable machine prefix for display copy.
      backupError = msg.startsWith('ineligible:')
        ? `Not eligible: ${msg.slice('ineligible:'.length).trim()}`
        : msg;
      optimisticBackup = null; // revert — backend rejected
    } finally {
      if (req === backupReq) backupPending = false;
    }
  }

  // ZEB-669 S4: "From" row copy. Introducer pet-name resolution lands when
  // a seam actually emits one (channelDownload/buddyPin are reserved —
  // no index-creation path today); until then the address fallback is
  // unreachable in practice but honest.
  function originLabel(origin: ContentOriginInfo): string {
    if (origin.kind === 'selfIngest') return 'Added by you';
    const kindCopy = origin.kind === 'channelDownload' ? 'Channel download' : 'Buddy pin';
    if (!origin.introducer) return kindCopy;
    const a = origin.introducer;
    return `${kindCopy} · ${a.length > 12 ? `${a.slice(0, 6)}…${a.slice(-4)}` : a}`;
  }

  // Copy-CID affordance — the InviteLinkManager idiom: flip to "✓ Copied"
  // for 2 s, clear the timer on unmount, surface clipboard denial inline.
  let copied = $state(false);
  let copyError = $state<string | null>(null);
  let copyTimer: ReturnType<typeof setTimeout> | null = null;

  async function copyCid() {
    copyError = null;
    if (!navigator.clipboard) {
      copyError = 'Clipboard unavailable';
      return;
    }
    try {
      await navigator.clipboard.writeText(detail.cid);
      copied = true;
      if (copyTimer) clearTimeout(copyTimer);
      copyTimer = setTimeout(() => {
        copied = false;
      }, 2000);
    } catch {
      copyError = 'Copy failed — clipboard permission denied';
    }
  }

  $effect(() => {
    return () => {
      if (copyTimer) clearTimeout(copyTimer);
    };
  });
</script>

<aside class="file-detail-panel" aria-label="File details">
  <section class="panel-section">
    <FileMetadata item={detail} />
  </section>

  {#if detail.origin}
    <section class="panel-section from-section" data-testid="from-row">
      <span class="from-label">From</span>
      <span class="from-value">{originLabel(detail.origin)}</span>
    </section>
  {/if}

  <section class="panel-section">
    <SensitivityBadge sensitivity={detail.sensitivity} />
  </section>

  <section class="panel-section cid-section">
    <span class="cid-label">Content ID</span>
    <div class="cid-box">
      <code class="cid-full" data-testid="cid-full">{detail.cid}</code>
    </div>
    <button type="button" class="copy-cid-btn" onclick={copyCid}>
      {copied ? '✓ Copied' : 'Copy CID'}
    </button>
    {#if copyError}
      <p class="copy-error" role="alert">{copyError}</p>
    {/if}
  </section>

  <section class="panel-section">
    <ReplicationStatus
      tier={detail.replicationTier}
      replicaCount={detail.replicaCount}
      {onTierChange}
    />
  </section>

  {#if onSetBackup && detail.sidecarId}
    <!-- Manifest-derived rows (empty sidecarId) have no sidecar to flag;
         no per-file buddy list here — hosting reports are aggregate. -->
    <section class="panel-section backup-section" data-testid="backup-section">
      <label class="backup-toggle">
        <input
          type="checkbox"
          checked={backupChecked}
          disabled={(!backupEligible && !detail.backup) || backupPending}
          onchange={handleBackupToggle}
          data-testid="backup-checkbox"
        />
        Back up with buddies
      </label>
      {#if !backupEligible && !detail.backup}
        <p class="toggle-hint">Only public files can be backed up by buddies.</p>
      {/if}
      {#if backupError}
        <p class="backup-error" role="alert">{backupError}</p>
      {/if}
    </section>
  {/if}

  {#if detail.encrypted && onGrantRead && onRevokeRead}
    <!-- ZEB-674 C5: honest per-file viewer ACL. Only coherent for a file
         encrypted at ingest (a public CID has no key to gate on) — public
         files render no ShareList at all, not an empty one. Same gating
         idiom as the backup section above: no handler wired → no section
         (never render a control the caller can't actually invoke). -->
    <section class="panel-section" data-testid="share-list-section">
      <ShareList
        grants={fileGrants}
        availableFriends={availableGrantFriends}
        isEncrypted={detail.encrypted}
        onGrant={onGrantRead}
        onRevoke={onRevokeRead}
        {resolveNickname}
        {resolveCard}
      />
    </section>
  {/if}

  {#if usedByVines > 0}
    <section class="panel-section">
      <span class="used-by-vines">Used by {usedByVines} vine{usedByVines === 1 ? '' : 's'}</span>
    </section>
  {/if}

  <section class="panel-section">
    <FileActions
      item={detail}
      {onBurn}
      {onArchive}
      {onPin}
      {onUnpin}
      {onExport}
    />
  </section>
</aside>

<style>
  .file-detail-panel {
    background: var(--bg-secondary);
    overflow-y: auto;
    padding: 0 16px;
  }

  .panel-section {
    padding: 12px 0;
    border-bottom: 1px solid var(--border);
  }

  .panel-section:last-child {
    border-bottom: none;
  }

  .cid-section {
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-start;
  }

  .cid-label {
    font-size: 0.7rem;
    color: var(--faint);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    font-weight: 600;
  }

  .cid-box {
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 8px;
    padding: 8px 10px;
    width: 100%;
  }

  .cid-full {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--text-primary);
    word-break: break-all;
  }

  .copy-cid-btn {
    font: inherit;
    font-size: 0.8rem;
    color: var(--text-primary);
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 5px;
    padding: 4px 10px;
    cursor: pointer;
  }

  .copy-cid-btn:hover {
    background: var(--bg-secondary);
  }

  .copy-cid-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .copy-error {
    margin: 0;
    font-size: 0.75rem;
    color: var(--danger);
  }

  .used-by-vines {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--text-secondary);
  }

  .from-section {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .from-label {
    font-size: 0.7rem;
    color: var(--faint);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    font-weight: 600;
  }

  .from-value {
    font-size: 0.8rem;
    color: var(--text-secondary);
  }

  .backup-section {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .backup-toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.85rem;
    color: var(--text-primary);
  }

  .toggle-hint {
    margin: 0;
    font-size: 0.72rem;
    color: var(--text-muted);
  }

  .backup-error {
    margin: 0;
    font-size: 0.75rem;
    color: var(--danger);
  }
</style>
