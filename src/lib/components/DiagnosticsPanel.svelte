<script lang="ts">
  /**
   * ZEB-321 Phase 1: dev-mode connectivity diagnostics.
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
  } from '../connectivity-adapter';
  import type {
    ReachabilityRecord,
    PeerReachability,
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

  async function refresh(): Promise<void> {
    try {
      myRecord = await getMyReachabilityRecord();
      peerRecords = await listPeerReachability();
      error = null;
    } catch (e) {
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
  });

  onDestroy(() => {
    destroyed = true;
    if (unlisten) unlisten();
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
  </div>
{/if}

<style>
  .connectivity-diagnostics {
    border: 1px dashed #888;
    padding: 1em;
    margin: 1em;
    font-family: monospace;
    font-size: 0.85em;
  }
  .error {
    color: crimson;
  }
  dl {
    display: grid;
    grid-template-columns: max-content 1fr;
    gap: 0.25em 1em;
  }
  dt {
    font-weight: bold;
  }
</style>
