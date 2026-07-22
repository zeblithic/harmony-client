<script lang="ts" module>
  import type { StatusPillVariant } from './status-pill-variant';
  export type { StatusPillVariant };

  const DEFAULT_LABELS: Record<StatusPillVariant, string> = {
    drafting: 'Drafting',
    open: '● Open',
    passing: 'Passing',
    failing: 'Failing',
    passed: '✓ Passed',
    failed: '✕ Failed',
    archived: 'Archived',
    recalled: 'Recalled',
  };
</script>

<script lang="ts">
  /**
   * ZEB-607 — Commons status pill (spec D3). Variant → token-pair
   * mapping is the single source of governance status colors; labels
   * default per variant and are overridable (e.g. lifecycle copy the
   * tests pin, or tier3StageLabel strings).
   */
  let {
    variant,
    label,
    ariaLabel,
  }: {
    variant: StatusPillVariant;
    label?: string;
    ariaLabel?: string;
  } = $props();
</script>

<span class="status-pill {variant}" aria-label={ariaLabel}>{label ?? DEFAULT_LABELS[variant]}</span>

<style>
  .status-pill {
    display: inline-block;
    font-weight: 600;
    font-size: 11px;
    line-height: 1.3;
    padding: 4px 11px;
    border-radius: 20px;
    white-space: nowrap;
  }
  .drafting,
  .archived {
    color: var(--status-drafting-fg);
    background: var(--status-drafting-bg);
  }
  .open {
    color: var(--status-open-fg);
    background: var(--status-open-bg);
  }
  .passing {
    color: var(--verdict-passing-fg);
    background: var(--verdict-passing-bg);
  }
  .failing {
    color: var(--verdict-failing-fg);
    background: var(--verdict-failing-bg);
  }
  .passed {
    color: var(--status-passed-fg);
    background: var(--status-passed-bg);
  }
  .failed {
    color: var(--status-failed-fg);
    background: var(--status-failed-bg);
  }
  .recalled {
    color: var(--status-recalled-fg);
    background: var(--status-recalled-bg);
  }
</style>
