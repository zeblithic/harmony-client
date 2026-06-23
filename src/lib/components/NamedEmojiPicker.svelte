<script lang="ts">
  import type { ChannelMessageService, EmojiNameDto } from '../channel-message-service';
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

  const filtered = $derived(
    query.trim()
      ? all.filter((e) => e.name.toLowerCase().includes(query.trim().toLowerCase()))
      : all,
  );

  async function refresh() {
    try {
      all = await channelMessageService.listEmojiNames();
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
  }

  async function commitRename(e: EmojiNameDto): Promise<void> {
    const name = renameValue.trim();
    renaming = null;
    if (!name || name === e.name) return;
    try {
      await channelMessageService.setEmojiName(e.cid, name, e.mime, e.size);
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
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
          <input
            class="named-rename"
            type="text"
            aria-label={`Rename ${e.name}`}
            bind:value={renameValue}
            onkeydown={(ev) => ev.key === 'Enter' && commitRename(e)}
            onblur={() => commitRename(e)}
          />
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
  .named-grid { display: flex; flex-wrap: wrap; gap: 0.35rem; }
  .named-tile { display: inline-flex; align-items: center; gap: 0.15rem; }
  .named-pick { display: inline-flex; align-items: center; gap: 0.25rem; cursor: pointer; }
  .named-label { font-size: 0.8em; opacity: 0.85; }
  .named-act { cursor: pointer; opacity: 0.6; }
  .named-act:hover { opacity: 1; }
  .named-empty, .named-error { font-size: 0.8em; opacity: 0.7; margin: 0; }
</style>
