<script lang="ts">
  import type { ContentDetail, ContentSensitivity, ReplicationTier, PeerRef, StorageBuddy } from '../types';
  import FileMetadata from './FileMetadata.svelte';
  import SensitivityBadge from './SensitivityBadge.svelte';
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

</script>

<aside class="file-detail-panel" aria-label="File details">
  <section class="panel-section">
    <FileMetadata item={detail} />
  </section>

  <section class="panel-section">
    <SensitivityBadge sensitivity={detail.sensitivity} />
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
</style>
