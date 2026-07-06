<script lang="ts">
  /**
   * ZEB-607 — Commons count chip (spec D2): the design's
   * quorum/conviction chip anatomy — soft box, small label, mono value.
   */
  let {
    label,
    value,
    tone = 'neutral',
  }: {
    label: string;
    value: string;
    tone?: 'sage' | 'clay' | 'neutral';
  } = $props();
</script>

<div class="count-chip {tone}">
  <span class="cc-label">{label}</span>
  <span class="cc-value">{value}</span>
</div>

<style>
  .count-chip {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 6px 10px;
    border-radius: 6px;
    background: var(--status-drafting-bg);
    min-width: 0;
  }
  .cc-label {
    font-size: 0.6rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-muted);
  }
  .cc-value {
    font-family: var(--font-mono);
    font-size: 0.82rem;
    color: var(--text-primary);
  }
  .sage {
    background: var(--primary-soft);
  }
  .sage .cc-value {
    color: var(--primary-deep);
  }
  .clay {
    background: var(--gov-clay-soft);
  }
  .clay .cc-value {
    color: var(--gov-clay-deep);
  }
</style>
