<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { PairingService, extractError, type PairingState } from '../pairing-service';

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
    try { await svc.cancel(); } catch (e) { /* ignore */ }
    onClose?.();
  }
</script>

<div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="invite-heading">
  <div class="modal">
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
  </div>
</div>

<style>
  .modal-overlay { position: fixed; inset: 0; background: rgba(0,0,0,0.5);
    display: flex; align-items: center; justify-content: center; z-index: 1000; }
  .modal { background: var(--bg-secondary); padding: 24px; border-radius: 8px;
    max-width: 480px; border: 1px solid var(--border); }
  .modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
  .primary, .secondary { padding: 6px 12px; border-radius: 4px; border: 1px solid var(--border);
    cursor: pointer; font-size: 13px; }
  .primary { background: var(--accent); color: white; border-color: var(--accent); }
  .secondary { background: var(--bg-primary); color: var(--text-primary); }
  .error { color: var(--danger); font-size: 13px; margin: 8px 0; }
  .hint { font-size: 12px; color: var(--text-muted); margin: 4px 0; }
  .peer-list { list-style: none; padding: 0; margin: 0; }
  .peer-row { display: block; width: 100%; text-align: left; padding: 8px;
    background: var(--bg-primary); border: 1px solid var(--border); border-radius: 4px;
    margin-bottom: 4px; cursor: pointer; }
  .peer-row:hover { background: var(--bg-tertiary); }
  .sas-display { font-family: monospace; font-size: 32px; font-weight: 600;
    text-align: center; padding: 16px; background: var(--bg-primary); border-radius: 8px;
    letter-spacing: 4px; }
</style>
