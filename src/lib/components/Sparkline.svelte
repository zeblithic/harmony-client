<script lang="ts">
  import type { RingBuffer } from '../ring-buffer';

  let {
    data,
    label,
    color = '#43b581',
    height = 32,
    fixedMin = null as number | null,
    fixedMax = null as number | null,
  }: {
    data: RingBuffer<number>;
    label: string;
    color?: string;
    height?: number;
    fixedMin?: number | null;
    fixedMax?: number | null;
  } = $props();

  let points = $derived.by(() => {
    const values = data.toArray();
    if (values.length < 2) return '';
    const lo = fixedMin ?? Math.min(...values);
    const hi = fixedMax ?? Math.max(...values, lo + 1);
    const range = hi - lo || 1;
    const stepX = 100 / (values.length - 1);
    return values
      .map((v, i) => `${i * stepX},${height - ((v - lo) / range) * height}`)
      .join(' ');
  });
</script>

<svg
  role="img"
  aria-label={label}
  width="100%"
  {height}
  viewBox="0 0 100 {height}"
  preserveAspectRatio="none"
  class="sparkline"
>
  {#if points}
    <polyline
      {points}
      fill="none"
      stroke={color}
      stroke-width="1.5"
      stroke-linejoin="round"
      stroke-linecap="round"
      vector-effect="non-scaling-stroke"
    />
  {/if}
</svg>

<style>
  .sparkline {
    display: block;
  }
</style>
