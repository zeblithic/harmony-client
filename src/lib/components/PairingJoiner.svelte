<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { PairingService, extractError, type PairingState } from '../pairing-service';
  import Modal from './Modal.svelte';

  // `onComplete` (optional) fires only when enrollment reaches the terminal
  // `complete` state, distinct from `onClose` (which also fires on cancel /
  // failure). The first-run onboarding gate (ZEB-494) needs that distinction to
  // tell "enrolled → load the new identity" apart from "cancelled → stay on the
  // gate"; `DevicesPanel` omits it and keeps its close-then-refresh behaviour.
  let { onClose, onComplete } = $props<{
    onClose?: () => void;
    onComplete?: () => void;
  }>();

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
    // A completed enrollment is terminal *success*, not a cancel — so Escape
    // must mirror the 'complete' Close button (onComplete ?? onClose). Without
    // this, the first-run onboarding gate's Escape path falls through to
    // onClose and bounces back to the explain pane with the identity installed
    // but unloaded, instead of reloading into the newly joined identity
    // (Qodo + CodeAnt, PR #283).
    if (state.kind === 'complete') {
      (onComplete ?? onClose)?.();
      return;
    }
    // Skip the backend cancel IPC in the other terminal state ('failed') —
    // there's nothing to cancel and the call would be a wasted invoke
    // (CodeRabbit, PR #68 round 2).
    if (state.kind !== 'failed') {
      try { await svc.cancel(); } catch (e) { /* ignore */ }
    }
    onClose?.();
  }
</script>

<!--
  canCancel intentionally omitted (defaults to true). Pairing's existing
  Cancel button is enabled in every non-terminal state — even during
  active operations like enroll/start — so Esc mirrors that always-
  available dismissal. The terminal-state IPC skip lives in handleCancel
  above; this Modal stays permissive on Esc.
-->
<Modal
  onCancel={handleCancel}
  ariaLabelledby="join-heading"
>
  <h3 id="join-heading">Join existing identity</h3>

  {#if state.kind === 'idle'}
    <label>
      Give this device a name
      <!-- maxlength=64: device names are broadcast in plaintext on every
           DISCOVER. The 64KB MAX_PAIRING_WIRE_BYTES backend cap would
           still reject a malicious payload, but a 60KB legitimate name
           would silently bloat every emit until that wire-level rejection. -->
      <input type="text" bind:value={displayName} maxlength={64} />
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
    <div class="modal-actions">
      <button class="secondary" onclick={handleCancel}>Cancel</button>
    </div>
  {:else if state.kind === 'complete'}
    <p>Done! This device is now part of the owner identity.</p>
    <div class="modal-actions">
      <button class="primary" onclick={onComplete ?? onClose}>Close</button>
    </div>
  {:else if state.kind === 'failed'}
    <p class="error" role="alert">Pairing failed: {state.reason}</p>
    <div class="modal-actions">
      <button class="primary" onclick={onClose}>Close</button>
    </div>
  {/if}
</Modal>

<style>
  .modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px; }
  .primary, .secondary { padding: 6px 12px; border-radius: 4px; border: 1px solid var(--border);
    cursor: pointer; font-size: 13px; }
  .primary { background: var(--accent); color: var(--text-bright); border-color: var(--accent); }
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
