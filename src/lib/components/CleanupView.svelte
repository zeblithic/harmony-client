<script lang="ts">
  import type { CleanupRecommendation, QuotaStatus } from '../types';
  import { formatBytes } from '../file-utils';
  import QuotaSummary from './QuotaSummary.svelte';
  import RecommendationCard from './RecommendationCard.svelte';
  import ConfirmDialog from './ConfirmDialog.svelte';

  type CleanupAction = 'burn' | 'archive' | 'release' | 'publish' | 'pin';

  let {
    quota,
    recommendations,
    onAction,
    onBulkBurn,
    onBulkArchive,
    onBulkRelease,
    onBulkPublish,
  }: {
    quota: QuotaStatus;
    recommendations: CleanupRecommendation[];
    onAction: (rec: CleanupRecommendation, action: CleanupAction) => void;
    onBulkBurn: (recs: CleanupRecommendation[]) => void;
    onBulkArchive: (recs: CleanupRecommendation[]) => void;
    onBulkRelease: (cids: string[]) => void;
    onBulkPublish: (cids: string[]) => void;
  } = $props();

  // ZEB-164: selection keyed by sidecarId for unambiguous row identity.
  // Recommendations that share a CID would be non-deterministic if keyed by CID.
  let selectedSidecarIds = $state<Set<string>>(new Set());

  // Prune selectedSidecarIds when recommendations change (e.g., after burning an item)
  let validSelectedSidecarIds = $derived.by(() => {
    const recSidecarIds = new Set(recommendations.map(r => r.sidecarId));
    const pruned = new Set<string>();
    for (const id of selectedSidecarIds) {
      if (recSidecarIds.has(id)) pruned.add(id);
    }
    return pruned;
  });

  // Keep cid-keyed set for backward compat with consumers that still need cids
  // (release/publish are content-addressed operations).
  let validSelectedCids = $derived.by(() => {
    const validRecs = recommendations.filter(r => validSelectedSidecarIds.has(r.sidecarId));
    return new Set(validRecs.map(r => r.cid));
  });

  let allSelected = $derived(
    recommendations.length > 0 && validSelectedSidecarIds.size === recommendations.length
  );

  let selectedCount = $derived(validSelectedSidecarIds.size);

  let totalRecoverable = $derived.by(() => {
    let total = 0;
    for (const rec of recommendations) {
      if (validSelectedSidecarIds.has(rec.sidecarId)) {
        total += rec.spaceRecoverable;
      }
    }
    return total;
  });

  let selectedRecsArray = $derived(
    recommendations.filter(r => validSelectedSidecarIds.has(r.sidecarId))
  );

  let selectedCidsArray = $derived(selectedRecsArray.map(r => r.cid));

  function toggleSelectAll() {
    if (allSelected) {
      selectedSidecarIds = new Set();
    } else {
      selectedSidecarIds = new Set(recommendations.map(r => r.sidecarId));
    }
  }

  function toggleItem(sidecarId: string) {
    const next = new Set(selectedSidecarIds);
    if (next.has(sidecarId)) {
      next.delete(sidecarId);
    } else {
      next.add(sidecarId);
    }
    selectedSidecarIds = next;
  }

  let itemWord = $derived(selectedCount === 1 ? 'item' : 'items');

  let showBulkBurnConfirm = $state(false);
  let showBulkArchiveConfirm = $state(false);
  let showBulkReleaseConfirm = $state(false);
  let showBulkPublishConfirm = $state(false);

  function confirmBulkBurn() {
    showBulkBurnConfirm = false;
    onBulkBurn(selectedRecsArray);
  }

  function confirmBulkArchive() {
    showBulkArchiveConfirm = false;
    onBulkArchive(selectedRecsArray);
  }

  function confirmBulkRelease() {
    showBulkReleaseConfirm = false;
    onBulkRelease(selectedCidsArray);
  }

  function confirmBulkPublish() {
    showBulkPublishConfirm = false;
    onBulkPublish(selectedCidsArray);
  }
</script>

<div class="cleanup-view">
  <QuotaSummary {quota} />

  <div class="cleanup-content">
    <div class="cleanup-header">
      <h2 class="cleanup-title">Cleanup Recommendations</h2>
      {#if recommendations.length > 0}
        <label class="select-all-label">
          <input
            type="checkbox"
            checked={allSelected}
            onchange={toggleSelectAll}
            aria-label="Select all"
          />
          Select all
        </label>
      {/if}
    </div>

    {#if selectedCount > 0}
      <div class="bulk-action-bar" role="toolbar" aria-label="Bulk actions">
        <span class="bulk-summary">
          {selectedCount} {itemWord} selected — {formatBytes(totalRecoverable)} recoverable
        </span>
        <div class="bulk-buttons">
          <button class="bulk-btn burn" onclick={() => { showBulkBurnConfirm = true; }}>Burn All</button>
          <button class="bulk-btn archive" onclick={() => { showBulkArchiveConfirm = true; }}>Archive All</button>
          <button class="bulk-btn release" onclick={() => { showBulkReleaseConfirm = true; }}>Release All</button>
          <button class="bulk-btn publish" onclick={() => { showBulkPublishConfirm = true; }}>Publish All</button>
        </div>
      </div>
    {/if}

    {#if recommendations.length === 0}
      <p class="empty-message">No cleanup recommendations — your storage looks good.</p>
    {:else}
      <div class="recommendations-list" role="list">
        {#each recommendations as rec (rec.sidecarId || rec.cid)}
          <div role="listitem">
            <RecommendationCard
              recommendation={rec}
              checked={validSelectedSidecarIds.has(rec.sidecarId)}
              {onAction}
              onToggle={(sidecarId) => toggleItem(sidecarId)}
            />
          </div>
        {/each}
      </div>
    {/if}
  </div>
</div>

{#if showBulkBurnConfirm}
  <ConfirmDialog
    title="Burn Selected Items"
    message="This will permanently delete {selectedCount} {itemWord} and free {formatBytes(totalRecoverable)} of storage quota. This cannot be undone."
    confirmLabel="Burn All"
    destructive={true}
    onConfirm={confirmBulkBurn}
    onCancel={() => { showBulkBurnConfirm = false; }}
  />
{/if}

{#if showBulkArchiveConfirm}
  <ConfirmDialog
    title="Archive Selected Items"
    message="Archive is not yet implemented. This action will have no effect."
    confirmLabel="Archive All"
    onConfirm={confirmBulkArchive}
    onCancel={() => { showBulkArchiveConfirm = false; }}
  />
{/if}

{#if showBulkReleaseConfirm}
  <ConfirmDialog
    title="Release Selected Items"
    message="This will release {selectedCount} {itemWord} ephemerally to the network. You retain no copy."
    confirmLabel="Release All"
    onConfirm={confirmBulkRelease}
    onCancel={() => { showBulkReleaseConfirm = false; }}
  />
{/if}

{#if showBulkPublishConfirm}
  <ConfirmDialog
    title="Publish Selected Items"
    message="This will permanently publish {selectedCount} {itemWord} to the network. Published content is network-owned and cannot be retracted."
    confirmLabel="Publish All"
    onConfirm={confirmBulkPublish}
    onCancel={() => { showBulkPublishConfirm = false; }}
  />
{/if}

<style>
  .cleanup-view {
    display: flex;
    flex-direction: column;
    height: 100%;
    overflow: hidden;
  }

  .cleanup-content {
    flex: 1;
    overflow-y: auto;
    padding: 16px;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .cleanup-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  .cleanup-title {
    margin: 0;
    font-size: 1rem;
    color: var(--text-primary, #f2f3f5);
    font-weight: 600;
  }

  .select-all-label {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 0.8rem;
    color: var(--text-secondary, #b5bac1);
    cursor: pointer;
  }

  .select-all-label input[type="checkbox"] {
    width: 16px;
    height: 16px;
    cursor: pointer;
  }

  .bulk-action-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 14px;
    background: color-mix(in srgb, var(--accent, #5865f2) 10%, var(--bg-secondary, #2b2d31));
    border: 1px solid var(--accent, #5865f2);
    border-radius: 8px;
    flex-wrap: wrap;
  }

  .bulk-summary {
    font-size: 0.85rem;
    color: var(--text-primary, #f2f3f5);
    font-weight: 500;
  }

  .bulk-buttons {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }

  .bulk-btn {
    padding: 6px 12px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: var(--bg-tertiary, #232428);
    color: var(--text-secondary, #b5bac1);
    font-size: 0.8rem;
    cursor: pointer;
    font: inherit;
  }

  .bulk-btn:hover {
    background: var(--bg-primary, #313338);
    color: var(--text-primary, #f2f3f5);
  }

  .bulk-btn.burn:hover {
    border-color: #d83c3e;
    color: #d83c3e;
  }

  .bulk-btn.publish:hover {
    border-color: #43b581;
    color: #43b581;
  }

  .recommendations-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .empty-message {
    text-align: center;
    color: var(--text-muted, #949ba4);
    font-size: 0.9rem;
    padding: 32px 16px;
    margin: 0;
  }
</style>
