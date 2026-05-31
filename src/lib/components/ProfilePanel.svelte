<script lang="ts">
  import Avatar from './Avatar.svelte';
  import type { ProfilePageResolver } from '../profile-page-resolver';

  /**
   * ZEB-345: right-side long-form profile panel. Opened from the owner-card
   * popover's "View full profile" action via App.svelte's `openProfileOwnerId`
   * state (T10).
   *
   * Reactivity on async resolution: the panel does NOT set `resolver.onChange`
   * itself. App.svelte (T10) owns a combined `onChange` that bumps a `$state`
   * version counter, which re-runs this component's `$derived doc` once a lazy
   * `fetch_profile_doc` lands. Until then the panel renders header-only.
   */
  let {
    ownerIdHex,
    card,
    resolver,
    onClose,
  }: {
    ownerIdHex: string;
    card: {
      displayName: string;
      statusText?: string;
      avatarUrl?: string;
      /** CID (hex) of the long-form profile-page doc, if the member has one. */
      profilePageRoot?: string;
    };
    resolver: ProfilePageResolver;
    onClose: () => void;
  } = $props();

  // Lazy: resolve() kicks off the fetch on a cache miss and returns undefined
  // until it lands. The re-render after the fetch resolves is driven by App's
  // resolver.onChange → version bump (see header comment), not from here.
  const doc = $derived(card?.profilePageRoot ? resolver.resolve(card.profilePageRoot) : undefined);

  let copied = $state(false);
  let copyTimer: ReturnType<typeof setTimeout> | null = null;

  async function copyOwnerId() {
    if (!ownerIdHex) return;
    // Only surface "Copied" after the write actually succeeds — don't claim a
    // copy if the clipboard is unavailable or the write rejects.
    if (!navigator.clipboard) return;
    try {
      await navigator.clipboard.writeText(ownerIdHex);
    } catch {
      return;
    }
    copied = true;
    if (copyTimer) clearTimeout(copyTimer);
    copyTimer = setTimeout(() => {
      copied = false;
      copyTimer = null;
    }, 1500);
  }

  function onCopyKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      void copyOwnerId();
    }
  }
</script>

<aside class="profile-panel" aria-label="Profile">
  <div class="panel-topbar">
    <button type="button" class="panel-close" aria-label="Close profile" onclick={onClose}>×</button>
  </div>

  <div class="panel-header">
    <Avatar address={ownerIdHex} displayName={card?.displayName ?? ''} avatarUrl={card?.avatarUrl} size={72} />
    <div class="panel-identity">
      <div class="panel-name">{card?.displayName || 'Name unavailable'}</div>
      {#if card?.statusText}
        <div class="panel-status">{card.statusText}</div>
      {/if}
      <button
        type="button"
        class="panel-ownerid"
        title="Copy owner ID"
        aria-label="Copy owner ID"
        onclick={copyOwnerId}
        onkeydown={onCopyKeydown}
      >
        <span class="ownerid-hex">{ownerIdHex}</span>
        <span class="ownerid-copy-hint">{copied ? 'Copied' : 'Copy'}</span>
      </button>
    </div>
  </div>

  {#if doc}
    <section class="panel-section panel-about">
      <h3 class="section-label">About</h3>
      <!-- Bio is plain text: {doc.bio} auto-escapes; pre-wrap preserves the
           author's newlines. NEVER {@html} here (untrusted peer content). -->
      <div class="about-bio">{doc.bio}</div>
    </section>

    {#if doc.links.length > 0}
      <section class="panel-section panel-links">
        <h3 class="section-label">Links</h3>
        <ul class="links-list">
          {#each doc.links as link}
            <!-- T11: scheme-split + safety (https: external, harmony: deep-link
                 router, frontend re-checks the scheme allowlist). Basic <a> for
                 now. -->
            <li><a href={link.url} rel="noopener noreferrer">{link.label}</a></li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if doc.fields.length > 0}
      <section class="panel-section panel-fields">
        <h3 class="section-label">Details</h3>
        <dl class="fields-list">
          {#each doc.fields as field}
            <div class="field-row">
              <dt class="field-key">{field.key}</dt>
              <dd class="field-value">{field.value}</dd>
            </div>
          {/each}
        </dl>
      </section>
    {/if}
  {:else}
    <div class="panel-empty">No page content.</div>
  {/if}
</aside>

<style>
  .profile-panel {
    display: flex;
    flex-direction: column;
    width: 320px;
    max-width: 100%;
    height: 100%;
    overflow-y: auto;
    background: var(--bg-tertiary);
    border-left: 1px solid var(--border);
    padding: 0 16px 16px;
  }

  .panel-topbar {
    display: flex;
    justify-content: flex-end;
    padding: 8px 0 0;
  }

  .panel-close {
    background: transparent;
    border: none;
    color: var(--text-muted);
    font-size: 20px;
    line-height: 1;
    cursor: pointer;
    padding: 2px 6px;
    border-radius: 4px;
  }
  .panel-close:hover {
    background: var(--bg-secondary);
    color: var(--text-primary);
  }
  .panel-close:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }

  .panel-header {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    padding-bottom: 12px;
    border-bottom: 1px solid var(--border);
  }

  .panel-identity {
    flex: 1;
    min-width: 0;
  }

  .panel-name {
    font-size: 18px;
    font-weight: 700;
    color: var(--text-primary);
  }

  .panel-status {
    font-size: 13px;
    color: var(--text-muted);
    margin-top: 2px;
  }

  .panel-ownerid {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    margin-top: 6px;
    padding: 2px 4px;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 4px;
    cursor: pointer;
    text-align: left;
  }
  .panel-ownerid:hover {
    background: var(--bg-secondary);
    border-color: var(--border);
  }
  .panel-ownerid:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
  .ownerid-hex {
    font-size: 11px;
    color: var(--text-muted);
    font-family: monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
  }
  .ownerid-copy-hint {
    font-size: 10px;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    flex-shrink: 0;
  }

  .panel-section {
    padding-top: 12px;
    margin-top: 4px;
  }

  .section-label {
    font-size: 11px;
    font-weight: 600;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin: 0 0 6px;
  }

  .about-bio {
    font-size: 13px;
    color: var(--text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }

  .links-list {
    list-style: none;
    padding: 0;
    margin: 0;
  }
  .links-list li {
    padding: 3px 0;
  }
  .links-list a {
    font-size: 13px;
    color: var(--accent, #5865f2);
    text-decoration: none;
    word-break: break-all;
  }
  .links-list a:hover {
    text-decoration: underline;
  }

  .fields-list {
    margin: 0;
  }
  .field-row {
    display: flex;
    gap: 8px;
    padding: 3px 0;
  }
  .field-key {
    font-size: 12px;
    font-weight: 600;
    color: var(--text-muted);
    flex-shrink: 0;
  }
  .field-value {
    font-size: 12px;
    color: var(--text-secondary);
    margin: 0;
    word-break: break-word;
  }

  .panel-empty {
    padding-top: 16px;
    font-size: 12px;
    color: var(--text-muted);
  }
</style>
