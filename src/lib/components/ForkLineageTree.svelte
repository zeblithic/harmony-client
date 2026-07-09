<script lang="ts">
  import type {
    CommunityLineageDto,
    ForkDescendantDto,
  } from '../types';
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
    /** Set of locally-known SpaceIds (hex) — used to gate clickability of
     *  ancestor / descendant rows. Caller typically passes the OwnerState
     *  Space-id set (e.g., the current NavService snapshot). */
    localNavIds?: Set<string>;
    /** ZEB-287 R3-1: resolves a hex SpaceId to its display name from the
     *  caller's local nav state (typically NavService.getCommunityNameBySpaceId).
     *  Returns null/undefined if the caller can't resolve, in which case we
     *  fall back to truncated hex (defense-in-depth). */
    resolveLocalName?: (spaceId: string) => string | null | undefined;
    /** Callback fired when a clickable row is activated. */
    onNavigate?: (spaceId: string) => void;
  } = $props();

  const MAX_DISPLAYED_LINEAGE = 16;

  let ancestorRows = $derived.by(() => {
    const rows = lineage.parentLineage;
    if (rows.length <= MAX_DISPLAYED_LINEAGE) return { rows, truncated: 0 };
    const truncated = rows.length - MAX_DISPLAYED_LINEAGE;
    return { rows: rows.slice(truncated), truncated };
  });

  let hasAnyForks = $derived(
    lineage.parentLineage.length > 0 || descendants.length > 0,
  );

  let selfDepth = $derived(
    ancestorRows.rows.length + 1 + (ancestorRows.truncated > 0 ? 1 : 0),
  );

  // Self sub-line: honest — "root" when this community has no parent, else
  // the fork date when we have it. No invented "founded" date.
  let selfSub = $derived(
    lineage.forkedFrom == null
      ? 'root'
      : lineage.forkedAtWallMs != null
        ? 'forked ' + formatDate(lineage.forkedAtWallMs)
        : 'forked',
  );

  function formatDate(wallMs: number | null | undefined): string {
    if (wallMs == null) return '';
    return new Date(wallMs).toISOString().slice(0, 10);
  }

  function truncSpaceId(hex: string): string {
    return '0x' + hex.slice(0, 8) + '…';
  }

  // Blank frozen names (legacy/corrupted snapshots) fall back to the real
  // space-id rather than rendering an empty card — matches the descendant
  // rows and the team nonEmpty() idiom (PR #270/#337).
  function nameOrId(name: string, spaceId: string): string {
    return nonEmpty(name) ?? truncSpaceId(spaceId);
  }

  function initial(text: string): string {
    return (text.trim().charAt(0) || '⑂').toUpperCase();
  }

  function handleClick(spaceId: string): void {
    onNavigate?.(spaceId);
  }
</script>

<ul role="tree" class="fork-lineage-tree" aria-label="Fork lineage tree">
  {#if ancestorRows.truncated > 0}
    <li
      role="treeitem"
      aria-level={1}
      aria-selected={false}
      class="lineage-row lineage-truncation"
    >
      &hellip;and {ancestorRows.truncated} earlier ancestors
    </li>
  {/if}

  {#each ancestorRows.rows as entry, i (entry.spaceId)}
    {@const depth = i + 1 + (ancestorRows.truncated > 0 ? 1 : 0)}
    {@const known = localNavIds.has(entry.spaceId)}
    <li
      role="treeitem"
      aria-level={depth}
      aria-selected={false}
      class="lineage-row lineage-ancestor"
      style="padding-left: calc({depth} * 1.5rem);"
    >
      {#if known}
        <button class="lineage-card lineage-clickable" onclick={() => handleClick(entry.spaceId)}>
          <span class="node-avatar" aria-hidden="true">{initial(nameOrId(entry.name, entry.spaceId))}</span>
          <span class="node-body">
            <span class="node-name">&#x21B3; {nameOrId(entry.name, entry.spaceId)}</span>
            {#if entry.forkedAtWallMs != null}<span class="node-sub">forked {formatDate(entry.forkedAtWallMs)}</span>{/if}
            {#if entry.reason}<span class="node-reason">“{entry.reason}”</span>{/if}
          </span>
        </button>
      {:else}
        <span class="lineage-card lineage-unknown" title="You're not a member of this community.">
          <span class="node-avatar" aria-hidden="true">{initial(nameOrId(entry.name, entry.spaceId))}</span>
          <span class="node-body">
            <span class="node-name">&#x21B3; {nameOrId(entry.name, entry.spaceId)}</span>
            {#if entry.forkedAtWallMs != null}<span class="node-sub">forked {formatDate(entry.forkedAtWallMs)}</span>{/if}
            {#if entry.reason}<span class="node-reason">“{entry.reason}”</span>{/if}
          </span>
        </span>
      {/if}
    </li>
  {/each}

  <li
    role="treeitem"
    aria-level={selfDepth}
    aria-current="page"
    aria-selected={true}
    class="lineage-row lineage-self"
    style="padding-left: calc({selfDepth} * 1.5rem);"
  >
    <span class="lineage-card self-card">
      <span class="node-avatar node-avatar-self" aria-hidden="true">{initial(nameOrId(lineage.selfName, lineage.selfSpaceId))}</span>
      <span class="node-body">
        <span class="node-name">{nameOrId(lineage.selfName, lineage.selfSpaceId)}</span>
        <span class="node-sub">{selfSub}</span>
        {#if lineage.forkReason}<span class="node-reason">“{lineage.forkReason}”</span>{/if}
      </span>
      <span class="lineage-badge you-here"><span aria-hidden="true">&#x25CF;</span> You are here</span>
    </span>
  </li>

  {#each descendants as desc (desc.forkSpaceId)}
    {@const known = desc.locallyKnown && localNavIds.has(desc.forkSpaceId)}
    {@const resolvedName = known ? resolveLocalName?.(desc.forkSpaceId) : null}
    {@const display = resolvedName ?? truncSpaceId(desc.forkSpaceId)}
    {@const forker = desc.forkerDisplayName ?? 'an unknown member'}
    <li
      role="treeitem"
      aria-level={selfDepth + 1}
      aria-selected={false}
      class="lineage-row lineage-descendant"
      style="padding-left: calc({selfDepth + 1} * 1.5rem);"
    >
      {#if known}
        <button class="lineage-card lineage-clickable card-member" onclick={() => handleClick(desc.forkSpaceId)}>
          <span class="node-avatar" aria-hidden="true">{initial(display)}</span>
          <span class="node-body">
            <span class="node-name">&#x21B3; {display}</span>
            <span class="node-sub">forked {formatDate(desc.forkedAtWallMs)} · by {forker}</span>
            {#if desc.reason}<span class="node-reason">“{desc.reason}”</span>{/if}
          </span>
          <span class="lineage-badge member"><span aria-hidden="true">&#x2713;</span> Member</span>
        </button>
      {:else}
        <span class="lineage-card lineage-unknown" title="You're not a member of this fork.">
          <span class="node-avatar" aria-hidden="true">{initial(display)}</span>
          <span class="node-body">
            <span class="node-name">&#x21B3; {display}</span>
            <span class="node-sub">forked {formatDate(desc.forkedAtWallMs)} · by {forker}</span>
            {#if desc.reason}<span class="node-reason">“{desc.reason}”</span>{/if}
          </span>
          <span class="lineage-badge not-joined">not joined</span>
        </span>
      {/if}
    </li>
  {/each}

  {#if !hasAnyForks}
    <li class="lineage-empty-hint" aria-hidden="true">(no forks yet)</li>
  {/if}
</ul>

<style>
  .fork-lineage-tree {
    position: relative;
    list-style: none;
    padding: 0;
    margin: 0.5rem 0;
    font-size: 0.9rem;
  }
  /* Lineage spine — a faint clay rail that reads the rows as a genealogy. */
  .fork-lineage-tree::before {
    content: '';
    position: absolute;
    top: 0.75rem;
    bottom: 0.75rem;
    left: 0.7rem;
    width: 1px;
    background: color-mix(in srgb, var(--gov-clay) 28%, transparent);
  }
  .lineage-row {
    padding: 0.2rem 0;
  }
  .lineage-card {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    box-sizing: border-box;
    padding: 8px 10px;
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 11px;
    box-shadow: var(--shadow-e1);
    text-align: left;
    font: inherit;
    color: var(--text-primary);
  }
  .self-card {
    border: 2px solid var(--accent);
    background: var(--surface-highlight);
  }
  .card-member {
    border-color: var(--primary-border);
  }
  .node-avatar {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 30px;
    border-radius: 9px;
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    font-family: var(--font-display);
    font-weight: 600;
    font-size: 0.9rem;
  }
  .node-avatar-self {
    background: var(--accent);
    color: var(--on-accent);
  }
  .node-body {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
  }
  .node-name {
    font-family: var(--font-display);
    font-size: 0.95rem;
    color: var(--text-primary);
  }
  .node-sub {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--faint);
  }
  .node-reason {
    /* ZEB-649: the quoted fork "why" — clay lead per the Commons fork
       accent (structural, never a dispute signal). */
    font-size: 0.75rem;
    font-style: italic;
    color: var(--gov-clay-deep);
  }
  .lineage-badge {
    margin-left: auto;
    flex: 0 0 auto;
    font-family: var(--font-mono);
    font-size: 0.62rem;
    font-weight: 600;
    letter-spacing: 0.03em;
    white-space: nowrap;
  }
  .lineage-badge.you-here,
  .lineage-badge.member {
    color: var(--primary-deep);
    background: var(--primary-soft);
    padding: 2px 8px;
    border-radius: 20px;
  }
  .lineage-badge.not-joined {
    color: var(--faint);
  }
  button.lineage-clickable {
    cursor: pointer;
  }
  button.lineage-clickable:hover {
    border-color: var(--accent);
  }
  .lineage-unknown {
    cursor: default;
  }
  .lineage-empty-hint {
    color: var(--text-muted);
    font-style: italic;
    padding-left: 1.5rem;
  }
  .lineage-truncation {
    color: var(--text-muted);
    font-style: italic;
    padding-left: 0;
  }
</style>
