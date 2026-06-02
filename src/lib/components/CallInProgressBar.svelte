<script lang="ts">
  import { onMount } from 'svelte';
  import type { CallSession } from '../call-session';

  let { session, onEnd }: { session: CallSession | null; onEnd: () => void } = $props();

  // Track the session's state store reactively so a replaced `session` prop
  // (e.g. the singleton rebuilt on an identity switch) re-points the
  // subscription instead of staying bound to the old store. Aliased to a
  // NON-rune name so `$callState` auto-subscribes in the markup (`$state` is a
  // rune and would collide).
  const callState = $derived(session?.state);

  // Timer tick — updated every second when the call is active.
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

  function peerLabel(hex: string | null | undefined): string {
    if (!hex) return 'In call';
    return `${hex.slice(0, 6)}…`;
  }

  // Fire-and-forget control actions (mirror VoiceChannelView).
  const swallow = (p: unknown) => { void Promise.resolve(p).catch(() => {}); };
  const toggleMute = () => { if (session && $callState) swallow(session.setMuted(!$callState.muted)); };
  const togglePtt = () => { if (session && $callState) swallow(session.setPttMode(!$callState.pttMode)); };
  const toggleDeafen = () => { if (session && $callState) swallow(session.setDeafened(!$callState.deafened)); };
  const pttDown = () => session?.setPttHeld(true);
  const pttUp = () => session?.setPttHeld(false);
</script>

{#if $callState?.phase === 'active' || $callState?.phase === 'connecting'}
  <div class="call-bar" data-testid="call-bar">
    <span class="peer-label">{peerLabel($callState?.peerOwnerHex)}</span>
    <time class="elapsed">{fmtElapsed($callState?.startedAt)}</time>

    <div class="controls">
      {#if $callState?.pttMode}
        <button
          class="ctrl ptt-hold"
          class:active={$callState?.pttHeld}
          aria-pressed={$callState?.pttHeld}
          data-testid="ptt-hold"
          onpointerdown={pttDown}
          onpointerup={pttUp}
          onpointerleave={pttUp}
          onpointercancel={pttUp}
          aria-label="Hold to talk"
        >
          {$callState?.pttHeld ? '🎙 Talking…' : '🎙 Hold'}
        </button>
      {:else}
        <button
          class="ctrl"
          class:active={!$callState?.muted}
          aria-pressed={!$callState?.muted}
          onclick={toggleMute}
          aria-label={$callState?.muted ? 'Unmute' : 'Mute'}
        >
          {$callState?.muted ? '🔇 Muted' : '🎙 Live'}
        </button>
      {/if}
      <button
        class="ctrl"
        class:active={$callState?.pttMode}
        aria-pressed={$callState?.pttMode}
        onclick={togglePtt}
        aria-label="Push to talk mode"
      >
        PTT
      </button>
      <button
        class="ctrl"
        class:active={$callState?.deafened}
        data-testid="deafen"
        aria-pressed={$callState?.deafened}
        onclick={toggleDeafen}
        aria-label={$callState?.deafened ? 'Undeafen' : 'Deafen'}
      >
        {$callState?.deafened ? '🔕 Deafened' : '🔈 Deafen'}
      </button>
    </div>

    <button class="btn-end" onclick={onEnd} aria-label="End call">End</button>
  </div>
{/if}

<style>
  .call-bar {
    position: fixed;
    bottom: 0;
    left: 0;
    right: 0;
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.6rem 1rem;
    background: var(--bg-secondary, rgba(20, 22, 30, 0.97));
    border-top: 1px solid var(--border, rgba(255, 255, 255, 0.1));
    z-index: 50;
    font-size: 0.875rem;
    color: var(--text-primary, #fff);
  }
  .peer-label {
    font-weight: 600;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .elapsed {
    color: var(--text-secondary, rgba(255, 255, 255, 0.7));
    font-variant-numeric: tabular-nums;
    white-space: nowrap;
  }
  .controls {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-left: auto;
  }
  .ctrl {
    border: 1px solid var(--border, rgba(255, 255, 255, 0.15));
    background: var(--bg-tertiary, rgba(255, 255, 255, 0.05));
    color: var(--text-secondary, rgba(255, 255, 255, 0.7));
    padding: 4px 12px;
    border-radius: 4px;
    font-size: 0.8rem;
    cursor: pointer;
  }
  .ctrl:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.1));
    color: var(--text-primary, #fff);
  }
  .ctrl.active {
    background: var(--accent, #4a9eff);
    border-color: var(--accent, #4a9eff);
    color: #fff;
  }
  .ptt-hold {
    touch-action: none;
    user-select: none;
  }
  .btn-end {
    border: none;
    background: var(--danger, #ef4444);
    color: #fff;
    padding: 4px 14px;
    border-radius: 4px;
    font-size: 0.85rem;
    cursor: pointer;
    flex-shrink: 0;
  }
  .btn-end:hover {
    filter: brightness(1.1);
  }
</style>
