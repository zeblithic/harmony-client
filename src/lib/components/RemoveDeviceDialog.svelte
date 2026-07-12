<script lang="ts">
  import Modal from './Modal.svelte';
  import type { RevokeReason } from '../owner-service';

  let {
    deviceName,
    isSelf,
    isSeedHolder,
    busy,
    error,
    onConfirm,
    onCancel,
    mode = 'remove',
  }: {
    deviceName: string;
    isSelf: boolean;
    isSeedHolder: boolean;
    busy: boolean;
    error: string | null;
    onConfirm: (reason: RevokeReason) => void;
    onCancel: () => void;
    // ZEB-668 S6: 'replace' = remove-then-re-pair flow. The reason is
    // locked to 'decommissioned' (the radios are hidden) and the copy
    // states removal-first semantics; same typed-confirm tier — the
    // removal itself is identical to a plain remove.
    mode?: 'remove' | 'replace';
  } = $props();

  let typed = $state('');
  let reason = $state<RevokeReason>('decommissioned');
  let matches = $derived(typed === deviceName);
  const titleId = `dialog-title-${Math.random().toString(36).slice(2)}`;

  const reasons: { value: RevokeReason; label: string; hint: string }[] = [
    {
      value: 'decommissioned',
      label: 'Decommissioned',
      hint: 'Retiring this device deliberately',
    },
    { value: 'lost', label: 'Lost', hint: "I can't find this device" },
    {
      value: 'compromised',
      label: 'Compromised',
      hint: 'Someone else may control this device',
    },
  ];

  const friendlyError = $derived.by(() => {
    if (!error) return null;
    if (error.startsWith('notMaster:'))
      return 'Only the device holding your master key can remove other devices.';
    if (error.startsWith('lastDevice:')) return "You can't remove your only active device.";
    return error;
  });
</script>

<Modal
  onCancel={() => {
    if (!busy) onCancel();
  }}
  ariaLabelledby={titleId}
>
  <h2 class="dialog-title" id={titleId}>
    {#if mode === 'replace'}
      Replace {deviceName}?
    {:else}
      {isSelf ? 'Remove this device?' : `Remove ${deviceName}?`}
    {/if}
  </h2>
  {#if mode === 'replace'}
    <p class="dialog-message">
      {deviceName} is removed first (recorded as decommissioned), then pairing opens to enroll
      its replacement. The removal takes effect immediately — even if you cancel pairing.
    </p>
    <p class="dialog-message">
      Your owner identity and master key are unchanged — the new device joins under the same
      identity, and this device's name carries over.
    </p>
  {:else}
    <p class="dialog-message">
      Removing this device cuts it off from posting in your communities, syncing with your other
      devices, and message deposits and relay.
    </p>
  {/if}
  <p class="dialog-message honesty">
    Its direct messages and vine feeds are separate surfaces and are not blocked yet — those
    cutoffs land in follow-up work.
  </p>
  {#if isSelf && isSeedHolder}
    <p class="dialog-message warning">
      This device holds your master key. Afterward you will need your recovery phrase to manage
      devices.
    </p>
  {/if}
  {#if isSelf}
    <p class="dialog-message honesty">Your local data on this device is not deleted.</p>
  {/if}

  {#if mode === 'remove'}
    <fieldset class="reason-group">
      <legend class="reason-legend">Reason</legend>
      {#each reasons as r (r.value)}
        <label class="reason-option">
          <input type="radio" name="revoke-reason" value={r.value} bind:group={reason} />
          <span class="reason-label">{r.label}</span>
          <span class="reason-hint">— {r.hint}</span>
        </label>
      {/each}
    </fieldset>
  {/if}

  <p class="dialog-hint">Type <code>{deviceName}</code> to confirm</p>
  <input class="dialog-input" type="text" aria-label="Type to confirm" bind:value={typed} />

  {#if friendlyError}
    <p class="dialog-error" role="alert">{friendlyError}</p>
  {/if}

  <div class="dialog-actions">
    <!-- Cancel is gated on busy too: closing mid-flight resets removeTarget
         while the shared in-flight flag still blocks the next confirm — the
         user's next attempt would be silently dropped (CodeRabbit PR #452). -->
    <button
      class="cancel-btn"
      disabled={busy}
      onclick={() => {
        if (!busy) onCancel();
      }}
    >
      Cancel
    </button>
    <button
      class="confirm-btn destructive"
      disabled={!matches || busy}
      onclick={() => onConfirm(mode === 'replace' ? 'decommissioned' : reason)}
    >
      {mode === 'replace' ? 'Remove & continue' : 'Remove device'}
    </button>
  </div>
</Modal>

<style>
  .dialog-title {
    color: var(--text-primary);
    font-family: var(--font-display);
    font-size: 1.1rem;
    margin: 0 0 12px;
  }

  .dialog-message {
    color: var(--text-secondary);
    font-size: 0.9rem;
    line-height: 1.5;
    margin: 0 0 8px;
  }

  .dialog-message.honesty {
    font-size: 0.85rem;
    color: var(--text-tertiary, var(--text-secondary));
  }

  .dialog-message.warning {
    color: var(--danger);
  }

  .reason-group {
    border: 1px solid var(--border);
    border-radius: 7px;
    padding: 10px 12px;
    margin: 0 0 14px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .reason-legend {
    color: var(--text-secondary);
    font-size: 0.8rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    padding: 0 4px;
  }

  .reason-option {
    display: flex;
    align-items: baseline;
    gap: 8px;
    cursor: pointer;
    font-size: 0.9rem;
  }

  .reason-label {
    color: var(--text-primary);
  }

  .reason-hint {
    color: var(--text-secondary);
    font-size: 0.85rem;
  }

  .dialog-hint {
    color: var(--text-secondary);
    font-size: 0.85rem;
    margin: 0 0 8px;
  }

  .dialog-hint code {
    background: var(--bg-tertiary);
    padding: 2px 6px;
    border-radius: 3px;
    font-family: var(--font-mono);
    color: var(--text-primary);
  }

  .dialog-input {
    width: 100%;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 5px;
    color: var(--text-primary);
    font-size: 0.9rem;
    margin-bottom: 16px;
    box-sizing: border-box;
  }

  .dialog-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }

  .dialog-error {
    color: var(--danger);
    font-size: 0.85rem;
    margin: 0 0 12px;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .cancel-btn {
    background: var(--surface-raised);
    color: var(--text-secondary);
    border: 1px solid var(--border);
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn {
    border: none;
    padding: 8px 16px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .confirm-btn.destructive {
    background: var(--danger-muted);
    color: var(--text-primary);
  }

  .confirm-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
