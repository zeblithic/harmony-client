<script lang="ts">
  /**
   * ZEB-292 Phase 3: Tier 2 delegation graph visualization.
   *
   * Force-directed SVG layout via d3-force. Nodes = community members
   * appearing in any edge (delegator or delegate); edges = Delegate
   * relationships (delegator → delegate). The local user's node is
   * highlighted so they can find themselves at a glance.
   *
   * Why SVG instead of canvas (matching NetworkGraph.svelte's approach):
   * - Delegation graphs change slowly (per-event, not per-frame), so
   *   the canvas-redraw cost doesn't pay off.
   * - SVG elements are queryable from vitest tests without standing up
   *   a full canvas render harness.
   * - Acceptance criterion is ≤100 nodes; SVG handles that range with
   *   no perf concern.
   *
   * Refreshes on voting-delegation-changed events for this community
   * (any change to ANY user, not just the local user — the whole graph
   * may have shifted).
   */

  import { onDestroy } from 'svelte';
  import {
    forceSimulation,
    forceLink,
    forceManyBody,
    forceCenter,
    forceCollide,
    type Simulation,
    type SimulationNodeDatum,
    type SimulationLinkDatum,
  } from 'd3-force';
  import type { CommunityMember } from '../types';
  import type { VotingAdapter } from '../voting-adapter';
  import type { DelegationEdgeExport } from '../types/voting';

  const WIDTH = 480;
  const HEIGHT = 320;
  const NODE_RADIUS = 14;

  interface SimNode extends SimulationNodeDatum {
    address: string;
    displayName: string;
    isLocal: boolean;
  }

  interface SimEdge extends SimulationLinkDatum<SimNode> {
    id: string;
  }

  let {
    communityId,
    adapter,
    myAddr,
    communityMembers,
  }: {
    communityId: string;
    adapter: VotingAdapter;
    /** Caller's 32-char hex OwnerAddr — highlighted in the graph. */
    myAddr: string;
    /** Community member roster for display-name resolution. Members
     *  not appearing in any edge are NOT rendered (the graph would be
     *  noise otherwise — a 100-member community with 5 delegations
     *  should not show 95 isolated nodes). */
    communityMembers: CommunityMember[];
  } = $props();

  let edges = $state<DelegationEdgeExport[]>([]);
  let loadError = $state<string | null>(null);
  let loading = $state(true);
  /** Force-simulation tick counter — bumped each animation frame so the
   *  reactive template re-evaluates node + edge positions. Avoids
   *  reassigning the simNodes / simEdges arrays which would trigger a
   *  full Svelte reactive re-walk. */
  let tickN = $state(0);
  let simNodes = $state<SimNode[]>([]);
  let simEdges = $state<SimEdge[]>([]);

  let simulation: Simulation<SimNode, SimEdge> | null = null;
  // Track the active load to detect stale completion (community switch
  // before previous load resolved).
  let latestLoadToken = 0;

  /** Map address → display name; falls back to short hex when the
   *  member is unknown (e.g. delegate present in graph but no longer
   *  in the member roster — race during a kick/leave). */
  let nameByAddr = $derived(() => {
    const m = new Map<string, string>();
    for (const member of communityMembers) {
      m.set(member.address, member.displayName ?? `${member.address.slice(0, 8)}…`);
    }
    return m;
  });

  function rebuildSimulation(edgeList: DelegationEdgeExport[]) {
    if (simulation) {
      simulation.stop();
      simulation = null;
    }
    if (edgeList.length === 0) {
      simNodes = [];
      simEdges = [];
      return;
    }
    // Collect every address appearing in any edge.
    const addrSet = new Set<string>();
    for (const e of edgeList) {
      addrSet.add(e.from);
      addrSet.add(e.to);
    }
    const nodes: SimNode[] = Array.from(addrSet).map((addr) => ({
      address: addr,
      displayName: nameByAddr().get(addr) ?? `${addr.slice(0, 8)}…`,
      isLocal: addr === myAddr,
    }));
    const nodeByAddr = new Map(nodes.map((n) => [n.address, n]));
    const links: SimEdge[] = edgeList
      .filter((e) => nodeByAddr.has(e.from) && nodeByAddr.has(e.to))
      .map((e) => ({
        id: `${e.from}->${e.to}`,
        source: nodeByAddr.get(e.from)!,
        target: nodeByAddr.get(e.to)!,
      }));
    simNodes = nodes;
    simEdges = links;
    simulation = forceSimulation<SimNode, SimEdge>(nodes)
      .force(
        'link',
        forceLink<SimNode, SimEdge>(links)
          .id((d) => d.address)
          .distance(80)
          .strength(0.6),
      )
      .force('charge', forceManyBody().strength(-220))
      .force('center', forceCenter(WIDTH / 2, HEIGHT / 2))
      .force('collide', forceCollide(NODE_RADIUS + 4))
      .alpha(0.9)
      .on('tick', () => {
        tickN += 1;
      });
  }

  async function refetch() {
    const token = ++latestLoadToken;
    loading = true;
    try {
      const next = await adapter.listDelegations(communityId);
      if (token !== latestLoadToken) return;
      edges = next;
      loadError = null;
      rebuildSimulation(next);
    } catch (e) {
      if (token !== latestLoadToken) return;
      loadError = e instanceof Error ? e.message : String(e);
    } finally {
      if (token === latestLoadToken) loading = false;
    }
  }

  $effect(() => {
    const cid = communityId;
    let cancelled = false;
    void (async () => {
      if (cancelled) return;
      await refetch();
    })();
    const unsub = adapter.subscribeDelegationChanged((p) => {
      if (cancelled || p.communityId !== cid) return;
      void refetch();
    });
    return () => {
      cancelled = true;
      unsub();
      if (simulation) {
        simulation.stop();
        simulation = null;
      }
    };
  });

  onDestroy(() => {
    if (simulation) {
      simulation.stop();
      simulation = null;
    }
  });

  // Reactive position readout: tickN drives the recomputation. We read
  // from simNodes/simEdges (mutated in place by d3) on each tick.
  let _tickSignal = $derived(tickN);
</script>

<section class="delegation-graph" aria-label="Delegation graph">
  {#if loading && edges.length === 0}
    <p class="dg-status">Loading delegation graph…</p>
  {:else if loadError}
    <p class="dg-status dg-error" role="alert">Couldn't load: {loadError}</p>
  {:else if edges.length === 0}
    <p class="dg-status">No active delegations in this community.</p>
  {:else}
    <svg
      class="dg-svg"
      viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
      role="img"
      aria-label="Force-directed graph of delegation edges"
    >
      <defs>
        <marker
          id="dg-arrow"
          viewBox="0 0 10 10"
          refX="10"
          refY="5"
          markerWidth="6"
          markerHeight="6"
          orient="auto-start-reverse"
        >
          <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--text-secondary)" />
        </marker>
      </defs>
      {#key _tickSignal}
        {#each simEdges as edge (edge.id)}
          <line
            class="dg-edge"
            x1={(edge.source as SimNode).x ?? 0}
            y1={(edge.source as SimNode).y ?? 0}
            x2={(edge.target as SimNode).x ?? 0}
            y2={(edge.target as SimNode).y ?? 0}
            marker-end="url(#dg-arrow)"
          />
        {/each}
        {#each simNodes as node (node.address)}
          <g
            class="dg-node-group"
            transform={`translate(${node.x ?? 0}, ${node.y ?? 0})`}
          >
            <circle
              class={node.isLocal ? 'dg-node dg-node-local' : 'dg-node'}
              r={NODE_RADIUS}
            />
            <text class="dg-label" y={NODE_RADIUS + 12} text-anchor="middle">
              {node.displayName}
            </text>
          </g>
        {/each}
      {/key}
    </svg>
  {/if}
</section>

<style>
  .delegation-graph {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
  }
  .dg-svg {
    width: 100%;
    height: auto;
    background: var(--bg-primary);
    border-radius: 4px;
  }
  .dg-status {
    margin: 0;
    color: var(--text-secondary);
    font-size: 0.85rem;
  }
  .dg-error {
    color: var(--danger, #f87171);
  }
  .dg-node {
    fill: var(--accent, #4ade80);
    stroke: var(--text-primary);
    stroke-width: 1;
  }
  .dg-node-local {
    fill: var(--warning, #facc15);
    stroke-width: 2;
  }
  .dg-edge {
    stroke: var(--text-secondary);
    stroke-width: 1.5;
    opacity: 0.7;
  }
  .dg-label {
    font-size: 11px;
    fill: var(--text-primary);
    pointer-events: none;
  }
</style>
