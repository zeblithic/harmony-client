<script lang="ts">
  import type { TauriAdapter } from '../zenoh-service';
  import { MintService } from '../mint-service';
  import type { Transaction, Account } from '../mint-types';
  import MintTransactionTable from './MintTransactionTable.svelte';

  let { adapter }: { adapter: TauriAdapter } = $props();

  const service = new MintService(adapter);
  let transactions = $state<Transaction[]>([]);
  let accounts = $state<Account[]>([]);
  let defaultCurrency = $state<string>('USD');
  let loading = $state(true);
  let error = $state<string | null>(null);

  // Filters
  let filterDateFrom = $state<string>('');
  let filterDateTo = $state<string>('');
  let filterAccountId = $state<string>('');

  async function load() {
    loading = true;
    error = null;
    try {
      const [txs, accs, def] = await Promise.all([
        service.listTransactions({
          dateFrom: filterDateFrom || undefined,
          dateTo: filterDateTo || undefined,
          accountId: filterAccountId || undefined,
        }),
        service.listAccounts(),
        service.getDefaultCurrency(),
      ]);
      transactions = txs;
      accounts = accs;
      defaultCurrency = def ?? 'USD';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  // Initial load on mount only. The effect does NOT re-fire when filter
  // state changes because load() reads filterDateFrom/filterDateTo/
  // filterAccountId only inside an async body after await — outside
  // Svelte's synchronous reactivity tracking window. Filter changes
  // trigger reloads via explicit `onchange={load}` handlers on each
  // input. If load() is ever refactored to read filter state before the
  // first await, this effect will start double-firing with the
  // onchange handlers — preserve the async-read pattern.
  $effect(() => { load(); });

  // Add/Edit dialog state — wired in Task 8
  let showAddEdit = $state(false);
  let editingTxId = $state<string | null>(null);

  // Account manager state — wired in Task 9
  let showAccountManager = $state(false);

  // CSV export state — wired in Task 10
  let exportInProgress = $state(false);
</script>

<section aria-label="Mint personal finance ledger" class="mint-ledger">
  <header class="mint-toolbar">
    <div class="filters">
      <label>From <input type="date" bind:value={filterDateFrom} onchange={load} /></label>
      <label>To <input type="date" bind:value={filterDateTo} onchange={load} /></label>
      <label>
        Account
        <select bind:value={filterAccountId} onchange={load}>
          <option value="">All</option>
          {#each accounts as a}<option value={a.id}>{a.name}</option>{/each}
        </select>
      </label>
      <span class="default-currency">Default: {defaultCurrency}</span>
    </div>
    <div class="actions">
      <button onclick={() => { editingTxId = null; showAddEdit = true; }}>+ Add Transaction</button>
      <button onclick={() => { showAccountManager = true; }}>Manage Accounts</button>
      <button onclick={() => { /* Task 10 */ }} disabled={exportInProgress}>Export CSV</button>
    </div>
  </header>

  {#if loading}
    <p>Loading…</p>
  {:else if error}
    <p role="alert" class="error">{error}</p>
  {:else}
    <MintTransactionTable
      {transactions}
      onEdit={(id) => { editingTxId = id; showAddEdit = true; }}
      onDelete={async (id) => {
        if (!confirm('Delete this transaction?')) return;
        await service.deleteTransaction(id);
        await load();
      }}
    />
  {/if}

  <!-- Add/Edit dialog: rendered in Task 8 -->
  <!-- Account manager dialog: rendered in Task 9 -->
</section>

<style>
  .mint-ledger { display: flex; flex-direction: column; height: 100%; padding: 1rem; }
  .mint-toolbar { display: flex; justify-content: space-between; gap: 1rem; flex-wrap: wrap; margin-bottom: 1rem; }
  .filters { display: flex; gap: 0.75rem; align-items: center; flex-wrap: wrap; }
  .default-currency { color: var(--color-text-secondary, #888); font-size: 0.9rem; }
  .actions { display: flex; gap: 0.5rem; }
  .error { color: var(--color-error, #c53030); }
</style>
