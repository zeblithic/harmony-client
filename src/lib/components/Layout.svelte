<script lang="ts">
  import type { Snippet } from 'svelte';
  import type { AppMode } from '../types';

  let { nav, textFeed, mediaFeed, vineFeed, fileBrowser, fileDetailPanel, settingsPanel, collapsed = false, showSettings = false, mode = 'messages' }: {
    nav: Snippet;
    textFeed: Snippet;
    mediaFeed: Snippet;
    vineFeed?: Snippet;
    fileBrowser?: Snippet;
    fileDetailPanel?: Snippet;
    settingsPanel?: Snippet;
    collapsed?: boolean;
    showSettings?: boolean;
    mode?: AppMode;
  } = $props();
</script>

<div class="layout" class:collapsed class:files-mode={mode === 'files' && fileBrowser} class:vine-mode={mode === 'vines' && vineFeed}>
  <aside class="nav-area">
    {@render nav()}
  </aside>
  {#if mode === 'files' && fileBrowser}
    <main class="files-area">
      {@render fileBrowser()}
    </main>
    {#if !collapsed && fileDetailPanel}
      <section class="detail-area">
        {@render fileDetailPanel()}
      </section>
    {/if}
  {:else if mode === 'vines' && vineFeed}
    <main class="vine-area">
      {@render vineFeed()}
    </main>
  {:else}
    <main class="text-area">
      {@render textFeed()}
    </main>
    {#if !collapsed}
      <section class="media-area">
        {#if showSettings && settingsPanel}
          {@render settingsPanel()}
        {:else}
          {@render mediaFeed()}
        {/if}
      </section>
    {/if}
  {/if}
</div>

<style>
  .layout {
    display: grid;
    grid-template-columns: var(--nav-width) 1fr 1fr;
    grid-template-areas: "nav text media";
    height: 100vh;
    overflow: hidden;
  }

  .layout.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav text";
  }

  .nav-area {
    grid-area: nav;
    background: var(--bg-secondary);
    border-right: 1px solid var(--border);
    overflow-y: auto;
  }

  .text-area {
    grid-area: text;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .media-area {
    grid-area: media;
    background: var(--bg-secondary);
    border-left: 1px solid var(--border);
    overflow-y: auto;
    padding: 12px;
  }

  .layout.files-mode {
    grid-template-columns: var(--nav-width) 1fr 320px;
    grid-template-areas: "nav files detail";
  }
  .layout.files-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav files";
  }
  .files-area {
    grid-area: files;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }
  .detail-area {
    grid-area: detail;
    background: var(--bg-secondary);
    overflow-y: auto;
    border-left: 1px solid var(--border);
  }

  .layout.vine-mode {
    grid-template-columns: var(--nav-width) 1fr;
    grid-template-areas: "nav vine";
  }

  .layout.vine-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav vine";
  }

  .vine-area {
    grid-area: vine;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }
</style>
