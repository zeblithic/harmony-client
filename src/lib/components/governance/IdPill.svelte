<script lang="ts">
  /**
   * ZEB-648 item 5 — shared short-id pill. The card header pill
   * (`.cp-id-pill`) and the panel doc-column breadcrumb pill
   * (`.doc-id-pill`) were byte-identical; this is the one primitive both
   * render so the styling can't drift.
   */
  import { shortId } from '../../short-addr';

  let {
    id,
    ariaLabel,
  }: {
    /** Full hex id; rendered via shortId(). */
    id: string;
    ariaLabel?: string;
  } = $props();
</script>

<span class="id-pill" aria-label={ariaLabel}>{shortId(id)}</span>

<style>
  .id-pill {
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 0.62rem;
    color: var(--text-bright);
    background: var(--gov-clay);
    padding: 2px 6px;
    border-radius: 3px;
  }
</style>
