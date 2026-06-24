<script lang="ts">
  import type { ChannelMessageService, EmojiNameDto } from '../channel-message-service';
  import { emojiNameError, MAX_EMOJI_NAME_LEN } from '../emoji-name-validation';
  import ReactionEmojiImage from './ReactionEmojiImage.svelte';

  let { channelMessageService, onpick }: {
    channelMessageService: ChannelMessageService;
    onpick: (descriptor: { cid: string; mime: string; size: number }) => void;
  } = $props();

  let all = $state<EmojiNameDto[]>([]);
  let query = $state('');
  let error = $state<string | null>(null);
  let renaming = $state<string | null>(null);
  let renameValue = $state('');
  let renameError = $state<string | null>(null);
  // Non-reactive in-flight guard: true while a setEmojiName IPC is awaiting so a
  // trailing blur / repeated Enter can't double-fire (we close on success only).
  let renameCommitting = false;

  const filtered = $derived(
    query.trim()
      ? all.filter((e) => e.name.toLowerCase().includes(query.trim().toLowerCase()))
      : all,
  );

  async function refresh() {
    try {
      all = await channelMessageService.listEmojiNames();
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    void refresh();
    return channelMessageService.onEmojiNamesChanged(() => void refresh());
  });

  function pick(e: EmojiNameDto): void {
    onpick({ cid: e.cid, mime: e.mime, size: e.size });
  }

  function startRename(e: EmojiNameDto): void {
    renaming = e.cid;
    renameValue = e.name;
    renameError = null;
  }

  function cancelRename(): void {
    renaming = null;
    renameError = null;
  }

  async function commitRename(e: EmojiNameDto): Promise<void> {
    // Guard against a trailing blur after Enter, and a repeated Enter while the
    // IPC is in flight — without it both would fire setEmojiName.
    if (renaming !== e.cid || renameCommitting) return;
    const name = renameValue.trim();
    if (!name || name === e.name) {
      cancelRename();
      return;
    }
    // Validate client-side: keep the editor OPEN on a bad name so the user's
    // typed text isn't lost to a raw backend bounce.
    const validationErr = emojiNameError(name);
    if (validationErr) {
      renameError = validationErr;
      return;
    }
    // Close only AFTER the backend confirms, so a transient failure keeps the
    // typed name on screen to retry (CodeRabbit, PR #330).
    renameCommitting = true;
    try {
      await channelMessageService.setEmojiName(e.cid, name, e.mime, e.size);
      cancelRename();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      renameCommitting = false;
    }
  }

  async function remove(e: EmojiNameDto): Promise<void> {
    try {
      await channelMessageService.setEmojiName(e.cid, null, e.mime, e.size);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }
</script>

<div class="named-emoji-picker" role="menu" aria-label="React with a named emoji">
  <input
    class="named-search"
    type="text"
    placeholder="Search named emoji…"
    aria-label="Search named emoji"
    bind:value={query}
  />
  {#if error}
    <p class="named-error" role="alert">{error}</p>
  {/if}
  <div class="named-grid">
    {#each filtered as e (e.cid)}
      <div class="named-tile">
        {#if renaming === e.cid}
          <span class="named-rename-wrap">
            <input
              class="named-rename"
              type="text"
              maxlength={MAX_EMOJI_NAME_LEN}
              aria-label={`Rename ${e.name}`}
              bind:value={renameValue}
              onkeydown={(ev) => {
                if (ev.key === 'Enter') commitRename(e);
                else if (ev.key === 'Escape') cancelRename();
              }}
              onblur={() => commitRename(e)}
            />
            {#if renameError}
              <span class="named-rename-error" role="alert">{renameError}</span>
            {/if}
          </span>
        {:else}
          <button type="button" class="named-pick" role="menuitem" title={e.name} onclick={() => pick(e)}>
            <ReactionEmojiImage cid={e.cid} {channelMessageService} />
            <span class="named-label">{e.name}</span>
          </button>
          <button type="button" class="named-act" aria-label={`Rename ${e.name}`} onclick={() => startRename(e)}>✎</button>
          <button type="button" class="named-act" aria-label={`Remove name ${e.name}`} onclick={() => remove(e)}>🗑</button>
        {/if}
      </div>
    {/each}
    {#if filtered.length === 0}
      <p class="named-empty">No named emoji yet. Name one from a reaction or at upload.</p>
    {/if}
  </div>
</div>

<style>
  .named-emoji-picker { display: flex; flex-direction: column; gap: 0.4rem; padding: 0.5rem; min-width: 14rem; max-width: 18rem; }
  .named-search, .named-rename { width: 100%; box-sizing: border-box; padding: 0.25rem 0.4rem; }
  .named-rename-wrap { display: inline-flex; flex-direction: column; gap: 0.1rem; }
  .named-rename-error { font-size: 0.7em; color: #d83c3e; }
  .named-grid { display: flex; flex-wrap: wrap; gap: 0.35rem; }
  .named-tile { display: inline-flex; align-items: center; gap: 0.15rem; }
  .named-pick { display: inline-flex; align-items: center; gap: 0.25rem; cursor: pointer; }
  .named-label { font-size: 0.8em; opacity: 0.85; }
  .named-act { cursor: pointer; opacity: 0.6; }
  .named-act:hover { opacity: 1; }
  .named-empty, .named-error { font-size: 0.8em; opacity: 0.7; margin: 0; }
</style>
