<script lang="ts">
  import type { CleanupRecommendation } from '../types';
  import { categoryIcon, formatBytes } from '../file-utils';
  import ConfirmDialog from './ConfirmDialog.svelte';

  type CleanupAction = 'burn' | 'archive' | 'release' | 'publish' | 'pin';

  let {
    recommendation,
    checked,
    onAction,
    onToggle,
  }: {
    recommendation: CleanupRecommendation;
    checked: boolean;
    onAction: (cid: string, action: CleanupAction) => void;
    onToggle: (cid: string) => void;
  } = $props();

  let icon = $derived(categoryIcon(recommendation.category));
  let size = $derived(formatBytes(recommendation.sizeBytes));
  let recoverable = $derived(formatBytes(recommendation.spaceRecoverable));
  let stalenessPercent = $derived(Math.round(recommendation.stalenessScore * 100));

  const REASON_HINTS: Record<string, string> = {
    stale: 'Publish to preserve it forever, release to free quota, or burn if disposable.',
    'over-replicated': 'Lower the replication tier in the detail panel to reclaim space without losing the file.',
    'duplicate-of-public': 'A public copy already exists on the network — burn this private copy to recover the quota.',
    expired: 'This content has passed its useful life. Burn it to free quota.',
  };

  let suggestion = $derived(
    `This costs you ${size} across your devices. ${REASON_HINTS[recommendation.reason] ?? 'Review this item and take the appropriate action.'}`
  );

  let showBurnConfirm = $state(false);
  let showPublishConfirm = $state(false);
  let showReleaseConfirm = $state(false);

  function handleAction(action: CleanupAction) {
    if (action === 'burn') {
      showBurnConfirm = true;
      return;
    }
    if (action === 'publish') {
      showPublishConfirm = true;
      return;
    }
    if (action === 'release') {
      showReleaseConfirm = true;
      return;
    }
    onAction(recommendation.cid, action);
  }

  function confirmBurn() {
    showBurnConfirm = false;
    onAction(recommendation.cid, 'burn');
  }

  function confirmPublish() {
    showPublishConfirm = false;
    onAction(recommendation.cid, 'publish');
  }

  function confirmRelease() {
    showReleaseConfirm = false;
    onAction(recommendation.cid, 'release');
  }
</script>

<article class="recommendation-card" class:checked>
  <div class="card-header">
    <input
      type="checkbox"
      checked={checked}
      onchange={() => onToggle(recommendation.cid)}
      aria-label="Select {recommendation.name}"
    />
    <span class="card-icon" aria-hidden="true">{icon}</span>
    <span class="card-name">{recommendation.name}</span>
    <span class="card-size">{size}</span>
  </div>

  <div class="card-meta">
    <span class="reason-badge {recommendation.reason}">{recommendation.reason}</span>
    <span class="recoverable">{recoverable} recoverable</span>
  </div>

  <div class="staleness-bar-track" aria-label="Staleness: {stalenessPercent}%">
    <div class="staleness-bar-fill" style:width="{stalenessPercent}%"></div>
  </div>

  <p class="suggestion">
    {suggestion}
  </p>

  <div class="card-actions">
    <button class="action-btn burn" onclick={() => handleAction('burn')} aria-label="Burn {recommendation.name}">Burn</button>
    <button class="action-btn archive" onclick={() => handleAction('archive')} aria-label="Archive {recommendation.name}">Archive</button>
    <button class="action-btn release" onclick={() => handleAction('release')} aria-label="Release {recommendation.name}">Release</button>
    <button class="action-btn publish" onclick={() => handleAction('publish')} aria-label="Publish {recommendation.name}">Publish</button>
    <button class="action-btn pin" onclick={() => handleAction('pin')} aria-label="Pin {recommendation.name}">Pin</button>
  </div>
</article>

{#if showBurnConfirm}
  <ConfirmDialog
    title="Burn Content"
    message='This will permanently delete "{recommendation.name}" and free {recoverable} of storage quota.'
    confirmLabel="Burn"
    destructive={true}
    onConfirm={confirmBurn}
    onCancel={() => { showBurnConfirm = false; }}
  />
{/if}

{#if showPublishConfirm}
  <ConfirmDialog
    title="Publish Content"
    message='Publishing "{recommendation.name}" makes it permanently public. Anyone can access, copy, and redistribute it. This cannot be undone.'
    confirmLabel="Publish"
    destructive={true}
    onConfirm={confirmPublish}
    onCancel={() => { showPublishConfirm = false; }}
  />
{/if}

{#if showReleaseConfirm}
  <ConfirmDialog
    title="Release Content"
    message='Releasing "{recommendation.name}" will make it publicly available. You retain no copy and it may persist or fade from the network.'
    confirmLabel="Release"
    onConfirm={confirmRelease}
    onCancel={() => { showReleaseConfirm = false; }}
  />
{/if}

<style>
  .recommendation-card {
    background: var(--bg-secondary, #2b2d31);
    border: 1px solid var(--border, #3f4147);
    border-radius: 8px;
    padding: 12px 16px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .recommendation-card.checked {
    border-color: var(--accent, #5865f2);
    background: color-mix(in srgb, var(--accent, #5865f2) 5%, var(--bg-secondary, #2b2d31));
  }

  .card-header {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .card-header input[type="checkbox"] {
    flex-shrink: 0;
    width: 16px;
    height: 16px;
    cursor: pointer;
  }

  .card-icon {
    flex-shrink: 0;
    font-size: 1.1rem;
  }

  .card-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-primary, #f2f3f5);
    font-weight: 500;
  }

  .card-size {
    flex-shrink: 0;
    font-size: 0.8rem;
    color: var(--text-muted, #949ba4);
  }

  .card-meta {
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: 0.8rem;
  }

  .reason-badge {
    display: inline-block;
    padding: 2px 8px;
    border-radius: 4px;
    font-size: 0.75rem;
    font-weight: 500;
  }

  .reason-badge.stale {
    color: #e67e22;
    background: rgba(230, 126, 34, 0.12);
  }

  .reason-badge.duplicate-of-public {
    color: #3498db;
    background: rgba(52, 152, 219, 0.12);
  }

  .reason-badge.over-replicated {
    color: #9b59b6;
    background: rgba(155, 89, 182, 0.12);
  }

  .reason-badge.expired {
    color: #d83c3e;
    background: rgba(216, 60, 62, 0.12);
  }

  .recoverable {
    color: var(--text-muted, #949ba4);
  }

  .staleness-bar-track {
    height: 4px;
    background: var(--bg-tertiary, #232428);
    border-radius: 2px;
    overflow: hidden;
  }

  .staleness-bar-fill {
    height: 100%;
    background: #e67e22;
    border-radius: 2px;
    transition: width 0.3s;
  }

  .suggestion {
    margin: 0;
    font-size: 0.78rem;
    color: var(--text-muted, #949ba4);
    line-height: 1.4;
    font-style: italic;
  }

  .card-actions {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }

  .action-btn {
    padding: 4px 10px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: var(--bg-tertiary, #232428);
    color: var(--text-secondary, #b5bac1);
    font-size: 0.78rem;
    cursor: pointer;
    font: inherit;
  }

  .action-btn:hover {
    background: var(--bg-primary, #313338);
    color: var(--text-primary, #f2f3f5);
  }

  .action-btn.burn:hover {
    border-color: #d83c3e;
    color: #d83c3e;
  }

  .action-btn.publish:hover {
    border-color: #43b581;
    color: #43b581;
  }

  .action-btn.pin:hover {
    border-color: #f1c40f;
    color: #f1c40f;
  }
</style>
