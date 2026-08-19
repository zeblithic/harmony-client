<script lang="ts">
  import Modal from './Modal.svelte';
  import type { CommunityService } from '../community-service';
  import { POWER_THRESHOLDS } from '../types';

  let {
    communityId,
    communityService,
    open,
    myPower,
    kickThreshold = POWER_THRESHOLDS.kick,
    onClose,
    onCreated,
  }: {
    communityId: string;
    communityService: CommunityService;
    open: boolean;
    myPower: number;
    /** ZEB-965: the community's customized kick threshold (ZEB-251 governance,
     *  what verify_event enforces since ZEB-733). Defaults to the global const
     *  for callers without a governance snapshot. */
    kickThreshold?: number;
    onClose: () => void;
    onCreated: (channelId: string) => void;
  } = $props();

  let name = $state('');
  // ZEB-349 Text|Voice, ZEB-612 adds Town Hall.
  let kind = $state<'text' | 'voice' | 'townhall'>('text');
  let writePower = $state(0); // v2 always 0; the slider+number pair below is hidden behind `// v3 unhide`
  let submitting = $state(false);
  let error = $state<string | null>(null);
  const titleId = `create-channel-title-${Math.random().toString(36).slice(2)}`;

  let trimmed = $derived(name.trim());
  let canSubmit = $derived(
    trimmed.length > 0 && trimmed.length <= 32 && !submitting,
  );

  // Per spec §7.5 and §10: if local user is demoted below kick threshold
  // mid-action, auto-close. Power gating is the backend's
  // responsibility, but closing the dialog spares the user a
  // surprise rejection on submit. ZEB-965: the threshold is the community's
  // customized value, matching backend enforcement (ZEB-733).
  $effect(() => {
    if (open && myPower < kickThreshold) {
      onClose();
    }
  });

  // ZEB-349: the dialog instance stays mounted in CommunityView (only `open`
  // toggles the inner Modal), so component state survives a close. Reset the
  // form on each open (edge-detected) so a canceled Voice selection — or a
  // stale name — never carries into the next open and creates the wrong kind.
  let wasOpen = false;
  $effect(() => {
    if (open && !wasOpen) {
      name = '';
      kind = 'text';
      writePower = 0;
      error = null;
      submitting = false;
    }
    wasOpen = open;
  });

  async function handleSubmit(e?: Event) {
    e?.preventDefault();
    if (!canSubmit) return;
    submitting = true;
    error = null;
    try {
      const channelId = await communityService.createChannel(communityId, trimmed, writePower, kind);
      onCreated(channelId);
      // Reset for next open; the modal's open=false from the parent is what unmounts.
      name = '';
      kind = 'text';
      writePower = 0;
      onClose();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }
</script>

{#if open}
  <Modal canCancel={!submitting} ariaLabelledby={titleId} onCancel={onClose}>
    <h3 class="dialog-title" id={titleId}>New channel</h3>
    <form onsubmit={handleSubmit}>
      <div class="kind-selector" role="group" aria-label="Channel type">
        <button
          type="button"
          class="kind-option"
          class:selected={kind === 'text'}
          aria-pressed={kind === 'text'}
          disabled={submitting}
          onclick={() => (kind = 'text')}
        >
          <span aria-hidden="true">#</span> Text
        </button>
        <button
          type="button"
          class="kind-option"
          class:selected={kind === 'voice'}
          aria-pressed={kind === 'voice'}
          disabled={submitting}
          onclick={() => (kind = 'voice')}
        >
          <span aria-hidden="true">🔊</span> Voice
        </button>
        <button
          type="button"
          class="kind-option"
          class:selected={kind === 'townhall'}
          aria-pressed={kind === 'townhall'}
          disabled={submitting}
          onclick={() => (kind = 'townhall')}
        >
          <span aria-hidden="true">⚖</span> Town Hall
        </button>
      </div>

      <label for="channel-name-input" class="sr-only">Channel name</label>
      <input
        id="channel-name-input"
        type="text"
        placeholder="Channel name"
        bind:value={name}
        class="name-input"
        disabled={submitting}
        maxlength={32}
        autofocus
      />
      <p class="hint">{trimmed.length}/32 characters</p>

      <!-- v3 unhide: per spec §7.5 + parent spec §12.3 — the
        write_power slider+number-input pair must exist from day one
        per the slider-pairing memory rule, but is hidden in v2 because
        v2 always submits write_power=0. v3 removes the `hidden` attr. -->
      <div class="control-row" hidden>
        <input
          type="range"
          min="0"
          max={POWER_THRESHOLDS.max}
          step="1"
          bind:value={writePower}
          class="slider"
          aria-label="Write-power threshold slider"
        />
        <input
          type="number"
          min="0"
          max={POWER_THRESHOLDS.max}
          step="1"
          bind:value={writePower}
          class="number-input"
          aria-label="Write-power threshold"
        />
      </div>

      {#if error}
        <div class="error-banner">{error}</div>
      {/if}

      <div class="dialog-actions">
        <button type="button" class="cancel-btn" onclick={onClose} disabled={submitting}>Cancel</button>
        <button type="submit" class="confirm-btn" disabled={!canSubmit}>
          {submitting ? 'Creating...' : 'Create'}
        </button>
      </div>
    </form>
  </Modal>
{/if}

<style>
  .dialog-title { color: var(--text-primary); font-size: 1.1rem; margin: 0 0 16px; }
  .sr-only {
    position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
    overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
  }
  .kind-selector {
    display: flex; gap: 0; margin-bottom: 16px;
    border: 1px solid var(--border); border-radius: 4px; overflow: hidden;
  }
  .kind-option {
    flex: 1; padding: 8px 12px; background: var(--bg-tertiary);
    border: none; color: var(--text-secondary); font-size: 0.85rem;
    cursor: pointer;
  }
  .kind-option + .kind-option { border-left: 1px solid var(--border); }
  .kind-option:hover:not(:disabled) { color: var(--text-primary); }
  .kind-option.selected { background: var(--accent); color: var(--on-accent); }
  .kind-option:disabled { opacity: 0.5; cursor: not-allowed; }
  .kind-option:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
  .name-input {
    width: 100%; padding: 8px 12px; background: var(--bg-tertiary);
    border: 1px solid var(--border); border-radius: 4px;
    color: var(--text-primary); font-size: 0.9rem; box-sizing: border-box;
  }
  .name-input:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
  .hint { color: var(--text-secondary); font-size: 0.75rem; margin: 4px 0 16px; }
  /* Keep the HTML `hidden` attribute authoritative: a bare `display` rule
     overrides the UA stylesheet's `[hidden] { display: none }`, so without
     this the `hidden` row would still render. The attribute-selector
     specificity (0,2,0) beats `.control-row` (0,1,0). v3 drops `hidden`. */
  .control-row[hidden] { display: none; }
  .control-row { display: flex; align-items: center; gap: 14px; margin-bottom: 16px; }
  .slider { flex: 1; }
  .number-input {
    width: 64px; background: var(--bg-tertiary); border: 1px solid var(--accent);
    border-radius: 4px; padding: 6px 8px; color: var(--text-primary);
    font-size: 0.9rem; text-align: center; font-family: var(--font-mono);
  }
  .error-banner {
    background: var(--bg-tertiary); border: 1px solid var(--danger-muted); color: var(--danger-muted);
    padding: 8px 10px; border-radius: 4px; font-size: 0.8rem; margin-bottom: 12px;
  }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 8px; }
  .cancel-btn, .confirm-btn {
    border: none; padding: 8px 16px; border-radius: 4px;
    cursor: pointer; font-size: 0.875rem;
  }
  .cancel-btn { background: var(--bg-tertiary); color: var(--text-secondary); }
  .confirm-btn { background: var(--accent); color: var(--on-accent); }
  .confirm-btn:disabled, .cancel-btn:disabled { opacity: 0.4; cursor: not-allowed; }
  .cancel-btn:focus-visible, .confirm-btn:focus-visible {
    outline: 2px solid var(--accent); outline-offset: 1px;
  }
</style>
