<script lang="ts">
  import type { ContentDetail, ContentSensitivity, ReplicationTier } from '../types';
  import FileMetadata from './FileMetadata.svelte';
  import SensitivityBadge from './SensitivityBadge.svelte';
  import ReplicationStatus from './ReplicationStatus.svelte';
  import FileActions from './FileActions.svelte';

  // ZEB-612 S3: ShareList / StorageBuddyList / origin were mock-backed and
  // are removed until the storage-buddies domain ships real hosting
  // accounting (ZEB-669).
  let {
    detail,
    usedByVines = 0,
    confirmationOverrides = {},
    onTierChange,
    onPublish,
    onRelease,
    onBurn,
    onArchive,
    onPin,
    onUnpin,
    onExport,
  }: {
    detail: ContentDetail;
    /** Vines referencing this CID (client-computed, real descriptors). */
    usedByVines?: number;
    confirmationOverrides?: Partial<Record<ContentSensitivity, number>>;
    onTierChange: (tier: ReplicationTier) => void;
    onPublish: (cid: string) => void;
    onRelease: (cid: string) => void;
    onBurn: () => void;
    onArchive: () => void;
    onPin: () => void;
    onUnpin: () => void;
    onExport: () => void;
  } = $props();

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

  {#if usedByVines > 0}
    <section class="panel-section">
      <span class="used-by-vines">Used by {usedByVines} vine{usedByVines === 1 ? '' : 's'}</span>
    </section>
  {/if}

  <section class="panel-section">
    <FileActions
      item={detail}
      {confirmationOverrides}
      {onPublish}
      {onRelease}
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
</style>
