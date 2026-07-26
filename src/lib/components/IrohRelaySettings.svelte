<script lang="ts">
  /**
   * ZEB-624: iroh transport relay configuration editor.
   *
   * Mirrors the pkarr relay manager in NetworkDiscoverabilitySettings.svelte,
   * but for the iroh transport relay map. Two differences from pkarr:
   *   1. The iroh wire carries no per-relay health — a row is just the URL + a
   *      Remove button (no health badge).
   *   2. Every verb returns an `IrohRelayInfo { relays, custom }`. `custom`
   *      distinguishes "following iroh's recommended defaults" (false) from a
   *      user-materialized custom list (true); it drives the state note.
   *
   * Server-authoritative read-modify-write: Add/Remove/Restore send only the
   * delta (or nothing) and apply the returned authoritative list directly, so a
   * stale client view can never clobber a fresher config and there is no
   * follow-up refetch that could fail and strand a stale list. The backend emits
   * `iroh-relays-changed` after every successful mutation; the subscription
   * below re-fetches so out-of-band changes (another window/IPC caller) stay in
   * sync.
   *
   * Svelte 5 runes (`$state`), consistent with NetworkDiscoverabilitySettings.
   */
  import { onMount, onDestroy } from 'svelte';
  import {
    getIrohRelays,
    addIrohRelay,
    removeIrohRelay,
    resetIrohRelays,
  } from '../connectivity-adapter';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import type { IrohRelayInfo } from '../types/network-health';

  // Effective relay list + whether it's a materialized custom list.
  let relays = $state<string[]>([]);
  let custom = $state(false);
  let newRelayUrl = $state('');
  let relayError = $state<string | null>(null);
  let relayPending = $state(false);
  // True once the initial get_iroh_relays fetch succeeds. Guards Add so it can
  // never submit before the base config is known (defense-in-depth; the input
  // and button are also disabled until then).
  let relayLoaded = $state(false);
  // Monotonic token to drop stale in-flight get_iroh_relays responses: if a
  // newer fetch starts before an older one resolves (e.g. a mutation-triggered
  // refetch racing an iroh-relays-changed refetch), only the newest result is
  // applied.
  let relayFetchSeq = 0;
  // Unlisten for iroh-relays-changed event.
  let relaysUnlisten: UnlistenFn | null = null;
  let relaysListenerDestroyed = false;

  async function fetchRelays(): Promise<void> {
    const seq = ++relayFetchSeq;
    try {
      const info = await getIrohRelays();
      // Drop the result if a newer fetch superseded this one in flight.
      if (seq !== relayFetchSeq) return;
      relays = info.relays ?? [];
      custom = info.custom;
      relayError = null;
      relayLoaded = true;
    } catch (e) {
      // Stale failures must not clobber a newer fetch's state either.
      if (seq !== relayFetchSeq) return;
      // fetchRelays is a REFRESH (onMount + the iroh-relays-changed listener).
      // Once a config is loaded — including one a mutation just applied
      // authoritatively — a failed refresh must NOT surface an error over a
      // config that is still current. Only an INITIAL load failure is fatal.
      if (!relayLoaded) {
        relayError = e instanceof Error ? e.message : String(e);
      }
    }
  }

  // Apply an authoritative IrohRelayInfo returned by a mutation (add/remove/
  // reset). Unlike fetchRelays this can't fail — the config is already in hand —
  // so it closes the "mutation succeeded but the follow-up refetch failed" gap.
  // Claims the latest fetch token so an in-flight refetch can't later overwrite
  // it with a stale read.
  function applyAuthoritative(info: IrohRelayInfo): void {
    relayFetchSeq++;
    relays = info.relays ?? [];
    custom = info.custom;
    relayError = null;
    relayLoaded = true;
  }

  async function handleAddRelay(): Promise<void> {
    const trimmed = newRelayUrl.trim();
    // Guard: never submit an Add before the initial fetch succeeds.
    if (!relayLoaded) return;
    if (!trimmed || relayPending) return;
    relayPending = true;
    relayError = null;
    try {
      // Server-authoritative read-modify-write: send only the new URL. Returns
      // the new authoritative config, applied directly — no refetch.
      const info = await addIrohRelay(trimmed);
      newRelayUrl = '';
      applyAuthoritative(info);
    } catch (e) {
      relayError = e instanceof Error ? e.message : String(e);
    } finally {
      relayPending = false;
    }
  }

  async function handleRemoveRelay(url: string): Promise<void> {
    if (relayPending) return;
    relayPending = true;
    relayError = null;
    try {
      // Server-authoritative read-modify-write: send only the URL to remove.
      // Removing the last custom relay is rejected server-side (the error tells
      // the user to reset); that message surfaces in the alert region.
      const info = await removeIrohRelay(url);
      applyAuthoritative(info);
    } catch (e) {
      relayError = e instanceof Error ? e.message : String(e);
    } finally {
      relayPending = false;
    }
  }

  async function handleRestoreRecommended(): Promise<void> {
    // Guard: never submit a Restore before the initial fetch succeeds (matches
    // Add's `!relayLoaded` gate; defense-in-depth — the button is also disabled
    // until then). A reset before the base config is known could clobber a
    // config the client hasn't observed yet.
    if (!relayLoaded) return;
    if (relayPending) return;
    relayPending = true;
    relayError = null;
    try {
      // Server-authoritative reset returns the recommended defaults, applied
      // directly — no refetch that could fail.
      const info = await resetIrohRelays();
      applyAuthoritative(info);
    } catch (e) {
      relayError = e instanceof Error ? e.message : String(e);
    } finally {
      relayPending = false;
    }
  }

  onMount(async () => {
    // Load the current config, then subscribe to live changes.
    await fetchRelays();
    // Race-safe: if destroyed before listen() resolves, immediately call the
    // returned unlisten. Wrap in try/catch so a failed Tauri event subscription
    // (e.g. during tests or early teardown) doesn't produce an unhandled
    // rejection.
    try {
      const resolved = await listen<null>('iroh-relays-changed', () => {
        void fetchRelays();
      });
      if (relaysListenerDestroyed) {
        resolved();
      } else {
        relaysUnlisten = resolved;
      }
    } catch (e) {
      console.error('IrohRelaySettings: failed to subscribe to relay changes', e);
    }
  });

  onDestroy(() => {
    relaysListenerDestroyed = true;
    relaysUnlisten?.();
  });
</script>

<div class="discoverability-section" data-testid="iroh-relay-manager">
  <div class="section-header">
    <h4 class="section-title">Transport relays (iroh)</h4>
  </div>
  <p class="toggle-hint">
    Relays carry traffic when a direct connection isn't possible. Leave on the recommended set
    unless you run your own relay.
  </p>

  {#if relayError}
    <p class="error-text" role="alert" data-testid="iroh-relay-error">{relayError}</p>
  {/if}

  <p class="relay-state-note" data-testid="iroh-relay-state-note">
    {custom ? 'Custom relay set' : 'Using recommended relays'}
  </p>

  <ul class="relay-list" data-testid="iroh-relay-list">
    {#each relays as url (url)}
      <li class="relay-row" data-testid="iroh-relay-row">
        <code class="relay-url" data-testid="iroh-relay-url">{url}</code>
        <button
          class="relay-remove"
          data-testid="iroh-relay-remove"
          disabled={!relayLoaded || relayPending}
          onclick={() => handleRemoveRelay(url)}
          aria-label={`Remove relay ${url}`}
        >
          Remove
        </button>
      </li>
    {/each}
  </ul>

  <div class="relay-add-row" data-testid="iroh-relay-add-row">
    <input
      type="url"
      class="relay-input"
      placeholder="https://relay.example.com"
      bind:value={newRelayUrl}
      disabled={!relayLoaded || relayPending}
      data-testid="iroh-relay-url-input"
      aria-label="New iroh relay URL"
    />
    <button
      class="relay-add-btn"
      disabled={!relayLoaded || !newRelayUrl.trim() || relayPending}
      onclick={handleAddRelay}
      data-testid="iroh-relay-add-button"
    >
      Add
    </button>
  </div>

  <button
    class="relay-restore-btn"
    onclick={handleRestoreRecommended}
    disabled={!relayLoaded || relayPending}
    data-testid="iroh-relay-restore-button"
  >
    Restore recommended
  </button>
</div>

<style>
  .discoverability-section {
    padding: 12px 0;
  }

  .section-header {
    margin-bottom: 8px;
  }

  .section-title {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .toggle-hint {
    font-size: 11px;
    color: var(--text-muted);
    line-height: 1.4;
  }

  .relay-state-note {
    font-size: 11px;
    color: var(--text-secondary);
    margin: 4px 0 8px;
  }

  .error-text {
    font-size: 12px;
    color: var(--danger-muted);
    margin: 4px 0 8px;
  }

  .relay-list {
    list-style: none;
    padding: 0;
    margin: 8px 0;
  }

  .relay-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 0;
    flex-wrap: wrap;
  }

  .relay-url {
    font-size: 11px;
    flex: 1;
    word-break: break-all;
  }

  /* ZEB-773: `.relay-remove` / `.relay-add-btn` / `.relay-restore-btn` are
     styled globally in src/app.css — the rules were duplicated verbatim here
     and in NetworkDiscoverabilitySettings.svelte, and had already drifted. */

  .relay-add-row {
    display: flex;
    gap: 6px;
    margin-top: 8px;
  }

  .relay-input {
    flex: 1;
    font-size: 12px;
    padding: 3px 6px;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border: 1px solid var(--border);
    /* ZEB-773: was 3px here and 5px in NetworkDiscoverabilitySettings — the
       duplicated block had already drifted. Normalised to the 4px used by
       every other control in the app. */
    border-radius: 4px;
  }
</style>
