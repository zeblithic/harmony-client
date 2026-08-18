<script lang="ts">
  /**
   * ZEB-649: the 2D fork genealogy graph (Commons design Frame A), hosted
   * in a modal from the settings Forks section. Honest caterpillar
   * topology from `layoutGenealogy` — ancestor chain → self → descendant
   * fan; sibling branches are unknowable (list_community_forks is
   * Joined-gated) so they are not invented.
   *
   * Rendering follows the mock's own structure: one SVG connector layer
   * (clay elbows + label chips) under absolutely-positioned HTML card
   * BUTTONS — keeping nodes real buttons is the DelegationGraph lesson
   * (keyboard-operable, vitest-queryable). Clicking a card selects it
   * into the right-hand INSPECTING panel; navigation to locally-known
   * communities happens from the panel's Open action.
   *
   * Dropped from the mock per the ZEB-609 §0 ledger: member counts,
   * dispute/amicable edge coloring (clay is the structural fork accent,
   * never a dispute signal), founder signatures, forker display names
   * (pending ZEB-281).
   */
  import type { CommunityLineageDto, ForkDescendantDto } from '../types';
  import {
    layoutGenealogy,
    GENEALOGY_CARD_W,
    GENEALOGY_CARD_H,
    type GenealogyNode,
  } from '../fork-genealogy-layout';
  import { nonEmpty } from '../display-label';

  let {
    lineage,
    descendants = [],
    localNavIds = new Set<string>(),
    resolveLocalName,
    onNavigate,
  }: {
    lineage: CommunityLineageDto;
    descendants?: ForkDescendantDto[];
    localNavIds?: Set<string>;
    resolveLocalName?: (spaceId: string) => string | null | undefined;
    onNavigate?: (spaceId: string) => void;
  } = $props();

  let layout = $derived(layoutGenealogy(lineage, descendants));
  let selectedId = $state<string | null>(null);
  let selected = $derived(
    layout.nodes.find((n) => n.spaceId === (selectedId ?? lineage.selfSpaceId)) ??
      layout.nodes.find((n) => n.kind === 'self'),
  );

  function truncId(spaceId: string): string {
    return `0x${spaceId.slice(0, 8)}…`;
  }
  function displayName(node: GenealogyNode): string {
    if (node.kind === 'descendant') {
      return (
        (isKnown(node) ? nonEmpty(resolveLocalName?.(node.spaceId)) : null) ??
        truncId(node.spaceId)
      );
    }
    return nonEmpty(node.name) ?? truncId(node.spaceId);
  }
  function isKnown(node: GenealogyNode): boolean {
    if (node.kind === 'self') return true;
    if (node.kind === 'descendant') {
      return node.locallyKnown && localNavIds.has(node.spaceId);
    }
    return localNavIds.has(node.spaceId);
  }
  function initial(display: string): string {
    return (display.trim().charAt(0) || '⑂').toUpperCase();
  }
  // ZEB-946: intentionally left locale-default. This is a word-month + year
  // fork-age label ("Aug 2026") with no day, so the date-order preference (which
  // only reorders numeric day/month/year fields) has nothing to reorder, and
  // there is no clock component for the 12h/24h axis to touch.
  function monthYear(wallMs: number): string {
    return new Date(wallMs).toLocaleDateString(undefined, {
      month: 'short',
      year: 'numeric',
    });
  }
  function chipText(forkedAtWallMs: number | null, reason: string | null): string {
    const date = forkedAtWallMs != null ? monthYear(forkedAtWallMs) : null;
    const why =
      reason != null && reason.length > 40 ? `${reason.slice(0, 40)}…` : reason;
    if (date && why) return `⑂ ${date} · ${why}`;
    if (date) return `⑂ ${date}`;
    if (why) return `⑂ ${why}`;
    return '⑂';
  }
  const kindLabel: Record<GenealogyNode['kind'], string> = {
    ancestor: 'ancestor',
    self: 'this community',
    descendant: 'fork of this community',
  };
</script>

<div class="genealogy" data-testid="fork-genealogy">
  <div class="genealogy-stage-scroll">
    <div
      class="genealogy-stage"
      role="group"
      aria-label="Fork genealogy graph"
      style="width: {layout.width}px; height: {layout.height}px;"
    >
      <svg
        class="genealogy-connectors"
        width={layout.width}
        height={layout.height}
        viewBox="0 0 {layout.width} {layout.height}"
        aria-hidden="true"
      >
        {#each layout.edges as edge (edge.childSpaceId)}
          <path class="genealogy-edge" d={edge.path} />
        {/each}
      </svg>

      {#each layout.edges as edge (edge.childSpaceId)}
        <span
          class="genealogy-edge-chip"
          aria-hidden="true"
          style="left: {edge.labelX}px; top: {edge.labelY}px;"
        >{chipText(edge.forkedAtWallMs, edge.reason)}</span>
      {/each}

      {#each layout.nodes as node (node.spaceId)}
        {@const display = displayName(node)}
        <button
          class="genealogy-card"
          class:card-self={node.kind === 'self'}
          class:card-known={node.kind !== 'self' && isKnown(node)}
          class:card-selected={selected?.spaceId === node.spaceId}
          aria-pressed={selected?.spaceId === node.spaceId}
          style="left: {node.x}px; top: {node.y}px; width: {GENEALOGY_CARD_W}px; min-height: {GENEALOGY_CARD_H}px;"
          onclick={() => (selectedId = node.spaceId)}
        >
          <span class="card-avatar" aria-hidden="true">{initial(display)}</span>
          <span class="card-body">
            <span class="card-name">{display}</span>
            <span class="card-sub">
              {kindLabel[node.kind]}{node.forkedAtWallMs != null
                ? ` · forked ${monthYear(node.forkedAtWallMs)}`
                : ''}
            </span>
          </span>
          {#if node.kind === 'self'}
            <span class="card-badge badge-here"><span aria-hidden="true">●</span> You are here</span>
          {:else if isKnown(node)}
            <span class="card-badge badge-member"><span aria-hidden="true">✓</span> Member</span>
          {/if}
        </button>
      {/each}
    </div>
  </div>

  <aside class="genealogy-inspect" aria-label="Inspecting">
    {#if selected}
      {@const display = displayName(selected)}
      <span class="inspect-eyebrow">Inspecting</span>
      <div class="inspect-head">
        <span class="inspect-avatar" aria-hidden="true">{initial(display)}</span>
        <span class="inspect-title">
          <span class="inspect-name">{display}</span>
          <span class="inspect-sub">
            {kindLabel[selected.kind]}{selected.forkedAtWallMs != null
              ? ` · forked ${monthYear(selected.forkedAtWallMs)}`
              : ''}
          </span>
        </span>
      </div>

      {#if selected.reason}
        <p class="inspect-reason">“{selected.reason}”</p>
      {/if}

      {#if selected.kind === 'self'}
        <div class="inspect-stat">
          <span class="stat-label">Direct forks</span>
          <span class="stat-value">{descendants.length}</span>
        </div>
        {#if descendants.length > 0}
          <span class="inspect-eyebrow">Forks of this community</span>
          <ul class="inspect-fork-list">
            {#each descendants as d (d.forkSpaceId)}
              {@const node = layout.nodes.find((n) => n.spaceId === d.forkSpaceId)}
              <li class="inspect-fork-card">
                <span class="fork-card-name">
                  {node ? displayName(node) : truncId(d.forkSpaceId)}
                </span>
                <span class="fork-card-meta">⑂ {monthYear(d.forkedAtWallMs)} · by {d.forkerDisplayName ?? 'an unknown member'}</span>
                {#if d.reason}
                  <span class="fork-card-reason">“{d.reason}”</span>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
      {:else if isKnown(selected)}
        <button
          class="inspect-open"
          onclick={() => onNavigate?.(selected.spaceId)}
        >Open community <span aria-hidden="true">↗</span></button>
      {:else}
        <p class="inspect-unknown">You're not a member of this community.</p>
      {/if}
    {/if}
  </aside>
</div>

<style>
  .genealogy {
    display: flex;
    gap: 0;
    min-height: 320px;
    max-height: 70vh;
  }
  .genealogy-stage-scroll {
    flex: 1;
    overflow: auto;
    display: grid;
    place-items: center;
  }
  .genealogy-stage {
    position: relative;
    flex: 0 0 auto;
  }
  .genealogy-connectors {
    position: absolute;
    inset: 0;
  }
  .genealogy-edge {
    fill: none;
    stroke: color-mix(in srgb, var(--gov-clay) 35%, transparent);
    stroke-width: 2;
  }
  .genealogy-edge-chip {
    position: absolute;
    transform: translate(-50%, -50%);
    font-family: var(--font-mono);
    font-size: 0.62rem;
    color: var(--gov-clay-deep);
    background: var(--surface-raised);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 25%, transparent);
    border-radius: 999px;
    padding: 1px 8px;
    white-space: nowrap;
    max-width: 220px;
    overflow: hidden;
    text-overflow: ellipsis;
    pointer-events: none;
  }
  .genealogy-card {
    position: absolute;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 12px;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 11px;
    cursor: pointer;
    text-align: left;
    font: inherit;
    color: var(--text-primary);
  }
  .genealogy-card.card-known {
    border-color: var(--primary-border);
  }
  .genealogy-card.card-self {
    border: 2px solid var(--accent);
  }
  .genealogy-card.card-selected {
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 25%, transparent);
  }
  .genealogy-card:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .card-avatar {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    border-radius: 8px;
    background: var(--primary-soft);
    color: var(--primary-deep);
    font-weight: 600;
    font-size: 0.85rem;
  }
  .card-body {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .card-name {
    font-weight: 600;
    font-size: 0.85rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .card-sub {
    font-family: var(--font-mono);
    font-size: 0.62rem;
    color: var(--text-muted);
    white-space: nowrap;
  }
  .card-badge {
    margin-left: auto;
    flex: 0 0 auto;
    font-size: 0.62rem;
    border-radius: 999px;
    padding: 2px 7px;
    white-space: nowrap;
  }
  .badge-here {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }
  .badge-member {
    border: 1px solid var(--primary-border);
    color: var(--primary-deep);
  }
  .genealogy-inspect {
    flex: 0 0 250px;
    border-left: 1px solid var(--border);
    padding: 14px 16px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .inspect-eyebrow {
    font-family: var(--font-mono);
    font-size: 0.62rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .inspect-head {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .inspect-avatar {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 38px;
    height: 38px;
    border-radius: 10px;
    background: var(--primary-soft);
    color: var(--primary-deep);
    font-weight: 600;
  }
  .inspect-title {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .inspect-name {
    font-family: var(--font-display);
    font-weight: 600;
    font-size: 1rem;
  }
  .inspect-sub {
    font-family: var(--font-mono);
    font-size: 0.65rem;
    color: var(--text-muted);
  }
  .inspect-reason {
    margin: 0;
    font-size: 0.85rem;
    font-style: italic;
    color: var(--gov-clay-deep);
    border-left: 3px solid color-mix(in srgb, var(--gov-clay) 40%, transparent);
    padding-left: 10px;
  }
  .inspect-stat {
    display: flex;
    flex-direction: column;
    gap: 2px;
    border: 1px solid var(--border);
    border-radius: 9px;
    padding: 8px 12px;
    width: fit-content;
  }
  .stat-label {
    font-family: var(--font-mono);
    font-size: 0.6rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .stat-value {
    font-family: var(--font-display);
    font-size: 1.4rem;
    font-weight: 600;
  }
  .inspect-fork-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .inspect-fork-card {
    border: 1px solid var(--border);
    border-radius: 9px;
    padding: 8px 10px;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .fork-card-name {
    font-weight: 600;
    font-size: 0.82rem;
  }
  .fork-card-meta {
    font-family: var(--font-mono);
    font-size: 0.62rem;
    color: var(--text-muted);
  }
  .fork-card-reason {
    font-size: 0.78rem;
    font-style: italic;
    color: var(--gov-clay-deep);
  }
  .inspect-open {
    align-self: flex-start;
    background: var(--surface-raised);
    border: 1px solid var(--primary-border);
    color: var(--primary-deep);
    border-radius: 7px;
    padding: 6px 12px;
    font: inherit;
    font-size: 0.82rem;
    cursor: pointer;
  }
  .inspect-open:focus-visible,
  .genealogy-card:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .inspect-unknown {
    margin: 0;
    font-size: 0.8rem;
    color: var(--text-muted);
  }
</style>
