<script lang="ts">
  import { trustScoreColor, trustScoreLabel } from '../trust-score';
  import type { TrustScore } from '../trust-score';

  let {
    score,
  }: {
    score: TrustScore | null;
  } = $props();

  let color = $derived(trustScoreColor(score));
  let label = $derived(trustScoreLabel(score));
</script>

<span
  class="trust-badge"
  role="img"
  aria-label={label}
  style="background: {color}"
></span>

<style>
  .trust-badge {
    display: inline-block;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
    vertical-align: middle;
  }
</style>
