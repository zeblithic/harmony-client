<script lang="ts">
  // V3 voice UI (ZEB-351). Driven by a VoiceSession controller: a join flow
  // (connects MUTED), a control bar (Mute / PTT / Deafen / Leave), and a hybrid
  // roster layout — an avatar-tile stage grid for ≤12 participants, collapsing
  // to a compact list beyond.
  import type { VoiceSession, RosterMember } from '../voice-session';
  import { hexName, type ResolvedName } from '../display-label';
  import PeerName from './PeerName.svelte';

  let { session, channelName, communityId, channelId, onBeforeJoin }: {
    session: VoiceSession;
    channelName: string;
    communityId: string;
    channelId: string;
    /** ZEB-352 D12: invoked before joining channel voice, so the app can tear
     *  down any active DM call first (one media engine at a time). */
    onBeforeJoin?: () => Promise<void>;
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

  // Clear a stale soft-cap banner when this view switches channels: `channelFull`
  // lives on the app-wide singleton session, so a bounce on one channel must not
  // surface "voice channel full" on another. Re-runs on `channelId` change (and
  // once at mount — a no-op when nothing was bounced).
  $effect(() => {
    channelId; // track: re-run when the viewed channel changes
    session.clearChannelFull();
  });

  // Beyond this count the avatar-tile grid collapses to a compact list.
  const GRID_MAX = 12;

  async function onJoin() {
    joining = true;
    error = null;
    try {
      await onBeforeJoin?.();
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

  // ZEB-358 moderation. A moderator (power ≥ 50) can act on lower-power members.
  // Self and co-equal admins are excluded automatically: self's roster `power`
  // equals selfPower, so `selfPower > m.power` is false.
  const MOD_POWER = 50;
  const canModerate = (m: RosterMember): boolean =>
    $voiceState.selfPower >= MOD_POWER && $voiceState.selfPower > m.power;
  const modMute = (m: RosterMember) =>
    swallow(session.moderate(m.ownerHex, m.modMuted ? 'unmute' : 'mute'));
  // Remove (kick) is a severe-but-reversible action → require a confirming
  // second click (tier-confirmation-to-severity). Tracks the device being
  // confirmed; any other click resets it.
  let confirmingKick = $state<string | null>(null);
  const askKick = (m: RosterMember) => { confirmingKick = m.deviceHex; };
  const doKick = (m: RosterMember) => { confirmingKick = null; swallow(session.moderate(m.ownerHex, 'kick')); };
  // The kick-confirm is transient: any click outside the moderation controls
  // cancels it, so a pending "Confirm" never lingers (Greptile P2). The kick
  // buttons live inside `.mod-controls`, so their own clicks are excluded.
  function onWindowClick(e: MouseEvent) {
    if (!confirmingKick) return;
    const t = e.target as HTMLElement | null;
    if (!t?.closest?.('.mod-controls')) confirmingKick = null;
  }

  // True while a moderator has server-muted or kicked us: the talk controls are
  // disabled (the session also blocks self-unmute defensively).
  const silenced = $derived($voiceState.selfModMuted || $voiceState.selfKicked);

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

  function label(m: Pick<RosterMember, 'displayName' | 'ownerHex'>): ResolvedName {
    return m.displayName ?? hexName(`${m.ownerHex.slice(0, 6)}…`);
  }
</script>

<svelte:window onkeydown={onKeyDown} onkeyup={onKeyUp} onblur={onWindowBlur} onclick={onWindowClick} />

<section class="voice-view" aria-label="Voice channel">
  <header class="voice-header">
    <span class="voice-glyph" aria-hidden="true">🔊</span>
    <span class="voice-title">{channelName}</span>
    <span class="voice-count">· {$voiceState.roster.length} here</span>
  </header>

  {#if $voiceState.channelFull}
    <!-- Soft-cap bounce (ZEB-353): the join was reactively refused because the
         channel was full. Session is back at idle; this explains why. -->
    <div class="voice-full-note" role="alert">Voice channel full — try again later.</div>
  {:else if error}
    <div class="voice-error" role="alert">{error}</div>
  {/if}

  {#if $voiceState.micBlocked}
    <!-- Listen-only fallback (ZEB-353): mic permission was denied or no input
         device exists, so the join continued without a sender. Persistent and
         informational (not an error) — distinct from the transient join `error`
         alert and the `channelFull` alert above. -->
    <div class="voice-mic-blocked" role="status" data-testid="voice-mic-blocked">
      🎤 Mic blocked — listening only
    </div>
  {/if}

  {#if $voiceState.phase === 'idle'}
    <div class="voice-join-pane">
      <span class="join-glyph" data-testid="join-glyph" aria-hidden="true">🔊</span>
      <span class="join-name">{channelName}</span>
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
              <span class="name"><PeerName name={label(m)} ownerIdHex={m.ownerHex} /></span>
              {#if m.muted && !m.modMuted}<span class="mute-glyph" aria-label="muted">🔇</span>{/if}
              {#if m.modMuted}
                <span class="mod-badge" data-testid="mod-muted-badge" title="Muted by a moderator" aria-label="muted by a moderator">🛡</span>
                <span class="mod-sub" data-testid="mod-sub">mod-muted</span>
              {/if}
              {#if canModerate(m)}
                <div class="mod-controls">
                  <button class="mod-btn" data-testid="mod-mute" onclick={() => modMute(m)}
                    aria-label={m.modMuted ? 'Unmute (moderator)' : 'Mute (moderator)'}>
                    {m.modMuted ? 'Unmute' : 'Mute'}
                  </button>
                  {#if confirmingKick === m.deviceHex}
                    <button class="mod-btn danger" data-testid="mod-remove-confirm" onclick={() => doKick(m)}
                      aria-label="Confirm remove">Confirm</button>
                  {:else}
                    <button class="mod-btn" data-testid="mod-remove" onclick={() => askKick(m)}
                      aria-label="Remove from voice">Remove</button>
                  {/if}
                </div>
              {/if}
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
              <span class="name"><PeerName name={label(m)} ownerIdHex={m.ownerHex} /></span>
              {#if m.muted && !m.modMuted}<span class="mute-glyph" aria-label="muted">🔇</span>{/if}
              {#if m.modMuted}
                <span class="mod-badge" data-testid="mod-muted-badge" title="Muted by a moderator" aria-label="muted by a moderator">🛡</span>
                <span class="mod-sub" data-testid="mod-sub">mod-muted</span>
              {/if}
              {#if canModerate(m)}
                <div class="mod-controls">
                  <button class="mod-btn" data-testid="mod-mute" onclick={() => modMute(m)}
                    aria-label={m.modMuted ? 'Unmute (moderator)' : 'Mute (moderator)'}>
                    {m.modMuted ? 'Unmute' : 'Mute'}
                  </button>
                  {#if confirmingKick === m.deviceHex}
                    <button class="mod-btn danger" data-testid="mod-remove-confirm" onclick={() => doKick(m)}
                      aria-label="Confirm remove">Confirm</button>
                  {:else}
                    <button class="mod-btn" data-testid="mod-remove" onclick={() => askKick(m)}
                      aria-label="Remove from voice">Remove</button>
                  {/if}
                </div>
              {/if}
            </li>
          {/each}
        </ul>
      {/if}
    </div>

    {#if $voiceState.selfModMuted}
      <div class="voice-mod-note" role="status" data-testid="self-mod-muted">
        🛡 You've been muted by a moderator. Your talk controls are disabled until they unmute you.
      </div>
    {/if}
    {#if $voiceState.selfKicked}
      <div class="voice-error" role="alert" data-testid="self-kicked">
        🛡️ You were removed from voice by a moderator.
      </div>
    {/if}

    <div class="voice-controls">
      {#if $voiceState.reconnecting}
        <!-- ZEB-353: the inbound media subscriber dropped and is re-declaring
             with backoff; surface a non-blocking "Reconnecting…" badge. -->
        <span class="reconnecting" role="status" data-testid="voice-reconnecting">
          Reconnecting…
        </span>
      {/if}
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
          title="Release to go quiet. Replaces the mute toggle while PTT mode is on."
          disabled={silenced}
        >
          {$voiceState.pttHeld ? '🎙 Transmitting… (hold Space)' : '🎙 Hold to Talk'}
        </button>
      {:else}
        <button
          class="ctrl"
          class:active={!$voiceState.muted}
          class:restrictive={$voiceState.muted}
          aria-pressed={!$voiceState.muted}
          onclick={toggleMute}
          aria-label={$voiceState.muted ? 'Unmute' : 'Mute'}
          disabled={silenced}
          title={silenced ? 'Muted by a moderator' : ''}
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
        disabled={silenced}
      >
        PTT
      </button>
      <button
        class="ctrl"
        class:restrictive={$voiceState.deafened}
        aria-pressed={$voiceState.deafened}
        onclick={toggleDeafen}
        aria-label="Deafen"
      >
        {$voiceState.deafened ? '🔕 Deafened' : '🎧 Deafen'}
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
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--text-secondary);
    white-space: nowrap;
  }

  .voice-error {
    background: var(--bg-tertiary);
    border: 1px solid var(--danger);
    color: var(--danger);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }

  /* Soft-cap bounce: a chosen-limit refusal, not a failure — gov-clay soft
     surface per TH frame C, distinct from the danger .voice-error. */
  .voice-full-note {
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    color: var(--gov-clay-deep);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }

  /* Listen-only note: a persistent, non-error info banner shown when the mic is
     blocked/absent and the session joined without a sender. Amber/informational,
     visually distinct from the red `.voice-error` alert. */
  .voice-mic-blocked {
    background: color-mix(in srgb, var(--warning) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    color: var(--warning);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }

  .voice-join-pane {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: var(--text-secondary);
  }
  .join-glyph {
    font-size: 2rem;
    line-height: 1;
  }
  .join-name {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary);
    margin-bottom: 4px;
  }
  .btn-primary {
    border: none;
    padding: 8px 22px;
    border-radius: 5px;
    background: var(--accent);
    color: var(--on-accent);
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
  /* Speaking ring per TH frame A/B: double ring — paper gap then accent. */
  .voice-tile.speaking {
    box-shadow:
      0 0 0 2.5px var(--bg-primary),
      0 0 0 5px var(--accent);
  }
  /* Muted member: badge chip on the avatar, name goes muted. */
  .voice-tile .mute-glyph {
    position: absolute;
    top: 6px;
    right: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .voice-tile:has(.mute-glyph) .name,
  .voice-list-row:has(.mute-glyph) .name {
    color: var(--text-muted);
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
    /* Anchor the absolutely-positioned `.mod-badge` to the row in the >12
       compact-list layout (the grid `.voice-tile` is already relative). */
    position: relative;
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
    border-radius: 5px;
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
    color: var(--on-accent);
  }
  .ctrl.restrictive {
    background: var(--gov-clay);
    border-color: var(--gov-clay);
    color: var(--text-primary);
  }
  /* Reconnecting badge: a small amber pill on the control bar while the inbound
     media transport is re-establishing. */
  .reconnecting {
    font-size: 0.78rem;
    color: var(--warning);
    background: color-mix(in srgb, var(--warning) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    padding: 3px 9px;
    border-radius: 999px;
    white-space: nowrap;
  }
  /* Hold-to-talk: suppress touch scroll/selection so a press-hold-release
     gesture stays a clean PTT hold on touch devices. Frame C: the held state
     is a full-width accent control with a soft glow ring. */
  .ptt-hold {
    touch-action: none;
    user-select: none;
    flex: 1;
  }
  .ptt-hold.active {
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 35%, transparent);
  }
  .btn-danger {
    margin-left: auto;
    border: none;
    background: var(--danger);
    color: var(--on-accent);
    padding: 6px 16px;
    border-radius: 5px;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .btn-danger:hover {
    filter: brightness(1.1);
  }

  .mod-controls {
    display: flex;
    gap: 4px;
    margin-top: 4px;
    /* Frame B: "Hover a tile to Mute / Remove (mods)" — reveal on hover or
       keyboard focus. pointer-events blocks stray clicks while hidden; the
       buttons stay keyboard-focusable, and :focus-within re-enables them. */
    opacity: 0;
    pointer-events: none;
    transition: opacity 0.12s ease;
  }
  .voice-tile:hover .mod-controls,
  .voice-tile:focus-within .mod-controls,
  .voice-list-row:hover .mod-controls,
  .voice-list-row:focus-within .mod-controls {
    opacity: 1;
    pointer-events: auto;
  }
  .mod-btn { border: 1px solid var(--border); background: var(--bg-tertiary); color: var(--text-secondary);
    font-size: 0.7rem; padding: 2px 6px; border-radius: 3px; cursor: pointer; }
  .mod-btn:hover { color: var(--text-primary); }
  .mod-btn.danger { color: var(--danger); border-color: var(--danger); }
  .mod-badge {
    position: absolute;
    top: 6px;
    left: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--status-recalled-bg);
    border: 1px solid color-mix(in srgb, var(--danger) 30%, transparent);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .mod-sub {
    font-size: 0.7rem;
    color: var(--danger);
    line-height: 1;
  }
  .voice-mod-note {
    background: var(--status-recalled-bg);
    border: 1px solid color-mix(in srgb, var(--danger) 35%, transparent);
    color: var(--danger);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 0 16px 8px;
    font-size: 0.85rem;
  }
</style>
