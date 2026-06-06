<script lang="ts">
  /**
   * ZEB-323 Phase 2b: Network Discoverability (case B) toggle.
   *
   * Renders a single opt-in toggle that controls whether this device
   * publishes its iroh routing to the pkarr DHT under its identity public
   * key. When enabled, anyone who has the user's identity address can reach
   * this device cross-WAN without a shared community or pending invite.
   *
   * Default is OFF — matches the backend `PkarrSettings` default.
   *
   * Uses Svelte 5 runes (`$state`, `$effect`) consistent with other
   * IPC-driven components in this codebase (e.g. DiagnosticsPanel.svelte).
   */
  import { onMount, onDestroy } from 'svelte';
  import {
    getIdentityDiscoverable,
    setIdentityDiscoverable,
    onIdentityDiscoverableChanged,
    getPkarrRelays,
    setPkarrRelays,
    resetPkarrRelays,
  } from '../connectivity-adapter';
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import type { RelayHealth } from '../types/network-health';

  // Current persisted value — loaded on mount, updated on toggle.
  let enabled = $state(false);
  let loading = $state(true);
  // True while a setIdentityDiscoverable call is in-flight. Prevents a second
  // toggle click from racing the first write (CodeRabbit PR #158 round 2).
  let pending = $state(false);
  let error = $state<string | null>(null);

  // Cleanup for the event listener.
  let stopListener: (() => void) | null = null;

  // ZEB-380: relay manager state.
  let relays = $state<RelayHealth[]>([]);
  let newRelayUrl = $state('');
  let relayError = $state<string | null>(null);
  let relayPending = $state(false);
  // True once the initial get_pkarr_relays fetch succeeds. Guards Add/Remove
  // so they can never submit a payload built from an empty/unknown base list,
  // which would clobber the persisted pool (Cursor Bugbot round-4 HIGH finding).
  let relayLoaded = $state(false);
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
    try {
      relays = (await getPkarrRelays()) ?? [];
      relayError = null;
      relayLoaded = true;
    } catch (e) {
      relayError = e instanceof Error ? e.message : String(e);
      // Do NOT set relayLoaded on failure — the base list is still unknown.
    }
  }

  async function handleAddRelay(): Promise<void> {
    const trimmed = newRelayUrl.trim();
    // Guard: never submit an Add whose payload would clobber an unknown/empty
    // base pool (i.e. if the initial fetch hasn't succeeded yet).
    if (!relayLoaded) return;
    if (!trimmed || relayPending) return;
    relayPending = true;
    relayError = null;
    try {
      const currentUrls = relays.map((r) => r.url);
      await setPkarrRelays([...currentUrls, trimmed]);
      newRelayUrl = '';
      await fetchRelays();
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
      const newList = relays.map((r) => r.url).filter((u) => u !== url);
      await setPkarrRelays(newList);
      await fetchRelays();
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
      await resetPkarrRelays();
      await fetchRelays();
    } catch (e) {
      relayError = e instanceof Error ? e.message : String(e);
    } finally {
      relayPending = false;
    }
  }

  function relayStateLabel(relay: RelayHealth): string {
    if (relay.state.kind === 'healthy') return 'Healthy';
    const secsLeft = Math.max(0, Math.ceil((relay.state.untilMs - now) / 1000));
    return `Cooling down (${secsLeft}s)`;
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
        <span
          class="relay-badge {relay.state.kind === 'healthy' ? 'badge-healthy' : 'badge-cooling'}"
          data-testid="relay-badge"
        >
          {relayStateLabel(relay)}
        </span>
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
    color: var(--text-muted, var(--text-secondary));
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
    background: var(--bg-tertiary, #444);
    border: 1px solid var(--border, #555);
    transition: background 0.15s;
    position: relative;
    flex-shrink: 0;
  }

  .toggle-track.on {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
  }

  .toggle-thumb {
    position: absolute;
    left: 2px;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--text-primary, #fff);
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
    color: #d83c3e;
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

  .relay-badge {
    font-size: 10px;
    font-weight: 600;
    padding: 1px 5px;
    border-radius: 3px;
    white-space: nowrap;
  }

  .badge-healthy {
    background: #1a4a1a;
    color: #5cb85c;
  }

  .badge-cooling {
    background: #4a3a00;
    color: #f0a020;
  }

  .relay-remove {
    font-size: 11px;
    padding: 1px 6px;
    cursor: pointer;
  }

  .relay-add-row {
    display: flex;
    gap: 6px;
    margin-top: 8px;
  }

  .relay-input {
    flex: 1;
    font-size: 12px;
    padding: 3px 6px;
    background: var(--bg-tertiary, #333);
    color: var(--text-primary, #eee);
    border: 1px solid var(--border, #555);
    border-radius: 3px;
  }

  .relay-add-btn,
  .relay-restore-btn {
    font-size: 11px;
    padding: 3px 8px;
    cursor: pointer;
  }

  .relay-restore-btn {
    margin-top: 6px;
    display: block;
  }
</style>
