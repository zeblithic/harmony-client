<script lang="ts">
  /**
   * ZEB-607 — Commons tally bar (spec D2). Flex segments on
   * --tally-track; each fill animates width .35s ease (the design's
   * only sanctioned motion). `token` is a CSS custom-property NAME
   * ('--vote-for') resolved at render via var().
   */
  let {
    segments,
    height = 8,
    label,
  }: {
    segments: Array<{ pct: number; token: string }>;
    height?: number;
    label?: string;
  } = $props();

  function clamp(pct: number): number {
    return Math.max(0, Math.min(100, pct));
  }
</script>

<div class="tally-track" style="height: {height}px" role="img" aria-label={label ?? 'Tally'}>
  {#each segments as seg, i (i)}
    <span class="tally-fill" style="width: {clamp(seg.pct)}%; background: var({seg.token})"></span>
  {/each}
</div>

<style>
  .tally-track {
    display: flex;
    background: var(--tally-track);
    border-radius: 4px;
    overflow: hidden;
  }
  .tally-fill {
    height: 100%;
    transition: width 0.35s ease;
  }
</style>
