<script lang="ts">
  import { save } from '@tauri-apps/plugin-dialog';
  import type { ChannelAttachmentDto, ChannelMessageService } from '../channel-message-service';
  import { formatBytes, mimeCategoryIcon } from '../file-utils';

  let { communityId, channelId, attachments, channelMessageService }: {
    communityId: string;
    channelId: string;
    attachments: ChannelAttachmentDto[];
    channelMessageService: ChannelMessageService;
  } = $props();

  // Defense in depth: a message can carry duplicate CIDs (same file attached
  // twice, or a malformed/old-client message). The keyed {#each} on cid throws
  // on duplicate keys — even in production — so render each cid once.
  let uniqueAttachments = $derived(
    attachments.filter((a, i) => attachments.findIndex((b) => b.cid === a.cid) === i),
  );

  type DownloadState = 'idle' | 'downloading' | 'saved' | 'error';
  // Per-cid state so each attachment downloads independently.
  let states = $state<Record<string, DownloadState>>({});
  let errors = $state<Record<string, string>>({});

  function stateOf(cid: string): DownloadState {
    return states[cid] ?? 'idle';
  }

  function filtersFor(name: string): { filters?: { name: string; extensions: string[] }[] } {
    const dot = name.lastIndexOf('.');
    if (dot <= 0 || dot === name.length - 1) return {};
    const ext = name.slice(dot + 1).toLowerCase();
    return { filters: [{ name: ext.toUpperCase(), extensions: [ext] }] };
  }

  async function download(att: ChannelAttachmentDto) {
    if (stateOf(att.cid) === 'downloading') return;
    let destPath: string | null;
    try {
      destPath = await save({ defaultPath: att.name, ...filtersFor(att.name) });
    } catch {
      // Treat a dialog backend error like a cancel — nothing downloaded.
      return;
    }
    if (!destPath) return; // user cancelled
    states = { ...states, [att.cid]: 'downloading' };
    errors = { ...errors, [att.cid]: '' };
    try {
      await channelMessageService.downloadArtifact(communityId, channelId, att, destPath);
      states = { ...states, [att.cid]: 'saved' };
    } catch (e) {
      // Tauri IPC rejections are raw strings in prod, Error in tests.
      states = { ...states, [att.cid]: 'error' };
      errors = { ...errors, [att.cid]: e instanceof Error ? e.message : String(e) };
    }
  }
</script>

<div class="attachments">
  {#each uniqueAttachments as att (att.cid)}
    <div class="attachment-chip" class:error={stateOf(att.cid) === 'error'}>
      <span class="att-icon" aria-hidden="true">{mimeCategoryIcon(att.mime)}</span>
      <span class="att-name" title={att.name}>{att.name}</span>
      <span class="att-size">{formatBytes(att.size)}</span>
      {#if att.encrypted}
        <span class="att-lock" title="Encrypted" aria-label="Encrypted">&#x1F512;</span>
      {/if}
      <button
        type="button"
        class="att-download"
        onclick={() => download(att)}
        disabled={stateOf(att.cid) === 'downloading'}
        aria-label={stateOf(att.cid) === 'error' ? `Retry download ${att.name}` : `Download ${att.name}`}
      >
        {#if stateOf(att.cid) === 'downloading'}&#x2026;
        {:else if stateOf(att.cid) === 'saved'}&#x2713;
        {:else if stateOf(att.cid) === 'error'}&#x21BB;
        {:else}&#x2913;{/if}
      </button>
    </div>
    {#if stateOf(att.cid) === 'error'}
      <div class="att-error" role="alert">{errors[att.cid]}</div>
    {/if}
  {/each}
</div>

<style>
  .attachments { display: flex; flex-direction: column; gap: 4px; margin-top: 4px; }
  .attachment-chip {
    display: flex;
    align-items: center;
    gap: 8px;
    max-width: 420px;
    padding: 6px 8px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-size: 0.8rem;
  }
  .attachment-chip.error { border-color: #d83c3e; }
  .att-icon { flex: 0 0 auto; }
  .att-name {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-primary);
  }
  .att-size { flex: 0 0 auto; color: var(--text-secondary); }
  .att-lock { flex: 0 0 auto; }
  .att-download {
    flex: 0 0 auto;
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    cursor: pointer;
    padding: 2px 8px;
    font: inherit;
  }
  .att-download:hover:not(:disabled) { background: rgba(255, 255, 255, 0.06); }
  .att-download:disabled { opacity: 0.6; cursor: default; }
  .att-error { color: #d83c3e; font-size: 0.72rem; padding: 0 8px; max-width: 420px; }
</style>
