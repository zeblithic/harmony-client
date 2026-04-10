<script lang="ts">
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

  function select(codec: CodecType) {
    if (disabled || codec === selected) return;
    onCodecChange?.(codec);
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
    border: 1px solid var(--border, #3f4147);
    border-radius: 6px;
    overflow: hidden;
    font-size: 0.75rem;
  }

  .codec-option {
    padding: 4px 10px;
    cursor: pointer;
    color: var(--text-secondary, #b5bac1);
    transition: all 0.15s ease;
    user-select: none;
  }

  .codec-option:not(:last-child) {
    border-right: 1px solid var(--border, #3f4147);
  }

  .codec-option.selected {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
  }

  .codec-option:hover:not(.selected):not([aria-disabled='true']) {
    background: rgba(88, 101, 242, 0.1);
  }

  .codec-option[aria-disabled='true'] {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .codec-option:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }
</style>
