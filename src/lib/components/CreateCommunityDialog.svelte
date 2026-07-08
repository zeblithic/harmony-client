<script lang="ts">
  import Modal from './Modal.svelte';

  let {
    onSubmit,
    onCancel,
    error = null,
    pending = false,
  }: {
    onSubmit: (name: string, kind: 'open' | 'invite-only') => void;
    onCancel: () => void;
    error?: string | null;
    pending?: boolean;
  } = $props();

  let name = $state('');
  let kind = $state<'open' | 'invite-only'>('invite-only');
  let canSubmit = $derived(name.trim().length > 0 && !pending);
  const titleId = `create-community-title-${Math.random().toString(36).slice(2)}`;

  function handleSubmit(e?: Event) {
    e?.preventDefault();
    if (!canSubmit) return;
    onSubmit(name.trim(), kind);
  }
</script>

<Modal {onCancel} canCancel={!pending} ariaLabelledby={titleId}>
  <h3 class="dialog-title" id={titleId}>New community</h3>

  <form onsubmit={handleSubmit}>
  <label for="community-name-input" class="sr-only">Community name</label>
  <input
    id="community-name-input"
    type="text"
    placeholder="Community name"
    bind:value={name}
    class="name-input"
    disabled={pending}
    autofocus
  />

  <div class="kind-row">
    <label class="kind-label">
      <input
        type="radio"
        name="kind"
        value="open"
        aria-label="Open"
        bind:group={kind}
        disabled={pending}
      />
      <span class="kind-text">Open</span>
      <span class="hint">Anyone with the URL can join</span>
    </label>
    <label class="kind-label">
      <input
        type="radio"
        name="kind"
        value="invite-only"
        aria-label="Invite-only"
        bind:group={kind}
        disabled={pending}
      />
      <span class="kind-text">Invite-only</span>
      <span class="hint">Each invite link works once</span>
    </label>
  </div>

  {#if error}
    <div class="error-banner">{error}</div>
  {/if}

  <div class="dialog-actions">
    <button type="button" class="cancel-btn" onclick={onCancel} disabled={pending}>Cancel</button>
    <button type="submit" class="confirm-btn" disabled={!canSubmit}>
      {pending ? 'Creating...' : 'Create'}
    </button>
  </div>
  </form>
</Modal>

<style>
  .dialog-title {
    color: var(--text-primary);
    font-size: 1.1rem;
    margin: 0 0 16px;
  }
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    white-space: nowrap;
    border: 0;
  }
  .name-input {
    width: 100%;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    font-size: 0.9rem;
    margin-bottom: 16px;
    box-sizing: border-box;
  }
  .name-input:focus {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }
  .kind-row {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 16px;
  }
  .kind-label {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.875rem;
    color: var(--text-primary);
    cursor: pointer;
  }
  .kind-text {
    font-size: 0.875rem;
  }
  .hint {
    color: var(--text-secondary);
    font-size: 0.75rem;
    margin-left: auto;
  }
  .error-banner {
    background: var(--bg-tertiary);
    border: 1px solid var(--danger-muted);
    color: var(--danger-muted);
    padding: 8px 10px;
    border-radius: 4px;
    font-size: 0.8rem;
    margin-bottom: 12px;
  }
  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .confirm-btn {
    background: var(--accent);
    color: var(--on-accent);
    border: none;
    padding: 8px 16px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .confirm-btn:disabled,
  .cancel-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .cancel-btn:focus-visible,
  .confirm-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
</style>
