<script lang="ts">
  import type { TrustScore } from '../trust-score';
  import {
    DIMENSIONS,
    buildScore,
    getIdentity,
    getCompliance,
    getAssociation,
    getEndorsement,
  } from '../trust-score';

  let {
    score,
    onScoreChange,
    onClear,
  }: {
    score: TrustScore | null;
    onScoreChange: (score: TrustScore) => void;
    onClear?: () => void;
  } = $props();

  const LEVELS = [0, 1, 2, 3];

  let dimensions = $derived(
    score !== null
      ? [getIdentity(score), getCompliance(score), getAssociation(score), getEndorsement(score)]
      : [null, null, null, null],
  );

  function handleChange(dimIndex: number, level: number) {
    const current = dimensions.map((d) => d ?? 0);
    current[dimIndex] = level;
    onScoreChange(buildScore(current[0], current[1], current[2], current[3]));
  }

  let hexDisplay = $derived(
    score !== null ? `0x${score.toString(16).padStart(2, '0')}` : '--',
  );

  let fractionDisplay = $derived(
    score !== null ? `${score}/255` : '',
  );
</script>

<div class="trust-editor">
  <h3 class="editor-heading">Trust</h3>

  {#each DIMENSIONS as dim, i}
    <fieldset
      class="dimension"
      role="radiogroup"
      aria-label={dim}
    >
      <legend class="dimension-label">{dim}</legend>
      <div class="level-options">
        {#each LEVELS as level}
          <label class="level-option">
            <input
              type="radio"
              name="trust-{dim}"
              value={level}
              checked={dimensions[i] === level}
              onclick={() => handleChange(i, level)}
            />
            <span class="level-value">{level}</span>
          </label>
        {/each}
      </div>
    </fieldset>
  {/each}

  <div class="score-footer">
    <span class="overall-score">
      Overall: {hexDisplay}
      {#if fractionDisplay}
        <span class="fraction">({fractionDisplay})</span>
      {/if}
    </span>
    {#if onClear}
      <button
        class="clear-button"
        onclick={() => onClear?.()}
        aria-label="Clear trust score"
      >
        Clear
      </button>
    {/if}
  </div>
</div>

<style>
  .trust-editor {
    padding: 12px;
    font-size: 13px;
    color: var(--text-primary, #dcddde);
  }

  .editor-heading {
    margin: 0 0 12px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-secondary, #b9bbbe);
  }

  .dimension {
    border: none;
    padding: 0;
    margin: 0 0 8px;
  }

  .dimension-label {
    font-size: 12px;
    font-weight: 500;
    color: var(--text-secondary, #b9bbbe);
    margin-bottom: 4px;
  }

  .level-options {
    display: flex;
    gap: 4px;
  }

  .level-option {
    display: flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
  }

  .level-option input[type='radio'] {
    position: absolute;
    opacity: 0;
    width: 0;
    height: 0;
  }

  .level-value {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 28px;
    border: 1px solid var(--bg-tertiary, #40444b);
    border-radius: 4px;
    background: var(--bg-secondary, #2f3136);
    color: var(--text-secondary, #b9bbbe);
    font-size: 12px;
    font-weight: 500;
    transition: background 0.1s, border-color 0.1s;
  }

  .level-option input[type='radio']:checked + .level-value {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
    color: #ffffff;
  }

  .level-option input[type='radio']:focus-visible + .level-value {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }

  .level-value:hover {
    background: var(--bg-tertiary, #40444b);
  }

  .score-footer {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-top: 12px;
    padding-top: 8px;
    border-top: 1px solid var(--bg-tertiary, #40444b);
  }

  .overall-score {
    font-family: monospace;
    font-size: 12px;
    color: var(--text-secondary, #b9bbbe);
  }

  .fraction {
    color: var(--text-muted, #72767d);
  }

  .clear-button {
    padding: 4px 12px;
    border: 1px solid var(--bg-tertiary, #40444b);
    border-radius: 4px;
    background: var(--bg-secondary, #2f3136);
    color: var(--text-secondary, #b9bbbe);
    font: inherit;
    font-size: 12px;
    cursor: pointer;
  }

  .clear-button:hover {
    background: var(--bg-tertiary, #40444b);
  }

  .clear-button:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
