<script lang="ts">
  import type { FlashcardLevel, SessionStats, Challenge } from '../flashcard-types';
  import { LEVELS, LEVEL_NAMES, initialSessionStats } from '../flashcard-types';
  import SpellList from './SpellList.svelte';
  import FlashcardView from './FlashcardView.svelte';

  let {
    stq8Service,
    onStatsUpdate,
  }: {
    stq8Service: {
      isReady(): boolean;
      getLevelInfo(l: FlashcardLevel): { total_bytes: number; bytes_per_row: number; num_rows: number; total_bits: number };
      generateChallenge(l: FlashcardLevel): Challenge;
    };
    onStatsUpdate?: (stats: SessionStats) => void;
  } = $props();

  type SpellbookTab = 'spells' | 'practice';
  let activeTab = $state<SpellbookTab>('practice');
  let level = $state<FlashcardLevel>(0);
  let expressLane = $state(false);
  let stats = $state(initialSessionStats());

  function handleLevelChange(e: Event) {
    const target = e.target as HTMLSelectElement;
    level = Number(target.value) as FlashcardLevel;
  }

  function handleStatsUpdate(newStats: SessionStats) {
    stats = newStats;
    onStatsUpdate?.(newStats);
  }
</script>

<div class="spellbook-mode">
  <header class="spellbook-toolbar">
    <div class="tab-bar" role="tablist" aria-label="Spellbook tabs">
      <button
        type="button"
        role="tab"
        aria-label="Spells"
        aria-selected={activeTab === 'spells'}
        class="tab-btn"
        class:active={activeTab === 'spells'}
        onclick={() => { activeTab = 'spells'; }}
      >Spells</button>
      <button
        type="button"
        role="tab"
        aria-label="Practice"
        aria-selected={activeTab === 'practice'}
        class="tab-btn"
        class:active={activeTab === 'practice'}
        onclick={() => { activeTab = 'practice'; }}
      >Practice</button>
    </div>

    {#if activeTab === 'practice'}
      <div class="toolbar-controls">
        <label class="level-selector">
          <span class="sr-only">Level</span>
          <select aria-label="Level" value={level} onchange={handleLevelChange}>
            {#each LEVELS as l}
              <option value={l}>{LEVEL_NAMES[l]}</option>
            {/each}
          </select>
        </label>

        <label class="express-toggle">
          <input
            type="checkbox"
            bind:checked={expressLane}
            aria-label="Express lane"
          />
          <span>Express</span>
        </label>
      </div>
    {/if}
  </header>

  <div class="spellbook-content" role="tabpanel">
    {#if activeTab === 'spells'}
      <SpellList />
    {:else}
      <FlashcardView
        {level}
        {expressLane}
        {stq8Service}
        onStatsUpdate={handleStatsUpdate}
      />
    {/if}
  </div>
</div>

<style>
  .spellbook-mode {
    display: flex;
    flex-direction: column;
    height: 100%;
  }

  .spellbook-toolbar {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--border, #3f4147);
    background: var(--bg-secondary, #2b2d31);
    flex-wrap: wrap;
  }

  .tab-bar {
    display: flex;
    gap: 2px;
  }

  .tab-btn {
    padding: 6px 16px;
    border: none;
    border-radius: 4px;
    background: var(--bg-tertiary, #313338);
    color: var(--text-muted, #949ba4);
    cursor: pointer;
    font-size: 0.8125rem;
    font-weight: 500;
  }

  .tab-btn.active {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
  }

  .toolbar-controls {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-left: auto;
  }

  .level-selector select {
    padding: 4px 8px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: var(--bg-tertiary, #313338);
    color: var(--text-primary, #f2f3f5);
    font-size: 0.8125rem;
  }

  .express-toggle {
    display: flex;
    align-items: center;
    gap: 4px;
    color: var(--text-secondary, #b5bac1);
    font-size: 0.8125rem;
    cursor: pointer;
  }

  .express-toggle input[type="checkbox"] {
    accent-color: var(--accent, #5865f2);
  }

  .spellbook-content {
    flex: 1;
    overflow-y: auto;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    padding: 0;
    margin: -1px;
    overflow: hidden;
    clip: rect(0, 0, 0, 0);
    border: 0;
  }
</style>
