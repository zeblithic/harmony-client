<script lang="ts">
  let {
    active = false,
    processing = false,
    disabled = false,
    onPttStart,
    onPttStop,
  }: {
    active?: boolean;
    processing?: boolean;
    disabled?: boolean;
    onPttStart?: () => void;
    onPttStop?: () => void;
  } = $props();

  function handleMouseDown() {
    if (disabled) return;
    onPttStart?.();
  }

  function handleMouseUp() {
    if (disabled) return;
    onPttStop?.();
  }

  function isFormControl(target: EventTarget | null): boolean {
    const el = target as HTMLElement | null;
    if (!el) return false;
    if (el.classList?.contains('ptt-button')) return false;
    const tag = el.tagName ?? '';
    return ['INPUT', 'SELECT', 'TEXTAREA', 'BUTTON', 'A'].includes(tag);
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.code !== 'Space' || e.repeat || disabled) return;
    if (isFormControl(e.target)) return;
    e.preventDefault();
    onPttStart?.();
  }

  function handleKeyUp(e: KeyboardEvent) {
    if (e.code !== 'Space' || disabled) return;
    if (isFormControl(e.target)) return;
    e.preventDefault();
    onPttStop?.();
  }
</script>

<svelte:window onkeydown={handleKeyDown} onkeyup={handleKeyUp} />

<button
  type="button"
  class="ptt-button"
  class:active
  class:processing
  aria-label="Push to talk"
  onmousedown={handleMouseDown}
  onmouseup={handleMouseUp}
  onmouseleave={active ? handleMouseUp : undefined}
  ontouchstart={(e) => { e.preventDefault(); handleMouseDown(); }}
  ontouchend={(e) => { e.preventDefault(); handleMouseUp(); }}
  {disabled}
>
  <span class="ptt-icon" aria-hidden="true">
    {#if processing}
      ...
    {:else}
      🎤
    {/if}
  </span>
</button>

<style>
  .ptt-button {
    width: 72px;
    height: 72px;
    border-radius: 50%;
    border: 3px solid var(--accent, #5865f2);
    background: transparent;
    color: var(--accent, #5865f2);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: all 0.15s ease;
    font-size: 1.5rem;
  }

  .ptt-button:hover:not(:disabled) {
    background: rgba(88, 101, 242, 0.1);
  }

  .ptt-button.active {
    background: var(--accent, #5865f2);
    color: var(--text-primary, #f2f3f5);
    box-shadow: 0 0 20px rgba(88, 101, 242, 0.4);
  }

  .ptt-button.processing {
    opacity: 0.6;
    cursor: wait;
  }

  .ptt-button:disabled {
    opacity: 0.3;
    cursor: not-allowed;
    border-color: var(--text-muted, #949ba4);
  }

  .ptt-icon {
    pointer-events: none;
  }
</style>
