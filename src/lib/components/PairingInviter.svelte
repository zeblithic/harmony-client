<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { PairingService, extractError, type PairingState } from '../pairing-service';
  import Modal from './Modal.svelte';

  let { hostname = 'this device', onClose } = $props<{ hostname?: string; onClose?: () => void }>();

  const svc = new PairingService();
  let state = $state<PairingState>({ kind: 'idle' });
  let error = $state<string | null>(null);

  svc.onChange = () => { state = svc.state; };

  onMount(async () => {
    try {
      await svc.init();
      state = svc.state;
      // Inviter starts immediately — no name-entry step (uses hostname).
      await svc.startInviter(hostname);
    } catch (e) {
      error = extractError(e);
    }
  });

  onDestroy(() => svc.dispose());

  async function handleSelectPeer(peerSessionId: string) {
    try { await svc.selectPeer(peerSessionId); } catch (e) { error = extractError(e); }
  }
  async function handleConfirm() {
    try { await svc.confirmSas(); } catch (e) { error = extractError(e); }
  }
  async function handleCancel() {
    // Skip the backend cancel IPC in terminal states — there's nothing to
    // cancel and the call would be a wasted invoke (CodeRabbit, PR #68 round 2).
    if (state.kind !== 'complete' && state.kind !== 'failed') {
      try { await svc.cancel(); } catch (e) { /* ignore */ }
    }
    onClose?.();
  }
</script>

<!--
  canCancel intentionally omitted (defaults to true). Pairing's existing
  Cancel button is enabled in every non-terminal state — even during
  active operations like enroll — so Esc mirrors that always-available
  dismissal. The terminal-state IPC skip lives in handleCancel above;
  this Modal stays permissive on Esc.
-->
<Modal
  onCancel={handleCancel}
  ariaLabelledby="invite-heading"
>
  <h3 id="invite-heading">Add another device</h3>

  {#if error}<p class="error" role="alert">{error}</p>{/if}

  {#if state.kind === 'idle' || state.kind === 'discovering'}
    <p>Looking for nearby devices in pairing mode…</p>
    <p class="hint">On the new device, tap "Join existing identity" in the empty Devices panel.</p>
    <div class="modal-actions">
      <button class="secondary" onclick={handleCancel}>Cancel</button>
    </div>
  {:else if state.kind === 'discovered'}
    <p>Devices in pairing mode nearby:</p>
    <ul class="peer-list">
      {#each state.peers as peer (peer.sessionId)}
        <li>
          <button class="peer-row" onclick={() => handleSelectPeer(peer.sessionId)}>
            <strong>{peer.displayName}</strong>
          </button>
        </li>
      {/each}
    </ul>
    <div class="modal-actions">
      <button class="secondary" onclick={handleCancel}>Cancel</button>
    </div>
  {:else if state.kind === 'handshaking'}
    <p>Confirm the codes match on both screens:</p>
    <p class="sas-display">
      {state.sasDigits.slice(0, 3)}&nbsp;{state.sasDigits.slice(3, 6)}
    </p>
    <div class="modal-actions">
      <button class="secondary" onclick={handleCancel}>No, don't match</button>
      <button class="primary" onclick={handleConfirm}>Yes, match</button>
    </div>
  {:else if state.kind === 'waitingPeerConfirm'}
    <p>Waiting for the other device to confirm…</p>
    <div class="modal-actions">
      <button class="secondary" onclick={handleCancel}>Cancel</button>
    </div>
  {:else if state.kind === 'enrolling'}
    <p>Enrolling the new device…</p>
    <div class="modal-actions">
      <button class="secondary" onclick={handleCancel}>Cancel</button>
    </div>
  {:else if state.kind === 'complete'}
    <p>Done! The new device is now part of your owner identity.</p>
    <div class="modal-actions">
      <button class="primary" onclick={onClose}>Close</button>
    </div>
  {:else if state.kind === 'failed'}
    <p class="error" role="alert">Pairing failed: {state.reason}</p>
    <div class="modal-actions">
      <button class="primary" onclick={onClose}>Close</button>
    </div>
  {/if}
</Modal>

<style>
  h3 {
    color: var(--text-primary);
    font-family: var(--font-display);
    font-size: 1.35rem;
    font-weight: 600;
    letter-spacing: -0.01em;
    margin: 0 0 16px;
  }
  p {
    color: var(--text-secondary);
    font-size: 0.9rem;
    line-height: 1.5;
    margin: 0 0 8px;
  }
  .hint { font-size: 0.8rem; color: var(--text-muted); margin: 4px 0 0; }
  .error { color: var(--danger); font-size: 0.85rem; margin: 8px 0; }
  .modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
  .primary, .secondary {
    padding: 8px 16px;
    border-radius: 6px;
    border: 1px solid var(--border-default);
    cursor: pointer;
    font-family: var(--font-ui);
    font-size: 0.875rem;
  }
  .primary { background: var(--accent); color: var(--text-bright); border-color: var(--accent); }
  .secondary { background: var(--bg-tertiary); color: var(--text-secondary); }
  .primary:focus-visible, .secondary:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .peer-list { list-style: none; padding: 0; margin: 8px 0 0; }
  .peer-row {
    display: block;
    width: 100%;
    text-align: left;
    padding: 10px 12px;
    background: var(--surface-raised);
    border: 1px solid var(--primary-border);
    border-radius: 8px;
    margin-bottom: 8px;
    cursor: pointer;
    color: var(--text-primary);
    font-family: var(--font-ui);
    transition: border-color 0.15s ease, background 0.15s ease;
  }
  .peer-row:hover {
    background: color-mix(in srgb, var(--accent) 8%, var(--surface-raised));
    border-color: var(--accent);
  }
  .peer-row:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .sas-display {
    font-family: var(--font-mono);
    font-size: 2rem;
    font-weight: 600;
    text-align: center;
    padding: 20px;
    background: var(--surface-raised);
    border: 1px solid var(--primary-border);
    border-radius: 10px;
    letter-spacing: 0.35em;
    color: var(--accent);
  }
</style>
