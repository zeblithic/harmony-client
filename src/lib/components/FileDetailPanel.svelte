<script lang="ts">
  import type { ContentDetail, ReplicationTier } from '../types';
  import FileMetadata from './FileMetadata.svelte';
  import SensitivityBadge from './SensitivityBadge.svelte';
  import StalenessIndicator from './StalenessIndicator.svelte';
  import ReplicationStatus from './ReplicationStatus.svelte';
  import FileActions from './FileActions.svelte';

  let {
    detail,
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
    onTierChange: (tier: ReplicationTier) => void;
    onPublish: () => void;
    onRelease: () => void;
    onBurn: () => void;
    onArchive: () => void;
    onPin: () => void;
    onUnpin: () => void;
    onExport: () => void;
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
    <div class="staleness-bar-track">
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
    width: 320px;
    flex-shrink: 0;
    background: var(--bg-secondary, #2b2d31);
    border-left: 1px solid var(--bg-tertiary, #313338);
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
