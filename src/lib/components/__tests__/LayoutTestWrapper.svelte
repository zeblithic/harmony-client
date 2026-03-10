<script lang="ts">
  import type { AppMode } from '../../types';
  import Layout from '../Layout.svelte';

  let { mode = 'messages', collapsed = false }: {
    mode?: AppMode;
    collapsed?: boolean;
  } = $props();
</script>

<Layout {collapsed} {mode}>
  {#snippet nav()}
    <div data-testid="nav">Nav</div>
  {/snippet}
  {#snippet textFeed()}
    <div data-testid="text-feed">Text Feed</div>
  {/snippet}
  {#snippet mediaFeed()}
    <div data-testid="media-feed">Media Feed</div>
  {/snippet}
  {#snippet fileBrowser()}
    <div data-testid="file-browser">File Browser</div>
  {/snippet}
  {#snippet fileDetailPanel()}
    <div data-testid="file-detail">File Detail</div>
  {/snippet}
</Layout>
