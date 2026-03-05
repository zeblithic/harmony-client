<script lang="ts">
  import './app.css';
  import Layout from './lib/components/Layout.svelte';
  import NavPanel from './lib/components/NavPanel.svelte';
  import { channels } from './lib/mock-data';

  let innerWidth = $state(window.innerWidth);
  let collapsed = $derived(innerWidth <= 768);
</script>

<svelte:window bind:innerWidth />

<Layout>
  {#snippet nav()}
    <NavPanel {channels} {collapsed} />
  {/snippet}
  {#snippet textFeed()}
    <div style="padding: 12px;">Text Feed</div>
  {/snippet}
  {#snippet mediaFeed()}
    <div style="padding: 12px;">Media Feed</div>
  {/snippet}
</Layout>
