<script lang="ts">
  /**
   * ZEB-612 S5 — TownHallView: voice fused with the Assembly (spec §6, TH
   * frame A). Rendered by CommunityView for kind === 'townhall'. Reuses the
   * app-lifetime VoiceSession (join/PTT/mute/deafen/mod) plus S4's hand +
   * invite surface.
   *
   * Honesty ledger (§8): no transcript quote (no transcription), elapsed-only
   * speaking timer (no speak-limit policy), no header meeting timer (no
   * room-start record), no agenda line (no channel topic exists — S4 pin 4).
   *
   * Dominant speaker is LOCAL inference (plan Pin 4): speaking-start stamps
   * recorded per device at flip-to-true; dominant = earliest active start
   * (longest floor hold); fallback = most recent speaker still in the room.
   */
  import { onMount } from 'svelte';
  import type { VoiceSession, RosterMember } from '../voice-session';
  import { speakerQueue } from '../voice-session';
  import type { VotingAdapter } from '../voting-adapter';
  import type { ChannelMessageService, ChannelMessageDto } from '../channel-message-service';
  import type { ResolvedCard } from '../member-card-service';
  import type { MentionCandidate } from '../mention-compose';
  import ChannelMessageFeed from './ChannelMessageFeed.svelte';
  import TownHallMotionCard from './TownHallMotionCard.svelte';

  let {
    session,
    channelName,
    communityId,
    channelId,
    channelSyncing = false,
    onBeforeJoin,
    ownAddress,
    myPower,
    adminQuorum,
    votingAdapter,
    channelMessageService,
    resolveCard,
    resolveNickname,
    resolveRosterName,
    onOpenCard,
    mentionCandidates = [],
    snapshotMessages = [],
    originalCommunityName = '',
    forkedAtMs = 0,
    forkReason = null,
    onOpenProposals,
  }: {
    session: VoiceSession;
    channelName: string;
    communityId: string;
    channelId: string;
    channelSyncing?: boolean;
    onBeforeJoin?: () => Promise<void>;
    ownAddress: string;
    myPower: number;
    /** Community admin quorum for the motion card; null while loading. */
    adminQuorum: number | null;
    votingAdapter?: VotingAdapter;
    channelMessageService: ChannelMessageService;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    resolveRosterName?: (ownerIdHex: string) => string | undefined;
    onOpenCard?: (
      payload: {
        ownerIdHex: string;
        displayName: string;
        statusText: string;
        power?: number;
        membershipStatus?: string;
        avatarUrl?: string;
      },
      ev: MouseEvent,
    ) => void;
    mentionCandidates?: MentionCandidate[];
    snapshotMessages?: ChannelMessageDto[];
    originalCommunityName?: string;
    forkedAtMs?: number;
    forkReason?: string | null;
    /** Deep-link to the community proposals view (motion-card async path). */
    onOpenProposals?: () => void;
  } = $props();

  // svelte-ignore state_referenced_locally
  const voiceState = session.state;

  let joining = $state(false);
  let error = $state<string | null>(null);

  // channelFull lives on the app-wide singleton; clear on mount/channel switch
  // (same discipline as VoiceChannelView).
  $effect(() => {
    channelId;
    session.clearChannelFull();
  });

  const GRID_VISIBLE = 18;
  const MOD_POWER = 50;

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

  const swallow = (p: unknown) => {
    void Promise.resolve(p).catch(() => {});
  };
  const toggleMute = () => swallow(session.setMuted(!$voiceState.muted));
  const toggleDeafen = () => swallow(session.setDeafened(!$voiceState.deafened));
  const togglePtt = () => swallow(session.setPttMode(!$voiceState.pttMode));
  const onLeave = () => swallow(session.leave());
  const pttDown = () => session.setPttHeld(true);
  const pttUp = () => session.setPttHeld(false);

  const silenced = $derived($voiceState.selfModMuted || $voiceState.selfKicked);

  // ── Raise hand (per-device beacon; optimistic local toggle) ─────────────
  let myHandRaised = $state(false);
  function toggleHand() {
    const next = !myHandRaised;
    myHandRaised = next;
    void session.setHand(next).catch(() => {
      myHandRaised = !next;
    });
  }
  // Leaving the room lowers the local toggle (the beacon dies with the session).
  $effect(() => {
    if ($voiceState.phase === 'idle') myHandRaised = false;
  });

  // ── Invited banner state ─────────────────────────────────────────────────
  // Accept/dismiss only mark the invite handled AFTER the awaited IPCs
  // succeed — a swallowed failure must not hide the banner while the backend
  // hand/mute state is unchanged (Qodo #443 round 1). While mod-silenced the
  // Unmute button is disabled instead (setMuted(false) is a defensive no-op
  // there, which would otherwise hide the banner with the user still muted).
  let inviteHandled = $state(false);
  let inviteBusy = $state(false);
  let inviteError = $state<string | null>(null);
  $effect(() => {
    if (!$voiceState.selfInvited) {
      inviteHandled = false;
      inviteError = null;
    }
  });
  async function acceptInvite() {
    if (inviteBusy) return;
    inviteBusy = true;
    inviteError = null;
    try {
      await session.setMuted(false);
      await session.setHand(false);
      myHandRaised = false;
      inviteHandled = true;
    } catch (e) {
      inviteError = e instanceof Error ? e.message : String(e);
    } finally {
      inviteBusy = false;
    }
  }
  async function dismissInvite() {
    if (inviteBusy) return;
    inviteBusy = true;
    inviteError = null;
    try {
      await session.setHand(false);
      myHandRaised = false;
      inviteHandled = true;
    } catch (e) {
      inviteError = e instanceof Error ? e.message : String(e);
    } finally {
      inviteBusy = false;
    }
  }

  // ── Presence & queue ─────────────────────────────────────────────────────
  let presentCount = $derived(new Set($voiceState.roster.map((m) => m.ownerHex)).size);
  let queue = $derived(speakerQueue($voiceState.roster));
  let visibleRoster = $derived($voiceState.roster.slice(0, GRID_VISIBLE));
  let overflowCount = $derived(Math.max(0, $voiceState.roster.length - GRID_VISIBLE));

  // ── Dominant-speaker inference (plan Pin 4) ─────────────────────────────
  // Non-reactive map: deviceHex → wall-clock ms when speaking flipped true.
  const speakStart = new Map<string, number>();
  let dominantKey = $state<string | null>(null);
  let lastSpeakerKey = $state<string | null>(null);
  let nowTick = $state(Date.now());

  $effect(() => {
    const roster = $voiceState.roster;
    const now = Date.now();
    const active = new Set<string>();
    for (const m of roster) {
      if (m.speaking) {
        active.add(m.deviceHex);
        if (!speakStart.has(m.deviceHex)) speakStart.set(m.deviceHex, now);
        lastSpeakerKey = m.deviceHex;
      }
    }
    for (const k of [...speakStart.keys()]) {
      if (!active.has(k)) speakStart.delete(k);
    }
    let best: string | null = null;
    let bestT = Infinity;
    for (const [k, t] of speakStart) {
      if (t < bestT) {
        bestT = t;
        best = k;
      }
    }
    dominantKey = best;
  });

  onMount(() => {
    const t = setInterval(() => {
      nowTick = Date.now();
    }, 1000);
    return () => clearInterval(t);
  });

  let spotlight = $derived.by((): RosterMember | null => {
    const roster = $voiceState.roster;
    if (dominantKey) return roster.find((m) => m.deviceHex === dominantKey) ?? null;
    if (lastSpeakerKey) return roster.find((m) => m.deviceHex === lastSpeakerKey) ?? null;
    return null;
  });

  function fmtElapsed(ms: number): string {
    const s = Math.max(0, Math.floor(ms / 1000));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  }
  let spotlightElapsed = $derived.by(() => {
    if (!spotlight || !spotlight.speaking || !dominantKey) return null;
    const start = speakStart.get(dominantKey);
    return start === undefined ? null : fmtElapsed(nowTick - start);
  });

  // ── Moderation (grid hover controls — capability parity with voice) ─────
  const canModerate = (m: RosterMember): boolean =>
    $voiceState.selfPower >= MOD_POWER && $voiceState.selfPower > m.power;
  const modMute = (m: RosterMember) =>
    swallow(session.moderate(m.ownerHex, m.modMuted ? 'unmute' : 'mute'));
  let confirmingKick = $state<string | null>(null);
  const askKick = (m: RosterMember) => {
    confirmingKick = m.deviceHex;
  };
  const doKick = (m: RosterMember) => {
    confirmingKick = null;
    swallow(session.moderate(m.ownerHex, 'kick'));
  };
  function onWindowClick(e: MouseEvent) {
    if (!confirmingKick) return;
    const t = e.target as HTMLElement | null;
    if (!t?.closest?.('.mod-controls')) confirmingKick = null;
  }

  // PTT Space hotkey (same rules as VoiceChannelView; the backchannel composer
  // is a typing target so Space there never transmits).
  function isTypingTarget(t: EventTarget | null): boolean {
    const el = t as HTMLElement | null;
    if (!el || !el.tagName) return false;
    return el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable;
  }
  function onKeyDown(e: KeyboardEvent) {
    if (!$voiceState.pttMode || e.code !== 'Space' || e.repeat) return;
    if (isTypingTarget(e.target)) return;
    e.preventDefault();
    session.setPttHeld(true);
  }
  function onKeyUp(e: KeyboardEvent) {
    if (e.code !== 'Space' || isTypingTarget(e.target)) return;
    session.setPttHeld(false);
  }
  const onWindowBlur = () => session.setPttHeld(false);

  function label(m: Pick<RosterMember, 'displayName' | 'ownerHex'>): string {
    return m.displayName ?? `${m.ownerHex.slice(0, 6)}…`;
  }
</script>

<svelte:window onkeydown={onKeyDown} onkeyup={onKeyUp} onblur={onWindowBlur} onclick={onWindowClick} />

<section class="townhall-view" aria-label="Town hall">
  <header class="th-header">
    <span class="th-glyph" aria-hidden="true">⚖</span>
    <span class="th-title">{channelName}</span>
    {#if $voiceState.roster.length > 0}
      <span class="th-live-dot" data-testid="th-live-dot" aria-label="live"></span>
    {/if}
    <span class="th-present" data-testid="th-present">· {presentCount} present</span>
  </header>

  {#if $voiceState.channelFull}
    <div class="th-full-note" role="alert">Assembly full — try again later.</div>
  {:else if error}
    <div class="th-error" role="alert">{error}</div>
  {/if}
  {#if $voiceState.micBlocked}
    <div class="th-mic-blocked" role="status">🎤 Mic blocked — listening only</div>
  {/if}

  {#if $voiceState.phase === 'idle'}
    <div class="th-join-pane">
      <span class="join-glyph" aria-hidden="true">⚖</span>
      <span class="join-name">{channelName}</span>
      <button class="btn-primary" onclick={onJoin} disabled={joining}>
        {joining ? 'Joining…' : 'Join Assembly'}
      </button>
      <p class="hint">Join the assembly — you'll join muted.</p>
    </div>
  {:else}
    <div class="th-body">
      <div class="th-main">
        <section class="th-spotlight-box" aria-label="On the floor">
          <h3 class="th-section-label">On the floor</h3>
          {#if spotlight}
            <div class="th-spotlight" data-testid="th-spotlight">
              {#if spotlight.avatarUrl}
                <img class="sp-avatar" src={spotlight.avatarUrl} alt="" />
              {:else}
                <div class="sp-avatar sp-avatar-fallback" aria-hidden="true"></div>
              {/if}
              <div class="sp-info">
                <span class="sp-name">
                  {label(spotlight)}
                  {#if spotlight.power >= MOD_POWER}
                    <span class="mod-pill" data-testid="th-mod-pill">MOD</span>
                  {/if}
                </span>
                {#if spotlight.speaking}
                  <span class="sp-status">
                    🎙 speaking
                    {#if spotlightElapsed}
                      · <span class="sp-timer" data-testid="th-timer">{spotlightElapsed}</span>
                    {/if}
                  </span>
                {/if}
              </div>
              <div
                class="waveform"
                class:live={spotlight.speaking}
                data-testid="th-waveform"
                aria-hidden="true"
              >
                {#each Array(7) as _, i (i)}
                  <span class="wf-bar" style="animation-delay: {-(i * 0.13)}s"></span>
                {/each}
              </div>
            </div>
          {:else}
            <p class="th-spotlight-empty" data-testid="th-spotlight-empty">No one has the floor.</p>
          {/if}
        </section>

        <section class="th-room" aria-label="In the room">
          <h3 class="th-section-label">In the room · {$voiceState.roster.length}</h3>
          <div class="th-grid">
            {#each visibleRoster as m (m.deviceHex)}
              <div class="th-tile" class:speaking={m.speaking} data-testid="th-tile">
                {#if m.avatarUrl}
                  <img class="avatar" src={m.avatarUrl} alt="" />
                {:else}
                  <div class="avatar avatar-fallback" aria-hidden="true"></div>
                {/if}
                {#if m.handRaisedAt !== null}
                  <span class="hand-badge" data-testid="th-hand-badge" aria-label="hand raised">✋</span>
                {/if}
                {#if m.muted && !m.modMuted}
                  <span class="mute-glyph" aria-label="muted">🔇</span>
                {/if}
                {#if m.modMuted}
                  <span class="mod-badge" title="Muted by a moderator" aria-label="muted by a moderator">🛡</span>
                {/if}
                <span class="name">{label(m)}</span>
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
            {#if overflowCount > 0}
              <div class="th-tile th-overflow" data-testid="th-overflow">+{overflowCount} more</div>
            {/if}
          </div>
        </section>
      </div>

      <aside class="th-rail" aria-label="Floor">
        <section class="th-queue" aria-label="Speaker queue">
          <h3 class="th-section-label">Speaker queue</h3>
          {#if queue.length === 0}
            <p class="th-queue-empty">No raised hands yet.</p>
          {:else}
            <ol class="th-queue-list">
              {#each queue as m, i (m.deviceHex)}
                <li class="th-queue-row" class:dimmed={m.invited} data-testid="th-queue-row">
                  <span class="q-num" class:first={i === 0}>{i + 1}</span>
                  {#if m.avatarUrl}
                    <img class="q-avatar" src={m.avatarUrl} alt="" />
                  {:else}
                    <div class="q-avatar q-avatar-fallback" aria-hidden="true"></div>
                  {/if}
                  <span class="q-text">
                    <span class="q-name">{label(m)}</span>
                    <span class="q-wants">wants to speak ✋</span>
                  </span>
                  {#if m.invited}
                    <span class="q-invited">invited</span>
                  {:else}
                    <button
                      type="button"
                      class="q-invite"
                      class:primary={i === 0}
                      data-testid="th-invite"
                      disabled={$voiceState.selfPower < MOD_POWER}
                      title={$voiceState.selfPower < MOD_POWER ? 'Moderators can invite to speak' : ''}
                      onclick={() => swallow(session.inviteToSpeak(m.ownerHex))}
                    >Invite</button>
                  {/if}
                </li>
              {/each}
            </ol>
          {/if}
        </section>

        <TownHallMotionCard
          {communityId}
          {channelId}
          adapter={votingAdapter}
          {presentCount}
          quorum={adminQuorum}
          canAct={$voiceState.phase === 'connected'}
          {onOpenProposals}
        />

        <section class="th-backchannel" aria-label="Backchannel">
          <h3 class="th-section-label">Backchannel</h3>
          <div class="th-backchannel-feed">
            <ChannelMessageFeed
              {communityId}
              {channelId}
              {channelName}
              {channelSyncing}
              {channelMessageService}
              {votingAdapter}
              {ownAddress}
              {myPower}
              {snapshotMessages}
              {originalCommunityName}
              {forkedAtMs}
              {forkReason}
              {resolveCard}
              {resolveNickname}
              {resolveRosterName}
              {onOpenCard}
              {mentionCandidates}
              composerPlaceholder="Message the room…"
            />
          </div>
        </section>
      </aside>
    </div>

    {#if $voiceState.selfInvited && !inviteHandled}
      <div class="th-invited-banner" role="status" data-testid="th-invited-banner">
        <span>You've been invited to speak — Unmute?</span>
        <button
          type="button"
          class="btn-primary"
          data-testid="th-invite-accept"
          disabled={inviteBusy || silenced}
          title={silenced ? 'Muted by a moderator' : ''}
          onclick={acceptInvite}
        >Unmute</button>
        <button
          type="button"
          class="ctrl"
          data-testid="th-invite-dismiss"
          disabled={inviteBusy}
          onclick={dismissInvite}
        >Dismiss</button>
        {#if inviteError}
          <span class="th-invite-error" role="alert">{inviteError}</span>
        {/if}
      </div>
    {/if}
    {#if $voiceState.selfModMuted}
      <div class="th-mod-note" role="status" data-testid="self-mod-muted">
        🛡 You've been muted by a moderator. Your talk controls are disabled until they unmute you.
      </div>
    {/if}
    {#if $voiceState.selfKicked}
      <div class="th-error" role="alert" data-testid="self-kicked">
        🛡️ You were removed from voice by a moderator.
      </div>
    {/if}

    <div class="th-controls">
      {#if $voiceState.reconnecting}
        <span class="reconnecting" role="status">Reconnecting…</span>
      {/if}
      {#if $voiceState.pttMode}
        <button
          class="ctrl ptt-hold"
          class:active={$voiceState.pttHeld}
          aria-pressed={$voiceState.pttHeld}
          onpointerdown={pttDown}
          onpointerup={pttUp}
          onpointerleave={pttUp}
          onpointercancel={pttUp}
          aria-label="Hold to talk (or hold Space)"
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
      >PTT</button>
      <button
        class="ctrl"
        class:restrictive={$voiceState.deafened}
        aria-pressed={$voiceState.deafened}
        onclick={toggleDeafen}
        aria-label="Deafen"
      >
        {$voiceState.deafened ? '🔕 Deafened' : '🎧 Deafen'}
      </button>
      <button
        class="ctrl"
        class:active={myHandRaised}
        aria-pressed={myHandRaised}
        data-testid="th-raise-hand"
        onclick={toggleHand}
        aria-label={myHandRaised ? 'Lower hand' : 'Raise hand'}
      >
        {myHandRaised ? '✋ Lower hand' : '✋ Raise hand'}
      </button>
      <button class="btn-danger" onclick={onLeave} aria-label="Leave assembly">Leave</button>
    </div>
  {/if}
</section>

<style>
  .townhall-view {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    height: 100%;
  }
  .th-header {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--border);
  }
  .th-glyph { font-size: 0.95rem; }
  .th-title {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .th-live-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent);
    flex-shrink: 0;
  }
  .th-present {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--text-secondary);
    white-space: nowrap;
  }
  .th-error {
    background: var(--bg-tertiary);
    border: 1px solid var(--danger);
    color: var(--danger);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }
  .th-full-note {
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    color: var(--gov-clay-deep);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }
  .th-mic-blocked {
    background: color-mix(in srgb, var(--warning) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    color: var(--warning);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }
  .th-join-pane {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: var(--text-secondary);
  }
  .join-glyph { font-size: 2rem; line-height: 1; }
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
  .btn-primary:hover:not(:disabled) { background: var(--accent-hover); }
  .btn-primary:disabled { opacity: 0.6; cursor: not-allowed; }
  .hint { font-size: 0.85rem; margin: 0; color: var(--text-muted); }

  .th-body {
    display: flex;
    flex: 1;
    min-height: 0;
  }
  .th-main {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 1rem;
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }
  .th-section-label {
    margin: 0 0 0.5rem;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }

  /* ── Spotlight ── */
  .th-spotlight {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 14px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-secondary);
  }
  .sp-avatar {
    width: 64px;
    height: 64px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
    flex-shrink: 0;
    /* Double ring per TH frame A: paper gap then accent. */
    box-shadow:
      0 0 0 3px var(--bg-primary),
      0 0 0 6px var(--accent);
  }
  .sp-avatar-fallback { background: var(--bg-tertiary); border: 1px solid var(--border); }
  .sp-info { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
  .sp-name {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary);
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .mod-pill {
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.06em;
    color: var(--gov-clay-deep);
    background: var(--gov-clay-soft);
    border-radius: 4px;
    padding: 1px 5px;
  }
  .sp-status { font-size: 0.82rem; color: var(--accent); }
  .sp-timer { font-family: var(--font-mono); color: var(--text-secondary); }
  .th-spotlight-empty {
    margin: 0;
    padding: 14px;
    border: 1px dashed var(--border);
    border-radius: 10px;
    color: var(--text-muted);
    font-size: 0.85rem;
  }

  /* Decorative waveform — animates only while the floor-holder is speaking. */
  .waveform {
    margin-left: auto;
    display: flex;
    align-items: flex-end;
    gap: 3px;
    height: 28px;
    flex-shrink: 0;
  }
  .wf-bar {
    width: 4px;
    height: 30%;
    border-radius: 2px;
    background: color-mix(in srgb, var(--accent) 45%, transparent);
  }
  .waveform.live .wf-bar {
    background: var(--accent);
    animation: wf-bounce 0.9s ease-in-out infinite alternate;
  }
  @keyframes wf-bounce {
    from { height: 20%; }
    to { height: 100%; }
  }
  @media (prefers-reduced-motion: reduce) {
    .wf-bar { animation: none !important; }
  }

  /* ── In-room grid ── */
  .th-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(104px, 1fr));
    gap: 0.65rem;
  }
  .th-tile {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.35rem;
    padding: 0.65rem 0.4rem;
    border-radius: 8px;
    background: var(--bg-secondary);
    position: relative;
  }
  .th-tile .avatar {
    width: 42px;
    height: 42px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
  }
  .avatar-fallback { background: var(--bg-tertiary); border: 1px solid var(--border); }
  .th-tile .name {
    font-size: 0.75rem;
    color: var(--text-primary);
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .th-tile.speaking {
    box-shadow:
      0 0 0 2.5px var(--bg-primary),
      0 0 0 5px var(--accent);
  }
  .hand-badge {
    position: absolute;
    top: 6px;
    left: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .mute-glyph {
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
  .mod-badge {
    position: absolute;
    top: 26px;
    right: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--status-recalled-bg);
    border: 1px solid color-mix(in srgb, var(--danger) 30%, transparent);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .th-tile:has(.mute-glyph) .name { color: var(--text-muted); }
  .th-overflow {
    justify-content: center;
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--text-secondary);
    min-height: 84px;
  }
  .mod-controls {
    display: flex;
    gap: 4px;
    margin-top: 4px;
    opacity: 0;
    pointer-events: none;
    transition: opacity 0.12s ease;
  }
  .th-tile:hover .mod-controls,
  .th-tile:focus-within .mod-controls {
    opacity: 1;
    pointer-events: auto;
  }
  .mod-btn {
    border: 1px solid var(--border);
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    font-size: 0.7rem;
    padding: 2px 6px;
    border-radius: 3px;
    cursor: pointer;
  }
  .mod-btn:hover { color: var(--text-primary); }
  .mod-btn.danger { color: var(--danger); border-color: var(--danger); }

  .th-mod-note {
    background: var(--status-recalled-bg);
    border: 1px solid color-mix(in srgb, var(--danger) 35%, transparent);
    color: var(--danger);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 0 16px 8px;
    font-size: 0.85rem;
  }

  /* ── Control bar ── */
  .th-controls {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    border-top: 1px solid var(--border);
    flex-wrap: wrap;
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
  .ctrl:hover { background: var(--bg-tertiary); color: var(--text-primary); }
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
  .reconnecting {
    font-size: 0.78rem;
    color: var(--warning);
    background: color-mix(in srgb, var(--warning) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    padding: 3px 9px;
    border-radius: 999px;
    white-space: nowrap;
  }
  .ptt-hold { touch-action: none; user-select: none; flex: 1; }
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
  .btn-danger:hover { filter: brightness(1.1); }

  /* ── Right rail ── */
  .th-rail {
    width: 340px;
    flex-shrink: 0;
    border-left: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 1rem 0.75rem;
    min-height: 0;
    overflow-y: auto;
  }
  .th-queue-empty { margin: 0; font-size: 0.8rem; color: var(--text-muted); }
  .th-queue-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .th-queue-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    border-radius: 6px;
    background: var(--bg-secondary);
  }
  .th-queue-row.dimmed { opacity: 0.55; }
  .q-num {
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--text-muted);
    width: 16px;
    text-align: center;
    flex-shrink: 0;
  }
  .q-num.first { color: var(--gov-clay); font-weight: 700; }
  .q-avatar {
    width: 26px;
    height: 26px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
    flex-shrink: 0;
  }
  .q-avatar-fallback { background: var(--bg-tertiary); border: 1px solid var(--border); }
  .q-text { display: flex; flex-direction: column; min-width: 0; flex: 1; }
  .q-name {
    font-size: 0.82rem;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .q-wants { font-size: 0.7rem; color: var(--text-muted); }
  .q-invited {
    font-size: 0.72rem;
    color: var(--text-muted);
    font-style: italic;
  }
  .q-invite {
    border: 1px solid var(--accent);
    background: transparent;
    color: var(--accent);
    padding: 3px 10px;
    border-radius: 5px;
    font-size: 0.75rem;
    cursor: pointer;
    flex-shrink: 0;
  }
  .q-invite.primary {
    background: var(--accent);
    color: var(--on-accent);
  }
  .q-invite:hover:not(:disabled) { filter: brightness(1.08); }
  .q-invite:disabled { opacity: 0.5; cursor: not-allowed; }
  .th-backchannel {
    flex: 1;
    min-height: 220px;
    display: flex;
    flex-direction: column;
  }
  .th-backchannel-feed {
    flex: 1;
    min-height: 0;
    display: flex;
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
  }
  .th-invited-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    color: var(--gov-clay-deep);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 0 16px 8px;
    font-size: 0.85rem;
    flex-wrap: wrap;
  }
  .th-invite-error {
    color: var(--danger);
    font-size: 0.8rem;
  }
</style>
