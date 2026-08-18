<script lang="ts">
  import Avatar from './Avatar.svelte';
  import { nonEmpty } from '../display-label';
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
    docVersion = 0,
    onClose,
    onHarmonyLink,
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
    /** T10: bumped by App.svelte on resolver.onChange so this component's `doc`
     *  $derived re-evaluates once a lazy fetch_profile_doc lands. Defaults to 0
     *  (existing tests render without it and stay header-only until resolved). */
    docVersion?: number;
    onClose: () => void;
    /**
     * T11: dispatch a `harmony:` profile link for in-app navigation. App.svelte
     * wires this to its deep-link routing (ZEB-338). A `harmony:` link renders
     * as a <button> (no live href — see the scheme-split comment below) whose
     * click calls this. Optional so existing render-only tests work without it;
     * a missing handler just means the `harmony:` click is swallowed (no raw
     * navigation), never an external open.
     */
    onHarmonyLink?: (url: string) => void;
  } = $props();

  // T11 render safety: scheme allowlist enforced on the RECEIVE side
  // (defense-in-depth — fetch_profile_doc already rejects non-allowlisted
  // schemes on ingest, but the UI must never be tricked into materializing a
  // javascript:/data: href even if a malformed doc slips through).
  //
  //   - https://  → real <a target="_blank" rel="noopener noreferrer"> (external)
  //   - harmony:  → a <button> (NO href, NO target) that dispatches via
  //                 onHarmonyLink. We deliberately do NOT render a live href for
  //                 a harmony: link: under Tauri's WebView a live href +
  //                 target="_blank" is reachable via the right-click context
  //                 menu ("Open in new tab"), which would fire a raw OS
  //                 harmony: deep-link and skip the in-app router entirely.
  //   - anything else → rendered INERT (plain text label, no href, not clickable)
  // These mirror the backend `link_scheme_allowed` (profile_page_doc.rs) exactly:
  // a non-empty https:// URL, or a harmony: deep-link with a non-empty path and
  // NO embedded scheme separator. The `:` guard rejects scheme lookalikes like
  // `harmony:javascript:alert(1)`, `harmony:data:…`, and the bare `harmony:`,
  // which a plain `startsWith('harmony:')` would wrongly render as a dispatching
  // <button>. A URL matching neither falls through to the inert <span> branch.
  function isHttps(u: string): boolean {
    return u.startsWith('https://') && u.length > 'https://'.length;
  }
  function isHarmony(u: string): boolean {
    if (!u.startsWith('harmony:')) return false;
    const rest = u.slice('harmony:'.length);
    return rest.length > 0 && !rest.includes(':'); // non-empty path, no embedded scheme
  }

  // Lazy: resolve() kicks off the fetch on a cache miss and returns undefined
  // until it lands. The re-render after the fetch resolves is driven by App's
  // resolver.onChange → docVersion bump (see header comment): reading docVersion
  // inside the derived registers it as a reactive dep so this re-runs on bump.
  const doc = $derived.by(() => {
    docVersion; // reactive dep: re-resolve when App signals a fetch resolved
    return card?.profilePageRoot ? resolver.resolve(card.profilePageRoot) : undefined;
  });

  // Resolution status, so a FAILED fetch shows an error state instead of an
  // endless "Loading…": resolver.resolve() returns undefined both while a fetch
  // is in flight AND after it has failed (during the retry cooldown), so the
  // panel can't tell the two apart from `doc` alone. Reads docVersion so it
  // re-evaluates on the same resolver.onChange bump that drives `doc`.
  // 'none' → no page root on the card at all (header-only).
  const docStatus = $derived.by(() => {
    docVersion; // reactive dep: re-evaluate when App signals a fetch settled
    return card?.profilePageRoot ? resolver.status(card.profilePageRoot) : 'none';
  });

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
      <div class="panel-name">{nonEmpty(card?.displayName) ?? 'Name unavailable'}</div>
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
            <!-- T11: scheme-split + render safety, rendered per-scheme:
                 - https: → real <a target="_blank" rel="noopener noreferrer">
                   (external open is intended).
                 - harmony: → a <button> with NO href / NO target, dispatched via
                   onHarmonyLink. A live href would be reachable through the
                   WebView right-click "Open in new tab", firing a raw OS
                   harmony: deep-link and bypassing the in-app router — so we
                   never materialize one.
                 - anything else → INERT text (no href, not clickable) so the UI
                   can't be tricked into a javascript:/data: navigation even if a
                   malformed doc reaches us. -->
            <li>
              {#if isHttps(link.url)}
                <a
                  href={link.url}
                  target="_blank"
                  rel="noopener noreferrer">{link.label}</a>
              {:else if isHarmony(link.url)}
                <button
                  type="button"
                  class="profile-link"
                  onclick={() => onHarmonyLink?.(link.url)}>{link.label}</button>
              {:else}
                <span class="link-inert" title="Unsupported link">{link.label}</span>
              {/if}
            </li>
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
  {:else if card?.profilePageRoot && docStatus === 'error'}
    <!-- The lazy fetch_profile_doc FAILED (resolve() stays undefined through the
         retry cooldown, so without this branch the panel would show "Loading…"
         forever). Surface a real error state instead. -->
    <div class="panel-empty" role="alert">Couldn't load this profile page.</div>
  {:else if card?.profilePageRoot}
    <!-- A page root is set but the lazy fetch_profile_doc hasn't resolved yet
         (in flight or not-yet-attempted): show a loading placeholder rather
         than a misleading "no content". -->
    <div class="panel-empty" role="status">Loading profile…</div>
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
    outline: 2px solid var(--accent);
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
    font-family: var(--font-display);
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
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .ownerid-hex {
    font-size: 11px;
    color: var(--text-muted);
    font-family: var(--font-mono);
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
  .links-list a,
  .links-list .profile-link {
    font-size: 13px;
    color: var(--accent);
    text-decoration: none;
    word-break: break-all;
  }
  .links-list a:hover,
  .links-list .profile-link:hover {
    text-decoration: underline;
  }
  /* T11: harmony: links render as a text-button (no live href — see script
     comment). Strip the native button chrome so it reads as a link. */
  .links-list .profile-link {
    display: inline;
    padding: 0;
    margin: 0;
    background: transparent;
    border: none;
    cursor: pointer;
    font: inherit;
    text-align: left;
  }
  .links-list .profile-link:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  /* T11: a link with a non-allowlisted scheme renders as inert text (no href,
     not clickable) — muted to signal it isn't actionable. */
  .links-list .link-inert {
    font-size: 13px;
    color: var(--text-muted);
    word-break: break-all;
    cursor: default;
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
