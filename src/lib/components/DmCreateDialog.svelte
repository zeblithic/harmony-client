<script lang="ts">
  import type { Profile } from '../types';
  import { nonEmpty } from '../display-label';

  let {
    profiles,
    onSubmit,
    onCancel,
    initialKind = 'dm',
    onConvertToCommunity,
    friendSourced = true,
  }: {
    profiles: Map<string, Profile>;
    onSubmit: (args: { kind: 'dm' | 'group-dm'; members: string[]; name: string }) => void;
    onCancel: () => void;
    /** Which entry-point opened the dialog. Affects framing only —
     *  the actual `kind` is still derived from selected.length. The
     *  FAB menu has two separate items ("New direct message" vs
     *  "New group DM") that must produce visibly distinct UX,
     *  per cursor's PR-91 review. */
    initialKind?: 'dm' | 'group-dm';
    /** Handoff to community-creation flow when the user hits the 16-member cap.
     *  Optional: when omitted (e.g., in contexts where community creation
     *  isn't available), the at-cap state shows only the cap-explanation copy
     *  with no orphan button. ZEB-228 acceptance criteria require this
     *  handoff once Sub-C ships. */
    onConvertToCommunity?: () => void;
    /** ZEB-431: whether `profiles` is sourced from the friend graph (Tauri,
     *  default) or the legacy presence/mock map (browser demo mode). Picks
     *  the matching empty-state guidance — "add a friend" is wrong advice
     *  when the list is presence-fed. */
    friendSourced?: boolean;
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
      nonEmpty(profiles.get(addr)?.displayName) ?? shortAddr(addr);
    const name =
      kind === 'dm'
        ? `DM with ${labelFor(selected[0])}`
        : `DM: ${truncate(selected.map(labelFor).join(', '), 80)}`;
    onSubmit({ kind, members: selected, name });
  }
</script>

<div class="dm-create-dialog" role="dialog" aria-modal="true" aria-labelledby="dm-create-title">
  <h2 id="dm-create-title">
    {initialKind === 'group-dm' ? 'New group DM' : 'New direct message'}
  </h2>
  {#if initialKind === 'group-dm'}
    <p class="subtitle">Add multiple recipients to start a group conversation.</p>
  {/if}

  <input
    type="text"
    placeholder={initialKind === 'group-dm' ? 'Search contacts to add…' : 'Search contacts…'}
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
          aria-label={`Remove ${nonEmpty(profile?.displayName) ?? shortAddr(addr)}`}
        >
          {nonEmpty(profile?.displayName) ?? shortAddr(addr)} ✕
        </button>
      {/each}
    </div>
  {/if}

  <div class="contact-list">
    {#each filteredProfiles as [addr, profile] (addr)}
      <button type="button" class="contact" onclick={() => toggleSelect(addr)}>
        {nonEmpty(profile.displayName) ?? shortAddr(addr)}
      </button>
    {/each}
    {#if filteredProfiles.length === 0}
      <p class="empty">
        {#if searchQuery === '' && profiles.size === 0}
          {#if friendSourced}
            No contacts yet — add a friend to start a DM.
          {:else}
            No contacts yet — they'll appear here as demo peers come online.
          {/if}
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
        Group DMs cap at 16 members (you + 15). Larger groups work better as
        communities.
        {#if onConvertToCommunity}
          <button
            type="button"
            class="convert-link"
            onclick={() => onConvertToCommunity?.()}
          >
            Convert to community →
          </button>
        {/if}
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
  .subtitle {
    color: var(--text-secondary);
    font-size: 0.85rem;
    margin: -8px 0 12px;
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
    /* ZEB-661 (#3): removable recipient person-token → Commons sage + 20px pill
       (was Library-blue + 12px). */
    background: color-mix(in srgb, var(--accent) 20%, transparent);
    border-radius: 20px;
    padding: 2px 8px;
    font-size: 12px;
    cursor: pointer;
    border: none;
  }
  .contact-list {
    max-height: 200px;
    overflow-y: auto;
    border-radius: 4px;
    background: var(--hover-bg);
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
    /* Deliberate visual fix (the one exception to this sweep's zero-visual-change
       rule): hover previously matched the .contact-list fill exactly, so rows had
       no visible hover state. Use the app-standard row-hover token instead. */
    background: var(--bg-hover);
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
    color: var(--gov-clay);
  }
  .counter .hint {
    display: block;
    margin-top: 4px;
  }
  .convert-link {
    display: inline;
    background: transparent;
    border: none;
    padding: 0;
    margin-left: 4px;
    color: var(--accent);
    cursor: pointer;
    font-size: inherit;
    text-decoration: underline;
  }
  .convert-link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 12px;
  }
  .actions button {
    padding: 6px 12px;
    /* ZEB-661 (#3): rubric button radius (5px); keeps Start DM + Cancel consistent. */
    border-radius: 5px;
  }
  .primary {
    /* ZEB-661 (#3): Commons sage identity, not Library-blue. */
    background: color-mix(in srgb, var(--accent) 40%, transparent);
  }
  .primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
