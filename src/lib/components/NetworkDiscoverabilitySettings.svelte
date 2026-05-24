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
  } from '../connectivity-adapter';

  // Current persisted value — loaded on mount, updated on toggle.
  let enabled = $state(false);
  let loading = $state(true);
  // True while a setIdentityDiscoverable call is in-flight. Prevents a second
  // toggle click from racing the first write (CodeRabbit PR #158 round 2).
  let pending = $state(false);
  let error = $state<string | null>(null);

  // Cleanup for the event listener.
  let stopListener: (() => void) | null = null;

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
  });

  onDestroy(() => {
    stopListener?.();
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
</style>
