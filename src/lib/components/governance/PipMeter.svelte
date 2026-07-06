<script lang="ts">
  /**
   * ZEB-608 — Commons k-of-n pip meter (spec D2). Discrete quorum pips —
   * deliberately distinct from TallyBar (contiguous percentage fills):
   * a quorum is a count of people, not a percentage.
   */
  let {
    filled,
    total,
    label,
  }: {
    filled: number;
    total: number;
    label?: string;
  } = $props();

  // Degenerate inputs (0 admins mid-roster-load, quorum > admin count after
  // an admin leaves, NaN) must render a sane meter, never throw or paint
  // an impossible state: total >= 1, 0 <= filled <= total.
  let safeTotal = $derived(Number.isFinite(total) ? Math.max(1, Math.trunc(total)) : 1);
  let safeFilled = $derived(
    Number.isFinite(filled) ? Math.max(0, Math.min(safeTotal, Math.trunc(filled))) : 0,
  );
</script>

<div class="pip-meter" role="img" aria-label={label ?? `${safeFilled} of ${safeTotal}`}>
  {#each { length: safeTotal } as _, i (i)}
    <span class="pip" class:filled={i < safeFilled}></span>
  {/each}
</div>

<style>
  .pip-meter {
    display: flex;
    gap: 5px;
  }
  .pip {
    flex: 1;
    height: 7px;
    border-radius: 4px;
    background: var(--vote-abstain);
  }
  .pip.filled {
    background: var(--vote-for);
  }
</style>
