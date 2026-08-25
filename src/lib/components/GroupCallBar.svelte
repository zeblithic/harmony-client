<script lang="ts">
  // ZEB-360 (group-DM voice, T13): the app-level in-call bar for group DM calls.
  // Mirrors CallInProgressBar (the 1:1 bar) but iterates `participants` instead of
  // a single peer: a participant tile strip (in-call / ringing / declined) plus the
  // shared Mute / PTT / Deafen / Leave controls and a "Reconnecting…" badge. The 1:1
  // tile component (VoiceChannelView) uses a different roster shape (RosterMember,
  // with power/modMuted), so the tiles are inlined here against `Participant`.
  import { onMount } from 'svelte';
  import type { GroupCallSession } from '../group-call-session';
  import { hexName, type ResolvedName } from '../display-label';
  import PeerName from './PeerName.svelte';

  let { session, groupName }: {
    session: GroupCallSession | null;
    /** Display name of the group DM (for the bar title). */
    groupName?: string;
  } = $props();

  // Track the session's state store reactively so a replaced `session` prop
  // (singleton rebuilt on identity switch) re-points the subscription. Aliased to
  // a NON-rune name so `$callState` auto-subscribes in the markup.
  const callState = $derived(session?.state);

  // Timer tick — updated every second while the call is up.
  let nowMs = $state(Date.now());
  let timerId: ReturnType<typeof setInterval> | null = null;
  onMount(() => {
    timerId = setInterval(() => { nowMs = Date.now(); }, 1000);
    return () => { if (timerId) clearInterval(timerId); };
  });

  function fmtElapsed(startedAt: number | null | undefined): string {
    if (!startedAt) return '0:00';
    const elapsed = Math.max(0, Math.floor((nowMs - startedAt) / 1000));
    const mm = Math.floor(elapsed / 60);
    const ss = String(elapsed % 60).padStart(2, '0');
    return `${mm}:${ss}`;
  }

  function tileName(p: { displayName?: ResolvedName; ownerHex: string }): ResolvedName {
    return p.displayName ?? hexName(`${p.ownerHex.slice(0, 6)}…`);
  }

  function stateLabel(s: 'in-call' | 'ringing' | 'declined'): string {
    return s === 'ringing' ? 'Ringing…' : s === 'declined' ? 'Declined' : '';
  }

  // Fire-and-forget control actions (mirror CallInProgressBar). setMuted/setPttMode
  // roll their own state back on an IPC rejection, so swallowing here is safe.
  const swallow = (p: unknown) => { void Promise.resolve(p).catch(() => {}); };
  const toggleMute = () => { if (session && $callState) swallow(session.setMuted(!$callState.muted)); };
  const togglePtt = () => { if (session && $callState) swallow(session.setPttMode(!$callState.pttMode)); };
  const toggleDeafen = () => { if (session && $callState) swallow(session.setDeafened(!$callState.deafened)); };
  const pttDown = () => session?.setPttHeld(true);
  const pttUp = () => session?.setPttHeld(false);
  // Keyboard hold-to-talk (a11y): Space/Enter hold the PTT gate like a pointer
  // press, so keyboard users aren't locked out of push-to-talk. `e.repeat`
  // guards the OS key-repeat stream so a held key doesn't re-fire setPttHeld.
  function pttKeyDown(e: KeyboardEvent) {
    if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) { e.preventDefault(); session?.setPttHeld(true); }
  }
  function pttKeyUp(e: KeyboardEvent) {
    if (e.key === ' ' || e.key === 'Enter') { e.preventDefault(); session?.setPttHeld(false); }
  }
  const onLeave = () => { if (session) swallow(session.leave()); };
</script>

{#if $callState && ($callState.phase === 'active' || $callState.phase === 'connecting' || $callState.phase === 'leaving')}
  <div class="group-call-bar" data-testid="group-call-bar">
    <div class="bar-head">
      <span class="group-label">{groupName ?? 'Group call'}</span>
      <time class="elapsed">{fmtElapsed($callState.startedAt)}</time>
      {#if $callState.reconnecting}
        <span class="reconnecting" role="status" data-testid="group-call-reconnecting">
          Reconnecting…
        </span>
      {/if}
    </div>

    {#if $callState.participants.length > 0}
      <ul class="tiles" data-testid="group-call-tiles">
        {#each $callState.participants as p (p.ownerHex + ':' + p.deviceHex)}
          <li
            class="tile"
            class:speaking={p.speaking}
            class:pending={p.state !== 'in-call'}
            class:declined={p.state === 'declined'}
            data-testid="group-call-tile"
            data-state={p.state}
          >
            {#if p.avatarUrl}
              <img class="avatar" src={p.avatarUrl} alt="" />
            {:else}
              <div class="avatar avatar-fallback" aria-hidden="true"></div>
            {/if}
            <span class="name"><PeerName name={tileName(p)} ownerIdHex={p.ownerHex} /></span>
            {#if p.state === 'in-call'}
              {#if p.muted}<span class="mute-glyph" aria-label="muted">🔇</span>{/if}
            {:else}
              <span class="pending-label">{stateLabel(p.state)}</span>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}

    <div class="controls">
      {#if $callState.pttMode}
        <button
          class="ctrl ptt-hold"
          class:active={$callState.pttHeld}
          aria-pressed={$callState.pttHeld}
          data-testid="group-ptt-hold"
          onpointerdown={pttDown}
          onpointerup={pttUp}
          onpointerleave={pttUp}
          onpointercancel={pttUp}
          onkeydown={pttKeyDown}
          onkeyup={pttKeyUp}
          onblur={pttUp}
          aria-label="Hold to talk"
        >
          {$callState.pttHeld ? '🎙 Talking…' : '🎙 Hold'}
        </button>
      {:else}
        <button
          class="ctrl"
          class:active={!$callState.muted}
          class:restrictive={$callState.muted}
          aria-pressed={!$callState.muted}
          onclick={toggleMute}
          aria-label={$callState.muted ? 'Unmute' : 'Mute'}
        >
          {$callState.muted ? '🔇 Muted' : '🎙 Live'}
        </button>
      {/if}
      <button
        class="ctrl"
        class:active={$callState.pttMode}
        aria-pressed={$callState.pttMode}
        onclick={togglePtt}
        aria-label="Push to talk mode"
      >
        PTT
      </button>
      <button
        class="ctrl"
        class:restrictive={$callState.deafened}
        data-testid="group-deafen"
        aria-pressed={$callState.deafened}
        onclick={toggleDeafen}
        aria-label={$callState.deafened ? 'Undeafen' : 'Deafen'}
      >
        {$callState.deafened ? '🔕 Deafened' : '🔈 Deafen'}
      </button>
      <button class="btn-leave" onclick={onLeave} aria-label="Leave call">Leave</button>
    </div>
  </div>
{/if}

<style>
  .group-call-bar {
    position: fixed;
    bottom: 0;
    left: 0;
    right: 0;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding: 0.6rem 1rem;
    background: var(--bg-secondary);
    border-top: 1px solid var(--border);
    z-index: 50;
    font-size: 0.875rem;
    color: var(--text-primary);
  }
  .bar-head {
    display: flex;
    align-items: center;
    gap: 0.75rem;
  }
  .group-label {
    font-weight: 600;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .elapsed {
    color: var(--text-secondary);
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
    font-family: var(--font-mono);
  }
  .reconnecting {
    font-size: 0.78rem;
    color: var(--warning);
    background: color-mix(in srgb, var(--warning) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    padding: 2px 8px;
    border-radius: 999px;
    white-space: nowrap;
  }

  .tiles {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
  .tile {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 4px 10px 4px 4px;
    border-radius: 999px;
    background: var(--bg-tertiary);
    position: relative;
  }
  /* Ringing/declined rows are greyed; declined is dimmer still. */
  .tile.pending {
    opacity: 0.6;
  }
  .tile.declined {
    opacity: 0.4;
  }
  .tile.speaking {
    outline: 2px solid var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 30%, transparent);
  }
  .tile .avatar {
    width: 28px;
    height: 28px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
    flex-shrink: 0;
  }
  .avatar-fallback {
    background: var(--bg-secondary);
    border: 1px solid var(--border);
  }
  .tile .name {
    font-size: 0.8rem;
    max-width: 120px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tile .mute-glyph {
    font-size: 0.72rem;
    line-height: 1;
  }
  .tile .pending-label {
    font-size: 0.72rem;
    color: var(--text-secondary);
    font-style: italic;
  }

  .controls {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .ctrl {
    border: 1px solid var(--border);
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    padding: 4px 12px;
    border-radius: 5px;
    font-size: 0.8rem;
    cursor: pointer;
  }
  .ctrl:hover {
    background: var(--bg-hover);
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
    color: var(--text-bright);
  }
  .ptt-hold {
    touch-action: none;
    user-select: none;
  }
  .btn-leave {
    margin-left: auto;
    border: none;
    background: var(--danger);
    color: var(--text-bright);
    padding: 4px 14px;
    border-radius: 5px;
    font-size: 0.85rem;
    cursor: pointer;
    flex-shrink: 0;
  }
  .btn-leave:hover {
    filter: brightness(1.1);
  }
</style>
