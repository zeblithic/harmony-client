<script lang="ts">
  let {
    showTable,
    onToggleView,
    onRecenter,
    onZoomFit,
  }: {
    showTable: boolean;
    onToggleView?: () => void;
    onRecenter?: () => void;
    onZoomFit?: () => void;
  } = $props();

  let toggleLabel = $derived(showTable ? 'Graph view' : 'Table view');
</script>

<nav class="network-toolbar" aria-label="Network visualization controls">
  <button
    class="toolbar-btn"
    aria-label="Re-center"
    onclick={() => onRecenter?.()}
  >
    Re-center
  </button>

  <button
    class="toolbar-btn"
    aria-label="Zoom to fit"
    onclick={() => onZoomFit?.()}
  >
    Fit
  </button>

  <div class="spacer"></div>

  <button
    class="toolbar-btn"
    aria-label={toggleLabel}
    onclick={() => onToggleView?.()}
  >
    {toggleLabel}
  </button>
</nav>

<style>
  .network-toolbar {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 4px 8px;
    background: var(--bg-secondary, #2b2d31);
    border-bottom: 1px solid var(--bg-tertiary, #313338);
  }

  .spacer {
    flex: 1;
  }

  .toolbar-btn {
    padding: 4px 10px;
    border: none;
    border-radius: 4px;
    background: var(--bg-primary, #1e1f22);
    color: var(--text-secondary, #b5bac1);
    font: inherit;
    font-size: 12px;
    cursor: pointer;
  }

  .toolbar-btn:hover {
    background: var(--bg-tertiary, #313338);
    color: var(--text-primary, #f2f3f5);
  }

  .toolbar-btn:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
