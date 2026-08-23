<script lang="ts">
  import type { ModerationEvent } from '../types';
  import type { ResolvedCard } from '../member-card-service';
  import { resolveMentionLabel } from '../mention-render';

  // ZEB-961: resolve the actor/target owner_ids through the shared display-name
  // ladder (nickname → card → short hex) instead of raw hex. The parent
  // (CommunityMembersPanel) threads its own resolveCard/resolveNickname down.
  let {
    events,
    resolveCard,
    resolveNickname,
  }: {
    events: ModerationEvent[];
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    resolveNickname?: (ownerIdHex: string) => string | undefined;
  } = $props();

  let expanded = $state(false);

  function relativeTime(wallMs: number): string {
    const diffMs = Date.now() - wallMs;
    const diffMin = Math.floor(diffMs / 60_000);
    if (diffMin < 1) return 'just now';
    if (diffMin < 60) return `${diffMin} minute${diffMin === 1 ? '' : 's'} ago`;
    const diffHr = Math.floor(diffMin / 60);
    if (diffHr < 24) return `${diffHr} hour${diffHr === 1 ? '' : 's'} ago`;
    const diffDay = Math.floor(diffHr / 24);
    return `${diffDay} day${diffDay === 1 ? '' : 's'} ago`;
  }

  function tierName(level: number): string {
    if (level >= 100) return 'Admin';
    if (level >= 50) return 'Moderator';
    return 'Member';
  }

  function describeEvent(ev: ModerationEvent): string {
    const actor = resolveMentionLabel(ev.actorAddr, resolveNickname, resolveCard).label;
    const target = resolveMentionLabel(ev.targetAddr, resolveNickname, resolveCard).label;
    if (ev.kind === 'kick') {
      const suffix = ev.reason ? ` ("${ev.reason}")` : '';
      return `${actor} kicked ${target}${suffix}`;
    }
    if (ev.kind === 'unban') {
      const suffix = ev.reason ? ` ("${ev.reason}")` : '';
      return `${actor} unbanned ${target}${suffix}`;
    }
    // set_power
    return `${actor} set ${target} to ${tierName(ev.newPower ?? 0)}`;
  }
</script>

<section class="recent-actions-badge" aria-label="Recent moderation actions">
  <button
    type="button"
    class="toggle"
    onclick={() => (expanded = !expanded)}
    aria-expanded={expanded}
  >
    {expanded ? '▾' : '▸'} Recent moderation actions ({events.length})
  </button>

  {#if expanded}
    {#if events.length === 0}
      <p class="empty">No recent moderation actions.</p>
    {:else}
      <ul class="events">
        {#each events as ev (ev.eventId)}
          <li>
            <span class="when">{relativeTime(ev.hlc.wallMs)}</span>
            —
            <span class="what">{describeEvent(ev)}</span>
          </li>
        {/each}
      </ul>
    {/if}
  {/if}
</section>

<style>
  .recent-actions-badge {
    border-bottom: 1px solid var(--border);
    background: var(--bg-secondary);
  }

  .toggle {
    display: flex;
    align-items: center;
    gap: 4px;
    width: 100%;
    padding: 10px 16px;
    background: transparent;
    border: none;
    color: var(--text-secondary);
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    cursor: pointer;
    text-align: left;
    user-select: none;
  }

  .toggle:hover {
    color: var(--text-primary);
    background: var(--bg-tertiary);
  }

  .toggle:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  .empty {
    margin: 0;
    padding: 10px 16px;
    font-size: 0.8rem;
    color: var(--text-secondary);
    text-align: center;
  }

  .events {
    list-style: none;
    margin: 0;
    padding: 0 16px 10px;
  }

  .events li {
    padding: 4px 0;
    font-size: 0.75rem;
    color: var(--text-secondary);
    border-bottom: 1px solid var(--border);
  }

  .events li:last-child {
    border-bottom: none;
  }

  .when {
    color: var(--text-secondary);
    font-style: italic;
  }

  .what {
    color: var(--text-primary);
    font-family: var(--font-mono);
  }
</style>
