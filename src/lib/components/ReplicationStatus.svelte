<script lang="ts">
  import type { ReplicationTier } from '../types';
  import { tierTarget } from '../file-utils';

  // ZEB-612 S3: replicaCount is the OBSERVED lower bound — 1 (self) +
  // distinct peer sessions seen announcing this CID. The copy must say
  // "copies seen", never claim global truth.
  let {
    tier,
    replicaCount,
    onTierChange,
  }: {
    tier: ReplicationTier;
    replicaCount: number;
    onTierChange: (tier: ReplicationTier) => void;
  } = $props();

  let target = $derived(tierTarget(tier));
  let met = $derived(replicaCount >= target);

  const TIERS: ReplicationTier[] = ['expendable', 'light', 'default', 'high', 'ultra'];

  function handleChange(e: Event) {
    const select = e.target as HTMLSelectElement;
    onTierChange(select.value as ReplicationTier);
  }
</script>

<div class="replication-status" class:met class:under={!met}>
  <span class="replica-count">×{replicaCount} · copies seen (this device + peers)</span>
  <span class="replica-target">
    {met ? 'Above' : 'Below'} the ×{target} target for {tier}.
  </span>

  <select
    class="tier-select"
    value={tier}
    onchange={handleChange}
    aria-label="Replication tier"
  >
    {#each TIERS as t}
      <option value={t}>{t}</option>
    {/each}
  </select>
</div>

<style>
  .replication-status {
    display: flex;
    flex-direction: column;
    gap: 6px;
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 8px;
    padding: 10px 12px;
  }

  .replica-count {
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 0.85rem;
    color: var(--text-primary);
  }

  .replica-target {
    font-size: 0.78rem;
  }

  .met .replica-target {
    color: var(--accent);
  }

  .under .replica-target {
    color: var(--gov-clay);
  }

  .tier-select {
    padding: 4px 8px;
    border-radius: 5px;
    border: 1px solid var(--border);
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-size: 0.85rem;
    cursor: pointer;
  }

  .tier-select:focus {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
