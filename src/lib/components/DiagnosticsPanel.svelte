<script lang="ts">
  /**
   * ZEB-321 Phase 1: dev-mode connectivity diagnostics.
   * ZEB-323 Phase 2b: extended with pkarr publication status section.
   *
   * Renders nothing in production builds — the `{#if isDevMode}` block
   * gates the entire DOM tree, and `import.meta.env.DEV` is a Vite
   * compile-time constant that evaluates to `false` in `vite build`
   * output. Tree-shaking elides the dead branch + its onMount data
   * fetches, so production runs make zero IPC calls.
   *
   * Svelte version: this component uses Svelte-5 runes (`$state`) for
   * the IPC-driven reactive bindings, mirroring `MintLedger.svelte`.
   * The plan's Svelte-4 `let` + `$:` syntax works for the runtime
   * (Svelte 5 in compatibility mode) but vitest's render of a
   * post-mount keyed-each block under Svelte 5.53 triggers a spurious
   * `effect_orphan` error. Runes thread the reactivity through the
   * component's effect graph explicitly, avoiding that trap.
   */
  import { onMount, onDestroy } from 'svelte';
  import {
    getMyReachabilityRecord,
    listPeerReachability,
    forceRepublish,
    onReachabilityChanged,
    pkarrPublicationStatus,
    onPkarrFallbackFired,
  } from '../connectivity-adapter';
  import type {
    ReachabilityRecord,
    PeerReachability,
    PkarrPublicationStatus,
  } from '../types/connectivity';

  // Vite injects `import.meta.env.DEV` as a build-time boolean. In tests
  // (vitest) the value is true by default; production builds force it
  // false. The `vi.stubEnv('DEV', ...)` mechanism in our vitest tests
  // overrides at runtime so we can validate both branches.
  const isDevMode: boolean = import.meta.env.DEV;

  let myRecord = $state<ReachabilityRecord | null>(null);
  let peerRecords = $state<PeerReachability[]>([]);
  let unlisten: (() => void) | null = null;
  // Set true in onDestroy. If onMount's awaited onReachabilityChanged
  // call resolves AFTER unmount, the listener-registration callback
  // detects this and tears down immediately so a re-render that
  // remounts the component doesn't accumulate live listeners
  // (CodeRabbit PR #157 round 1).
  let destroyed = false;
  let error = $state<string | null>(null);

  // ── ZEB-323 Phase 2b: pkarr section ─────────────────────────────────────
  let pkarrStatus = $state<PkarrPublicationStatus | null>(null);
  let pkarrFallbackEvents = $state<
    { peerAddrShort: string; communityId: string; hit: boolean }[]
  >([]);
  let pkarrSectionOpen = $state(false);
  let stopFallbackListener: (() => void) | null = null;
  let pkarrRefreshInterval: ReturnType<typeof setInterval> | null = null;

  async function refreshPkarr(): Promise<void> {
    try {
      const status = await pkarrPublicationStatus();
      // ZEB-871: refreshPkarr() runs on a 5s setInterval; an in-flight tick can
      // settle after onDestroy clears the interval. Guard the $state write.
      // Svelte-5 post-teardown writes are silent, so this is defensive
      // consistency with the FriendsPanel (ZEB-793) idiom, not a visible bug.
      if (destroyed) return;
      pkarrStatus = status;
    } catch {
      // Non-fatal — pkarr not yet initialized on this boot is expected.
      if (destroyed) return;
      pkarrStatus = null;
    }
  }

  async function refresh(): Promise<void> {
    try {
      const my = await getMyReachabilityRecord();
      const peers = await listPeerReachability();
      // ZEB-871: refresh() is driven by the onReachabilityChanged listener, so
      // an in-flight call can settle after onDestroy. Defer both $state writes
      // past a single destroyed-guard (also avoids a partial write if we unmount
      // between the two awaits).
      if (destroyed) return;
      myRecord = my;
      peerRecords = peers;
      error = null;
    } catch (e) {
      if (destroyed) return;
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function handleForceRepublish(): Promise<void> {
    try {
      await forceRepublish();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(async () => {
    // Defense-in-depth: even though the `{#if isDevMode}` block gates the
    // DOM, the `onMount` body runs unconditionally. We early-return here
    // so a misconfigured production build (e.g. tree-shaking failed for
    // some reason) still avoids hitting the IPC layer.
    if (!isDevMode) return;
    await refresh();
    // The `await` here is the race window: if the component unmounts
    // between `refresh()` and the listener resolving, `onDestroy`'s
    // `if (unlisten)` check finds nothing and the registered listener
    // leaks. Capture the resolved unlisten into a local first, then
    // either tear it down immediately (if we've already unmounted) or
    // hand it to the module-level binding for `onDestroy` to handle
    // (CodeRabbit PR #157 round 1).
    const resolved = await onReachabilityChanged(() => {
      void refresh();
    });
    if (destroyed) {
      resolved();
    } else {
      unlisten = resolved;
    }

    // Phase 2b pkarr section: initial fetch + 5-second auto-refresh.
    await refreshPkarr();
    pkarrRefreshInterval = setInterval(() => {
      void refreshPkarr();
    }, 5000);

    // Subscribe to fallback-fired events; keep the last 5.
    stopFallbackListener = onPkarrFallbackFired((ev) => {
      pkarrFallbackEvents = [ev, ...pkarrFallbackEvents].slice(0, 5);
    });
  });

  onDestroy(() => {
    destroyed = true;
    if (unlisten) unlisten();
    if (pkarrRefreshInterval !== null) clearInterval(pkarrRefreshInterval);
    stopFallbackListener?.();
  });
</script>

{#if isDevMode}
  <div class="connectivity-diagnostics" data-testid="diag-root">
    <h3>ZEB-321 connectivity diagnostics (dev only)</h3>

    {#if error}
      <p class="error" data-testid="diag-error">Error: {error}</p>
    {/if}

    <section>
      <h4>This device</h4>
      {#if myRecord}
        <dl>
          <dt>Iroh NodeId</dt>
          <dd data-testid="diag-my-node-id">{myRecord.irohNodeId}</dd>
          <dt>Home relay</dt>
          <dd data-testid="diag-my-relay">{myRecord.homeRelayUrl || '(none)'}</dd>
          <dt>Direct addresses</dt>
          <dd data-testid="diag-my-direct">
            {myRecord.directAddresses.join(', ') || '(none)'}
          </dd>
        </dl>
      {:else}
        <p data-testid="diag-my-empty">Iroh endpoint not ready</p>
      {/if}
      <button onclick={handleForceRepublish} data-testid="diag-force-republish">
        Force republish
      </button>
    </section>

    <section>
      <h4>Known peers ({peerRecords.length})</h4>
      {#if peerRecords.length === 0}
        <p data-testid="diag-peers-empty">No peer reachability records yet.</p>
      {:else}
        <ul>
          {#each peerRecords as peer (peer.ownerAddress)}
            <li data-testid="diag-peer">
              <strong>{peer.ownerAddress.slice(0, 12)}…</strong> →
              {peer.record.irohNodeId.slice(0, 12)}…
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <section data-testid="pkarr-section">
      <button
        class="collapsible-header"
        onclick={() => { pkarrSectionOpen = !pkarrSectionOpen; }}
        aria-expanded={pkarrSectionOpen}
        data-testid="pkarr-section-toggle"
      >
        <h4 style="display:inline;margin:0;">Network Discovery (pkarr)</h4>
        <span>{pkarrSectionOpen ? '▲' : '▼'}</span>
      </button>

      {#if pkarrSectionOpen}
        <div class="pkarr-content" data-testid="pkarr-content">
          {#if pkarrStatus === null}
            <p data-testid="pkarr-not-ready">pkarr publisher not initialized yet.</p>
          {:else}
            <dl>
              <dt>Pending invite publications (case A)</dt>
              <dd data-testid="pkarr-invite-count">{pkarrStatus.inviteCount}</dd>
              <dt>Identity-keyed publication (case B)</dt>
              <dd data-testid="pkarr-identity-active">{pkarrStatus.identityActive ? 'Active' : 'Inactive'}</dd>
              <dt>Per-community publications (case C)</dt>
              <dd data-testid="pkarr-community-count">{pkarrStatus.communityCount}</dd>
            </dl>
          {/if}

          <div class="pkarr-fallback-log" data-testid="pkarr-fallback-log">
            <strong>Recent fallback events (last 5)</strong>
            {#if pkarrFallbackEvents.length === 0}
              <p data-testid="pkarr-fallback-empty">No fallback lookups yet.</p>
            {:else}
              <ul>
                {#each pkarrFallbackEvents as ev, i (i)}
                  <li data-testid="pkarr-fallback-event">
                    {ev.peerAddrShort}… in {ev.communityId.slice(0, 8)}… —
                    {ev.hit ? 'hit ✓' : 'miss ✗'}
                  </li>
                {/each}
              </ul>
            {/if}
          </div>
        </div>
      {/if}
    </section>
  </div>
{/if}

<style>
  .connectivity-diagnostics {
    border: 1px dashed var(--color-text-secondary);
    padding: 1em;
    margin: 1em;
    font-family: var(--font-mono);
    font-size: 0.85em;
  }
  .error {
    color: var(--danger);
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.25em 1em;
  }
  dt {
    font-weight: bold;
  }
  .collapsible-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    width: 100%;
    background: none;
    border: none;
    cursor: pointer;
    padding: 4px 0;
    color: inherit;
    font-family: inherit;
    font-size: inherit;
    text-align: left;
    gap: 8px;
  }
  .pkarr-content {
    margin-top: 8px;
  }
  .pkarr-fallback-log {
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px dashed var(--muted);
  }
</style>
