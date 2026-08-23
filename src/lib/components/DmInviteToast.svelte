<script lang="ts">
  import { fly } from 'svelte/transition';
  import type { PendingDmInviteDto } from '../dm-invite-service';
  // ZEB-961: resolve the inviter's broadcast card name when available (a
  // non-friend inviter can still have published a profile card), else short hex.
  import { resolveMentionLabel } from '../mention-render';
  import PeerName from './PeerName.svelte';
  import type { ResolvedCard } from '../member-card-service';

  let {
    invite,
    onAccept,
    onDecline,
    onLater,
    resolveCard,
    resolveNickname,
  }: {
    invite: PendingDmInviteDto;
    onAccept: () => void | Promise<void>;
    onDecline: () => void | Promise<void>;
    onLater: () => void;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    // ZEB-977: petname rung — THE first-contact spoof surface; your name for a
    // known identity must outrank whatever the inviter published.
    resolveNickname?: (ownerIdHex: string) => string | undefined;
  } = $props();

  // Map the SpaceKind wire tag ('d'/'g') to a human label; fall back to the
  // raw tag for any unexpected value (owner_state_types.rs SpaceKind).
  function kindLabel(kind: string): string {
    if (kind === 'd') return 'DM';
    if (kind === 'g') return 'Group DM';
    return kind;
  }

  // Accept/Decline call the backend and can be slow (or racy if double-
  // clicked); disable both buttons while either is in flight. "Later" is
  // purely local/session-side (App just filters it out of the queue) so it
  // never enters the busy state.
  let busy = $state(false);

  async function handleAccept(): Promise<void> {
    if (busy) return;
    busy = true;
    try {
      await onAccept();
    } finally {
      busy = false;
    }
  }

  async function handleDecline(): Promise<void> {
    if (busy) return;
    busy = true;
    try {
      await onDecline();
    } finally {
      busy = false;
    }
  }
</script>

{#if invite}
  <!-- role="status" = polite ARIA live region: announce the arriving invite
       to assistive tech without interrupting (parity with other async
       user-facing notifications). -->
  <div
    class="dm-invite-toast"
    data-testid="dm-invite-toast"
    role="status"
    transition:fly={{ y: 20, duration: 200 }}
  >
    <div class="invite-info">
      <span class="invite-title">DM invite</span>
      <span class="invite-body">From <PeerName name={resolveMentionLabel(invite.inviterOwnerIdHex, resolveNickname, resolveCard)} ownerIdHex={invite.inviterOwnerIdHex} /> ({kindLabel(invite.kind)})</span>
    </div>
    <div class="invite-actions">
      <button
        type="button"
        class="btn-accept"
        data-testid="dm-invite-accept"
        disabled={busy}
        onclick={handleAccept}
      >
        Accept
      </button>
      <button
        type="button"
        class="btn-decline"
        data-testid="dm-invite-decline"
        disabled={busy}
        onclick={handleDecline}
      >
        Decline
      </button>
      <button
        type="button"
        class="btn-later"
        data-testid="dm-invite-later"
        onclick={onLater}
      >
        Later
      </button>
    </div>
  </div>
{/if}

<style>
  .dm-invite-toast {
    /* Positioning constraint: incoming-call toasts own bottom-center
       (IncomingCallToast.svelte); DM invites own bottom-right so the two
       surfaces never overlap when both are visible at once. */
    position: fixed;
    bottom: 1rem;
    right: 1rem;
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.75rem 1rem;
    background: var(--toast-bg);
    color: var(--toast-fg);
    border-radius: 6px;
    box-shadow: 0 4px 12px var(--shadow-soft);
    min-width: 280px;
    max-width: 380px;
    font-size: 0.9rem;
    z-index: 9999;
    pointer-events: auto;
  }
  .invite-info {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 0;
  }
  .invite-title {
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .invite-body {
    font-size: 0.8rem;
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .invite-actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }
  .btn-accept,
  .btn-decline,
  .btn-later {
    border: 0;
    border-radius: 4px;
    padding: 0.4rem 0.6rem;
    font-size: 0.8rem;
    line-height: 1;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .btn-accept {
    background: var(--accent);
    color: var(--on-accent);
  }
  .btn-accept:hover {
    filter: brightness(1.1);
  }
  .btn-decline {
    background: var(--danger);
    color: var(--text-bright);
  }
  .btn-decline:hover {
    filter: brightness(1.1);
  }
  .btn-later {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border);
  }
  .btn-later:hover {
    filter: brightness(1.1);
  }
  .btn-accept:disabled,
  .btn-decline:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
