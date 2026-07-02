<script lang="ts">
  import type { ReplicationTier } from '../types';
  import { tierTarget } from '../file-utils';

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
  let underReplicated = $derived(replicaCount < target);

  const TIERS: ReplicationTier[] = ['expendable', 'light', 'default', 'high', 'ultra'];

  function handleChange(e: Event) {
    const select = e.target as HTMLSelectElement;
    onTierChange(select.value as ReplicationTier);
  }
</script>

<div class="replication-status">
  <div class="replication-count">
    <span class="replica-indicator" class:met class:under={!met}>
      {replicaCount} of {target}
    </span>
    <span class="replica-label">replicas</span>
  </div>

  {#if underReplicated}
    <p class="replication-warning">Under-replicated</p>
  {/if}

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
  }

  .replication-count {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .replica-indicator {
    font-weight: 600;
    font-size: 0.9rem;
  }

  .replica-indicator.met {
    color: #43b581;
  }

  .replica-indicator.under {
    color: #f0b232;
  }

  .replica-label {
    font-size: 0.8rem;
    color: var(--text-secondary);
  }

  .replication-warning {
    margin: 0;
    font-size: 0.8rem;
    color: #f0b232;
    font-weight: 500;
  }

  .tier-select {
    padding: 4px 8px;
    border-radius: 4px;
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
