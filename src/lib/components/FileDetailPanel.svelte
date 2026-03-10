<script lang="ts">
  import type { ContentDetail, ContentSensitivity, ReplicationTier, PeerRef, StorageBuddy } from '../types';
  import FileMetadata from './FileMetadata.svelte';
  import SensitivityBadge from './SensitivityBadge.svelte';
  import StalenessIndicator from './StalenessIndicator.svelte';
  import ReplicationStatus from './ReplicationStatus.svelte';
  import FileActions from './FileActions.svelte';
  import ShareList from './ShareList.svelte';
  import StorageBuddyList from './StorageBuddyList.svelte';

  let {
    detail,
    availablePeers = [],
    storageBuddyDetails = [],
    confirmationOverrides = {},
    onTierChange,
    onPublish,
    onRelease,
    onBurn,
    onArchive,
    onPin,
    onUnpin,
    onExport,
    onShareAdd,
    onShareRemove,
    onBuddyAdd,
    onBuddyRemove,
  }: {
    detail: ContentDetail;
    availablePeers?: PeerRef[];
    storageBuddyDetails?: StorageBuddy[];
    confirmationOverrides?: Partial<Record<ContentSensitivity, number>>;
    onTierChange: (tier: ReplicationTier) => void;
    onPublish: (cid: string) => void;
    onRelease: (cid: string) => void;
    onBurn: () => void;
    onArchive: () => void;
    onPin: () => void;
    onUnpin: () => void;
    onExport: () => void;
    onShareAdd?: (peer: PeerRef) => void;
    onShareRemove?: (peer: PeerRef) => void;
    onBuddyAdd?: (peer: PeerRef) => void;
    onBuddyRemove?: (buddy: StorageBuddy) => void;
  } = $props();

  let stalenessText = $derived(detail.stalenessScore.toFixed(2));
</script>

<aside class="file-detail-panel" aria-label="File details">
  <section class="panel-section">
    <FileMetadata item={detail} />
  </section>

  <section class="panel-section">
    <SensitivityBadge sensitivity={detail.sensitivity} />
  </section>

  <section class="panel-section staleness-section">
    <div class="staleness-bar-row">
      <span class="staleness-label">Staleness:</span>
      <span class="staleness-value">{stalenessText}</span>
      <StalenessIndicator score={detail.stalenessScore} pinned={detail.pinned} />
    </div>
    <div class="staleness-bar-track" role="progressbar" aria-label="Staleness" aria-valuenow={Math.round(detail.stalenessScore * 100)} aria-valuemin={0} aria-valuemax={100}>
      <div
        class="staleness-bar-fill"
        style="width: {Math.min(detail.stalenessScore * 100, 100)}%"
      ></div>
    </div>
  </section>

  <section class="panel-section">
    <ReplicationStatus
      tier={detail.replicationTier}
      replicaCount={detail.replicaCount}
      {onTierChange}
    />
  </section>

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

  <section class="panel-section">
    <ShareList
      sharedWith={detail.sharedWith}
      {availablePeers}
      onAdd={(peer) => onShareAdd?.(peer)}
      onRemove={(peer) => onShareRemove?.(peer)}
    />
  </section>

  <section class="panel-section">
    <StorageBuddyList
      buddies={storageBuddyDetails}
      {availablePeers}
      onAdd={(peer) => onBuddyAdd?.(peer)}
      onRemove={(buddy) => onBuddyRemove?.(buddy)}
    />
  </section>
</aside>

<style>
  .file-detail-panel {
    background: var(--bg-secondary, #2b2d31);
    overflow-y: auto;
    padding: 0 16px;
  }

  .panel-section {
    padding: 12px 0;
    border-bottom: 1px solid var(--border, #3f4147);
  }

  .panel-section:last-child {
    border-bottom: none;
  }

  .staleness-section {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .staleness-bar-row {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 0.85rem;
  }

  .staleness-label {
    color: var(--text-secondary, #b5bac1);
  }

  .staleness-value {
    color: var(--text-primary, #f2f3f5);
    font-weight: 600;
  }

  .staleness-bar-track {
    height: 4px;
    border-radius: 2px;
    background: var(--bg-tertiary, #232428);
    overflow: hidden;
  }

  .staleness-bar-fill {
    height: 100%;
    border-radius: 2px;
    background: var(--accent, #5865f2);
    transition: width 0.2s ease;
  }
</style>
