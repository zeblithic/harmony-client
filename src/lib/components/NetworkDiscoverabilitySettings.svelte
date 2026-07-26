<script lang="ts">
  /**
   * ZEB-323 Phase 2b: Network Discoverability (case B) toggle.
   *
   * Renders a single opt-in toggle that controls whether this device
   * publishes its iroh routing to the pkarr DHT under its identity public
   * key. When enabled, anyone who has the user's identity address can reach
   * this device cross-WAN without a shared community or pending invite.
   *
   * Default is OFF — matches the backend `ConnectivitySettings` default.
   *
   * Uses Svelte 5 runes (`$state`, `$effect`) consistent with other
   * IPC-driven components in this codebase (e.g. DiagnosticsPanel.svelte).
   */
  import { onMount, onDestroy } from 'svelte';
  import {
    getIdentityDiscoverable,
    setIdentityDiscoverable,
    onIdentityDiscoverableChanged,
    getPresenceVisibility,
    setPresenceVisibility,
    getPkarrRelays,
    addPkarrRelay,
    removePkarrRelay,
    resetPkarrRelays,
  } from '../connectivity-adapter';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import type { RelayHealth } from '../types/network-health';
  import NetworkStatusPill from './NetworkStatusPill.svelte';
  import { relayStatusLabel } from '../relay-status-label';

  // Current persisted value — loaded on mount, updated on toggle.
  let enabled = $state(false);
  let loading = $state(true);
  // True while a setIdentityDiscoverable call is in-flight. Prevents a second
  // toggle click from racing the first write (CodeRabbit PR #158 round 2).
  let pending = $state(false);
  let error = $state<string | null>(null);

  // Cleanup for the event listener.
  let stopListener: (() => void) | null = null;

  // ZEB-600: presence visibility ("Appear offline"). presenceVisible=true means
  // this node publishes beacons (others see it online). The toggle is INVERTED —
  // it reads "Appear offline", so checked = !presenceVisible.
  let presenceVisible = $state(true);
  let presenceLoading = $state(true);
  let presencePending = $state(false);
  let presenceError = $state<string | null>(null);

  // ZEB-380: relay manager state.
  let relays = $state<RelayHealth[]>([]);
  let newRelayUrl = $state('');
  let relayError = $state<string | null>(null);
  let relayPending = $state(false);
  // True once the initial get_pkarr_relays fetch succeeds. Guards Add/Remove
  // so they can never submit a payload built from an empty/unknown base list,
  // which would clobber the persisted pool (Cursor Bugbot round-4 HIGH finding).
  let relayLoaded = $state(false);
  // Monotonic token to drop stale in-flight get_pkarr_relays responses: if a
  // newer fetch starts before an older one resolves (e.g. a mutation-triggered
  // refetch racing a connectivity-relays-changed refetch), only the newest
  // result is applied (Cursor Bugbot round-5 Medium — out-of-order responses
  // overwriting a fresher list).
  let relayFetchSeq = 0;
  // Unlisten for connectivity-relays-changed event.
  let relaysUnlisten: UnlistenFn | null = null;
  let relaysListenerDestroyed = false;

  // ZEB-380 Fix 3: ticking `now` for cooling-down countdown badges.
  let now = $state(Date.now());
  let nowTimer: ReturnType<typeof setInterval> | null = null;
  // Reactive: start/stop the 1s tick depending on whether any relay is cooling down.
  $effect(() => {
    const hasCooling = relays.some((r) => r.state.kind === 'coolingDown');
    if (hasCooling && nowTimer === null) {
      nowTimer = setInterval(() => {
        now = Date.now();
      }, 1000);
    } else if (!hasCooling && nowTimer !== null) {
      clearInterval(nowTimer);
      nowTimer = null;
    }
  });

  async function fetchRelays(): Promise<void> {
    const seq = ++relayFetchSeq;
    try {
      const next = (await getPkarrRelays()) ?? [];
      // Drop the result if a newer fetch superseded this one in flight.
      if (seq !== relayFetchSeq) return;
      relays = next;
      relayError = null;
      relayLoaded = true;
    } catch (e) {
      // Stale failures must not clobber a newer fetch's state either.
      if (seq !== relayFetchSeq) return;
      // fetchRelays is a REFRESH (onMount + the connectivity-relays-changed
      // listener). Once a list is loaded — including one a mutation just
      // applied authoritatively — a failed refresh must NOT surface an error
      // over a list that is still current (the mutation already succeeded and
      // persisted). Only an INITIAL load failure (no list yet) is user-facing.
      if (!relayLoaded) {
        relayError = e instanceof Error ? e.message : String(e);
      }
      // Do NOT set relayLoaded on failure — the base list is still unknown.
    }
  }

  // Apply an authoritative relay list returned by a mutation (add/remove/reset).
  // Unlike fetchRelays this can't fail — the list is already in hand — so it
  // closes the "mutation succeeded but the follow-up refetch failed, leaving a
  // stale list" gap (Cursor round-10 Medium). Claims the latest fetch token so
  // an in-flight refetch can't later overwrite it with a stale read.
  function applyAuthoritativeRelays(next: RelayHealth[]): void {
    relayFetchSeq++;
    relays = next ?? [];
    relayError = null;
    relayLoaded = true;
  }

  async function handleAddRelay(): Promise<void> {
    const trimmed = newRelayUrl.trim();
    // Guard: never submit an Add before the initial fetch succeeds (relayLoaded
    // is defense-in-depth; the button is also disabled until then).
    if (!relayLoaded) return;
    if (!trimmed || relayPending) return;
    relayPending = true;
    relayError = null;
    try {
      // Server-authoritative read-modify-write: send only the new URL.
      // The backend appends it to the CURRENT persisted list and re-validates,
      // so a stale in-memory `relays` view can never clobber a fresher pool.
      // It returns the new authoritative list, so we apply it directly — no
      // refetch that could fail and strand a stale view.
      const next = await addPkarrRelay(trimmed);
      newRelayUrl = '';
      applyAuthoritativeRelays(next);
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
      // The backend filters the CURRENT persisted list and re-validates,
      // guarding the >=1 invariant server-side as well. It returns the new
      // authoritative list, applied directly — no refetch that could fail.
      const next = await removePkarrRelay(url);
      applyAuthoritativeRelays(next);
    } catch (e) {
      relayError = e instanceof Error ? e.message : String(e);
    } finally {
      relayPending = false;
    }
  }

  async function handleRestoreRecommended(): Promise<void> {
    if (relayPending) return;
    relayPending = true;
    relayError = null;
    try {
      // Server-authoritative reset returns the new authoritative list (the
      // recommended defaults), applied directly — no refetch that could fail.
      const next = await resetPkarrRelays();
      applyAuthoritativeRelays(next);
    } catch (e) {
      relayError = e instanceof Error ? e.message : String(e);
    } finally {
      relayPending = false;
    }
  }

  onMount(async () => {
    // Subscribe to backend-side change events so that if another window
    // or IPC caller toggles the setting, this component stays in sync.
    stopListener = onIdentityDiscoverableChanged((newEnabled) => {
      enabled = newEnabled;
    });

    try {
      enabled = await getIdentityDiscoverable();
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }

    // ZEB-600: seed the "Appear offline" toggle from the persisted/live setting.
    try {
      presenceVisible = await getPresenceVisibility();
      presenceError = null;
    } catch (e) {
      presenceError = e instanceof Error ? e.message : String(e);
    } finally {
      presenceLoading = false;
    }

    // ZEB-380: load relay pool and subscribe to live changes.
    await fetchRelays();
    // Race-safe: if destroyed before listen() resolves, immediately call
    // the returned unlisten (mirrors onReachabilityChanged pattern).
    // Wrap in try/catch so a failed Tauri event subscription (e.g. during
    // tests or early teardown) does not produce an unhandled rejection.
    try {
      const resolved = await listen<null>('connectivity-relays-changed', () => {
        void fetchRelays();
      });
      if (relaysListenerDestroyed) {
        resolved();
      } else {
        relaysUnlisten = resolved;
      }
    } catch (e) {
      console.error('NetworkDiscoverabilitySettings: failed to subscribe to relay changes', e);
    }
  });

  onDestroy(() => {
    stopListener?.();
    relaysListenerDestroyed = true;
    relaysUnlisten?.();
    if (nowTimer !== null) {
      clearInterval(nowTimer);
      nowTimer = null;
    }
  });

  async function handleToggle(e: Event) {
    if (pending) return;
    const target = e.target as HTMLInputElement;
    const newVal = target.checked;
    // Optimistic update.
    enabled = newVal;
    pending = true;
    try {
      await setIdentityDiscoverable(newVal);
      error = null;
    } catch (err) {
      // Roll back.
      enabled = !newVal;
      error = err instanceof Error ? err.message : String(err);
    } finally {
      pending = false;
    }
  }

  // ZEB-600: the checkbox is "Appear offline", so checked=true means INVISIBLE.
  // presenceVisible is the inverse of what the box shows.
  async function handlePresenceToggle(e: Event) {
    if (presencePending) return;
    const target = e.target as HTMLInputElement;
    const appearOffline = target.checked;
    const newVisible = !appearOffline;
    // Optimistic update.
    presenceVisible = newVisible;
    presencePending = true;
    try {
      await setPresenceVisibility(newVisible);
      presenceError = null;
    } catch (err) {
      // Roll back.
      presenceVisible = !newVisible;
      presenceError = err instanceof Error ? err.message : String(err);
    } finally {
      presencePending = false;
    }
  }
</script>

<div class="discoverability-section" data-testid="network-discoverability-settings">
  <div class="section-header">
    <h4 class="section-title">Network Discoverability</h4>
  </div>

  {#if error}
    <p class="error-text" data-testid="discoverability-error">{error}</p>
  {/if}

  <label class="toggle-row" for="discoverability-toggle">
    <div class="toggle-text">
      <span class="toggle-label">Allow discovery by identity address</span>
      <span class="toggle-hint">
        When on, anyone who has your identity address can connect to your devices over the internet.
        When off, you can only be reached through invite links and communities you already share.
      </span>
    </div>
    <div class="toggle-control">
      <input
        id="discoverability-toggle"
        type="checkbox"
        role="switch"
        class="visually-hidden"
        checked={enabled}
        disabled={loading || pending}
        onchange={handleToggle}
        data-testid="discoverability-toggle"
        aria-checked={enabled}
        aria-label="Allow discovery by identity address"
      />
      <span class="toggle-track" class:on={enabled} aria-hidden="true">
        <span class="toggle-thumb"></span>
      </span>
      <span class="toggle-value" data-testid="discoverability-value">
        {loading ? '…' : enabled ? 'On' : 'Off'}
      </span>
    </div>
  </label>
</div>

<!-- ZEB-600: presence "Appear offline" toggle. Inverted — checked = invisible. -->
<div class="discoverability-section" data-testid="presence-visibility-settings">
  <div class="section-header">
    <h4 class="section-title">Presence</h4>
  </div>

  {#if presenceError}
    <p class="error-text" data-testid="presence-error">{presenceError}</p>
  {/if}

  <label class="toggle-row" for="appear-offline-toggle">
    <div class="toggle-text">
      <span class="toggle-label">Appear offline</span>
      <span class="toggle-hint">
        When on, no one sees you as online — your device stops broadcasting presence.
        You still see who else is around. When off, communities you share show you as online.
      </span>
    </div>
    <div class="toggle-control">
      <input
        id="appear-offline-toggle"
        type="checkbox"
        role="switch"
        class="visually-hidden"
        checked={!presenceVisible}
        disabled={presenceLoading || presencePending}
        onchange={handlePresenceToggle}
        data-testid="appear-offline-toggle"
        aria-checked={!presenceVisible}
        aria-label="Appear offline"
      />
      <span class="toggle-track" class:on={!presenceVisible} aria-hidden="true">
        <span class="toggle-thumb"></span>
      </span>
      <span class="toggle-value" data-testid="appear-offline-value">
        {presenceLoading ? '…' : !presenceVisible ? 'On' : 'Off'}
      </span>
    </div>
  </label>
</div>

<!-- ZEB-380: relay manager -->
<div class="discoverability-section" data-testid="relay-manager">
  <div class="section-header">
    <h4 class="section-title">Discovery Relays</h4>
  </div>
  <p class="toggle-hint">
    Pkarr relays used for identity publishing and lookup. Changes apply live — no restart needed.
  </p>

  {#if relayError}
    <p class="error-text" data-testid="relay-error">{relayError}</p>
  {/if}

  <ul class="relay-list" data-testid="relay-list">
    {#each relays as relay (relay.url)}
      <li class="relay-row" data-testid="relay-row">
        <code class="relay-url" data-testid="relay-url">{relay.url}</code>
        <NetworkStatusPill
          variant={relay.state.kind === 'healthy' ? 'healthy' : 'cooling'}
          label={relayStatusLabel(relay, now)}
          data-testid="relay-badge"
        />
        <button
          class="relay-remove"
          data-testid="relay-remove"
          disabled={!relayLoaded || relays.length <= 1 || relayPending}
          onclick={() => handleRemoveRelay(relay.url)}
          aria-label={`Remove relay ${relay.url}`}
        >
          Remove
        </button>
      </li>
    {/each}
  </ul>

  <div class="relay-add-row" data-testid="relay-add-row">
    <input
      type="url"
      class="relay-input"
      placeholder="https://relay.example.com"
      bind:value={newRelayUrl}
      disabled={!relayLoaded || relayPending}
      data-testid="relay-url-input"
      aria-label="New relay URL"
    />
    <button
      class="relay-add-btn"
      disabled={!relayLoaded || !newRelayUrl.trim() || relayPending}
      onclick={handleAddRelay}
      data-testid="relay-add-button"
    >
      Add
    </button>
  </div>

  <button
    class="relay-restore-btn"
    onclick={handleRestoreRecommended}
    disabled={relayPending}
    data-testid="relay-restore-button"
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

  .toggle-row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
    cursor: pointer;
  }

  .toggle-text {
    display: flex;
    flex-direction: column;
    gap: 4px;
    flex: 1;
  }

  .toggle-label {
    font-size: 13px;
    color: var(--text-primary);
  }

  .toggle-hint {
    font-size: 11px;
    color: var(--text-muted);
    line-height: 1.4;
  }

  .toggle-control {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-shrink: 0;
  }

  /* Accessible visually-hidden checkbox — screen readers see it, sighted
     users see the custom toggle track/thumb below. */
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }

  .toggle-track {
    display: inline-flex;
    align-items: center;
    width: 32px;
    height: 18px;
    border-radius: 9px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    transition: background 0.15s;
    position: relative;
    flex-shrink: 0;
  }

  .toggle-track.on {
    background: var(--accent);
    border-color: var(--accent);
  }

  .toggle-thumb {
    position: absolute;
    left: 2px;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--text-primary);
    transition: transform 0.15s;
  }

  .toggle-track.on .toggle-thumb {
    transform: translateX(14px);
  }

  .toggle-value {
    font-size: 12px;
    color: var(--text-secondary);
    min-width: 2.5em;
  }

  .error-text {
    font-size: 12px;
    color: var(--danger-muted);
    margin: 4px 0 8px;
  }

  /* ZEB-380: relay manager */
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
     and in IrohRelaySettings.svelte, and had already drifted. */

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
    /* ZEB-773: was 5px here and 3px in IrohRelaySettings — the duplicated
       block had already drifted. Normalised to the 4px used by every other
       control in the app. */
    border-radius: 4px;
  }
</style>
