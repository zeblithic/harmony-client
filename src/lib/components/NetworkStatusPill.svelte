<script lang="ts" module>
  /**
   * ZEB-651 — shared network-status pill. Reuses the Commons StatusPill
   * anatomy (20px pill, 11px / 600) with network-domain variants, replacing
   * the byte-duplicated relay `.badge*` in NetworkHealthView and
   * NetworkDiscoverabilitySettings plus the `.peer-incompat` alarm badge.
   *
   * Deliberately separate from governance/StatusPill.svelte, which owns
   * governance status colors only. Variant → token pairs live here so the
   * network `--net-*` semantics stay out of the governance enum.
   */
  export type NetworkStatusVariant = 'healthy' | 'cooling' | 'incompat';
</script>

<script lang="ts">
  import type { HTMLAttributes } from 'svelte/elements';

  let {
    variant,
    label,
    class: className,
    ...rest
  }: {
    variant: NetworkStatusVariant;
    label: string;
  } & HTMLAttributes<HTMLSpanElement> = $props();

  // Merge (not override) a caller-supplied class: `class` is pulled out of
  // `rest` so the `{...rest}` spread can't clobber the base + variant classes.
  let classAttr = $derived(['net-pill', variant, className].filter(Boolean).join(' '));
</script>

<span class={classAttr} {...rest}>{label}</span>

<style>
  .net-pill {
    display: inline-block;
    font-weight: 600;
    font-size: 11px;
    line-height: 1.3;
    padding: 2px 10px;
    border-radius: 20px;
    white-space: nowrap;
  }
  .healthy {
    background: var(--net-ok-bg);
    color: var(--net-ok-fg);
  }
  .cooling {
    background: var(--net-warn-bg);
    color: var(--net-warn-fg);
  }
  .incompat {
    background: var(--net-danger-bg);
    color: var(--net-danger-fg);
    border: 1px solid var(--net-danger-fg);
  }
</style>
