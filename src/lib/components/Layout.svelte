<script lang="ts">
  import type { Snippet } from 'svelte';

  let { nav, textFeed, mediaFeed, settingsPanel, collapsed = false, showSettings = false }: {
    nav: Snippet;
    textFeed: Snippet;
    mediaFeed: Snippet;
    settingsPanel?: Snippet;
    collapsed?: boolean;
    showSettings?: boolean;
  } = $props();
</script>

<div class="layout" class:collapsed>
  <aside class="nav-area">
    {@render nav()}
  </aside>
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
</style>
