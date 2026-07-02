<script lang="ts">
  import type { MintService } from '../mint-service';
  import type { Account } from '../mint-types';

  let { service, accounts, onClose, onChanged }: {
    service: MintService;
    accounts: Account[];
    onClose: () => void;
    onChanged: () => void;
  } = $props();

  let newName = $state('');
  let creating = $state(false);
  let error = $state<string | null>(null);

  // Per-account rename state, keyed by id
  let editingId = $state<string | null>(null);
  let editingName = $state('');

  // Per-account delete confirm state
  let confirmDeleteId = $state<string | null>(null);
  let reassignTo = $state<string>(''); // '' = no reassign

  async function create() {
    if (!newName.trim()) return;
    creating = true;
    error = null;
    try {
      await service.createAccount(newName.trim());
      newName = '';
      onChanged();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      creating = false;
    }
  }

  function startRename(a: Account) {
    // Close any open delete-confirm — only one inline edit UI at a time.
    confirmDeleteId = null;
    reassignTo = '';
    editingId = a.id;
    editingName = a.name;
    error = null;
  }

  async function commitRename() {
    if (!editingId) return;
    error = null;
    try {
      await service.renameAccount(editingId, editingName.trim());
      editingId = null;
      onChanged();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  function startDelete(a: Account) {
    // Close any open rename session — a user shouldn't have rename
    // and delete-confirm UI visible at the same time.
    editingId = null;
    confirmDeleteId = a.id;
    reassignTo = '';
    error = null;
  }

  async function commitDelete() {
    if (!confirmDeleteId) return;
    error = null;
    try {
      await service.deleteAccount(confirmDeleteId, reassignTo || null);
      confirmDeleteId = null;
      reassignTo = '';
      onChanged();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div role="dialog" aria-modal="true" aria-label="Manage accounts" class="mint-dialog">
  <div class="dialog-body">
    <h2>Manage accounts</h2>
    <div class="new-account">
      <label for="mint-new-account-name" class="sr-only">New account name</label>
      <input
        id="mint-new-account-name"
        type="text"
        bind:value={newName}
        placeholder="New account name"
        maxlength="256"
        onkeydown={(e) => { if (e.key === 'Enter') create(); }}
      />
      <button onclick={create} disabled={!newName.trim() || creating}>Add</button>
    </div>
    <ul class="accounts-list">
      {#each accounts as a (a.id)}
        <li>
          {#if editingId === a.id}
            <input
              type="text"
              bind:value={editingName}
              aria-label="New name for {a.name}"
              onkeydown={(e) => { if (e.key === 'Enter') commitRename(); }}
            />
            <button onclick={commitRename}>Save</button>
            <button onclick={() => { editingId = null; }}>Cancel</button>
          {:else}
            <span class="name">{a.name}</span>
            <span class="count">({a.transactionCount} txn{a.transactionCount === 1 ? '' : 's'})</span>
            <button onclick={() => startRename(a)} aria-label="Rename {a.name}">Rename</button>
            <button onclick={() => startDelete(a)} aria-label="Delete {a.name}">Delete</button>
          {/if}
        </li>
      {/each}
    </ul>
    {#if confirmDeleteId}
      {@const deletingAccount = accounts.find((a) => a.id === confirmDeleteId)}
      {@const cnt = deletingAccount?.transactionCount ?? 0}
      <div class="confirm-delete">
        <p>
          Delete account "{deletingAccount?.name}"?
          {#if cnt > 0}
            <br />Reassign {cnt} transaction{cnt === 1 ? '' : 's'} to:
            <select bind:value={reassignTo}>
              <option value="">— select account —</option>
              {#each accounts.filter((a) => a.id !== confirmDeleteId) as opt}
                <option value={opt.id}>{opt.name}</option>
              {/each}
            </select>
          {/if}
        </p>
        <button onclick={commitDelete} disabled={cnt > 0 && !reassignTo}>Confirm Delete</button>
        <button onclick={() => { confirmDeleteId = null; reassignTo = ''; }}>Cancel</button>
      </div>
    {/if}
    {#if error}<p role="alert" class="error">{error}</p>{/if}
    <div class="dialog-actions">
      <button onclick={onClose}>Close</button>
    </div>
  </div>
</div>

<style>
  .mint-dialog { position: fixed; inset: 0; background: var(--overlay); display: flex; align-items: center; justify-content: center; z-index: 1000; }
  .dialog-body { background: var(--color-bg, #fff); padding: 1.5rem; border-radius: 8px; min-width: 480px; max-width: 90vw; display: flex; flex-direction: column; gap: 0.75rem; }
  .new-account { display: flex; gap: 0.5rem; }
  .new-account input { flex: 1; }
  .accounts-list { list-style: none; padding: 0; margin: 0; }
  .accounts-list li { display: flex; align-items: center; gap: 0.5rem; padding: 0.4rem 0; border-bottom: 1px solid var(--color-border, #eee); }
  .name { flex: 1; }
  .count { color: var(--color-text-secondary, #888); font-size: 0.85em; }
  .confirm-delete { padding: 0.75rem; background: var(--color-bg-warning, #fff8e1); border-radius: 4px; }
  .dialog-actions { display: flex; justify-content: flex-end; }
  .error { color: var(--color-error, #c53030); }
  .sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0; }
</style>
