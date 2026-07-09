<script lang="ts">
  /**
   * ZEB-663: synthetic "＋ add channel" row rendered by NavTree inside an
   * expanded community when the viewer can manage its channels — NOT a
   * NavNode. Mirrors ProposalsNavRow's row anatomy + keyboard model.
   */
  let {
    communityId,
    indent = 0,
    onAdd,
  }: {
    communityId: string;
    indent?: number;
    onAdd?: (communityId: string) => void;
  } = $props();

  let paddingLeft = $derived(indent * 4 + 8);

  function activate(e: MouseEvent | KeyboardEvent) {
    e.stopPropagation();
    onAdd?.(communityId);
  }
</script>

<div
  class="add-channel-row"
  role="button"
  tabindex="0"
  data-testid="add-channel-row-{communityId}"
  onclick={activate}
  onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') activate(e); }}
>
  <span class="row-content" style="padding-left: {paddingLeft}px">
    <span class="add-glyph" aria-hidden="true">＋</span>
    <span class="row-label">add channel</span>
  </span>
</div>

<style>
  .add-channel-row {
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
  .add-channel-row:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }
  .row-content {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: 1;
    min-width: 0;
  }
  .add-glyph {
    flex-shrink: 0;
    width: 16px;
    text-align: center;
    color: var(--text-muted);
  }
  .row-label {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
