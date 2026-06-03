<script lang="ts">
  // ZEB-360 (group-DM voice, T13): a small in-conversation banner shown inside a
  // group-DM view when a call is in progress in that DM AND the local user is NOT
  // already in it. "📞 Call in progress · N in call" + a Join button. Self-hides
  // when there's no active call or when the local user is in the call (the in-call
  // bar takes over). Mirrors the 1:1 surfaces; siblings receive `invoke`/`groupCall`
  // via props (App.svelte owns the Tauri adapter + the singleton session).
  import { get } from 'svelte/store';
  import { groupCallBanners } from '../group-call-banner-store';
  import { ensureGroupMembers } from '../group-dm-members-cache';
  import type { GroupCallSession } from '../group-call-session';

  let { spaceId, invoke, groupCall }: {
    spaceId: string;
    invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
    groupCall: GroupCallSession | null;
  } = $props();

  // Track the session's phase/callId reactively so the banner hides the instant
  // the local user joins (in-call bar takes over) and re-appears on leave.
  const callState = $derived(groupCall?.state);

  // The active-call entry for this space (if any). Auto-subscribes to the store.
  const entry = $derived($groupCallBanners[spaceId]);

  // Show only when: there's an active call in this DM AND the local user isn't
  // already in *that* call. "In that call" = the session's callId matches the
  // banner's callId (any non-idle phase means we're joining/in it).
  const inThisCall = $derived(
    !!$callState && $callState.phase !== 'idle' && $callState.callId === entry?.callId,
  );
  const show = $derived(!!entry && !inThisCall);
  const count = $derived(entry?.roster.length ?? 0);

  let joining = $state(false);

  async function join() {
    if (!groupCall || !entry || joining) return;
    // Re-read the latest entry at click time (the roster/callId may have advanced
    // since render); also bail if we've since joined this call.
    const latest = get(groupCallBanners)[spaceId];
    if (!latest) return;
    if (groupCall && get(groupCall.state).callId === latest.callId) return;
    joining = true;
    try {
      await ensureGroupMembers(invoke, spaceId);
      await groupCall.joinActive(latest.callId, spaceId);
    } catch {
      // joinActive resets to idle on failure; surfacing isn't required here —
      // the banner stays visible so the user can retry.
    } finally {
      joining = false;
    }
  }
</script>

{#if show}
  <div class="group-call-banner" data-testid="group-call-banner" role="status">
    <span class="banner-label">📞 Call in progress · {count} in call</span>
    <button
      type="button"
      class="btn-join"
      data-testid="group-call-join"
      onclick={join}
      disabled={joining}
    >
      {joining ? 'Joining…' : 'Join'}
    </button>
  </div>
{/if}

<style>
  .group-call-banner {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    padding: 0.45rem 1rem;
    background: color-mix(in srgb, var(--accent, #4a9eff) 12%, transparent);
    border-bottom: 1px solid color-mix(in srgb, var(--accent, #4a9eff) 35%, transparent);
    font-size: 0.85rem;
    color: var(--text-primary, #fff);
    flex-shrink: 0;
  }
  .banner-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .btn-join {
    border: none;
    background: var(--accent, #4a9eff);
    color: #fff;
    padding: 4px 14px;
    border-radius: 4px;
    font-size: 0.85rem;
    cursor: pointer;
    flex-shrink: 0;
  }
  .btn-join:hover:not(:disabled) {
    filter: brightness(1.1);
  }
  .btn-join:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
</style>
