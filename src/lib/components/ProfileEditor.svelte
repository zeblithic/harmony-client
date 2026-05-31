<script lang="ts">
  import { untrack } from 'svelte';
  import type { Profile } from '../types';
  import Avatar from './Avatar.svelte';
  import { normalizeAvatar } from '../avatar-normalize';

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
  let displayName = $state(untrack(() => profile.displayName));
  let statusText = $state(untrack(() => profile.statusText ?? ''));
  // Avatar edits are staged locally and folded into the saved profile on save.
  // `undefined` means "no avatar change this session" → the existing
  // avatarCid/avatarUrl on the profile are preserved.
  let avatarCid = $state<string | undefined>(undefined);
  let avatarUrl = $state<string | undefined>(untrack(() => profile.avatarUrl));
  let avatarBusy = $state(false);
  let avatarError = $state<string | null>(null);
  let saved = $state(false);
  let savedTimer: ReturnType<typeof setTimeout> | null = null;

  // Release any blob: preview URL we created when the editor unmounts, so
  // object URLs don't accumulate across a long session.
  $effect(() => () => {
    if (avatarUrl?.startsWith('blob:')) URL.revokeObjectURL(avatarUrl);
  });

  async function handleAvatarPick(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    avatarBusy = true;
    avatarError = null;
    try {
      const bytes = await normalizeAvatar(file);
      const { invoke } = await import('@tauri-apps/api/core');
      const cidHex = (await invoke('ingest_avatar_bytes', {
        bytes: Array.from(bytes),
      })) as string;
      avatarCid = cidHex;
      // Self-seed a local blob preview so the user sees the new avatar
      // immediately, with zero network round-trip. Revoke the previous
      // preview URL first so repeated picks don't leak object URLs.
      if (avatarUrl?.startsWith('blob:')) URL.revokeObjectURL(avatarUrl);
      avatarUrl = URL.createObjectURL(
        new Blob([new Uint8Array(bytes)], { type: 'image/png' }),
      );
    } catch (err) {
      avatarError = err instanceof Error ? err.message : String(err);
    } finally {
      avatarBusy = false;
      input.value = '';
    }
  }

  function handleSave() {
    const updated: Profile = {
      ...profile,
      displayName: displayName.trim() || 'Anonymous',
      statusText: statusText.trim() || undefined,
      // Only override avatarCid when the user staged a new avatar this
      // session; otherwise keep whatever the spread carried.
      ...(avatarCid !== undefined ? { avatarCid } : {}),
      avatarUrl,
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
      {avatarUrl}
      size={80}
    />
  </div>

  <div class="field">
    <label class="field-label" for="avatar-input">Avatar</label>
    <input
      id="avatar-input"
      class="field-input avatar-input"
      type="file"
      accept="image/*"
      onchange={handleAvatarPick}
      disabled={avatarBusy}
      aria-label="Avatar image"
    />
    {#if avatarBusy}
      <span class="avatar-status" role="status">Processing image…</span>
    {/if}
    {#if avatarError}
      <span class="avatar-error" role="alert">{avatarError}</span>
    {/if}
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

  .avatar-input {
    padding: 6px 8px;
    cursor: pointer;
  }

  .avatar-input:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .avatar-status {
    font-size: 12px;
    color: var(--text-muted);
    font-style: italic;
  }

  .avatar-error {
    font-size: 12px;
    color: var(--danger, #d9534f);
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
