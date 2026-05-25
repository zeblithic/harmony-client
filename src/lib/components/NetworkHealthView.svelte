<script lang="ts">
  /**
   * ZEB-329 — Network Health panel (dedicated /network route).
   *
   * Spec §7. Owns snapshot fetch + event subscription + self-test launch.
   * Renders summary card + per-peer rows + self-test results pane +
   * "Submit diagnostics" button.
   *
   * Task 11: "Submit diagnostics" button now opens DiagnosticExportModal.
   *
   * Svelte 5 runes (`$state`, `$effect`) mirror DiagnosticsPanel.svelte's
   * established pattern. Race-safe cleanup (destroyed flag + unlisten)
   * matches CodeRabbit PR #157 round 1.
   */
  import { onMount, onDestroy } from 'svelte';
  import {
    snapshot as fetchSnapshot,
    runSelfTest as runSelfTestIpc,
    onNetworkHealthChanged,
    explainNatClass,
    redactAddr,
  } from '../network-health-adapter';
  import type {
    NetworkHealthSnapshot,
    SelfTestReport,
    PeerHealth,
  } from '../types/network-health';
  import DiagnosticExportModal from './DiagnosticExportModal.svelte';

  let snap = $state<NetworkHealthSnapshot | null>(null);
  let report = $state<SelfTestReport | null>(null);
  let runningSelfTest = $state(false);
  let selfTestError = $state<string | null>(null);
  // Task 11: open/close state for the DiagnosticExportModal. Opened by
  // the "Submit diagnostics…" button; closed via the modal's onClose prop.
  let exportOpen = $state(false);

  let unlisten: (() => void) | null = null;
  let destroyed = false;

  // Edge case 6.4 #1: auto-retry every 2s for 30s when iroh isn't ready.
  let startupRetryHandle: ReturnType<typeof setInterval> | null = null;
  let startupRetryElapsedMs = 0;

  async function refresh(): Promise<void> {
    try {
      snap = await fetchSnapshot();
    } catch (e) {
      // Spec §6.3: never show top-level error banner — render empty.
      // The "diagnostics unavailable" banner shows only if snap stays null
      // for the entire startup window.
      // eslint-disable-next-line no-console
      console.warn(
        `[network-health] snapshot failed: ${e instanceof Error ? e.message : String(e)}`,
      );
    }
  }

  async function handleSelfTest(): Promise<void> {
    if (runningSelfTest) return;
    runningSelfTest = true;
    selfTestError = null;
    try {
      report = await runSelfTestIpc();
    } catch (e) {
      selfTestError = e instanceof Error ? e.message : String(e);
    } finally {
      runningSelfTest = false;
    }
  }

  function startStartupRetry(): void {
    if (startupRetryHandle) return;
    startupRetryHandle = setInterval(async () => {
      startupRetryElapsedMs += 2000;
      await refresh();
      if (snap?.myNetwork || startupRetryElapsedMs >= 30000) {
        if (startupRetryHandle) clearInterval(startupRetryHandle);
        startupRetryHandle = null;
      }
    }, 2000);
  }

  onMount(async () => {
    await refresh();
    if (!snap?.myNetwork) startStartupRetry();
    // Race window: if the component unmounts between `refresh()` and the
    // listener resolving, the registered listener would leak. Capture the
    // resolved unlisten into a local first and tear down immediately if
    // we've already unmounted (mirrors DiagnosticsPanel race-safe pattern).
    const resolved = await onNetworkHealthChanged(() => {
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
    if (startupRetryHandle) clearInterval(startupRetryHandle);
  });

  function peerStatusIcon(p: PeerHealth): string {
    if (p.connectionMode === 'direct') return '✓';
    if (p.connectionMode === 'relay') return '⚠';
    return '✗';
  }
</script>

<div class="network-health" data-testid="network-health-root">
  <h1>Network Health</h1>

  {#if !snap}
    <p data-testid="nh-initial-loading">Loading…</p>
  {:else if !snap.myNetwork}
    <section class="starting-up" data-testid="nh-starting-up">
      <p>Network is starting up…</p>
      <p class="muted">This can take 10–30 seconds on first launch.</p>
      <button onclick={refresh}>Retry now</button>
    </section>
  {:else}
    {@const my = snap.myNetwork}
    {@const explain = explainNatClass(my.natClassification)}
    <section class="my-network" data-testid="nh-my-network">
      <h2>Your network</h2>
      <p class="status status-{my.reachability}">
        <strong data-testid="nh-headline">{explain.headline}</strong>
        <span class="info-hover" title={explain.detail}>…</span>
      </p>
      <p class="detail">{explain.detail}</p>
      {#if my.homeRelayUrl}
        <p>Relay: <code data-testid="nh-relay">{my.homeRelayUrl}</code></p>
      {/if}
      {#if my.relayRttMs !== null}
        <p>RTT to relay: {my.relayRttMs}ms</p>
      {/if}
    </section>

    <section class="peers" data-testid="nh-peers">
      <h2>Peers ({snap.peers.length})</h2>
      {#if snap.peers.length === 0}
        <p data-testid="nh-peers-empty">No peers in shared communities yet.</p>
      {:else}
        <ul>
          {#each snap.peers as p (p.ownerAddr)}
            <li data-testid="nh-peer">
              {peerStatusIcon(p)}
              <strong>{redactAddr(p.ownerAddr, false)}</strong>
              <span>{p.connectionMode}</span>
              {#if p.rttMs !== null}<span>{p.rttMs}ms</span>{/if}
              {#if p.lastSeenMs !== null}
                <span class="muted"
                  >last seen {Math.floor(
                    (Date.now() - p.lastSeenMs) / 1000,
                  )}s ago</span
                >
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <section class="self-test" data-testid="nh-self-test">
      <h2>Self-test</h2>
      <button
        onclick={handleSelfTest}
        disabled={runningSelfTest}
        data-testid="nh-self-test-button"
      >
        {runningSelfTest ? 'Running…' : 'Run self-test'}
      </button>
      {#if selfTestError}
        <p class="error" data-testid="nh-self-test-error">
          Self-test couldn't start: {selfTestError}
        </p>
      {/if}
      {#if report}
        <ul class="self-test-steps">
          {#each report.steps as step (step.name)}
            <li data-testid="nh-self-test-step">
              {#if step.outcome.type === 'pass'}
                ✓ {step.name} ({step.outcome.durationMs}ms)
              {:else if step.outcome.type === 'fail'}
                ✗ {step.name}
                <span title={step.outcome.reason}>(failed)</span>
              {:else}
                ⊘ {step.name}
                <span title={step.outcome.reason}>(skipped)</span>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <!--
      Task 11: opens DiagnosticExportModal. Modal owns its own fetch
      cycle (calls exportPayload(includeFullIds) on mount + on toggle).
    -->
    <button
      onclick={() => (exportOpen = true)}
      data-testid="nh-export-button"
    >
      Submit diagnostics…
    </button>
  {/if}
</div>

{#if exportOpen}
  <DiagnosticExportModal onClose={() => (exportOpen = false)} />
{/if}

<style>
  .network-health {
    padding: 1rem;
    max-width: 800px;
  }
  .muted {
    color: #888;
  }
  .status-reachable {
    color: green;
  }
  .status-degraded {
    color: orange;
  }
  .status-unreachable {
    color: crimson;
  }
  .info-hover {
    cursor: help;
    margin-left: 0.5em;
  }
  .error {
    color: crimson;
  }
  .self-test-steps {
    list-style: none;
    padding-left: 0;
    font-family: monospace;
  }
</style>
