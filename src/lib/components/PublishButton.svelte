<script lang="ts">
  import type { ContentSensitivity } from '../types';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import DoubleConfirmDialog from './DoubleConfirmDialog.svelte';
  import TypeToConfirmDialog from './TypeToConfirmDialog.svelte';

  let {
    filename,
    cid,
    sensitivity,
    confirmationOverrides = {},
    onPublish,
    onRelease,
  }: {
    filename: string;
    cid: string;
    sensitivity: ContentSensitivity;
    confirmationOverrides?: Partial<Record<ContentSensitivity, number>>;
    onPublish: (cid: string) => void;
    onRelease: (cid: string) => void;
  } = $props();

  /**
   * Confirmation levels:
   *   2 = double confirm (DoubleConfirmDialog)
   *   3 = triple confirm (DoubleConfirmDialog -> TypeToConfirmDialog)
   *   4 = triple + OOB  (DoubleConfirmDialog -> TypeToConfirmDialog -> ConfirmDialog stub)
   */
  const BASE_LEVELS: Record<ContentSensitivity, number> = {
    public: 2,
    private: 2,
    intimate: 3,
    confidential: 4,
  };

  type FlowMode = 'publish' | 'release';
  type GateStage = 'idle' | 'double' | 'type' | 'oob';

  let flowMode = $state<FlowMode | null>(null);
  let gate = $state<GateStage>('idle');

  let confirmLevel = $derived.by(() => {
    const base = BASE_LEVELS[sensitivity];
    const override = confirmationOverrides[sensitivity];
    if (override !== undefined && override > base) {
      return override;
    }
    return base;
  });

  function startPublish() {
    flowMode = 'publish';
    gate = 'double';
  }

  function startRelease() {
    flowMode = 'release';
    gate = 'double';
  }

  function cancelFlow() {
    flowMode = null;
    gate = 'idle';
  }

  function onDoubleConfirm() {
    if (flowMode === 'release') {
      // Release always completes after double confirm
      onRelease(cid);
      cancelFlow();
      return;
    }

    // Publish flow: check if we need more gates
    if (confirmLevel >= 3) {
      gate = 'type';
    } else {
      onPublish(cid);
      cancelFlow();
    }
  }

  function onTypeConfirm() {
    if (confirmLevel >= 4) {
      gate = 'oob';
    } else {
      onPublish(cid);
      cancelFlow();
    }
  }

  function onOobConfirm() {
    onPublish(cid);
    cancelFlow();
  }
</script>

<div class="publish-button-group">
  <button class="publish-btn" onclick={startPublish} aria-label="Publish (permanent)">
    Publish
  </button>
  <button class="release-btn" onclick={startRelease} aria-label="Release (ephemeral)">
    Release
  </button>
</div>

{#if gate === 'double' && flowMode === 'publish'}
  <DoubleConfirmDialog
    title="Publish Content"
    firstMessage="Publishing makes this content permanently public. Anyone in the world can access, copy, and redistribute it. This cannot be undone."
    secondMessage="You are about to publish {filename}. This is irreversible."
    confirmLabel="Confirm Publish"
    destructive={true}
    onConfirm={onDoubleConfirm}
    onCancel={cancelFlow}
  />
{/if}

{#if gate === 'double' && flowMode === 'release'}
  <DoubleConfirmDialog
    title="Release Content"
    firstMessage="This will make your content publicly available. It may persist on the network or fade over time. You can't take it back, but nobody's obligated to keep it either."
    secondMessage="You are about to release {filename}."
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
    confirmText={filename}
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
  .publish-button-group {
    display: flex;
    gap: 4px;
  }

  .publish-btn,
  .release-btn {
    flex: 1;
    padding: 8px 12px;
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font: inherit;
    font-size: 0.85rem;
  }

  .publish-btn {
    background: var(--accent, #5865f2);
    color: #ffffff;
  }

  .publish-btn:hover {
    background: var(--accent-hover, #4752c4);
  }

  .release-btn {
    background: var(--bg-tertiary, #232428);
    color: var(--text-primary, #f2f3f5);
  }

  .release-btn:hover {
    background: var(--bg-tertiary-hover, #35373c);
  }

  .publish-btn:focus-visible,
  .release-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
