<script lang="ts">
  import { onMount } from 'svelte';
  import type { TauriAdapter } from '../zenoh-service';
  import { MintService } from '../mint-service';
  import type { Transaction, Account } from '../mint-types';
  import { save as saveDialog } from '@tauri-apps/plugin-dialog';
  import MintTransactionTable from './MintTransactionTable.svelte';
  import MintTransactionDialog from './MintTransactionDialog.svelte';
  import MintAccountManager from './MintAccountManager.svelte';

  let { adapter }: { adapter: TauriAdapter } = $props();

  const service = new MintService(adapter);
  let transactions = $state<Transaction[]>([]);
  let accounts = $state<Account[]>([]);
  let defaultCurrency = $state<string>('USD');
  let loading = $state(true);
  let loadError = $state<string | null>(null);
  let operationError = $state<string | null>(null);

  // Filters
  let filterDateFrom = $state<string>('');
  let filterDateTo = $state<string>('');
  let filterAccountId = $state<string>('');

  // Plain let (not $state) — epoch is an instance counter, not a reactive
  // value. Reads and writes are synchronous so no reactivity needed.
  let loadEpoch = 0;

  async function load() {
    loadEpoch++;
    const myEpoch = loadEpoch;
    loading = true;
    loadError = null;
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
      // Stale result guard — only the most recent load() writes to state.
      if (myEpoch !== loadEpoch) return;
      transactions = txs;
      accounts = accs;
      defaultCurrency = def ?? 'USD';
      // Drop stale account filter if the account no longer exists. Avoids
      // the "table mysteriously empty after delete" UX trap.
      if (filterAccountId && !accs.some((a) => a.id === filterAccountId)) {
        filterAccountId = '';
        // The current load() ran with a stale filter — reissue with the
        // cleared filter so the table updates immediately. The recursive
        // call increments loadEpoch, so this load()'s subsequent state
        // writes (and the finally block's loading=false) are skipped via
        // the epoch guards already in place.
        void load();
        return;
      }
    } catch (e) {
      if (myEpoch !== loadEpoch) return;
      loadError = e instanceof Error ? e.message : String(e);
    } finally {
      // Only the most-recent load clears the loading flag — prevents
      // a stale completion from prematurely flipping off a spinner that
      // a newer load is still waiting on.
      if (myEpoch === loadEpoch) loading = false;
    }
  }

  onMount(() => {
    void load();
  });

  async function exportCsv() {
    operationError = null;
    exportInProgress = true;
    try {
      const path = await saveDialog({
        defaultPath: `mint-export-${new Date().toISOString().slice(0, 10)}.csv`,
        filters: [{ name: 'CSV', extensions: ['csv'] }],
      });
      if (!path) return; // user cancelled — exits try cleanly, finally still resets exportInProgress
      const summary = await service.exportCsv(path, {
        dateFrom: filterDateFrom || undefined,
        dateTo: filterDateTo || undefined,
      });
      alert(`Exported ${summary.rowsWritten} transactions to ${summary.outputPath} (${summary.byteSize} bytes)`);
    } catch (e) {
      operationError = e instanceof Error ? e.message : String(e);
    } finally {
      exportInProgress = false;
    }
  }

  // Add/Edit dialog state.
  let showAddEdit = $state(false);
  let editingTxId = $state<string | null>(null);

  // Account manager state.
  let showAccountManager = $state(false);

  // CSV export state.
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
      <button onclick={exportCsv} disabled={exportInProgress}>Export CSV</button>
    </div>
  </header>

  {#if loading}
    <p>Loading…</p>
  {:else if loadError}
    <p role="alert" class="error">{loadError}</p>
  {:else}
    {#if operationError}
      <p role="alert" class="error operation-error">
        {operationError}
        <button
          type="button"
          aria-label="Dismiss error"
          class="dismiss"
          onclick={() => { operationError = null; }}
        >×</button>
      </p>
    {/if}
    <MintTransactionTable
      {transactions}
      onEdit={(id) => { editingTxId = id; showAddEdit = true; }}
      onDelete={async (id) => {
        if (!confirm('Delete this transaction?')) return;
        operationError = null;
        try {
          await service.deleteTransaction(id);
          await load();
        } catch (e) {
          operationError = e instanceof Error ? e.message : String(e);
        }
      }}
    />
  {/if}

  {#if showAddEdit}
    <MintTransactionDialog
      {service}
      {accounts}
      {defaultCurrency}
      editingId={editingTxId}
      onClose={() => { showAddEdit = false; editingTxId = null; }}
      onSaved={load}
    />
  {/if}

  {#if showAccountManager}
    <MintAccountManager
      {service}
      {accounts}
      onClose={() => { showAccountManager = false; }}
      onChanged={load}
    />
  {/if}
</section>

<style>
  .mint-ledger { display: flex; flex-direction: column; height: 100%; padding: 1rem; }
  .mint-toolbar { display: flex; justify-content: space-between; gap: 1rem; flex-wrap: wrap; margin-bottom: 1rem; }
  .filters { display: flex; gap: 0.75rem; align-items: center; flex-wrap: wrap; }
  .default-currency { color: var(--color-text-secondary, #888); font-size: 0.9rem; }
  .actions { display: flex; gap: 0.5rem; }
  .error { color: var(--color-error, #c53030); }
  .operation-error {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    background: var(--color-bg-warning, #fff8e1);
    border-radius: 4px;
    margin-bottom: 0.75rem;
  }
  .dismiss {
    margin-left: auto;
    background: none;
    border: none;
    font-size: 1.2rem;
    cursor: pointer;
    padding: 0 0.25rem;
  }
</style>
