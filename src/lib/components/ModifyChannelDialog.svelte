<script lang="ts">
  import Modal from './Modal.svelte';
  import type { CommunityService, ChannelInfo } from '../community-service';
  import { POWER_THRESHOLDS } from '../types';

  let {
    communityId,
    channel,
    communityService,
    open,
    myPower,
    kickThreshold = POWER_THRESHOLDS.kick,
    onClose,
  }: {
    communityId: string;
    channel: ChannelInfo;
    communityService: CommunityService;
    open: boolean;
    myPower: number;
    /** ZEB-965: the community's customized kick threshold (ZEB-251 governance,
     *  what verify_event enforces since ZEB-733). Defaults to the global const
     *  for callers without a governance snapshot. */
    kickThreshold?: number;
    onClose: () => void;
  } = $props();

  let name = $state(channel.name);
  let writePower = $state(channel.writePower);
  let submitting = $state(false);
  let error = $state<string | null>(null);
  const titleId = `modify-channel-title-${Math.random().toString(36).slice(2)}`;

  // When the parent opens this dialog for a different channel, refresh the
  // pre-filled values. (Without this, the second open would still show the
  // first channel's name — Svelte won't re-init $state on prop change.)
  $effect(() => {
    if (open) {
      name = channel.name;
      writePower = channel.writePower;
      error = null;
    }
  });

  let trimmed = $derived(name.trim());
  let nameChanged = $derived(trimmed !== channel.name);
  let writePowerChanged = $derived(writePower !== channel.writePower);
  let nameValid = $derived(trimmed.length > 0 && trimmed.length <= 32);
  let canSubmit = $derived(
    !submitting &&
      (nameChanged || writePowerChanged) &&
      // If name didn't change, validity doesn't apply to it; but if it did
      // change, it must be valid.
      (!nameChanged || nameValid),
  );

  // Per spec §7.6 + §10: auto-close on demotion below kick threshold.
  // ZEB-965: the threshold is the community's customized value, matching
  // backend enforcement (ZEB-733).
  $effect(() => {
    if (open && myPower < kickThreshold) {
      onClose();
    }
  });

  async function handleSubmit(e?: Event) {
    e?.preventDefault();
    if (!canSubmit) return;
    submitting = true;
    error = null;
    try {
      const submitName = nameChanged ? trimmed : undefined;
      const submitWritePower = writePowerChanged ? writePower : undefined;
      // Defense-in-depth: backend rejects all-None too, but never reaching
      // the IPC saves a round-trip.
      if (submitName === undefined && submitWritePower === undefined) {
        return;
      }
      await communityService.modifyChannel(
        communityId,
        channel.channelId,
        submitName,
        submitWritePower,
      );
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
    <h3 class="dialog-title" id={titleId}>Modify #{channel.name}</h3>
    <form onsubmit={handleSubmit}>
      <label for="modify-channel-name-input" class="sr-only">Channel name</label>
      <input
        id="modify-channel-name-input"
        type="text"
        placeholder="Channel name"
        bind:value={name}
        class="name-input"
        disabled={submitting}
        maxlength={32}
        autofocus
      />
      <p class="hint">{trimmed.length}/32 characters</p>

      <!-- v3 unhide: same as CreateChannelDialog. v2 keeps writePower
        immutable through the UI (always equals channel.writePower).
        v3 removes the `hidden` attr. -->
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
          {submitting ? 'Saving...' : 'Save'}
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
  .name-input {
    width: 100%; padding: 8px 12px; background: var(--bg-tertiary);
    border: 1px solid var(--border); border-radius: 4px;
    color: var(--text-primary); font-size: 0.9rem; box-sizing: border-box;
  }
  .name-input:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
  .hint { color: var(--text-secondary); font-size: 0.75rem; margin: 4px 0 16px; }
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
