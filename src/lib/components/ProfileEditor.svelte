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

  // --- About section (ZEB-345 long-form profile page) -----------------------
  // Caps mirror the backend `ingest_profile_doc` validation limits.
  const BIO_MAX_BYTES = 4096;
  const LINKS_MAX = 10;
  const FIELDS_MAX = 16;

  let bio = $state('');
  let links = $state<{ label: string; url: string }[]>([]);
  let fields = $state<{ key: string; value: string }[]>([]);
  // Soft, non-blocking note shown if prefill fails — the editor still works.
  let aboutNote = $state<string | null>(null);
  let aboutError = $state<string | null>(null);
  // True once the user makes ANY edit to the About section. Used to avoid
  // clobbering their edits with a late-resolving prefill (ZEB-345 prefill race).
  let aboutDirty = $state(false);
  // True once the prefill has SETTLED into a known state — either there's no
  // existing page (nothing to load) or a fetch succeeded. Stays false while a
  // fetch is pending OR after a fetch FAILURE, so handleSave knows the existing
  // page hasn't been loaded and must be preserved rather than dropped.
  let prefillLoaded = $state(false);

  function markAboutDirty() {
    aboutDirty = true;
  }

  // UTF-8 byte length of the bio, for the counter (chars ≠ bytes once emoji /
  // non-ASCII are present, and the backend cap is in bytes).
  const bioBytes = $derived(new TextEncoder().encode(bio).length);

  // Prefill from an existing profile-page doc, if the profile already has one,
  // so opening the editor doesn't wipe an authored page. Runs once on mount.
  // Failures are soft: leave the fields empty + a note; never block editing.
  $effect(() => {
    const root = untrack(() => profile.profilePageRoot);
    // No existing page → nothing to load; the prefill is "settled" immediately.
    if (!root) {
      prefillLoaded = true;
      return;
    }
    let cancelled = false;
    (async () => {
      try {
        const { invoke } = await import('@tauri-apps/api/core');
        const dto = (await invoke('fetch_profile_doc', { cid: root })) as {
          bio: string;
          links: { label: string; url: string }[];
          fields: { key: string; value: string }[];
        };
        if (cancelled) return;
        // Only apply the fetched content if the user hasn't started editing —
        // a late prefill must never clobber edits already typed.
        if (!aboutDirty) {
          bio = dto.bio ?? '';
          links = (dto.links ?? []).map((l) => ({ label: l.label, url: l.url }));
          fields = (dto.fields ?? []).map((f) => ({ key: f.key, value: f.value }));
        }
        prefillLoaded = true;
      } catch (err) {
        if (!cancelled) {
          // Leave prefillLoaded false: the existing page didn't load, so
          // handleSave must preserve the existing root rather than drop it.
          const msg = err instanceof Error ? err.message : String(err);
          aboutNote =
            "Couldn't load your existing profile page — saving will replace it.";
          console.warn('Profile-page prefill failed:', msg);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  });

  function addLink() {
    if (links.length >= LINKS_MAX) return;
    markAboutDirty();
    links = [...links, { label: '', url: '' }];
  }
  function removeLink(i: number) {
    markAboutDirty();
    links = links.filter((_, idx) => idx !== i);
  }
  function addField() {
    if (fields.length >= FIELDS_MAX) return;
    markAboutDirty();
    fields = [...fields, { key: '', value: '' }];
  }
  function removeField(i: number) {
    markAboutDirty();
    fields = fields.filter((_, idx) => idx !== i);
  }

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

  async function handleSave() {
    aboutError = null;
    // Drop rows the user added but left entirely blank, so an empty trailing
    // row doesn't get ingested as `{ label: '', url: '' }`.
    const cleanLinks = links
      .map((l) => ({ label: l.label.trim(), url: l.url.trim() }))
      .filter((l) => l.label !== '' || l.url !== '');
    const cleanFields = fields
      .map((f) => ({ key: f.key.trim(), value: f.value.trim() }))
      .filter((f) => f.key !== '' || f.value !== '');

    // Page-root computation has three cases:
    //  - The user authored About content → ingest a fresh doc.
    //  - No authored content, but the profile already HAS a page that hasn't
    //    finished prefilling (fetch pending or failed) → PRESERVE the existing
    //    root rather than silently dropping the page (ZEB-345 prefill race).
    //  - Genuinely no/cleared page → undefined (card stays byte-identical, §7).
    let profilePageRoot: string | undefined;
    const hasAbout =
      bio.trim() !== '' || cleanLinks.length > 0 || cleanFields.length > 0;
    if (hasAbout) {
      try {
        const { invoke } = await import('@tauri-apps/api/core');
        profilePageRoot = (await invoke('ingest_profile_doc', {
          bio,
          links: cleanLinks,
          fields: cleanFields,
        })) as string;
      } catch (err) {
        // Surface like the avatar ingest error; abort the save so we never
        // emit a profile referencing a doc that wasn't ingested.
        aboutError = err instanceof Error ? err.message : String(err);
        return;
      }
    } else if (profile.profilePageRoot && !prefillLoaded) {
      // Existing page not yet loaded (prefill pending/failed) and the user
      // didn't author anything → preserve the existing root.
      profilePageRoot = profile.profilePageRoot;
    } else {
      profilePageRoot = undefined; // genuinely no/cleared page
    }

    const updated: Profile = {
      ...profile,
      displayName: displayName.trim() || 'Anonymous',
      statusText: statusText.trim() || undefined,
      // Only override avatarCid when the user staged a new avatar this
      // session; otherwise keep whatever the spread carried.
      ...(avatarCid !== undefined ? { avatarCid } : {}),
      avatarUrl,
      profilePageRoot,
    };
    onSave(updated);
    saved = true;
    if (savedTimer !== null) clearTimeout(savedTimer);
    savedTimer = setTimeout(() => { saved = false; savedTimer = null; }, 2000);
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      void handleSave();
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

  <div class="about-section" aria-label="About">
    <h4 class="section-subtitle">About</h4>

    {#if aboutNote}
      <p class="about-note" role="status">{aboutNote}</p>
    {/if}

    <div class="field">
      <label class="field-label" for="profile-bio">Bio</label>
      <textarea
        id="profile-bio"
        class="field-input bio-input"
        bind:value={bio}
        oninput={markAboutDirty}
        rows="4"
        placeholder="Tell people about yourself…"
        aria-label="Bio"
      ></textarea>
      <span
        class="char-counter"
        class:over-limit={bioBytes > BIO_MAX_BYTES}
        aria-live="polite"
      >
        {bioBytes} / {BIO_MAX_BYTES} bytes
      </span>
    </div>

    <div class="field">
      <span class="field-label" id="links-label">Links</span>
      <div class="repeat-list" aria-labelledby="links-label">
        {#each links as link, i (i)}
          <div class="repeat-row">
            <input
              class="field-input repeat-input"
              type="text"
              bind:value={link.label}
              oninput={markAboutDirty}
              placeholder="Label"
              aria-label={`Link label ${i + 1}`}
            />
            <input
              class="field-input repeat-input"
              type="text"
              bind:value={link.url}
              oninput={markAboutDirty}
              placeholder="https://…"
              aria-label={`Link URL ${i + 1}`}
            />
            <button
              type="button"
              class="repeat-remove"
              onclick={() => removeLink(i)}
              aria-label={`Remove link ${i + 1}`}
            >
              ×
            </button>
          </div>
        {/each}
      </div>
      <button
        type="button"
        class="repeat-add"
        onclick={addLink}
        disabled={links.length >= LINKS_MAX}
      >
        Add link
      </button>
    </div>

    <div class="field">
      <span class="field-label" id="fields-label">Fields</span>
      <div class="repeat-list" aria-labelledby="fields-label">
        {#each fields as field, i (i)}
          <div class="repeat-row">
            <input
              class="field-input repeat-input"
              type="text"
              bind:value={field.key}
              oninput={markAboutDirty}
              placeholder="Key"
              aria-label={`Field key ${i + 1}`}
            />
            <input
              class="field-input repeat-input"
              type="text"
              bind:value={field.value}
              oninput={markAboutDirty}
              placeholder="Value"
              aria-label={`Field value ${i + 1}`}
            />
            <button
              type="button"
              class="repeat-remove"
              onclick={() => removeField(i)}
              aria-label={`Remove field ${i + 1}`}
            >
              ×
            </button>
          </div>
        {/each}
      </div>
      <button
        type="button"
        class="repeat-add"
        onclick={addField}
        disabled={fields.length >= FIELDS_MAX}
      >
        Add field
      </button>
    </div>

    {#if aboutError}
      <span class="avatar-error" role="alert">{aboutError}</span>
    {/if}
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

  .about-section {
    display: flex;
    flex-direction: column;
    gap: 16px;
    padding-top: 8px;
    border-top: 1px solid var(--border);
  }

  .section-subtitle {
    margin: 0;
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
  }

  .about-note {
    margin: 0;
    font-size: 12px;
    color: var(--text-muted);
    font-style: italic;
  }

  .bio-input {
    resize: vertical;
    min-height: 64px;
    font-family: inherit;
  }

  .char-counter {
    align-self: flex-end;
    font-size: 11px;
    color: var(--text-muted);
  }

  .char-counter.over-limit {
    color: var(--danger, #d9534f);
  }

  .repeat-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .repeat-row {
    display: flex;
    gap: 6px;
    align-items: center;
  }

  .repeat-input {
    flex: 1;
    min-width: 0;
  }

  .repeat-remove {
    flex: 0 0 auto;
    width: 28px;
    height: 28px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-muted);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
  }

  .repeat-remove:hover {
    color: var(--danger, #d9534f);
    border-color: var(--danger, #d9534f);
  }

  .repeat-add {
    align-self: flex-start;
    padding: 4px 12px;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--bg-primary);
    color: var(--text-secondary);
    font-size: 12px;
    cursor: pointer;
  }

  .repeat-add:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--text-primary);
  }

  .repeat-add:disabled {
    opacity: 0.5;
    cursor: default;
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
