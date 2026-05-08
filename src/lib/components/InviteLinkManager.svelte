<script lang="ts">
  let {
    kind,
    onGenerate,
  }: {
    kind: 'open' | 'invite-only';
    onGenerate: () => Promise<string>;
  } = $props();

  let url = $state<string | null>(null);
  let pending = $state(false);
  let copied = $state(false);

  async function handleGenerate() {
    pending = true;
    try {
      url = await onGenerate();
    } finally {
      pending = false;
    }
  }

  async function handleCopy() {
    if (!url) return;
    await navigator.clipboard.writeText(url);
    copied = true;
    setTimeout(() => (copied = false), 2000);
  }
</script>

<div class="invite-manager">
  {#if !url}
    <p class="explanation">Generate a one-time invite link to share via DM, email, or any side channel.</p>
    <button class="generate-btn" onclick={handleGenerate} disabled={pending}>
      {pending ? 'Generating...' : '+ Generate invite link'}
    </button>
  {:else}
    {#if kind === 'invite-only'}
      <p class="warning">Don't post publicly — it embeds your admin bootstrap signature. Each link can only be redeemed once.</p>
    {:else}
      <p class="warning">Anyone with this URL can join. The same link works indefinitely.</p>
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
  .warning { color: #ffb84a; font-size: 0.8rem; margin: 0 0 12px; }
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
    font-family: monospace;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .actions { display: flex; gap: 8px; }
  .generate-btn,
  .copy-btn,
  .regen-btn {
    background: var(--accent);
    color: var(--text-primary);
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
