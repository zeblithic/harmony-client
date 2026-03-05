<script lang="ts">
  import type { MessagePriority } from '../types';

  let { onSend, channelName = 'general' }: {
    onSend?: (text: string, priority: MessagePriority) => void;
    channelName?: string;
  } = $props();

  let draft = $state('');
  let priority = $state<MessagePriority>('standard');

  const PRIORITY_ICONS: Record<MessagePriority, string> = {
    quiet: '🔇',
    standard: '🔔',
    loud: '📢',
  };

  const PRIORITY_LABELS: Record<string, string> = {
    quiet: 'sending quietly',
    loud: 'sending loudly',
  };

  function send(overridePriority?: MessagePriority) {
    const text = draft.trim();
    if (!text) return;
    onSend?.(text, overridePriority ?? priority);
    draft = '';
    priority = 'standard';
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      if (e.ctrlKey) {
        send('quiet');
      } else {
        send();
      }
    }
  }
</script>

<div class="compose-bar">
  <div class="priority-toggle">
    {#each (['quiet', 'standard', 'loud'] as MessagePriority[]) as p}
      <button
        class="priority-btn {priority === p ? 'active' : ''}"
        data-priority={p}
        onclick={() => { priority = p; }}
        title={p}
      >{PRIORITY_ICONS[p]}</button>
    {/each}
  </div>
  <div class="compose-input-wrapper">
    <textarea
      class="compose-input"
      placeholder="Message #{channelName}"
      bind:value={draft}
      onkeydown={handleKeyDown}
      rows="1"
    ></textarea>
    {#if priority !== 'standard'}
      <span class="priority-label">{PRIORITY_LABELS[priority]}</span>
    {/if}
  </div>
</div>

<style>
  .compose-bar {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    padding: 12px 16px;
    border-top: 1px solid var(--border);
    flex-shrink: 0;
  }

  .priority-toggle {
    display: flex;
    gap: 2px;
    padding-top: 6px;
  }

  .priority-btn {
    border: none;
    background: none;
    padding: 4px 6px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 14px;
    opacity: 0.4;
    transition: opacity 0.15s;
  }

  .priority-btn:hover {
    opacity: 0.7;
    background: var(--bg-tertiary);
  }

  .priority-btn.active {
    opacity: 1;
    background: var(--bg-tertiary);
  }

  .compose-input-wrapper {
    flex: 1;
    position: relative;
  }

  .compose-input {
    width: 100%;
    padding: 10px 12px;
    border: none;
    border-radius: 8px;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-size: 14px;
    outline: none;
    resize: none;
    font-family: inherit;
    line-height: 1.4;
  }

  .compose-input::placeholder {
    color: var(--text-muted);
  }

  .compose-input:focus {
    box-shadow: 0 0 0 2px var(--accent);
  }

  .priority-label {
    position: absolute;
    bottom: -16px;
    left: 12px;
    font-size: 11px;
    color: var(--text-muted);
  }
</style>
