<script lang="ts">
  // The Harmony mark (ZEB-605), lifted from the Commons reference headers.
  // The four hexes are fixed BRAND constants — deliberately not theme tokens;
  // they live in markup attributes, which the style-token guard does not scan.
  let { size = 24, withDot = false }: { size?: number; withDot?: boolean } = $props();
  const strokeWidth = $derived(size >= 40 ? 4 : 5);
</script>

<svg width={size} height={size} viewBox="0 0 92 92" aria-hidden="true" class="harmony-mark">
  <circle cx="46" cy="34" r="22" fill="none" stroke="#466b4c" stroke-width={strokeWidth} />
  <circle cx="32" cy="56" r="22" fill="none" stroke="#283450" stroke-width={strokeWidth} />
  <circle cx="60" cy="56" r="22" fill="none" stroke="#c56a46" stroke-width={strokeWidth} />
  {#if withDot}
    <circle cx="46" cy="49" r="4" fill="#20241c" />
  {/if}
</svg>

<style>
  .harmony-mark {
    flex-shrink: 0;
  }
</style>
