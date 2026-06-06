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
  import { listen } from '@tauri-apps/api/event';
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
    RelayOutcome,
  } from '../types/network-health';

  function relayOutcomeLabel(outcome: RelayOutcome): string {
    if (outcome.kind === 'timeout') return 'Last error: timeout';
    if (outcome.kind === 'transport') return 'Last error: transport';
    if (outcome.kind === 'http') return `Last error: http ${outcome.status}`;
    return '';
  }
  import DiagnosticExportModal from './DiagnosticExportModal.svelte';

  let snap = $state<NetworkHealthSnapshot | null>(null);
  let report = $state<SelfTestReport | null>(null);
  let runningSelfTest = $state(false);
  let selfTestError = $state<string | null>(null);
  // Task 11: open/close state for the DiagnosticExportModal. Opened by
  // the "Submit diagnostics…" button; closed via the modal's onClose prop.
  let exportOpen = $state(false);

  let unlisten: (() => void) | null = null;
  let unlistenRelays: (() => void) | null = null;
  let destroyed = false;

  // ZEB-380 Fix 3: ticking `now` for cooling-down countdown badges.
  let now = $state(Date.now());
  let nowTimer: ReturnType<typeof setInterval> | null = null;

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

  // ZEB-380 Fix 3: start/stop the 1s tick depending on whether any relay is cooling down.
  $effect(() => {
    const relayList = snap?.pkarrStatus?.relays ?? [];
    const hasCooling = relayList.some((r) => r.state.kind === 'coolingDown');
    if (hasCooling && nowTimer === null) {
      nowTimer = setInterval(() => {
        now = Date.now();
      }, 1000);
    } else if (!hasCooling && nowTimer !== null) {
      clearInterval(nowTimer);
      nowTimer = null;
    }
  });

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
    // ZEB-380 Fix 2: re-fetch snapshot when the relay pool is hot-swapped so
    // the relay rows stay fresh after a live `set_pkarr_relays` / `reset_pkarr_relays`.
    try {
      const resolvedRelays = await listen<null>('connectivity-relays-changed', () => {
        void refresh();
      });
      if (destroyed) {
        resolvedRelays();
      } else {
        unlistenRelays = resolvedRelays;
      }
    } catch (e) {
      console.error('[network-health] failed to subscribe to relay changes:', e);
    }
  });

  onDestroy(() => {
    destroyed = true;
    if (unlisten) unlisten();
    if (unlistenRelays) unlistenRelays();
    if (startupRetryHandle) clearInterval(startupRetryHandle);
    if (nowTimer !== null) {
      clearInterval(nowTimer);
      nowTimer = null;
    }
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
  {:else}
    {#if !snap.myNetwork}
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

      {#if snap.dialStatus}
        {@const dial = snap.dialStatus}
        {@const recentHits = [...dial.recent].sort(
          (a, b) => b.capturedAtMs - a.capturedAtMs,
        )}
        <section class="dynamic-dials" data-testid="nh-dynamic-dials">
          <h2>Dynamic dials</h2>
          <p class="muted dial-explain">
            Proactive iroh dials to peers learned mid-session.
          </p>
          <ul class="dial-counters">
            <li data-testid="nh-dial-attempts">
              Attempts: <strong>{dial.attempts}</strong>
            </li>
            <li class="dial-ok" data-testid="nh-dial-succeeded">
              Succeeded: <strong>{dial.succeeded}</strong>
            </li>
            <li class="dial-fail" data-testid="nh-dial-failed">
              Failed: <strong>{dial.failed}</strong>
            </li>
            <li class="muted" data-testid="nh-dial-skipped">
              Skipped (dup): <strong>{dial.skippedDuplicate}</strong>
            </li>
          </ul>
          {#if recentHits.length === 0}
            <p class="muted" data-testid="nh-dial-empty">No dynamic dials yet.</p>
          {:else}
            <ul class="dial-recent">
              {#each recentHits as hit (`${hit.capturedAtMs}-${hit.nodeIdShort}-${hit.ownerShort}`)}
                <li data-testid="nh-dial-hit">
                  {hit.outcome === 'succeeded' ? '✓' : '✗'}
                  <code>{hit.nodeIdShort}</code>
                  <span class="muted">owner {hit.ownerShort}</span>
                  <span class="muted"
                    >{Math.floor((Date.now() - hit.capturedAtMs) / 1000)}s ago</span
                  >
                </li>
              {/each}
            </ul>
          {/if}
        </section>
      {/if}

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

    <!-- pkarr-relays renders whenever snap exists — independent of myNetwork.
         Moved out of the inner myNetwork {:else} so relay health is visible
         during startup (Cursor Bugbot round-4 Medium finding). -->
    <section class="pkarr-relays" data-testid="nh-pkarr-relays">
      <h2>Discovery (pkarr) relays</h2>
      {#if (snap.pkarrStatus.relays ?? []).length === 0}
        <p class="muted" data-testid="nh-relays-empty">No relays configured.</p>
      {:else}
        <ul>
          {#each snap.pkarrStatus.relays ?? [] as relay (relay.url)}
            <li data-testid="nh-relay-row">
              <code>{relay.url}</code>
              {#if relay.state.kind === 'healthy'}
                <span class="badge badge-healthy" data-testid="nh-relay-badge">Healthy</span>
              {:else}
                <span class="badge badge-cooling" data-testid="nh-relay-badge"
                  >Cooling down ({Math.max(
                    0,
                    Math.ceil((relay.state.untilMs - now) / 1000),
                  )}s)</span
                >
              {/if}
              {#if relay.lastOutcome && relay.lastOutcome.kind !== 'success'}
                <span class="muted" data-testid="nh-relay-last-error"
                  >{relayOutcomeLabel(relay.lastOutcome)}</span
                >
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </section>
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
  .dial-counters {
    list-style: none;
    padding-left: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 1rem;
  }
  .dial-ok {
    color: green;
  }
  .dial-fail {
    color: crimson;
  }
  .dial-recent {
    list-style: none;
    padding-left: 0;
    font-family: monospace;
  }
  .badge {
    display: inline-block;
    padding: 1px 6px;
    border-radius: 3px;
    font-size: 11px;
    font-weight: 600;
    margin-left: 6px;
  }
  .badge-healthy {
    background: #1a4a1a;
    color: #5cb85c;
  }
  .badge-cooling {
    background: #4a3a00;
    color: #f0a020;
  }
  .pkarr-relays ul {
    list-style: none;
    padding-left: 0;
  }
  .pkarr-relays li {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.2rem 0;
    flex-wrap: wrap;
  }
</style>
