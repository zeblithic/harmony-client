<script lang="ts">
  import type {
    CommunityLineageDto,
    ForkDescendantDto,
  } from '../types';

  let {
    lineage,
    descendants = [],
    localNavIds = new Set<string>(),
    onNavigate,
  }: {
    lineage: CommunityLineageDto;
    descendants?: ForkDescendantDto[];
    /** Set of locally-known SpaceIds (hex) — used to gate clickability of
     *  ancestor / descendant rows. Caller typically passes the OwnerState
     *  Space-id set (e.g., the current NavService snapshot). */
    localNavIds?: Set<string>;
    /** Callback fired when a clickable row is activated. Caller routes
     *  the spaceId to its own community-navigation primitive (NavService). */
    onNavigate?: (spaceId: string) => void;
  } = $props();

  // Defensive display-cap (separate from the 16-deep build-time cap on
  // wire data — caller may receive a >16-deep chain from a future
  // protocol revision, which we render with a truncation marker rather
  // than failing).
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

  // Depth of the "you are here" row: one level below the deepest ancestor.
  // Truncation marker (when present) consumes one depth slot.
  let selfDepth = $derived(
    ancestorRows.rows.length + 1 + (ancestorRows.truncated > 0 ? 1 : 0),
  );

  function formatDate(wallMs: number | null | undefined): string {
    if (wallMs == null) return '';
    return new Date(wallMs).toISOString().slice(0, 10);
  }

  function truncSpaceId(hex: string): string {
    return '0x' + hex.slice(0, 8) + '…';
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
        <button class="lineage-clickable" onclick={() => handleClick(entry.spaceId)}>
          &#x21B3; {entry.name}{entry.forkedAtWallMs != null ? ' ' + formatDate(entry.forkedAtWallMs) : ''}
        </button>
      {:else}
        <span class="lineage-unknown" title="You're not a member of this community.">
          &#x21B3; {entry.name}{entry.forkedAtWallMs != null ? ' ' + formatDate(entry.forkedAtWallMs) : ''}
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
    You are here &#x2190; {lineage.selfName}
  </li>

  {#each descendants as desc (desc.forkSpaceId)}
    {@const known = desc.locallyKnown && localNavIds.has(desc.forkSpaceId)}
    {@const display = known ? desc.forkSpaceId : truncSpaceId(desc.forkSpaceId)}
    {@const forker = desc.forkerDisplayName ?? 'an unknown member'}
    <li
      role="treeitem"
      aria-level={selfDepth + 1}
      aria-selected={false}
      class="lineage-row lineage-descendant"
      style="padding-left: calc({selfDepth + 1} * 1.5rem);"
    >
      {#if known}
        <button class="lineage-clickable" onclick={() => handleClick(desc.forkSpaceId)}>
          &#x21B3; {display} {formatDate(desc.forkedAtWallMs)} by {forker}
        </button>
      {:else}
        <span class="lineage-unknown" title="You're not a member of this fork.">
          &#x21B3; {display} {formatDate(desc.forkedAtWallMs)} by {forker}
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
    list-style: none;
    padding: 0;
    margin: 0.5rem 0;
    font-size: 0.9rem;
  }
  .lineage-row {
    padding: 0.25rem 0;
  }
  .lineage-self {
    background: var(--surface-highlight, rgba(255, 200, 0, 0.1));
    font-weight: 600;
  }
  .lineage-ancestor,
  .lineage-descendant {
    color: var(--text-muted, #888);
  }
  .lineage-clickable {
    background: none;
    border: none;
    color: var(--text-link, #5c8fff);
    cursor: pointer;
    padding: 0;
    font: inherit;
    text-align: left;
  }
  .lineage-clickable:hover {
    text-decoration: underline;
  }
  .lineage-unknown {
    cursor: default;
  }
  .lineage-empty-hint {
    color: var(--text-muted, #999);
    font-style: italic;
    padding-left: 1.5rem;
  }
  .lineage-truncation {
    color: var(--text-muted, #888);
    font-style: italic;
    padding-left: 0;
  }
</style>
