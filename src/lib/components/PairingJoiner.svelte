<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { PairingService, extractError, type PairingState } from '../pairing-service';

  let { onClose } = $props<{ onClose?: () => void }>();

  const svc = new PairingService();
  let state = $state<PairingState>({ kind: 'idle' });
  let displayName = $state('');
  let starting = $state(false);
  let error = $state<string | null>(null);

  svc.onChange = () => { state = svc.state; };

  onMount(async () => {
    try { await svc.init(); state = svc.state; } catch (e) { error = extractError(e); }
  });

  onDestroy(() => svc.dispose());

  async function handleStart() {
    if (!displayName.trim()) {
      error = 'Please enter a name for this device.';
      return;
    }
    starting = true;
    error = null;
    try {
      await svc.startJoiner(displayName.trim());
    } catch (e) {
      error = extractError(e);
    } finally {
      starting = false;
    }
  }

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

<div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="join-heading">
  <div class="modal">
    <h3 id="join-heading">Join existing identity</h3>

    {#if state.kind === 'idle'}
      <label>
        Give this device a name
        <input type="text" bind:value={displayName} aria-label="Give this device a name" />
      </label>
      {#if error}<p class="error" role="alert">{error}</p>{/if}
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
        <button class="primary" onclick={handleStart} disabled={starting}>
          {starting ? 'Starting…' : 'Start pairing'}
        </button>
      </div>
    {:else if state.kind === 'discovering'}
      <p>Looking for nearby devices…</p>
      <div class="modal-actions">
        <button class="secondary" onclick={handleCancel}>Cancel</button>
      </div>
    {:else if state.kind === 'discovered'}
      <p>Devices nearby:</p>
      <ul class="peer-list">
        {#each state.peers as peer (peer.sessionId)}
          <li>
            <button class="peer-row" onclick={() => handleSelectPeer(peer.sessionId)}>
              <strong>{peer.displayName}</strong>
              {#if peer.ownerIdIfInviter}
                <span class="owner-id">owner {peer.ownerIdIfInviter.slice(0, 8)}…</span>
              {/if}
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
      <p>Installing your enrollment…</p>
    {:else if state.kind === 'complete'}
      <p>Done! This device is now part of the owner identity.</p>
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
  .primary:disabled, .secondary:disabled { opacity: 0.5; cursor: not-allowed; }
  .error { color: var(--danger); font-size: 13px; margin: 8px 0; }
  .peer-list { list-style: none; padding: 0; margin: 0; }
  .peer-row { display: block; width: 100%; text-align: left; padding: 8px;
    background: var(--bg-primary); border: 1px solid var(--border); border-radius: 4px;
    margin-bottom: 4px; cursor: pointer; }
  .peer-row:hover { background: var(--bg-tertiary); }
  .owner-id { font-family: monospace; font-size: 11px; color: var(--text-muted); margin-left: 8px; }
  .sas-display { font-family: monospace; font-size: 32px; font-weight: 600;
    text-align: center; padding: 16px; background: var(--bg-primary); border-radius: 8px;
    letter-spacing: 4px; }
</style>
