<script lang="ts">
  // V3 voice UI (ZEB-351). Driven by a VoiceSession controller: a join flow
  // (connects MUTED), a control bar (Mute / PTT / Deafen / Leave), and a hybrid
  // roster layout — an avatar-tile stage grid for ≤12 participants, collapsing
  // to a compact list beyond.
  import type { VoiceSession, RosterMember } from '../voice-session';

  let { session, channelName, communityId, channelId }: {
    session: VoiceSession;
    channelName: string;
    communityId: string;
    channelId: string;
  } = $props();

  // Alias the store to a NON-rune name: `$state` is a Svelte 5 rune, so
  // `$state.foo` would collide with the rune rather than auto-subscribe to the
  // store. Use `$voiceState` for store auto-subscription throughout the markup.
  // The `session` controller is stable for this view's lifetime, so capturing
  // its store reference once is intentional.
  // svelte-ignore state_referenced_locally
  const voiceState = session.state;

  // Local reactive component state (the `$state` rune is fine here).
  let joining = $state(false);
  let error = $state<string | null>(null);

  // Beyond this count the avatar-tile grid collapses to a compact list.
  const GRID_MAX = 12;

  async function onJoin() {
    joining = true;
    error = null;
    try {
      await session.join(communityId, channelId);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      joining = false;
    }
  }

  // Fire-and-forget control actions. These call async session methods; an IPC
  // failure (e.g. set_voice_muted) must not surface as an unhandled promise
  // rejection from a click handler. setMuted already rolls its local state back
  // on failure. Promise.resolve() tolerates sync- or async-returning methods.
  const swallow = (p: unknown) => { void Promise.resolve(p).catch(() => {}); };
  const toggleMute = () => swallow(session.setMuted(!$voiceState.muted));
  const toggleDeafen = () => swallow(session.setDeafened(!$voiceState.deafened));
  const togglePtt = () => swallow(session.setPttMode(!$voiceState.pttMode));
  const onLeave = () => swallow(session.leave());

  // Push-to-talk hold. Pointer events cover mouse/touch; a window-level Space
  // hotkey covers keyboard. Both drive session.setPttHeld so PTT mode actually
  // transmits (the gate sends only while held).
  const pttDown = () => session.setPttHeld(true);
  const pttUp = () => session.setPttHeld(false);

  function isTypingTarget(t: EventTarget | null): boolean {
    const el = t as HTMLElement | null;
    if (!el || !el.tagName) return false;
    return el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable;
  }
  function onKeyDown(e: KeyboardEvent) {
    if (!$voiceState.pttMode || e.code !== 'Space' || e.repeat) return;
    if (isTypingTarget(e.target)) return;
    e.preventDefault(); // stop page scroll / focused-button activation
    session.setPttHeld(true);
  }
  function onKeyUp(e: KeyboardEvent) {
    if (e.code !== 'Space' || isTypingTarget(e.target)) return;
    session.setPttHeld(false);
  }
  // Losing focus (alt-tab) must drop the hold so the mic can't stick open.
  const onWindowBlur = () => session.setPttHeld(false);

  function label(m: Pick<RosterMember, 'displayName' | 'ownerHex'>): string {
    return m.displayName ?? `${m.ownerHex.slice(0, 6)}…`;
  }
</script>

<svelte:window onkeydown={onKeyDown} onkeyup={onKeyUp} onblur={onWindowBlur} />

<section class="voice-view" aria-label="Voice channel">
  <header class="voice-header">
    <span class="voice-glyph" aria-hidden="true">🔊</span>
    <span class="voice-title">{channelName}</span>
    <span class="voice-count">· {$voiceState.roster.length} here</span>
  </header>

  {#if error}
    <div class="voice-error" role="alert">{error}</div>
  {/if}

  {#if $voiceState.phase === 'idle'}
    <div class="voice-join-pane">
      <button class="btn-primary" onclick={onJoin} disabled={joining}>
        {joining ? 'Joining…' : 'Join Voice'}
      </button>
      <p class="hint">You'll join muted — unmute when you're ready.</p>
    </div>
  {:else}
    <div class="voice-stage" data-testid="voice-stage">
      {#if $voiceState.roster.length <= GRID_MAX}
        <div class="voice-grid" data-testid="voice-grid">
          {#each $voiceState.roster as m (m.deviceHex)}
            <div
              class="voice-tile"
              class:speaking={m.speaking}
              data-testid="voice-tile"
            >
              {#if m.avatarUrl}
                <img class="avatar" src={m.avatarUrl} alt="" />
              {:else}
                <div class="avatar avatar-fallback" aria-hidden="true"></div>
              {/if}
              <span class="name">{label(m)}</span>
              {#if m.muted}<span class="mute-glyph" aria-label="muted">🔇</span>{/if}
            </div>
          {/each}
        </div>
      {:else}
        <ul class="voice-list" data-testid="voice-list">
          {#each $voiceState.roster as m (m.deviceHex)}
            <li
              class="voice-list-row"
              class:speaking={m.speaking}
              data-testid="voice-list-row"
            >
              <span class="dot" class:on={m.speaking} aria-hidden="true"></span>
              <span class="name">{label(m)}</span>
              {#if m.muted}<span class="mute-glyph" aria-label="muted">🔇</span>{/if}
            </li>
          {/each}
        </ul>
      {/if}
    </div>

    <div class="voice-controls">
      {#if $voiceState.pttMode}
        <!-- In PTT mode the mic is hold-gated, so the talk control replaces the
             mute toggle. Press-and-hold (pointer) or hold Space to transmit. -->
        <button
          class="ctrl ptt-hold"
          class:active={$voiceState.pttHeld}
          aria-pressed={$voiceState.pttHeld}
          data-testid="ptt-hold"
          onpointerdown={pttDown}
          onpointerup={pttUp}
          onpointerleave={pttUp}
          onpointercancel={pttUp}
          aria-label="Hold to talk (or hold Space)"
        >
          {$voiceState.pttHeld ? '🎙 Talking…' : '🎙 Hold to Talk'}
        </button>
      {:else}
        <button
          class="ctrl"
          class:active={!$voiceState.muted}
          aria-pressed={!$voiceState.muted}
          onclick={toggleMute}
          aria-label={$voiceState.muted ? 'Unmute' : 'Mute'}
        >
          {$voiceState.muted ? '🔇 Muted' : '🎙 Live'}
        </button>
      {/if}
      <button
        class="ctrl"
        class:active={$voiceState.pttMode}
        aria-pressed={$voiceState.pttMode}
        onclick={togglePtt}
        aria-label="Push to talk mode"
      >
        PTT
      </button>
      <button
        class="ctrl"
        class:active={$voiceState.deafened}
        aria-pressed={$voiceState.deafened}
        onclick={toggleDeafen}
        aria-label="Deafen"
      >
        {$voiceState.deafened ? '🔕 Deafened' : '🔈 Deafen'}
      </button>
      <button class="btn-danger" onclick={onLeave} aria-label="Leave voice">Leave</button>
    </div>
  {/if}
</section>

<style>
  .voice-view {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    height: 100%;
  }
  .voice-header {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--border);
  }
  .voice-glyph {
    font-size: 0.95rem;
  }
  .voice-title {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .voice-count {
    font-size: 0.85rem;
    color: var(--text-secondary);
    white-space: nowrap;
  }

  .voice-error {
    background: var(--bg-tertiary);
    border: 1px solid var(--danger);
    color: var(--danger);
    padding: 8px 14px;
    border-radius: 4px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }

  .voice-join-pane {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.75rem;
    color: var(--text-secondary);
  }
  .btn-primary {
    border: none;
    padding: 8px 22px;
    border-radius: 4px;
    background: var(--accent);
    color: var(--text-primary);
    font-size: 0.9rem;
    cursor: pointer;
  }
  .btn-primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .btn-primary:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .hint {
    font-size: 0.85rem;
    margin: 0;
    color: var(--text-muted);
  }

  /* ---- Roster stage ---- */
  .voice-stage {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 1rem;
  }

  .voice-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(120px, 1fr));
    gap: 0.75rem;
  }
  .voice-tile {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.4rem;
    padding: 0.75rem 0.5rem;
    border-radius: 8px;
    background: var(--bg-secondary);
    position: relative;
  }
  .voice-tile .avatar {
    width: 48px;
    height: 48px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
  }
  .avatar-fallback {
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
  }
  .voice-tile .name {
    font-size: 0.8rem;
    color: var(--text-primary);
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* Speaking ring: accent-colored outline + glow. */
  .voice-tile.speaking {
    outline: 2px solid var(--accent);
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--accent) 30%, transparent);
  }
  .voice-tile .mute-glyph {
    position: absolute;
    top: 6px;
    right: 6px;
    font-size: 0.75rem;
    line-height: 1;
  }

  /* ---- Compact list (>12) ---- */
  .voice-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .voice-list-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 6px 8px;
    border-radius: 4px;
  }
  .voice-list-row:hover {
    background: var(--bg-secondary);
  }
  .voice-list-row .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--text-muted);
    flex-shrink: 0;
  }
  .voice-list-row .dot.on {
    background: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 30%, transparent);
  }
  .voice-list-row.speaking .name {
    color: var(--accent);
  }
  .voice-list-row .name {
    font-size: 0.9rem;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .voice-list-row .mute-glyph {
    margin-left: auto;
    font-size: 0.8rem;
  }

  /* ---- Control bar ---- */
  .voice-controls {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    border-top: 1px solid var(--border);
  }
  .ctrl {
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    padding: 6px 14px;
    border-radius: 4px;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .ctrl:hover {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }
  .ctrl.active {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--text-primary);
  }
  /* Hold-to-talk: suppress touch scroll/selection so a press-hold-release
     gesture stays a clean PTT hold on touch devices. */
  .ptt-hold {
    touch-action: none;
    user-select: none;
  }
  .btn-danger {
    margin-left: auto;
    border: none;
    background: var(--danger);
    color: var(--text-primary);
    padding: 6px 16px;
    border-radius: 4px;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .btn-danger:hover {
    filter: brightness(1.1);
  }
</style>
