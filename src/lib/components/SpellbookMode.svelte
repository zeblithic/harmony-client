<script lang="ts">
  import type { FlashcardLevel, SessionStats, ExpressMode } from '../flashcard-types';
  import type { Stq8ServiceLike } from '../stq8-service';
  import { LEVELS, LEVEL_NAMES, EXPRESS_MODES, EXPRESS_MODE_LABELS, initialSessionStats } from '../flashcard-types';
  import SpellList from './SpellList.svelte';
  import FlashcardView from './FlashcardView.svelte';
  import CalibrationView from './CalibrationView.svelte';

  let {
    stq8Service,
    onStatsUpdate,
  }: {
    stq8Service: Stq8ServiceLike;
    onStatsUpdate?: (stats: SessionStats) => void;
  } = $props();

  type SpellbookTab = 'spells' | 'practice' | 'calibrate';
  let activeTab = $state<SpellbookTab>('practice');
  // Tracked as local $state so the Calibrate tab's completion handler and
  // the tab-label suffix re-render immediately, without needing the parent
  // to swap out the stq8Service reference just to signal a calibration
  // change.
  let calibrated = $state(stq8Service.isCalibrated());
  function handleCalibrated() {
    calibrated = true;
  }
  // Re-sync if the parent swaps the service (e.g. WASM finishes loading
  // after this component already mounted with a null-backed service).
  $effect(() => {
    calibrated = stq8Service.isCalibrated();
  });
  let level = $state<FlashcardLevel>(0);
  let expressMode = $state<ExpressMode>('off');
  // Preserved across tab switches — FlashcardView remounts with this state
  let lastStats = $state(initialSessionStats());

  function handleLevelChange(e: Event) {
    const target = e.target as HTMLSelectElement;
    level = Number(target.value) as FlashcardLevel;
  }

  function handleStatsUpdate(newStats: SessionStats) {
    lastStats = newStats;
    onStatsUpdate?.(newStats);
  }
</script>

<div class="spellbook-mode">
  <header class="spellbook-toolbar">
    <div class="tab-bar" role="tablist" aria-label="Spellbook tabs">
      <button
        type="button"
        role="tab"
        id="tab-spells"
        aria-label="Spells"
        aria-selected={activeTab === 'spells'}
        aria-controls="tabpanel-spellbook"
        class="tab-btn"
        class:active={activeTab === 'spells'}
        onclick={() => { activeTab = 'spells'; }}
      >Spells</button>
      <button
        type="button"
        role="tab"
        id="tab-practice"
        aria-label="Practice"
        aria-selected={activeTab === 'practice'}
        aria-controls="tabpanel-spellbook"
        class="tab-btn"
        class:active={activeTab === 'practice'}
        onclick={() => { activeTab = 'practice'; }}
      >Practice</button>
      <button
        type="button"
        role="tab"
        id="tab-calibrate"
        aria-label="Calibrate"
        aria-selected={activeTab === 'calibrate'}
        aria-controls="tabpanel-spellbook"
        class="tab-btn"
        class:active={activeTab === 'calibrate'}
        onclick={() => { activeTab = 'calibrate'; }}
      >Calibrate{calibrated ? ' ✓' : ''}</button>
    </div>

    {#if activeTab === 'practice'}
      <div class="toolbar-controls">
        <label class="level-selector">
          <select aria-label="Level" value={level} onchange={handleLevelChange}>
            {#each LEVELS as l}
              <option value={l}>{LEVEL_NAMES[l]}</option>
            {/each}
          </select>
        </label>

        <label class="express-selector">
          <span>Express</span>
          <select
            aria-label="Express lane mode"
            value={expressMode}
            onchange={(e) => { expressMode = (e.target as HTMLSelectElement).value as ExpressMode; }}
          >
            {#each EXPRESS_MODES as mode}
              <option value={mode}>{EXPRESS_MODE_LABELS[mode]}</option>
            {/each}
          </select>
        </label>
      </div>
    {/if}
  </header>

  <div
    class="spellbook-content"
    role="tabpanel"
    id="tabpanel-spellbook"
    aria-labelledby={
      activeTab === 'spells' ? 'tab-spells'
      : activeTab === 'practice' ? 'tab-practice'
      : 'tab-calibrate'
    }
  >
    {#if activeTab === 'spells'}
      <SpellList />
    {:else if activeTab === 'practice'}
      <FlashcardView
        {level}
        {expressMode}
        {stq8Service}
        initialStats={lastStats}
        onStatsUpdate={handleStatsUpdate}
      />
    {:else}
      <CalibrationView
        {stq8Service}
        isCalibrated={calibrated}
        onCalibrated={handleCalibrated}
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

  .express-selector {
    display: flex;
    align-items: center;
    gap: 4px;
    color: var(--text-secondary, #b5bac1);
    font-size: 0.8125rem;
  }

  .express-selector select {
    padding: 4px 8px;
    border: 1px solid var(--border, #3f4147);
    border-radius: 4px;
    background: var(--bg-tertiary, #313338);
    color: var(--text-primary, #f2f3f5);
    font-size: 0.8125rem;
  }

  .spellbook-content {
    flex: 1;
    overflow-y: auto;
  }

</style>
