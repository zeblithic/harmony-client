<script lang="ts">
  import type { CleanupRecommendation, ContentSensitivity } from '../types';
  import { categoryIcon, formatBytes } from '../file-utils';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import DoubleConfirmDialog from './DoubleConfirmDialog.svelte';
  import TypeToConfirmDialog from './TypeToConfirmDialog.svelte';

  type CleanupAction = 'burn' | 'archive' | 'release' | 'publish' | 'pin';

  let {
    recommendation,
    checked,
    onAction,
    onToggle,
  }: {
    recommendation: CleanupRecommendation;
    checked: boolean;
    onAction: (rec: CleanupRecommendation, action: CleanupAction) => void;
    onToggle: (sidecarId: string) => void;
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

  // Graduated confirmation gates — matching PublishButton's system
  const BASE_LEVELS: Record<ContentSensitivity, number> = {
    public: 2, private: 2, intimate: 3, confidential: 4,
  };

  type GateStage = 'idle' | 'double' | 'type' | 'oob';
  type FlowMode = 'publish' | 'release';

  let showBurnConfirm = $state(false);
  let flowMode = $state<FlowMode | null>(null);
  let gate = $state<GateStage>('idle');

  let confirmLevel = $derived(BASE_LEVELS[recommendation.sensitivity] ?? 2);

  function handleAction(action: CleanupAction) {
    if (action === 'burn') {
      showBurnConfirm = true;
      return;
    }
    if (action === 'publish') {
      flowMode = 'publish';
      gate = 'double';
      return;
    }
    if (action === 'release') {
      flowMode = 'release';
      gate = 'double';
      return;
    }
    onAction(recommendation, action);
  }

  function cancelFlow() {
    flowMode = null;
    gate = 'idle';
  }

  function onDoubleConfirm() {
    if (flowMode === 'release') {
      // Release completes after double confirm regardless of sensitivity.
      // Release is ephemeral — lower stakes than durable publish.
      onAction(recommendation, 'release');
      cancelFlow();
      return;
    }
    if (confirmLevel >= 3) {
      gate = 'type';
    } else {
      onAction(recommendation, 'publish');
      cancelFlow();
    }
  }

  function onTypeConfirm() {
    if (confirmLevel >= 4) {
      gate = 'oob';
    } else {
      onAction(recommendation, 'publish');
      cancelFlow();
    }
  }

  function onOobConfirm() {
    onAction(recommendation, 'publish');
    cancelFlow();
  }

  function confirmBurn() {
    showBurnConfirm = false;
    onAction(recommendation, 'burn');
  }
</script>

<article class="recommendation-card" class:checked>
  <div class="card-header">
    <input
      type="checkbox"
      checked={checked}
      onchange={() => onToggle(recommendation.sidecarId)}
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

  <div class="staleness-bar-track" role="progressbar" aria-label="Staleness" aria-valuenow={stalenessPercent} aria-valuemin={0} aria-valuemax={100}>
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

{#if gate === 'double' && flowMode === 'publish'}
  <DoubleConfirmDialog
    title="Publish Content"
    firstMessage='Publishing makes this content permanently public. Anyone in the world can access, copy, and redistribute it. This cannot be undone.'
    secondMessage='You are about to publish "{recommendation.name}". This is irreversible.'
    confirmLabel="Confirm Publish"
    destructive={true}
    onConfirm={onDoubleConfirm}
    onCancel={cancelFlow}
  />
{/if}

{#if gate === 'double' && flowMode === 'release'}
  <DoubleConfirmDialog
    title="Release Content"
    firstMessage='This will make your content publicly available. It may persist on the network or fade over time. You can&apos;t take it back, but nobody&apos;s obligated to keep it either.'
    secondMessage='You are about to release "{recommendation.name}".'
    confirmLabel="Confirm Release"
    destructive={false}
    onConfirm={onDoubleConfirm}
    onCancel={cancelFlow}
  />
{/if}

{#if gate === 'type' && flowMode === 'publish'}
  <TypeToConfirmDialog
    title="Confirm Publish"
    message="This action is irreversible. Type the filename to confirm."
    confirmText={recommendation.name}
    confirmLabel="Confirm Publish"
    destructive={true}
    onConfirm={onTypeConfirm}
    onCancel={cancelFlow}
  />
{/if}

{#if gate === 'oob' && flowMode === 'publish'}
  <ConfirmDialog
    title="Out-of-Band Verification"
    message="In a future version, verify on another device. For now, confirm here."
    confirmLabel="Confirm"
    destructive={true}
    onConfirm={onOobConfirm}
    onCancel={cancelFlow}
  />
{/if}

<style>
  .recommendation-card {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 12px 16px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .recommendation-card.checked {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 5%, var(--bg-secondary));
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
    color: var(--text-primary);
    font-weight: 500;
  }

  .card-size {
    flex-shrink: 0;
    font-size: 0.8rem;
    color: var(--text-muted);
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
    color: var(--text-muted);
  }

  .staleness-bar-track {
    height: 4px;
    background: var(--bg-tertiary);
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
    color: var(--text-muted);
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
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    font-size: 0.78rem;
    cursor: pointer;
    font: inherit;
  }

  .action-btn:hover {
    background: var(--bg-primary);
    color: var(--text-primary);
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
