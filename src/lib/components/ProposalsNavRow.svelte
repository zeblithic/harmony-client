<script lang="ts">
  /**
   * ZEB-606: synthetic "proposals" nav row rendered by NavTree inside each
   * expanded community — NOT a NavNode (NavService never sees it). Mirrors
   * NavNodeRow's row anatomy (16px icon cell + 6px gap, 4px-per-ancestor
   * indent) so it aligns with sibling channel rows, and its keyboard model
   * (role="button", Enter/Space activate).
   */
  let {
    communityId,
    indent = 0,
    count,
    active = false,
    onSelect,
  }: {
    communityId: string;
    /** Folder-ancestry depth of sibling channel rows (community rows are not
     *  folders, so children share the community's own ancestry length). */
    indent?: number;
    /** Active Tier-2 proposal count; undefined = not yet known (no badge). */
    count: number | undefined;
    active?: boolean;
    onSelect?: () => void;
  } = $props();

  let paddingLeft = $derived(indent * 4 + 8);

  function activate(e: MouseEvent | KeyboardEvent) {
    e.stopPropagation();
    onSelect?.();
  }
</script>

<div
  class="proposals-row"
  class:active
  role="button"
  tabindex="0"
  data-testid="proposals-row-{communityId}"
  onclick={activate}
  onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') activate(e); }}
>
  <span class="row-content" style="padding-left: {paddingLeft}px">
    <span class="gov-glyph" aria-hidden="true">⚖</span>
    <span class="row-label">proposals</span>
    {#if count !== undefined && count > 0}
      <span class="count-badge" aria-label="{count} open proposals">{count}</span>
    {/if}
  </span>
</div>

<style>
  .proposals-row {
    position: relative;
    display: flex;
    align-items: center;
    width: 100%;
    min-height: 32px;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 14px;
    cursor: pointer;
    text-align: left;
  }
  .proposals-row:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }
  .proposals-row.active {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }
  .row-content {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1;
    min-width: 0;
  }
  .gov-glyph {
    flex-shrink: 0;
    width: 16px;
    text-align: center;
    color: var(--gov-clay);
  }
  .row-label {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .count-badge {
    background: var(--gov-clay);
    color: var(--text-bright);
    font-family: var(--font-mono);
    font-size: 10px;
    font-weight: 700;
    padding: 1px 6px;
    border-radius: 9px;
    flex-shrink: 0;
    margin-right: 8px;
  }
</style>
