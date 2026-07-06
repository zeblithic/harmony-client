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
  let simNodes = $state<SimNode[]>([]);
  let simEdges = $state<SimEdge[]>([]);

  let simulation: Simulation<SimNode, SimEdge> | null = null;
  // Track the active load to detect stale completion (community switch
  // before previous load resolved).
  let latestLoadToken = 0;

  /** Map address → display name; falls back to short hex when the
   *  member is unknown (e.g. delegate present in graph but no longer
   *  in the member roster — race during a kick/leave). $derived.by
   *  (not $derived) because the expression is a multi-statement
   *  function literal, not a bare value — $derived(fn) stores the
   *  function itself and skips reactivity (Cursor R1). */
  let nameByAddr = $derived.by(() => {
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
      displayName: nameByAddr.get(addr) ?? `${addr.slice(0, 8)}…`,
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
        // d3 mutates SimNode objects in place; reassign the array
        // references with `.slice()` so Svelte 5 sees the change and
        // re-runs the keyed {#each} bodies (reusing existing keyed
        // DOM elements — only attributes re-evaluate).
        simNodes = simNodes.slice();
        simEdges = simEdges.slice();
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
    // CR R1: reset state synchronously on community swap so the
    // previous community's nodes don't briefly render against the new
    // community's name lookup while the new fetch is in flight.
    edges = [];
    simNodes = [];
    simEdges = [];
    if (simulation) {
      simulation.stop();
      simulation = null;
    }
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

  // Simulation teardown is handled by the $effect cleanup above —
  // Svelte 5 runs $effect cleanups on both community swap and
  // component destroy (Greptile R5 P2: prior onDestroy was redundant).

  /** Shorten an edge endpoint by NODE_RADIUS along the source→target
   *  vector so the arrowhead at the target end sits at the circle
   *  perimeter rather than the center (where it would be fully
   *  occluded by the target node — Cursor R5). Returns the original
   *  target coords if source and target coincide (degenerate). */
  function shortenedEndpoint(
    sx: number,
    sy: number,
    tx: number,
    ty: number,
  ): { x: number; y: number } {
    const dx = tx - sx;
    const dy = ty - sy;
    const len = Math.sqrt(dx * dx + dy * dy);
    if (len <= NODE_RADIUS) return { x: tx, y: ty };
    return { x: tx - (dx / len) * NODE_RADIUS, y: ty - (dy / len) * NODE_RADIUS };
  }

  // Tick-driven reactivity: the d3 tick handler reassigns simNodes /
  // simEdges with `.slice()` so the keyed {#each} re-renders attribute
  // values (existing elements are reused via the address key — no
  // full destroy/recreate per tick, which `{#key …}` would do).
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
      {#each simEdges as edge (edge.id)}
        {@const sx = (edge.source as SimNode).x ?? 0}
        {@const sy = (edge.source as SimNode).y ?? 0}
        {@const tx = (edge.target as SimNode).x ?? 0}
        {@const ty = (edge.target as SimNode).y ?? 0}
        {@const end = shortenedEndpoint(sx, sy, tx, ty)}
        <line
          class="dg-edge"
          x1={sx}
          y1={sy}
          x2={end.x}
          y2={end.y}
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
    color: var(--danger);
  }
  .dg-node {
    fill: var(--accent);
    stroke: var(--text-primary);
    stroke-width: 1;
  }
  .dg-node-local {
    fill: var(--warning);
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
