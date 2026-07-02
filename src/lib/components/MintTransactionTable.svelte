<script lang="ts">
  import type { Transaction } from '../mint-types';

  let { transactions, onEdit, onDelete }: {
    transactions: Transaction[];
    onEdit: (id: string) => void;
    onDelete: (id: string) => void;
  } = $props();
</script>

<table aria-label="Transactions" class="mint-tx-table">
  <thead>
    <tr>
      <th>Date</th>
      <th>Account</th>
      <th>Amount</th>
      <th>Currency</th>
      <th>Description</th>
      <th>Metadata</th>
      <th aria-label="Actions"></th>
    </tr>
  </thead>
  <tbody>
    {#each transactions as tx (tx.id)}
      <tr>
        <td>{tx.transactionDate}</td>
        <td>{tx.accountName}</td>
        <td class="amount">{tx.amount}</td>
        <td>{tx.currency}</td>
        <td>{tx.description}</td>
        <td class="metadata">
          {#if tx.metadata}<code title={tx.metadata}>{tx.metadata.slice(0, 40)}{tx.metadata.length > 40 ? '…' : ''}</code>{/if}
        </td>
        <td>
          <button onclick={() => onEdit(tx.id)} aria-label="Edit transaction">Edit</button>
          <button onclick={() => onDelete(tx.id)} aria-label="Delete transaction">Delete</button>
        </td>
      </tr>
    {:else}
      <tr><td colspan="7" class="empty">No transactions yet.</td></tr>
    {/each}
  </tbody>
</table>

<style>
  .mint-tx-table { width: 100%; border-collapse: collapse; }
  .mint-tx-table th, .mint-tx-table td { padding: 0.4rem 0.6rem; text-align: left; border-bottom: 1px solid var(--color-border-soft); }
  .amount { font-variant-numeric: tabular-nums; text-align: right; }
  .metadata code { font-size: 0.85em; }
  .empty { text-align: center; color: var(--color-text-secondary); padding: 2rem; }
</style>
