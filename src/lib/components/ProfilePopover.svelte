<script lang="ts">
  import { untrack } from 'svelte';
  import type { Profile } from '../types';
  import Avatar from './Avatar.svelte';

  let { profile, x, y, onClose }: {
    profile: Profile;
    x: number;
    y: number;
    onClose: () => void;
  } = $props();

  const SOUND_LABELS = { quiet: 'Quiet', standard: 'Standard', loud: 'Loud' } as const;

  $effect(() => {
    const close = untrack(() => onClose);
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === 'Escape') close();
    }
    function onClickOutside(e: MouseEvent) {
      const target = e.target as HTMLElement;
      if (!target.closest('.profile-popover') && !target.closest('.avatar.interactive')) {
        close();
      }
    }
    window.addEventListener('keydown', onKeyDown);
    const timer = setTimeout(() => {
      window.addEventListener('click', onClickOutside);
    }, 0);
    return () => {
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('click', onClickOutside);
      clearTimeout(timer);
    };
  });
</script>

<div class="profile-popover" style="left: {x}px; top: {y}px;">
  <div class="popover-header">
    <Avatar address={profile.address} displayName={profile.displayName} avatarUrl={profile.avatarUrl} size={64} />
    <div class="popover-identity">
      <div class="popover-name">{profile.displayName}</div>
      {#if profile.statusText}
        <div class="popover-status">{profile.statusText}</div>
      {/if}
      <div class="popover-address">{profile.address}</div>
    </div>
  </div>
  <div class="popover-sounds">
    <div class="sounds-label">Notification sounds</div>
    {#each (['quiet', 'standard', 'loud'] as const) as slot}
      <div class="sound-row">
        <span class="sound-slot">{SOUND_LABELS[slot]}</span>
        <span class="sound-value">
          {profile.notificationSounds?.[slot] || 'System default'}
        </span>
      </div>
    {/each}
  </div>
</div>

<style>
  .profile-popover {
    position: fixed;
    z-index: 100;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 16px;
    min-width: 240px;
    max-width: 300px;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.3);
  }

  .popover-header {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    margin-bottom: 12px;
  }

  .popover-identity {
    flex: 1;
    min-width: 0;
  }

  .popover-name {
    font-size: 16px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .popover-status {
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 2px;
  }

  .popover-address {
    font-size: 11px;
    color: var(--text-muted);
    font-family: monospace;
    margin-top: 4px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .popover-sounds {
    border-top: 1px solid var(--border);
    padding-top: 10px;
  }

  .sounds-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }

  .sound-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 3px 0;
  }

  .sound-slot {
    font-size: 12px;
    color: var(--text-secondary);
  }

  .sound-value {
    font-size: 12px;
    color: var(--text-muted);
  }
</style>
