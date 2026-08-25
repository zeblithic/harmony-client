<script lang="ts">
  import { fly } from 'svelte/transition';
  import type { ResolvedName } from '../display-label';
  import PeerName from './PeerName.svelte';

  let {
    incomingCall,
    onAccept,
    onDecline,
  }: {
    // ZEB-980: `callerName` carries provenance so the toast renders it through
    // PeerName (petname badge preserved). `callerOwner` arms ZEB-979 collision
    // detection on this surface — the one the ticket flags for impersonation
    // pressure. `groupName`, when present, is the group-DM suffix (replaces the
    // old `caller · group` string concat the caller model used to do).
    incomingCall: {
      callId: string;
      callerName: ResolvedName;
      callerOwner?: string;
      groupName?: string;
      callerAvatarUrl?: string;
    } | null;
    onAccept: (callId: string) => void;
    onDecline: (callId: string) => void;
  } = $props();
</script>

{#if incomingCall}
  <div class="incoming-call" data-testid="incoming-call" transition:fly={{ y: 20, duration: 200 }}>
    {#if incomingCall.callerAvatarUrl}
      <img class="avatar" src={incomingCall.callerAvatarUrl} alt="" />
    {:else}
      <div class="avatar avatar-fallback" aria-hidden="true"></div>
    {/if}
    <div class="call-info">
      <span class="caller-name"><PeerName
          name={incomingCall.callerName}
          ownerIdHex={incomingCall.callerOwner}
        />{#if incomingCall.groupName} · {incomingCall.groupName}{/if}</span>
      <span class="call-label">Incoming call</span>
    </div>
    <div class="call-actions">
      <button
        type="button"
        class="btn-accept"
        aria-label="Accept call"
        onclick={() => onAccept(incomingCall!.callId)}
      >
        ✓
      </button>
      <button
        type="button"
        class="btn-decline"
        aria-label="Decline call"
        onclick={() => onDecline(incomingCall!.callId)}
      >
        ✗
      </button>
    </div>
  </div>
{/if}

<style>
  .incoming-call {
    position: fixed;
    bottom: 1rem;
    left: 50%;
    transform: translateX(-50%);
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.75rem 1rem;
    background: var(--toast-bg);
    color: var(--toast-fg);
    border-radius: 8px;
    box-shadow: var(--shadow-e3);
    min-width: 280px;
    max-width: 380px;
    font-size: 0.9rem;
    z-index: 9999;
    pointer-events: auto;
  }
  .avatar {
    width: 40px;
    height: 40px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
    flex-shrink: 0;
  }
  .avatar-fallback {
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
  }
  .call-info {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 0;
  }
  .caller-name {
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .call-label {
    font-size: 0.8rem;
    color: var(--text-secondary);
  }
  .call-actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }
  .btn-accept,
  .btn-decline {
    border: 0;
    border-radius: 50%;
    width: 36px;
    height: 36px;
    font-size: 1.1rem;
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
</style>
