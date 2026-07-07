<script lang="ts">
  let {
    steps,
    activeIndex,
    showCounter = true,
  }: {
    steps: { label: string; accent: 'sage' | 'clay' }[];
    activeIndex: number;
    showCounter?: boolean;
  } = $props();
</script>

<div class="wizard-progress" data-testid="wizard-progress">
  {#if showCounter}
    <span class="wizard-progress-counter">Step {activeIndex + 1} of {steps.length}</span>
  {/if}
  <ol class="wizard-progress-pips" aria-hidden="true">
    {#each steps as step, i (i)}
      <li
        class="wizard-progress-pip"
        class:is-active={i === activeIndex}
        class:accent-clay={step.accent === 'clay'}
      ></li>
    {/each}
  </ol>
</div>

<style>
  .wizard-progress {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 10px;
  }
  .wizard-progress-counter {
    font-family: var(--font-mono);
    font-size: 0.69rem;
    font-weight: 600;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--text-muted);
  }
  .wizard-progress-pips {
    display: flex;
    align-items: center;
    gap: 7px;
    list-style: none;
    padding: 0;
    margin: 0;
  }
  .wizard-progress-pip {
    width: 6px;
    height: 6px;
    border-radius: 3px;
    background: var(--faint);
    transition: width 0.2s ease, background 0.2s ease;
  }
  .wizard-progress-pip.is-active {
    width: 24px;
    background: var(--accent);
  }
  .wizard-progress-pip.is-active.accent-clay {
    background: var(--gov-clay);
  }
</style>
