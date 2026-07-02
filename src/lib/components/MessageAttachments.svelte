<script lang="ts">
  import { save } from '@tauri-apps/plugin-dialog';
  import type { ChannelAttachmentDto, ChannelMessageService } from '../channel-message-service';
  import { formatBytes, mimeCategoryIcon } from '../file-utils';
  import { isPreviewable, isImage, isText, decodeTextHead, type TextHead } from '../artifact-preview';
  import { assertHeaderDimsOk, assertDecodedDimsOk, parseImageHeaderDims } from '../avatar-normalize';
  import { mapContentFetchError } from '../content-fetch-errors';
  import { untrack } from 'svelte';

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

  type PreviewState = 'idle' | 'loading' | 'shown' | 'error';
  let previewStates = $state<Record<string, PreviewState>>({});
  let previewUrls = $state<Record<string, string>>({});   // image blob URLs
  let previewTexts = $state<Record<string, TextHead>>({});
  let previewExpanded = $state<Record<string, boolean>>({}); // text "show more"
  let previewErrors = $state<Record<string, string>>({});

  function previewStateOf(cid: string): PreviewState {
    return previewStates[cid] ?? 'idle';
  }

  // Set true in the unmount cleanup so preview work that resolves AFTER teardown
  // never creates or stores state on a dead component (mirrors AvatarResolver's
  // `destroyed` re-check after its async decode gap).
  let destroyed = false;
  // A preview result is only still wanted if the component is mounted AND the cid
  // is still in the current attachment set (the feed reuses instances positionally
  // under an unkeyed {#each}, so the prop can change mid-fetch).
  function isLive(cid: string): boolean {
    return !destroyed && uniqueAttachments.some((a) => a.cid === cid);
  }

  function revokeUrl(cid: string) {
    const url = previewUrls[cid];
    if (url) {
      URL.revokeObjectURL(url);
      const { [cid]: _drop, ...rest } = previewUrls;
      previewUrls = rest;
    }
  }

  async function togglePreview(att: ChannelAttachmentDto) {
    const st = previewStateOf(att.cid);
    if (st === 'loading') return;
    if (st === 'shown') {
      // collapse + free (drop all per-cid preview state so a re-preview starts clean)
      revokeUrl(att.cid);
      const { [att.cid]: _t, ...t } = previewTexts; previewTexts = t;
      const { [att.cid]: _x, ...x } = previewExpanded; previewExpanded = x;
      const { [att.cid]: _e, ...e } = previewErrors; previewErrors = e;
      previewStates = { ...previewStates, [att.cid]: 'idle' };
      return;
    }
    previewStates = { ...previewStates, [att.cid]: 'loading' };
    previewErrors = { ...previewErrors, [att.cid]: '' };
    try {
      const bytes = await channelMessageService.previewArtifact(communityId, channelId, att);
      // Bail if torn down / reused during the IPC — nothing was created yet.
      if (!isLive(att.cid)) return;
      if (isImage(att)) {
        // Decode-bomb guards mirror avatar-resolver: header dims BEFORE decode,
        // decoded dims AFTER (8192px limit). A throw lands in catch → error.
        //
        // Content-validate FIRST: `createImageBitmap` decodes by the BYTES, not
        // the mime label, and `assertHeaderDimsOk` is a no-op for formats it
        // can't parse — so an extension-spoofed non-PNG/JPEG (e.g. a WebP/GIF
        // decode bomb renamed `.jpg`/`.png`, which `detect_mime` would then
        // classify image/*) would otherwise be decoded unguarded. Require a
        // genuine PNG/JPEG header so only formats the pre-decode dimension guard
        // actually bounds ever reach the decoder. (CodeAnt, PR #329.)
        if (!parseImageHeaderDims(bytes)) {
          throw new Error('Not a previewable image — download it to view.');
        }
        assertHeaderDimsOk(bytes);
        const blob = new Blob([bytes], { type: att.mime });
        const bmp = await createImageBitmap(blob);
        try {
          assertDecodedDimsOk(bmp.width, bmp.height);
        } finally {
          bmp.close();
        }
        // Re-check after the decode await, BEFORE creating the blob URL, so a
        // late-resolving preview can't leak a URL onto a dead/stale component.
        if (!isLive(att.cid)) return;
        previewUrls = { ...previewUrls, [att.cid]: URL.createObjectURL(blob) };
      } else {
        previewTexts = { ...previewTexts, [att.cid]: decodeTextHead(bytes) };
      }
      previewStates = { ...previewStates, [att.cid]: 'shown' };
    } catch (e) {
      // Don't resurrect state on a torn-down / reused component.
      if (!isLive(att.cid)) return;
      // Map raw transport strings (leaked zenoh key-exprs, "timed out after 30s")
      // to friendly copy; keep the raw detail in the console for diagnostics.
      const raw = e instanceof Error ? e.message : String(e);
      console.warn(`preview '${att.name}' failed:`, raw);
      previewStates = { ...previewStates, [att.cid]: 'error' };
      previewErrors = { ...previewErrors, [att.cid]: mapContentFetchError(raw) };
    }
  }

  // Leak safety net: revoke every blob URL when this component unmounts. Mirrors
  // AvatarResolver.destroy().
  $effect(() => {
    return () => {
      destroyed = true;
      for (const url of Object.values(previewUrls)) URL.revokeObjectURL(url);
    };
  });

  // Drop per-cid preview state for attachments no longer present, revoking any
  // orphaned blob URL. The feed renders MessageAttachments under an UNKEYED
  // {#each}, so on a channel switch / message churn Svelte reuses this instance
  // positionally with a DIFFERENT `attachments` prop instead of unmounting it —
  // without this, an open image preview's blob URL would leak (the unmount
  // cleanup above only fires on a real teardown). Depend on `uniqueAttachments`
  // only; read/write the preview records under untrack so this never self-loops.
  function pruneRecord<T>(rec: Record<string, T>, live: Set<string>): Record<string, T> {
    const keys = Object.keys(rec);
    if (keys.every((k) => live.has(k))) return rec; // nothing orphaned — same ref
    const next: Record<string, T> = {};
    for (const k of keys) if (live.has(k)) next[k] = rec[k];
    return next;
  }
  $effect(() => {
    const live = new Set(uniqueAttachments.map((a) => a.cid));
    untrack(() => {
      for (const cid of Object.keys(previewUrls)) {
        if (!live.has(cid)) URL.revokeObjectURL(previewUrls[cid]);
      }
      const nextUrls = pruneRecord(previewUrls, live);
      if (nextUrls !== previewUrls) previewUrls = nextUrls;
      const nextTexts = pruneRecord(previewTexts, live);
      if (nextTexts !== previewTexts) previewTexts = nextTexts;
      const nextStates = pruneRecord(previewStates, live);
      if (nextStates !== previewStates) previewStates = nextStates;
      const nextErrors = pruneRecord(previewErrors, live);
      if (nextErrors !== previewErrors) previewErrors = nextErrors;
      const nextExpanded = pruneRecord(previewExpanded, live);
      if (nextExpanded !== previewExpanded) previewExpanded = nextExpanded;
    });
  });

  async function download(att: ChannelAttachmentDto) {
    if (stateOf(att.cid) === 'downloading') return;
    // att.name is sender-supplied; strip any path components so the dialog's
    // default is a plain basename, never a traversal or absolute path.
    const base = att.name.split(/[/\\]/).pop() || 'attachment';
    let destPath: string | null;
    try {
      destPath = await save({ defaultPath: base, ...filtersFor(base) });
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
      // Tauri IPC rejections are raw strings in prod, Error in tests. Map the
      // raw transport detail to friendly copy; keep the raw in the console.
      const raw = e instanceof Error ? e.message : String(e);
      console.warn(`download '${att.name}' failed:`, raw);
      states = { ...states, [att.cid]: 'error' };
      errors = { ...errors, [att.cid]: mapContentFetchError(raw) };
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
      {#if isPreviewable(att)}
        <button
          type="button"
          class="att-preview-btn"
          onclick={() => togglePreview(att)}
          disabled={previewStateOf(att.cid) === 'loading'}
          aria-label={previewStateOf(att.cid) === 'shown' ? `Hide preview ${att.name}` : `Preview ${att.name}`}
        >
          {#if previewStateOf(att.cid) === 'loading'}&#x2026;
          {:else if previewStateOf(att.cid) === 'shown'}&#x2715;
          {:else if previewStateOf(att.cid) === 'error'}&#x21BB;
          {:else}&#x1F441;{/if}
        </button>
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
    {#if previewStateOf(att.cid) === 'shown'}
      {#if isImage(att) && previewUrls[att.cid]}
        <img class="att-preview-img" src={previewUrls[att.cid]} alt={att.name} />
      {:else if isText(att) && previewTexts[att.cid]}
        <pre class="att-preview-text">{previewExpanded[att.cid] ? previewTexts[att.cid].full : previewTexts[att.cid].head}</pre>
        {#if previewTexts[att.cid].truncated}
          <button
            type="button"
            class="att-preview-more"
            onclick={() => (previewExpanded = { ...previewExpanded, [att.cid]: !previewExpanded[att.cid] })}
          >{previewExpanded[att.cid] ? 'Show less' : 'Show more'}</button>
        {/if}
      {/if}
    {/if}
    {#if previewStateOf(att.cid) === 'error'}
      <div class="att-error" role="alert">{previewErrors[att.cid]}</div>
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
  .attachment-chip.error { border-color: var(--danger-muted); }
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
  .att-download:hover:not(:disabled) { background: var(--bg-hover-subtle); }
  .att-download:disabled { opacity: 0.6; cursor: default; }
  .att-error { color: var(--danger-muted); font-size: 0.72rem; padding: 0 8px; max-width: 420px; }
  .att-preview-btn {
    flex: 0 0 auto;
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text-primary);
    cursor: pointer;
    padding: 2px 8px;
    font: inherit;
  }
  .att-preview-btn:hover:not(:disabled) { background: var(--bg-hover-subtle); }
  .att-preview-btn:disabled { opacity: 0.6; cursor: default; }
  .att-preview-img {
    display: block;
    max-width: 420px;
    max-height: 320px;
    margin-top: 4px;
    border: 1px solid var(--border);
    border-radius: 6px;
    object-fit: contain;
  }
  .att-preview-text {
    max-width: 420px;
    max-height: 320px;
    overflow: auto;
    margin-top: 4px;
    padding: 8px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 6px;
    font-size: 0.75rem;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .att-preview-more {
    background: transparent; border: none; color: var(--text-secondary);
    cursor: pointer; font: inherit; font-size: 0.72rem; padding: 2px 0;
  }
</style>
