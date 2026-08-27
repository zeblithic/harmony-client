<script lang="ts">
  // ZEB-944: Time & date format settings — two independent radiogroups (Clock
  // and Date order) driving the owner-scoped time-format preference. Keyboard
  // model mirrors AppearanceSettings.svelte exactly: arrows/Home/End move and
  // select, Space/Enter select, roving tabindex + focus follows selection,
  // applied per group.
  import { tick } from 'svelte';
  import {
    setTimeFormatSettings,
    timeFormatSettings,
    type ClockPref,
    type DateOrderPref,
  } from '../time-format-service';

  const CLOCK_OPTIONS: { value: ClockPref; label: string; hint: string }[] = [
    { value: 'system', label: 'System', hint: "Follow this device's clock convention" },
    { value: '12h', label: '12-hour', hint: 'e.g. 8:11 PM' },
    { value: '24h', label: '24-hour', hint: 'e.g. 20:11' },
  ];

  const DATE_OPTIONS: { value: DateOrderPref; label: string; hint: string }[] = [
    { value: 'system', label: 'System', hint: "Follow this device's locale" },
    { value: 'mdy', label: 'M/D', hint: '8/14 — US order' },
    { value: 'dmy', label: 'D/M', hint: '14/8 — European order' },
    { value: 'ymd', label: 'Y-M-D', hint: '2026-08-14 — ISO order' },
  ];

  const settings = $derived($timeFormatSettings);

  // Element refs keyed by "group:value", for programmatic focus after a change.
  // `$state` so `bind:this` into its properties stays reactive (Svelte 5).
  const optionRefs: Record<string, HTMLElement | undefined> = $state({});

  async function selectClock(value: ClockPref): Promise<void> {
    setTimeFormatSettings({ ...settings, clock: value });
    await tick();
    optionRefs[`clock:${value}`]?.focus();
  }

  async function selectDate(value: DateOrderPref): Promise<void> {
    setTimeFormatSettings({ ...settings, dateOrder: value });
    await tick();
    optionRefs[`date:${value}`]?.focus();
  }

  /** Map a key event to the index it should move+select to, or null to ignore. */
  function nextIndex(e: KeyboardEvent, index: number, len: number): number | null {
    if (e.key === 'Enter' || e.key === ' ') return index;
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') return (index + 1) % len;
    if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') return (index - 1 + len) % len;
    if (e.key === 'Home') return 0;
    if (e.key === 'End') return len - 1;
    return null;
  }

  function onClockKeydown(e: KeyboardEvent, index: number): void {
    const next = nextIndex(e, index, CLOCK_OPTIONS.length);
    if (next === null) return;
    e.preventDefault();
    selectClock(CLOCK_OPTIONS[next].value);
  }

  function onDateKeydown(e: KeyboardEvent, index: number): void {
    const next = nextIndex(e, index, DATE_OPTIONS.length);
    if (next === null) return;
    e.preventDefault();
    selectDate(DATE_OPTIONS[next].value);
  }
</script>

<section class="time-format-settings">
  <h3>Time &amp; date</h3>

  <div class="setting-row">
    <div class="setting-text">
      <span class="setting-label" id="time-format-clock-label">Clock</span>
      <span class="setting-hint">
        12-hour (AM/PM) or 24-hour. System follows your device. Saved per identity on this device.
      </span>
    </div>
    <div class="segmented" role="radiogroup" aria-labelledby="time-format-clock-label">
      {#each CLOCK_OPTIONS as option, i (option.value)}
        <button
          type="button"
          role="radio"
          aria-checked={settings.clock === option.value}
          tabindex={settings.clock === option.value ? 0 : -1}
          class="segment"
          class:selected={settings.clock === option.value}
          title={option.hint}
          bind:this={optionRefs[`clock:${option.value}`]}
          onclick={() => selectClock(option.value)}
          onkeydown={(e) => onClockKeydown(e, i)}
        >
          {option.label}
        </button>
      {/each}
    </div>
  </div>

  <div class="setting-row">
    <div class="setting-text">
      <span class="setting-label" id="time-format-date-label">Date order</span>
      <span class="setting-hint">
        How dates read when a message isn't from today. System follows your locale.
      </span>
    </div>
    <div class="segmented" role="radiogroup" aria-labelledby="time-format-date-label">
      {#each DATE_OPTIONS as option, i (option.value)}
        <button
          type="button"
          role="radio"
          aria-checked={settings.dateOrder === option.value}
          tabindex={settings.dateOrder === option.value ? 0 : -1}
          class="segment"
          class:selected={settings.dateOrder === option.value}
          title={option.hint}
          bind:this={optionRefs[`date:${option.value}`]}
          onclick={() => selectDate(option.value)}
          onkeydown={(e) => onDateKeydown(e, i)}
        >
          {option.label}
        </button>
      {/each}
    </div>
  </div>
</section>

<style>
  /* Tokens only (ZEB-604 ratchet). Mirrors AppearanceSettings.svelte: row and
     typography model from NetworkDiscoverabilitySettings.svelte; segmented
     control from AppearanceSettings.svelte — borders var(--border), selected
     fill var(--primary-soft), selected text var(--primary-deep). */
  .time-format-settings {
    padding: 12px 0;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .time-format-settings h3 {
    margin: 0;
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

  .segmented {
    display: inline-flex;
    border: 1px solid var(--border);
    border-radius: 6px;
    overflow: hidden;
    font-size: 0.75rem;
    flex-shrink: 0;
  }

  .segment {
    padding: 4px 12px;
    border: none;
    background: none;
    cursor: pointer;
    color: var(--text-secondary);
    transition: all 0.15s ease;
    user-select: none;
    white-space: nowrap;
  }

  .segment:not(:last-child) {
    border-right: 1px solid var(--border);
  }

  .segment.selected {
    background: var(--primary-soft);
    color: var(--primary-deep);
  }

  .segment:hover:not(.selected) {
    background: color-mix(in srgb, var(--accent) 10%, transparent);
  }

  .segment:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>
