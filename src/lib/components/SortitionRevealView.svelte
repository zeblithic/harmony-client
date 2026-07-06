<script lang="ts">
  /**
   * ZEB-311 — Renders the sortition draw result: primary mini-public
   * + backup pool + declines. Highlights the caller's membership if
   * any. Renders OwnerAddr as a short hex (first 8 + last 4 chars).
   *
   * Per ZEB-287 R4: every $props field destructured below.
   */
  import type { Tier3PollExport } from '../types/voting';
  import { shortAddr } from '../short-addr';

  let {
    detail,
    myAddr,
  }: {
    detail: Tier3PollExport;
    myAddr: string;
  } = $props();

  // Banner state is driven by `detail.myRole`, which the backend projects
  // from the *effective* mini-public (current_mini_public after declines +
  // backup promotion). Deriving from the static `miniPublic` / `backupPool`
  // rosters would misclassify promoted backups (role=mini_public, still in
  // static backupPool) and declined primaries (role=observer, still in
  // static miniPublic). The roster lists below stay tied to the static
  // sortition draw — they're a historical record, not active state.
  let declinedSet = $derived(new Set(detail.declined.map(([owner]) => owner)));
</script>

{#if detail.myRole === 'mini_public'}
  <p class="selected-banner">🎯 You were selected for the mini-public!</p>
{:else if detail.myRole === 'backup'}
  <p class="backup-banner">You're in the backup pool — you'll be promoted if a primary member declines.</p>
{/if}

<section class="sortition-reveal">
  <h5>Mini-public ({detail.miniPublic.length})</h5>
  <ul class="roster">
    {#each detail.miniPublic as addr (addr)}
      <li class:declined={declinedSet.has(addr)} class:self={addr === myAddr}>
        <code>{shortAddr(addr)}</code>
        {#if declinedSet.has(addr)}<span class="tag">declined</span>{/if}
        {#if addr === myAddr}<span class="tag">you</span>{/if}
      </li>
    {/each}
  </ul>

  {#if detail.backupPool.length > 0}
    <h5>Backup pool ({detail.backupPool.length})</h5>
    <ul class="roster">
      {#each detail.backupPool as addr (addr)}
        <li class:self={addr === myAddr}>
          <code>{shortAddr(addr)}</code>
          {#if addr === myAddr}<span class="tag">you</span>{/if}
        </li>
      {/each}
    </ul>
  {/if}

  {#if detail.declined.length > 0}
    <h5>Declined ({detail.declined.length})</h5>
    <ul class="roster">
      {#each detail.declined as [owner, hlcMs] (owner)}
        <li class="declined" class:self={owner === myAddr}>
          <code>{shortAddr(owner)}</code>
          {#if owner === myAddr}<span class="tag">you</span>{/if}
          <span class="tag" title={`declined at HLC wall_ms ${hlcMs}`}>declined</span>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .selected-banner {
    background: var(--accent);
    color: var(--text-bright);
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
    font-weight: 500;
  }
  .backup-banner {
    background: var(--primary-soft);
    color: var(--primary-deep);
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
  }
  .roster {
    list-style: none;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: 0.25rem 0.5rem;
  }
  .roster li {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    font-size: 0.85rem;
    padding: 0.15rem 0.4rem;
    background: var(--input-bg);
    border-radius: 3px;
  }
  .roster li.declined code { text-decoration: line-through; opacity: 0.6; }
  .roster li.self { background: var(--primary-soft); }
  .tag {
    font-size: 0.7rem;
    color: var(--text-faint);
    background: var(--chip-bg);
    padding: 0 0.35rem;
    border-radius: 2px;
  }
  h5 { margin: 1rem 0 0.25rem; font-size: 0.9rem; }
</style>
