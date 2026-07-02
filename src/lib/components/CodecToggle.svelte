<script lang="ts">
  import { tick } from 'svelte';
  import type { CodecType } from '../voice/voice-codec';

  let {
    selected = 'opus' as CodecType,
    disabled = false,
    onCodecChange,
  }: {
    selected?: CodecType;
    disabled?: boolean;
    onCodecChange?: (codec: CodecType) => void;
  } = $props();

  const options: { value: CodecType; label: string }[] = [
    { value: 'opus', label: 'Opus' },
    { value: 'codec2', label: 'codec2' },
  ];

  /** Element refs keyed by codec type, for programmatic focus. */
  const optionRefs: Record<string, HTMLElement | undefined> = {};

  async function select(codec: CodecType) {
    if (disabled || codec === selected) return;
    onCodecChange?.(codec);
    // After Svelte updates tabindex, move focus to the newly selected option
    await tick();
    optionRefs[codec]?.focus();
  }

  function handleKeyDown(e: KeyboardEvent, codec: CodecType) {
    if (disabled) return;

    if (e.key === 'Enter') {
      select(codec);
    } else if (e.key === ' ') {
      e.preventDefault();
      select(codec);
    } else if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
      e.preventDefault();
      const idx = options.findIndex((o) => o.value === codec);
      const next = options[(idx + 1) % options.length];
      select(next.value);
    } else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
      e.preventDefault();
      const idx = options.findIndex((o) => o.value === codec);
      const prev = options[(idx - 1 + options.length) % options.length];
      select(prev.value);
    }
  }
</script>

<div
  class="codec-toggle"
  role="radiogroup"
  aria-label="Voice codec"
>
  {#each options as option}
    <div
      class="codec-option"
      class:selected={selected === option.value}
      role="radio"
      aria-checked={selected === option.value}
      aria-label={option.label}
      aria-disabled={disabled}
      tabindex={disabled ? -1 : selected === option.value ? 0 : -1}
      bind:this={optionRefs[option.value]}
      onclick={() => select(option.value)}
      onkeydown={(e) => handleKeyDown(e, option.value)}
    >
      {option.label}
    </div>
  {/each}
</div>

<style>
  .codec-toggle {
    display: inline-flex;
    border: 1px solid var(--border);
    border-radius: 6px;
    overflow: hidden;
    font-size: 0.75rem;
  }

  .codec-option {
    padding: 4px 10px;
    cursor: pointer;
    color: var(--text-secondary);
    transition: all 0.15s ease;
    user-select: none;
  }

  .codec-option:not(:last-child) {
    border-right: 1px solid var(--border);
  }

  .codec-option.selected {
    background: var(--accent);
    color: var(--text-primary);
  }

  .codec-option:hover:not(.selected):not([aria-disabled='true']) {
    background: rgba(88, 101, 242, 0.1);
  }

  .codec-option[aria-disabled='true'] {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .codec-option:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>
