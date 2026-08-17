<script lang="ts">
  import { onDestroy } from 'svelte';

  let {
    kind,
    onGenerate,
  }: {
    kind: 'open' | 'invite-only' | 'unknown';
    onGenerate: () => Promise<string>;
  } = $props();

  let url = $state<string | null>(null);
  let pending = $state(false);
  let copied = $state(false);
  let error = $state<string | null>(null);
  let copyResetTimer: ReturnType<typeof setTimeout> | null = null;

  // Clear any pending copy-feedback timer on unmount. Without this,
  // the setTimeout callback can fire on a destroyed component and
  // write to copied/$state, which Svelte 5 warns about.
  onDestroy(() => {
    if (copyResetTimer) {
      clearTimeout(copyResetTimer);
      copyResetTimer = null;
    }
  });

  async function handleGenerate() {
    pending = true;
    error = null;
    try {
      url = await onGenerate();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      error = `Couldn't generate invite link: ${msg}`;
    } finally {
      pending = false;
    }
  }

  async function handleCopy() {
    if (!url) return;
    try {
      await navigator.clipboard.writeText(url);
    } catch (e) {
      // navigator.clipboard.writeText rejects with NotAllowedError in
      // sandboxed webviews or when permission is revoked. Surface to
      // the user via the existing error banner rather than producing
      // an unhandled promise rejection (greptile P1).
      const msg = e instanceof Error ? e.message : String(e);
      error = `Couldn't copy to clipboard: ${msg}`;
      return;
    }
    copied = true;
    if (copyResetTimer) clearTimeout(copyResetTimer);
    copyResetTimer = setTimeout(() => {
      copied = false;
      copyResetTimer = null;
    }, 2000);
  }
</script>

<div class="invite-manager">
  {#if error}
    <p class="error" role="alert">{error}</p>
  {/if}
  {#if !url}
    <p class="explanation">Generate a one-time invite link to share via DM, email, or any side channel.</p>
    <button class="generate-btn" onclick={handleGenerate} disabled={pending}>
      {pending ? 'Generating...' : '+ Generate invite link'}
    </button>
  {:else}
    {#if kind === 'invite-only'}
      <p class="warning">Don't post publicly — it embeds your invite signature. Each link can only be redeemed once.</p>
    {:else if kind === 'open'}
      <p class="warning">Anyone with this URL can join. The same link works indefinitely.</p>
    {:else}
      <p class="warning">Treat this link as sensitive — share only via a private channel.</p>
    {/if}

    <div class="url-row">
      <code class="url">{url}</code>
      <button class="copy-btn" onclick={handleCopy}>
        {copied ? '✓ Copied' : '📋 Copy'}
      </button>
    </div>

    <div class="actions">
      <button class="regen-btn" onclick={handleGenerate} disabled={pending}>↻ Regenerate</button>
    </div>
  {/if}
</div>

<style>
  .invite-manager { font-size: 0.875rem; }
  .explanation { color: var(--text-secondary); font-size: 0.8rem; margin: 0 0 12px; }
  .warning { color: var(--role-mod); font-size: 0.8rem; margin: 0 0 12px; }
  .error { color: var(--danger-muted); font-size: 0.8rem; margin: 0 0 12px; }
  .url-row {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 10px 12px;
    margin-bottom: 12px;
  }
  .url {
    flex: 1;
    font-size: 0.7rem;
    color: var(--text-secondary);
    font-family: var(--font-mono);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .actions { display: flex; gap: 8px; }
  .generate-btn,
  .copy-btn,
  .regen-btn {
    background: var(--accent);
    color: var(--on-accent);
    border: none;
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 0.75rem;
  }
  .copy-btn { padding: 4px 10px; }
  .regen-btn { background: var(--bg-tertiary); color: var(--text-secondary); }
  button:disabled { opacity: 0.4; cursor: not-allowed; }
</style>
