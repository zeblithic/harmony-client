<script lang="ts">
  let {
    path,
    onNavigate,
    onSegmentDrop,
    dragMime,
  }: {
    path: Array<{ cid: string | null; name: string }>;
    onNavigate: (cid: string | null) => void;
    /** ZEB-162: drop handler wired by FileBrowser. Receives the
     *  breadcrumb segment index (0 = root sentinel, ≥1 = nested). */
    onSegmentDrop?: (e: DragEvent, segIdx: number) => void;
    /** Custom MIME type used to gate dragover acceptance. */
    dragMime?: string;
  } = $props();

  function handleDragOver(e: DragEvent) {
    if (!onSegmentDrop) return;
    // Accept drops by preventing default. We do NOT gate on the MIME
    // here because dragover events don't expose getData() — that's only
    // available on the drop event itself. The drop handler will validate.
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
  }

  function handleDrop(e: DragEvent, segIdx: number) {
    if (!onSegmentDrop) return;
    if (dragMime && !e.dataTransfer?.types?.includes(dragMime)) return;
    onSegmentDrop(e, segIdx);
  }
</script>

<nav class="breadcrumbs" aria-label="File navigation">
  {#each path as segment, i}
    {#if i > 0}
      <span class="separator" aria-hidden="true">/</span>
    {/if}
    <button
      class="breadcrumb-segment"
      onclick={() => onNavigate(segment.cid)}
      ondragover={onSegmentDrop ? handleDragOver : undefined}
      ondrop={onSegmentDrop ? (e) => handleDrop(e, i) : undefined}
    >{segment.name}</button>
  {/each}
</nav>

<style>
  .breadcrumbs {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px;
    padding: 8px 12px;
    color: var(--text-secondary);
  }

  .separator {
    color: var(--text-muted);
    font-size: 0.85rem;
    user-select: none;
  }

  .breadcrumb-segment {
    background: none;
    border: none;
    padding: 2px 4px;
    font: inherit;
    font-size: 0.85rem;
    color: var(--text-secondary);
    cursor: pointer;
    border-radius: 2px;
  }

  .breadcrumb-segment:hover {
    color: var(--text-primary);
    text-decoration: underline;
  }

  .breadcrumb-segment:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .breadcrumb-segment:last-child {
    color: var(--text-primary);
    font-weight: 600;
  }
</style>
