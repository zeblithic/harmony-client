<script lang="ts">
  // ZEB-605 T3: Appearance settings — a three-option theme radiogroup
  // (System / Light / Dark) that drives the owner-scoped theme preference.
  // Keyboard model (originally from the removed CodecToggle.svelte, ZEB-976):
  // arrows/Home/End move and select, Space/Enter select, roving tabindex +
  // focus follows selection. This file is now the canonical segmented-control
  // reference (TimeFormatSettings.svelte mirrors it).
  import { tick } from 'svelte';
  import {
    setThemePreference,
    themePreference,
    type ThemePreference,
  } from '../theme-service';

  const OPTIONS: { value: ThemePreference; label: string; hint: string }[] = [
    { value: 'system', label: 'System', hint: 'Follow the operating system appearance' },
    { value: 'light', label: 'Light', hint: 'Commons light' },
    { value: 'dark', label: 'Dark', hint: 'Commons warm dark' },
  ];

  // Bind the STORE (the preference), not dataset.theme: before an owner
  // resolves the preference reads 'system' while dataset.theme may already show
  // the device-hint theme — the control must reflect the preference.
  // Store auto-subscription reads synchronously, so the initial render already
  // shows the resolved preference (PR #407 R1: CodeRabbit + Qodo).
  const current = $derived($themePreference);

  /** Element refs keyed by preference value, for programmatic focus.
   *  `$state` so `bind:this` into its properties is reactive (Svelte 5). */
  const optionRefs: Record<string, HTMLElement | undefined> = $state({});

  async function select(value: ThemePreference): Promise<void> {
    setThemePreference(value);
    // After Svelte updates the roving tabindex, move focus to the new option.
    await tick();
    optionRefs[value]?.focus();
  }

  function onKeydown(e: KeyboardEvent, index: number): void {
    if (e.key === 'Enter') {
      e.preventDefault();
      select(OPTIONS[index].value);
    } else if (e.key === ' ') {
      e.preventDefault();
      select(OPTIONS[index].value);
    } else if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
      e.preventDefault();
      select(OPTIONS[(index + 1) % OPTIONS.length].value);
    } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
      e.preventDefault();
      select(OPTIONS[(index - 1 + OPTIONS.length) % OPTIONS.length].value);
    } else if (e.key === 'Home') {
      e.preventDefault();
      select(OPTIONS[0].value);
    } else if (e.key === 'End') {
      e.preventDefault();
      select(OPTIONS[OPTIONS.length - 1].value);
    }
  }
</script>

<section class="appearance-settings">
  <h3>Appearance</h3>
  <div class="setting-row">
    <div class="setting-text">
      <span class="setting-label" id="theme-label">Theme</span>
      <span class="setting-hint">
        System follows your OS setting. Your choice is saved per identity on this device.
      </span>
    </div>
    <div class="theme-options" role="radiogroup" aria-labelledby="theme-label">
      {#each OPTIONS as option, i (option.value)}
        <button
          type="button"
          role="radio"
          aria-checked={current === option.value}
          tabindex={current === option.value ? 0 : -1}
          class="theme-option"
          class:selected={current === option.value}
          title={option.hint}
          bind:this={optionRefs[option.value]}
          onclick={() => select(option.value)}
          onkeydown={(e) => onKeydown(e, i)}
        >
          {option.label}
        </button>
      {/each}
    </div>
  </div>
</section>

<style>
  /* Tokens only (ZEB-604 ratchet). Row/typography model on
     NetworkDiscoverabilitySettings.svelte; segmented control (canonical here
     since CodecToggle.svelte was removed, ZEB-976) — borders var(--border),
     selected fill var(--primary-soft), selected text var(--primary-deep). */
  .appearance-settings {
    padding: 12px 0;
  }

  .appearance-settings h3 {
    margin: 0 0 8px;
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .setting-row {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }

  .setting-text {
    display: flex;
    flex-direction: column;
    gap: 4px;
    flex: 1;
  }

  .setting-label {
    font-size: 13px;
    color: var(--text-primary);
  }

  .setting-hint {
    font-size: 11px;
    color: var(--text-muted);
    line-height: 1.4;
  }

  .theme-options {
    display: inline-flex;
    border: 1px solid var(--border);
    border-radius: 6px;
    overflow: hidden;
    font-size: 0.75rem;
    flex-shrink: 0;
  }

  .theme-option {
    padding: 4px 12px;
    border: none;
    background: none;
    cursor: pointer;
    color: var(--text-secondary);
    transition: all 0.15s ease;
    user-select: none;
  }

  .theme-option:not(:last-child) {
    border-right: 1px solid var(--border);
  }

  .theme-option.selected {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }

  .theme-option:hover:not(.selected) {
    background: color-mix(in srgb, var(--accent) 10%, transparent);
  }

  .theme-option:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>
