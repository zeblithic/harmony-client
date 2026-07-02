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
    disabled={showTable}
    onclick={() => onRecenter?.()}
  >
    Re-center
  </button>

  <button
    class="toolbar-btn"
    aria-label="Zoom to fit"
    disabled={showTable}
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
    background: var(--bg-secondary);
    border-bottom: 1px solid var(--bg-tertiary);
  }

  .spacer {
    flex: 1;
  }

  .toolbar-btn {
    padding: 4px 10px;
    border: none;
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-secondary);
    font: inherit;
    font-size: 12px;
    cursor: pointer;
  }

  .toolbar-btn:disabled {
    opacity: 0.4;
    cursor: default;
  }

  .toolbar-btn:hover:not(:disabled) {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }

  .toolbar-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
