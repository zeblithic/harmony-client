<script lang="ts">
  import type { Profile } from '../types';

  let {
    profiles,
    onSubmit,
    onCancel,
  }: {
    profiles: Map<string, Profile>;
    onSubmit: (args: { kind: 'dm' | 'group-dm'; members: string[]; name: string }) => void;
    onCancel: () => void;
  } = $props();

  const MAX_RECIPIENTS = 15;

  let searchQuery = $state('');
  let selected: string[] = $state([]);

  let filteredProfiles = $derived.by(() => {
    const q = searchQuery.toLowerCase();
    return Array.from(profiles.entries())
      .filter(([_addr, p]) => (p.displayName ?? '').toLowerCase().includes(q))
      .filter(([addr, _p]) => !selected.includes(addr))
      .slice(0, 50);
  });

  let kind: 'dm' | 'group-dm' = $derived(selected.length === 1 ? 'dm' : 'group-dm');
  let canAddMore = $derived(selected.length < MAX_RECIPIENTS);
  let canSubmit = $derived(selected.length >= 1 && selected.length <= MAX_RECIPIENTS);

  function shortAddr(addr: string): string {
    return addr.length > 8 ? addr.slice(0, 8) + '…' : addr;
  }

  function toggleSelect(addr: string) {
    if (selected.includes(addr)) {
      selected = selected.filter((a) => a !== addr);
    } else if (canAddMore) {
      selected = [...selected, addr];
    }
    // 16th-attempt: silent no-op (cap hint already explains why; an
    // additional toast would be noise).
  }

  function handleSubmit() {
    if (!canSubmit) return;
    const truncate = (s: string, n: number) =>
      s.length > n ? s.slice(0, n - 1) + '…' : s;
    const labelFor = (addr: string) =>
      profiles.get(addr)?.displayName ?? shortAddr(addr);
    const name =
      kind === 'dm'
        ? `DM with ${labelFor(selected[0])}`
        : `DM: ${truncate(selected.map(labelFor).join(', '), 80)}`;
    onSubmit({ kind, members: selected, name });
  }
</script>

<div class="dm-create-dialog" role="dialog" aria-labelledby="dm-create-title">
  <h2 id="dm-create-title">New direct message</h2>

  <input
    type="text"
    placeholder="Search contacts…"
    aria-label="Search contacts"
    bind:value={searchQuery}
    class="search-input"
  />

  {#if selected.length > 0}
    <div class="selected-chips">
      {#each selected as addr}
        {@const profile = profiles.get(addr)}
        <button
          type="button"
          class="chip"
          onclick={() => toggleSelect(addr)}
          aria-label={`Remove ${profile?.displayName ?? shortAddr(addr)}`}
        >
          {profile?.displayName ?? shortAddr(addr)} ✕
        </button>
      {/each}
    </div>
  {/if}

  <div class="contact-list">
    {#each filteredProfiles as [addr, profile] (addr)}
      <button type="button" class="contact" onclick={() => toggleSelect(addr)}>
        {profile.displayName ?? shortAddr(addr)}
      </button>
    {/each}
    {#if filteredProfiles.length === 0}
      <p class="empty">
        {#if searchQuery === '' && profiles.size === 0}
          No contacts yet — they'll appear here as your peers come online.
        {:else if searchQuery === ''}
          No more contacts available.
        {:else}
          No contacts match "{searchQuery}".
        {/if}
      </p>
    {/if}
  </div>

  <div class="counter" class:at-cap={selected.length === MAX_RECIPIENTS}>
    {selected.length} of {MAX_RECIPIENTS} recipients
    {#if selected.length === MAX_RECIPIENTS}
      <span class="hint">
        Group DMs cap at 16 members (you + 15). Communities (coming soon) work
        better for larger groups.
      </span>
    {/if}
  </div>

  <div class="actions">
    <button type="button" onclick={onCancel}>Cancel</button>
    <button
      type="button"
      onclick={handleSubmit}
      disabled={!canSubmit}
      class="primary"
    >
      Start DM
    </button>
  </div>
</div>

<style>
  .dm-create-dialog {
    padding: 16px;
    max-width: 360px;
  }
  .search-input {
    width: 100%;
    box-sizing: border-box;
    margin-bottom: 8px;
  }
  .selected-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-bottom: 8px;
  }
  .chip {
    background: rgba(120, 140, 200, 0.2);
    border-radius: 12px;
    padding: 2px 8px;
    font-size: 12px;
    cursor: pointer;
    border: none;
  }
  .contact-list {
    max-height: 200px;
    overflow-y: auto;
    border-radius: 4px;
    background: rgba(255, 255, 255, 0.05);
    margin-bottom: 8px;
  }
  .contact {
    display: block;
    width: 100%;
    text-align: left;
    padding: 6px 8px;
    background: transparent;
    border: none;
    cursor: pointer;
  }
  .contact:hover {
    background: rgba(255, 255, 255, 0.05);
  }
  .empty {
    padding: 12px;
    opacity: 0.6;
    font-size: 12px;
    text-align: center;
  }
  .counter {
    font-size: 11px;
    opacity: 0.7;
    margin: 8px 0;
  }
  .counter.at-cap {
    color: #f99;
  }
  .counter .hint {
    display: block;
    margin-top: 4px;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 12px;
  }
  .actions button {
    padding: 6px 12px;
  }
  .primary {
    background: rgba(120, 140, 200, 0.4);
  }
  .primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
