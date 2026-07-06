<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    open,
    jobId,
    completed,
    total,
    currentPath,
    cancelRequested,
    onCancel,
  }: {
    open: boolean;
    jobId: string | null;
    completed: number;
    total: number;
    currentPath: string;
    cancelRequested: boolean;
    onCancel: () => void;
  } = $props();

  const titleId = `folder-ingest-progress-title-${Math.random().toString(36).slice(2)}`;

  // Determinate when the pre-walk produced a non-zero total; indeterminate
  // when `total === -1` (pre-walk failed / not yet emitted) or `total === 0`
  // (no leaves to count — vacuously indeterminate so we don't divide by 0).
  let determinate = $derived(total > 0);
  let percent = $derived(determinate ? Math.min(100, Math.floor((completed / total) * 100)) : 0);

  // Truncate the current path in the middle so both the root segment and the
  // currently-walked leaf remain readable. Keeps the modal width stable for
  // very long paths without tooltip plumbing.
  function truncateMiddle(s: string, max = 64): string {
    if (s.length <= max) return s;
    const head = Math.ceil((max - 1) / 2);
    const tail = Math.floor((max - 1) / 2);
    return `${s.slice(0, head)}…${s.slice(s.length - tail)}`;
  }

  function handleCancelClick() {
    if (cancelRequested) return;
    onCancel();
  }

  // Modal owns Esc via trapFocus. When cancelRequested is true we set
  // canCancel=false so a second Esc tap doesn't re-fire onCancel (the spec
  // wants Esc to be a no-op once cancellation is in flight).
  let canCancel = $derived(!cancelRequested);
</script>

{#if open}
  <Modal onCancel={handleCancelClick} {canCancel} ariaLabelledby={titleId}>
    <h2 id={titleId} class="title">Ingesting folder…</h2>

    <div
      class="progress-bar"
      role="progressbar"
      aria-valuemin="0"
      aria-valuemax={determinate ? total : undefined}
      aria-valuenow={determinate ? completed : undefined}
      aria-busy={!determinate ? 'true' : undefined}
    >
      {#if determinate}
        <div class="progress-bar-fill" style="width: {percent}%"></div>
      {:else}
        <div class="progress-bar-fill indeterminate"></div>
      {/if}
    </div>

    <p class="counter">
      {#if determinate}
        {completed} of {total} files
      {:else}
        Counting files…
      {/if}
    </p>

    {#if currentPath}
      <p class="current-path" title={currentPath}>{truncateMiddle(currentPath)}</p>
    {/if}

    <div class="actions">
      <button
        type="button"
        class="cancel-btn"
        onclick={handleCancelClick}
        disabled={cancelRequested}
      >
        {cancelRequested ? 'Cancelling…' : 'Cancel'}
      </button>
    </div>

    {#if jobId}
      <span class="visually-hidden">Job id {jobId}</span>
    {/if}
  </Modal>
{/if}

<style>
  .title {
    color: var(--text-primary);
    font-size: 1.1rem;
    margin: 0 0 16px;
  }

  .progress-bar {
    width: 100%;
    height: 8px;
    background: var(--bg-tertiary);
    border-radius: 4px;
    overflow: hidden;
    margin-bottom: 12px;
  }

  .progress-bar-fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.2s;
  }

  .progress-bar-fill.indeterminate {
    width: 35%;
    animation: indeterminate-slide 1.4s ease-in-out infinite;
  }

  @keyframes indeterminate-slide {
    0% { transform: translateX(-100%); }
    50% { transform: translateX(180%); }
    100% { transform: translateX(-100%); }
  }

  .counter {
    color: var(--text-secondary);
    font-size: 0.9rem;
    margin: 0 0 8px;
  }

  .current-path {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--text-muted);
    word-break: break-all;
    margin: 0 0 16px;
    opacity: 0.85;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }

  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-primary);
    border: 1px solid var(--border);
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .cancel-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .cancel-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .visually-hidden {
    position: absolute; width: 1px; height: 1px; padding: 0;
    margin: -1px; overflow: hidden; clip: rect(0, 0, 0, 0);
    white-space: nowrap; border: 0;
  }
</style>
