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
  import { relayStatusLabel } from '../relay-status-label';
  import { peerBadge, stalenessLabel } from '../networkHealthStaleness';
  import DiagnosticExportModal from './DiagnosticExportModal.svelte';
  import NetworkStatusPill from './NetworkStatusPill.svelte';

  function relayOutcomeLabel(outcome: RelayOutcome): string {
    if (outcome.kind === 'timeout') return 'Last error: timeout';
    if (outcome.kind === 'transport') return 'Last error: transport';
    if (outcome.kind === 'http') return `Last error: http ${outcome.status}`;
    return '';
  }

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

  // ZEB-804 (PR #566 review): steady-state refetch while mounted. The
  // staleness tier and the self-test overlay TTL are computed server-side AT
  // SNAPSHOT TIME, so a panel that refetches only on mount + events would
  // freeze a quiet peer's badge at its mount-time tier and let an expired
  // self-test overlay linger. 30s matches the backend's own liveness tick.
  let steadyRefetchHandle: ReturnType<typeof setInterval> | null = null;

  async function refresh(): Promise<void> {
    try {
      const s = await fetchSnapshot();
      // ZEB-871: refresh() is driven by the 30s steady-refetch interval, the
      // startup retry, and two event listeners, so an in-flight call can settle
      // after onDestroy. Guard the $state write (the catch writes no $state, so
      // it needs none). Post-teardown writes are silent in Svelte 5 — this is
      // defensive consistency with the FriendsPanel (ZEB-793) idiom.
      if (destroyed) return;
      snap = s;
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

  // ZEB-450: manual re-check from the transport-disabled banner. Auto-retry is
  // suppressed while transport is disabled (recovery needs a node restart, so
  // polling the same dead session forever is wasteful), but the user still needs
  // a way to pick up a recovery — e.g. after restarting the node with a keychain
  // or HARMONY_PASSPHRASE set. If the re-check clears the reason but transport is
  // still coming up (myNetwork absent), re-enter the normal startup polling so
  // the view converges without further clicks. (Qodo: banner could otherwise
  // stale if no network-health-changed event fires after recovery.)
  async function recheckTransport(): Promise<void> {
    startupRetryElapsedMs = 0;
    await refresh();
    if (!snap?.transportDisabledReason && !snap?.myNetwork) startStartupRetry();
  }

  function startStartupRetry(): void {
    // ZEB-871: never arm the retry interval on a torn-down component — covers
    // both the onMount continuation and the user-driven recheckTransport() path.
    if (destroyed) return;
    if (startupRetryHandle) return;
    startupRetryHandle = setInterval(async () => {
      startupRetryElapsedMs += 2000;
      await refresh();
      // ZEB-450: stop once transport is up, the budget elapses, OR transport is
      // disabled this session (a restart is required — retrying is pointless).
      if (snap?.myNetwork || snap?.transportDisabledReason || startupRetryElapsedMs >= 30000) {
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
    // ZEB-804 (PR #566 review): register the steady-state refetch before the
    // first await so an unmount mid-`refresh()` cannot leak it (cleared in
    // onDestroy, same lifecycle as nowTimer/startupRetryHandle).
    steadyRefetchHandle = setInterval(() => {
      if (!destroyed) void refresh();
    }, 30_000);
    await refresh();
    // ZEB-871: if we unmounted during the await, onDestroy has already run.
    // Bail before registering any further intervals/listeners below, or they
    // leak (onDestroy cleared only what existed when it ran).
    if (destroyed) return;
    // ZEB-450: don't auto-retry when transport is disabled this session — it
    // won't recover without a restart, so the banner (not a spinner) is shown.
    if (!snap?.myNetwork && !snap?.transportDisabledReason) startStartupRetry();
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
    if (steadyRefetchHandle !== null) {
      clearInterval(steadyRefetchHandle);
      steadyRefetchHandle = null;
    }
    if (nowTimer !== null) {
      clearInterval(nowTimer);
      nowTimer = null;
    }
  });

  // ZEB-804: badge derivation lives in the pure helpers
  // (networkHealthStaleness.ts) so the dark-forces-⚠ override and the age
  // annotation are unit-testable without a component render.
  function peerStatusIcon(p: PeerHealth): string {
    return peerBadge(p.connectionMode, p.staleness ?? null);
  }

  // ZEB-804: traffic-evidence age for the staleness annotation. `null` when
  // the peer has no traffic evidence ever (the incident shape — the label
  // then reads "no traffic observed" with no age).
  function trafficAgeMs(p: PeerHealth): number | null {
    const traffic = p.lastTrafficMs ?? null;
    return traffic === null ? null : Date.now() - traffic;
  }

  // ZEB-804: '' when fresh / pre-field snapshot; "quiet for Xm" /
  // "no traffic for Xm" otherwise.
  function peerStalenessAnnotation(p: PeerHealth): string {
    return stalenessLabel(p.staleness ?? null, trafficAgeMs(p));
  }

  // ZEB-622: hover text that disambiguates the shared ⚠ icon (relay vs
  // degraded) and names each state for accessibility.
  // ZEB-623: when the peer is protocol-incompatible, prepend the reason so the
  // status hover explains the more severe failure first.
  // ZEB-804: non-fresh staleness appends its annotation so the hover explains
  // why a connected-looking peer carries the warn badge.
  function peerStatusTitle(p: PeerHealth): string {
    const base =
      p.connectionMode === 'direct'
        ? 'Direct connection'
        : p.connectionMode === 'relay'
          ? 'Relay-mediated connection'
          : p.connectionMode === 'degraded'
            ? 'Link up — no selected path yet (degraded)'
            : 'No connection';
    const annotation = peerStalenessAnnotation(p);
    const withStaleness = annotation ? `${base} — ${annotation}` : base;
    if (p.protocolIncompatReason) {
      return `Incompatible protocol: ${p.protocolIncompatReason} — ${withStaleness}`;
    }
    return withStaleness;
  }

  // ZEB-622: recent dial-ring markers. Dial outcomes succeeded/failed plus the
  // reconnect-supervisor transitions reconnected/retrying/dormant.
  function dialHitIcon(outcome: string): string {
    switch (outcome) {
      case 'succeeded':
        return '✓';
      case 'reconnected':
        return '↻';
      case 'retrying':
        return '…';
      case 'dormant':
        return '💤';
      case 'failed':
      default:
        return '✗';
    }
  }
</script>

<div class="network-health" data-testid="network-health-root">
  <h1>Network Health</h1>

  {#if !snap}
    <p data-testid="nh-initial-loading">Loading…</p>
  {:else}
    {#if snap.transportDisabledReason}
      <!-- ZEB-450: iroh transport failed to come up this session. Surface it
           loudly here instead of leaving it in a boot log line. A restart is
           required (with a keychain or HARMONY_PASSPHRASE[_FILE] configured),
           so the "starting up…" auto-retry below is intentionally suppressed. -->
      <section class="transport-disabled" data-testid="nh-transport-disabled" role="alert">
        <h2>⚠ This node can’t network</h2>
        <p class="reason" data-testid="nh-transport-disabled-reason">
          {snap.transportDisabledReason}
        </p>
        <p class="muted">
          Networking is off for this session. Restart with the OS keychain
          available, or set <code>HARMONY_PASSPHRASE</code> /
          <code>HARMONY_PASSPHRASE_FILE</code> (and avoid
          <code>HARMONY_DISABLE_KEYCHAIN</code> on a real launch). See
          <code>docs/headless-install.md</code>.
        </p>
        <button onclick={recheckTransport} data-testid="nh-transport-recheck">
          Check again
        </button>
      </section>
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
                <span
                  class="peer-status-icon"
                  title={peerStatusTitle(p)}
                  data-testid="nh-peer-icon">{peerStatusIcon(p)}</span
                >
                <strong>{redactAddr(p.ownerAddr, false)}</strong>
                <span>{p.connectionMode}</span>
                {#if p.protocolIncompatReason}
                  <!-- ZEB-623: loud per-peer signal that the tunnel-v2 hello
                       negotiation could not agree a compatible protocol. Scaled-
                       down sibling of the transport-disabled role="alert" banner. -->
                  <NetworkStatusPill
                    variant="incompat"
                    label="⚠ incompatible"
                    role="alert"
                    title={p.protocolIncompatReason}
                    data-testid="nh-peer-incompat"
                  />
                {/if}
                {#if p.rttMs !== null}<span>{p.rttMs}ms</span>{/if}
                {#if p.staleness === 'quiet' || p.staleness === 'dark'}
                  <!-- ZEB-804 (spec §6): mode/rtt above are the LAST CONFIRMED
                       claim, not live truth — qualify them and say how long
                       traffic has been silent. -->
                  <span class="muted" data-testid="nh-peer-staleness"
                    >(last confirmed — {peerStalenessAnnotation(p)})</span
                  >
                {/if}
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
            <!-- ZEB-620/622: live per-peer transport-state tally from the
                 reconnect supervisor (folded into the dial summary). -->
            <li class="dial-peer-states" data-testid="nh-dial-peer-states">
              Peers: <strong>{dial.connected ?? 0}</strong> connected ·
              <strong>{dial.retrying ?? 0}</strong> retrying ·
              <strong>{dial.dormant ?? 0}</strong> dormant
            </li>
          </ul>
          {#if recentHits.length === 0}
            <p class="muted" data-testid="nh-dial-empty">No dynamic dials yet.</p>
          {:else}
            <ul class="dial-recent">
              {#each recentHits as hit (`${hit.capturedAtMs}-${hit.nodeIdShort}-${hit.ownerShort}`)}
                <li data-testid="nh-dial-hit">
                  {dialHitIcon(hit.outcome)}
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
              <NetworkStatusPill
                variant={relay.state.kind === 'healthy' ? 'healthy' : 'cooling'}
                label={relayStatusLabel(relay, now)}
                data-testid="nh-relay-badge"
              />
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
  /* ZEB-651: Commons card anatomy for the content sections. */
  .my-network,
  .peers,
  .dynamic-dials,
  .self-test,
  .pkarr-relays {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
    padding: 16px;
    margin-bottom: 16px;
  }
  .network-health h1,
  .network-health h2 {
    font-family: var(--font-display);
  }
  .network-health code {
    font-family: var(--font-mono);
  }
  .muted {
    color: var(--color-text-secondary);
  }
  .status-reachable {
    color: var(--net-ok-deep);
  }
  .status-degraded {
    color: var(--net-warn-fg);
  }
  .status-unreachable {
    color: var(--net-danger-fg);
  }
  /* ZEB-450: persistent "node can't network" banner. */
  /* ZEB-651: Commons danger attention-card (left-border idiom). */
  .transport-disabled {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-left: 3px solid var(--net-danger-fg);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
    padding: 0.75rem 1rem;
    margin-bottom: 1rem;
  }
  .transport-disabled h2 {
    color: var(--net-danger-fg);
    margin-top: 0;
  }
  .transport-disabled .reason {
    font-family: var(--font-mono);
    word-break: break-word;
  }
  .info-hover {
    cursor: help;
    margin-left: 0.5em;
  }
  .error {
    color: var(--net-danger-fg);
  }
  .self-test-steps {
    list-style: none;
    padding-left: 0;
    font-family: var(--font-mono);
  }
  .dial-counters {
    list-style: none;
    padding-left: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 1rem;
  }
  .dial-ok {
    color: var(--net-ok-deep);
  }
  .dial-fail {
    color: var(--net-danger-fg);
  }
  .dial-recent {
    list-style: none;
    padding-left: 0;
    font-family: var(--font-mono);
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
