<script lang="ts">
  import { onMount } from 'svelte';
  import { OwnerService, extractError, type OwnerStateView } from '../owner-service';

  let svc = new OwnerService();
  let state = $state<OwnerStateView | null>(null);
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let modalOpen = $state(false);
  let mintInFlight = $state(false);
  let mintError = $state<string | null>(null);
  let recoveryToken = $state<string | null>(null);

  svc.onChange = () => { state = svc.state; };

  onMount(async () => {
    try {
      await svc.refresh();
    } catch (e) {
      loadError = extractError(e);
    } finally {
      loading = false;
    }
  });

  async function handleConfirmMint() {
    mintInFlight = true;
    mintError = null;
    try {
      const result = await svc.mint();
      recoveryToken = result.recoveryToken;
      modalOpen = false;
    } catch (e) {
      mintError = extractError(e);
    } finally {
      mintInFlight = false;
    }
  }
</script>

<section class="devices-panel" aria-labelledby="devices-heading">
  <h2 id="devices-heading">Devices</h2>

  {#if loading}
    <p class="loading">Loading…</p>
  {:else if loadError}
    <p class="error" role="alert">Failed to load: {loadError}</p>
  {:else if state === null}
    <div class="empty">
      <p class="explainer">
        You haven't created an owner identity yet. Once you do, this device will be
        bound to it, and any other devices you add later will appear here.
      </p>
      <button class="primary" onclick={() => { modalOpen = true; }}>
        Bind this device to a new owner identity →
      </button>
    </div>
  {:else}
    <!-- Populated state added in Task 8 -->
    <div class="populated">
      <h3>My Devices ({state.devices.length})</h3>
    </div>
  {/if}

  {#if modalOpen}
    <div class="modal-overlay" role="dialog" aria-modal="true" aria-labelledby="modal-heading">
      <div class="modal">
        <h3 id="modal-heading">Create your owner identity</h3>
        <p>
          This will create your owner identity. This device will be bound as the first device.
          You'll receive a recovery file to back up — you can do this immediately or later.
        </p>
        {#if mintError}
          <p class="error" role="alert">{mintError}</p>
        {/if}
        <div class="modal-actions">
          <button class="secondary" onclick={() => { modalOpen = false; }} disabled={mintInFlight}>
            Cancel
          </button>
          <button class="primary" onclick={handleConfirmMint} disabled={mintInFlight}>
            {mintInFlight ? 'Creating…' : 'Create owner identity'}
          </button>
        </div>
      </div>
    </div>
  {/if}
</section>

<style>
  .devices-panel {
    padding: 16px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 6px;
    margin-bottom: 16px;
  }
  .devices-panel h2 {
    margin: 0 0 12px;
    font-size: 14px;
    color: var(--text-primary);
  }
  .empty .explainer {
    color: var(--text-secondary);
    font-size: 13px;
    margin-bottom: 12px;
  }
  .primary, .secondary {
    padding: 6px 12px;
    border-radius: 4px;
    border: 1px solid var(--border);
    cursor: pointer;
    font-size: 13px;
  }
  .primary {
    background: var(--accent);
    color: white;
    border-color: var(--accent);
  }
  .secondary {
    background: var(--bg-primary);
    color: var(--text-primary);
  }
  .primary:disabled, .secondary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .modal-overlay {
    position: fixed;
    inset: 0;
    background: rgba(0,0,0,0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal {
    background: var(--bg-secondary);
    padding: 24px;
    border-radius: 8px;
    max-width: 480px;
    border: 1px solid var(--border);
  }
  .modal-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .error {
    color: var(--danger);
    font-size: 13px;
    margin: 8px 0;
  }
  .loading {
    color: var(--text-muted);
    font-size: 13px;
  }
</style>
