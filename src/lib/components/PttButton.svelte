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
    // Intentionally NOT guarded by `disabled`: a release must always
    // unwind state for an input that was previously activated. If
    // `disabled` flips true mid-hold (e.g., calibration lost, permission
    // revoked, rate limit tripped), the parent still needs onPttStop to
    // fire or its `pttActive` stays stuck true forever. The has() check
    // below already rejects releases for sources that were never
    // activated in the first place.
    if (!activeInputs.has(source)) return;
    activeInputs.delete(source);
    if (activeInputs.size === 0) onPttStop?.();
  }

  function isFormControl(target: EventTarget | null): boolean {
    const el = target as HTMLElement | null;
    if (!el) return false;
    if (el.classList?.contains('ptt-button')) return false;
    const tag = el.tagName ?? '';
    if (['INPUT', 'SELECT', 'TEXTAREA', 'BUTTON', 'A'].includes(tag)) return true;
    // Also treat elements with interactive ARIA roles as form controls
    // (e.g., role="radio"/"switch" divs rendered by sibling settings controls)
    const role = el.getAttribute?.('role') ?? '';
    return ['button', 'radio', 'checkbox', 'switch', 'slider', 'spinbutton', 'combobox', 'listbox', 'textbox'].includes(role);
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.code !== 'Space' || e.repeat || disabled) return;
    if (isFormControl(e.target)) return;
    e.preventDefault();
    activate('keyboard');
  }

  function handleKeyUp(e: KeyboardEvent) {
    if (e.code !== 'Space') return;
    // No form-control guard here — if keyboard was activated, it must deactivate
    // even if focus moved to a form control before release. No `disabled` guard
    // either, for the same reason as deactivate(): a release of an input that
    // was previously activated must always unwind. The has() guard in
    // deactivate() already handles the case where keyboard was never activated.
    deactivate('keyboard');
  }

  function handleMouseUp() {
    // Window-level mouse release. Modern browsers (Chrome M120+, Safari 17+,
    // Firefox 124+) block mouseup/click on disabled form controls per HTML
    // spec, so if `disabled` flipped true during a held press, the button's
    // own onmouseup never fires — listening on window bypasses that filter.
    // Also fires for any page-wide mouseup when the user wasn't holding PTT,
    // but deactivate()'s has('mouse') check makes that a no-op.
    deactivate('mouse');
  }
</script>

<svelte:window onkeydown={handleKeyDown} onkeyup={handleKeyUp} onmouseup={handleMouseUp} />

<div class="ptt-container">
  <button
    type="button"
    class="ptt-button"
    class:active
    class:processing
    aria-label="Push to talk"
    onmousedown={() => activate('mouse')}
    onmouseup={() => deactivate('mouse')}
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
</div>

<style>
  .ptt-button {
    width: 72px;
    height: 72px;
    border-radius: 50%;
    border: 3px solid var(--accent);
    background: transparent;
    color: var(--accent);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: all 0.15s ease;
    font-size: 1.5rem;
  }

  .ptt-button:hover:not(:disabled) {
    background: color-mix(in srgb, var(--accent) 10%, transparent);
  }

  .ptt-button.active {
    background: var(--accent);
    color: var(--on-accent);
    box-shadow: 0 0 20px color-mix(in srgb, var(--accent) 40%, transparent);
  }

  .ptt-button.processing {
    opacity: 0.6;
    cursor: wait;
  }

  .ptt-button:disabled {
    opacity: 0.3;
    cursor: not-allowed;
    border-color: var(--text-muted);
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
