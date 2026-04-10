<script lang="ts">
  import type { CodecType } from '$lib/voice/voice-codec';
  import CodecToggle from './CodecToggle.svelte';

  let {
    active = false,
    processing = false,
    disabled = false,
    selectedCodec = 'opus' as CodecType,
    onPttStart,
    onPttStop,
    onCodecChange,
  }: {
    active?: boolean;
    processing?: boolean;
    disabled?: boolean;
    selectedCodec?: CodecType;
    onPttStart?: () => void;
    onPttStop?: () => void;
    onCodecChange?: (codec: CodecType) => void;
  } = $props();

  // Track which input sources are currently held to prevent one release
  // from canceling another (e.g. mouse-up while spacebar still held).
  const activeInputs = new Set<string>();

  function activate(source: string) {
    if (disabled) return;
    const wasEmpty = activeInputs.size === 0;
    activeInputs.add(source);
    if (wasEmpty) onPttStart?.();
  }

  function deactivate(source: string) {
    if (disabled) return;
    if (!activeInputs.has(source)) return;
    activeInputs.delete(source);
    if (activeInputs.size === 0) onPttStop?.();
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
    activate('keyboard');
  }

  function handleKeyUp(e: KeyboardEvent) {
    if (e.code !== 'Space' || disabled) return;
    // No form-control guard here — if keyboard was activated, it must deactivate
    // even if focus moved to a form control before release. The has() guard in
    // deactivate() already handles the case where keyboard was never activated.
    deactivate('keyboard');
  }
</script>

<svelte:window onkeydown={handleKeyDown} onkeyup={handleKeyUp} />

<div class="ptt-container">
  <button
    type="button"
    class="ptt-button"
    class:active
    class:processing
    aria-label="Push to talk"
    onmousedown={() => activate('mouse')}
    onmouseup={() => deactivate('mouse')}
    onmouseleave={() => deactivate('mouse')}
    ontouchstart={(e) => { e.preventDefault(); activate('touch'); }}
    ontouchend={(e) => { e.preventDefault(); deactivate('touch'); }}
    ontouchcancel={() => deactivate('touch')}
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
  <CodecToggle
    selected={selectedCodec}
    disabled={active}
    {onCodecChange}
  />
</div>

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

  .ptt-container {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
  }
</style>
