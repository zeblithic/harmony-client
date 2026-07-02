<script lang="ts">
  import { fly } from 'svelte/transition';

  let {
    incomingCall,
    onAccept,
    onDecline,
  }: {
    incomingCall: { callId: string; callerName: string; callerAvatarUrl?: string } | null;
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
      <span class="caller-name">{incomingCall.callerName}</span>
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
    background: var(--toast-bg, rgba(20, 22, 30, 0.95));
    color: var(--toast-fg, #fff);
    border-radius: 6px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.2);
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
    color: #fff;
  }
  .btn-accept:hover {
    filter: brightness(1.1);
  }
  .btn-decline {
    background: var(--danger);
    color: #fff;
  }
  .btn-decline:hover {
    filter: brightness(1.1);
  }
</style>
