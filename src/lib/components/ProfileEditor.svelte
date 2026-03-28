<script lang="ts">
  import type { Profile } from '../types';
  import Avatar from './Avatar.svelte';

  let {
    profile,
    onSave,
  }: {
    profile: Profile;
    onSave: (profile: Profile) => void;
  } = $props();

  // Local edit state — initialized from props once. Not re-synced on prop
  // changes because the user is actively editing. If future network sync
  // updates the profile while the editor is open, add a $effect to re-sync.
  let displayName = $state(profile.displayName);
  let statusText = $state(profile.statusText ?? '');
  let saved = $state(false);
  let savedTimer: ReturnType<typeof setTimeout> | null = null;

  function handleSave() {
    const updated: Profile = {
      ...profile,
      displayName: displayName.trim() || 'Anonymous',
      statusText: statusText.trim() || undefined,
    };
    onSave(updated);
    saved = true;
    if (savedTimer !== null) clearTimeout(savedTimer);
    savedTimer = setTimeout(() => { saved = false; savedTimer = null; }, 2000);
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSave();
    }
  }
</script>

<section class="profile-editor" aria-label="Edit your profile">
  <h3 class="section-title">Your Profile</h3>

  <div class="avatar-preview">
    <Avatar
      address={profile.address}
      displayName={displayName || 'Anonymous'}
      avatarUrl={profile.avatarUrl}
      size={80}
    />
  </div>

  <div class="field">
    <label class="field-label" for="profile-name">Display Name</label>
    <input
      id="profile-name"
      class="field-input"
      type="text"
      bind:value={displayName}
      placeholder="Anonymous"
      onkeydown={handleKeydown}
      aria-label="Display name"
    />
  </div>

  <div class="field">
    <label class="field-label" for="profile-status">Status</label>
    <input
      id="profile-status"
      class="field-input"
      type="text"
      bind:value={statusText}
      placeholder="What are you up to?"
      onkeydown={handleKeydown}
      aria-label="Status text"
    />
  </div>

  <div class="actions">
    <button class="save-btn" onclick={handleSave}>
      Save
    </button>
    {#if saved}
      <span class="saved-message" role="status">Profile saved</span>
    {/if}
  </div>

  <p class="address-note">
    Address: <code>{profile.address}</code>
  </p>
</section>

<style>
  .profile-editor {
    display: flex;
    flex-direction: column;
    gap: 16px;
    padding: 16px;
    background: var(--bg-secondary);
    border-radius: 8px;
  }

  .section-title {
    margin: 0;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .avatar-preview {
    display: flex;
    justify-content: center;
    padding: 8px 0;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .field-label {
    font-size: 12px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }

  .field-input {
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-primary);
    font-size: 13px;
    outline: none;
  }

  .field-input:focus {
    border-color: var(--accent);
  }

  .field-input::placeholder {
    color: var(--text-muted);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .save-btn {
    padding: 8px 20px;
    border: none;
    border-radius: 4px;
    background: var(--accent);
    color: #fff;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }

  .save-btn:hover {
    opacity: 0.9;
  }

  .saved-message {
    font-size: 13px;
    color: var(--text-muted);
    font-style: italic;
  }

  .address-note {
    margin: 0;
    font-size: 11px;
    color: var(--text-muted);
    word-break: break-all;
  }

  .address-note code {
    font-family: monospace;
    color: var(--text-secondary);
  }
</style>
