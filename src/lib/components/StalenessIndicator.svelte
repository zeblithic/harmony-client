<script lang="ts">
  let {
    score,
    pinned = false,
  }: {
    score: number;
    pinned?: boolean;
  } = $props();

  let level = $derived.by(() => {
    if (pinned) return 'none';
    if (score >= 0.85) return 'red';
    if (score >= 0.6) return 'orange';
    if (score >= 0.3) return 'yellow';
    return 'none';
  });

  let shouldShow = $derived(level !== 'none');

  let ariaLabel = $derived.by(() => {
    if (level === 'yellow') return 'Slightly stale';
    if (level === 'orange') return 'Moderately stale';
    if (level === 'red') return 'Very stale';
    return '';
  });
</script>

{#if shouldShow}
  <span
    class="staleness-dot"
    class:yellow={level === 'yellow'}
    class:orange={level === 'orange'}
    class:red={level === 'red'}
    aria-label={ariaLabel}
  ></span>
{/if}

<style>
  .staleness-dot {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
    vertical-align: middle;
  }

  .staleness-dot.yellow {
    background: var(--text-warning);
  }

  .staleness-dot.orange {
    background: var(--cat-orange);
  }

  .staleness-dot.red {
    background: var(--danger-muted);
  }
</style>
