<script lang="ts">
  import type { MintService } from '../mint-service';
  import type { Transaction, Account, NewTransaction, UpdateTransactionPayload } from '../mint-types';

  let {
    service,
    accounts,
    defaultCurrency,
    editingId,
    onClose,
    onSaved,
  }: {
    service: MintService;
    accounts: Account[];
    defaultCurrency: string;
    editingId: string | null;
    onClose: () => void;
    onSaved: () => void;
  } = $props();

  let date = $state(new Date().toISOString().slice(0, 10));
  let amount = $state('');
  let currency = $state(defaultCurrency);
  let accountId = $state(accounts[0]?.id ?? '');
  let description = $state('');
  let metadata = $state('');
  let error = $state<string | null>(null);
  let saving = $state(false);

  // Load existing transaction when in edit mode.
  $effect(() => {
    if (editingId) {
      service.getTransaction(editingId)
        .then((tx: Transaction | null) => {
          if (!tx) return;
          date = tx.transactionDate;
          amount = tx.amount;
          currency = tx.currency;
          accountId = tx.accountId;
          description = tx.description;
          metadata = tx.metadata ?? '';
        })
        .catch((e) => {
          error = e instanceof Error ? e.message : String(e);
        });
    } else {
      // Defensive reset: if editingId transitions to null while the
      // component is mounted (currently impossible because the dialog
      // unmounts via {#if showAddEdit} in MintLedger, but kept robust
      // against future refactors that might keep the dialog mounted).
      date = new Date().toISOString().slice(0, 10);
      amount = '';
      currency = defaultCurrency;
      accountId = accounts[0]?.id ?? '';
      description = '';
      metadata = '';
      error = null;
    }
  });

  function isJsonValid(s: string): boolean {
    try { JSON.parse(s); return true; } catch { return false; }
  }

  // Client-side validation gates the Save button. Server remains authoritative.
  // Note: description.length uses JS UTF-16 code units, not bytes — for typical
  // ASCII/Latin inputs these are equivalent. The 4096-byte server limit is
  // enforced server-side; this check is approximate but sufficient for UX.
  let isValid = $derived(
    /^\d{4}-\d{2}-\d{2}$/.test(date) &&
    /^-?\d+(\.\d+)?$/.test(amount) &&
    /^[A-Z]{1,5}$/.test(currency) &&
    accountId !== '' &&
    description.trim().length > 0 &&
    description.length <= 4096 &&
    (metadata === '' || isJsonValid(metadata))
  );

  async function save() {
    saving = true;
    error = null;
    try {
      if (editingId) {
        // Edit mode: metadata === '' sends null to explicitly clear the field.
        // Absent key = leave alone; null = clear; string = set new value.
        const payload: UpdateTransactionPayload = {
          transactionDate: date,
          amount,
          currency,
          accountId,
          description,
          metadata: metadata === '' ? null : metadata,
        };
        await service.updateTransaction(editingId, payload);
      } else {
        // Add mode: metadata === '' omits the key entirely (Rust treats as None).
        const payload: NewTransaction = {
          transactionDate: date,
          amount,
          currency,
          accountId,
          description,
          ...(metadata !== '' ? { metadata } : {}),
        };
        await service.createTransaction(payload);
      }
      onSaved();
      onClose();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      saving = false;
    }
  }
</script>

<!-- svelte-ignore a11y_interactive_supports_focus -->
<div
  role="dialog"
  aria-modal="true"
  aria-label={editingId ? 'Edit transaction' : 'Add transaction'}
  class="mint-dialog"
>
  <div class="dialog-body">
    <h2>{editingId ? 'Edit transaction' : 'Add transaction'}</h2>

    <label>
      Date
      <input type="date" bind:value={date} />
    </label>

    <label>
      Amount
      <input type="text" inputmode="decimal" bind:value={amount} placeholder="-42.50" />
    </label>

    <label>
      Currency
      <input
        type="text"
        bind:value={currency}
        oninput={(e) => { currency = (e.currentTarget as HTMLInputElement).value.toUpperCase(); }}
        maxlength={5}
      />
    </label>

    <label>
      Account
      <select bind:value={accountId}>
        {#each accounts as a (a.id)}
          <option value={a.id}>{a.name}</option>
        {/each}
      </select>
    </label>

    <label>
      Description
      <textarea bind:value={description} rows={2}></textarea>
    </label>

    <label>
      Metadata (JSON, optional)
      <textarea bind:value={metadata} rows={4} placeholder={"JSON, e.g. {\"tag\":\"travel\"}"}></textarea>
    </label>

    {#if error}
      <p role="alert" class="error">{error}</p>
    {/if}

    <div class="dialog-actions">
      <button onclick={onClose} disabled={saving}>Cancel</button>
      <button onclick={save} disabled={!isValid || saving}>{saving ? 'Saving…' : 'Save'}</button>
    </div>
  </div>
</div>

<style>
  .mint-dialog {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }

  .dialog-body {
    background: var(--bg-secondary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    min-width: 400px;
    max-width: 90vw;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    border: 1px solid var(--border);
  }

  .dialog-body h2 {
    margin: 0;
    font-size: 1.1rem;
    color: var(--text-primary);
  }

  .dialog-body label {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
    font-size: 0.875rem;
    color: var(--text-secondary);
  }

  .dialog-body input,
  .dialog-body select,
  .dialog-body textarea {
    font-size: 0.9rem;
    padding: 6px 8px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-primary, #fff);
    color: var(--text-primary);
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }

  .dialog-actions button {
    padding: 8px 16px;
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }

  .dialog-actions button:first-child {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
  }

  .dialog-actions button:last-child {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #fff);
  }

  .dialog-actions button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .error {
    color: var(--color-error, #c53030);
    font-size: 0.875rem;
    margin: 0;
  }
</style>
