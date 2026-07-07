<script lang="ts">
  import { trustScoreColor, trustScoreLabel } from '../trust-score';
  import type { TrustScore } from '../trust-score';

  let {
    score = null,
  }: {
    score?: TrustScore | null;
  } = $props();

  let color = $derived(trustScoreColor(score));
  let label = $derived(trustScoreLabel(score));
</script>

<span
  class="trust-badge"
  role="img"
  aria-label={label}
  style="color: {color}"
>{label}</span>

<style>
  /* ZEB-661 (#4): a labeled pill (StatusPill anatomy), not a bare colour dot —
     the label used to be screen-reader-only, a colour-only encoding that failed
     colour-blind sighted users in the TrustOverview table. Tone comes from
     trustScoreColor via the inline `color` (full-strength text, matching the
     adjacent score cells); the fill is a currentColor tint. The tint lives here
     rather than inline so the inline `color` parses cleanly (jsdom drops a whole
     inline style string on an unparseable color-mix) and budget-0 holds —
     currentColor/transparent are compositional keywords, not colour literals. */
  .trust-badge {
    display: inline-block;
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 11px;
    padding: 4px 11px;
    border-radius: 20px;
    white-space: nowrap;
    flex-shrink: 0;
    vertical-align: middle;
    background: color-mix(in srgb, currentColor 16%, transparent);
  }
</style>
