<script lang="ts">
  import './app.css';
  import Layout from './lib/components/Layout.svelte';
</script>

<Layout>
  {#snippet nav()}
    <div style="padding: 12px; color: var(--text-secondary);">Nav</div>
  {/snippet}
  {#snippet textFeed()}
    <div style="padding: 12px;">Text Feed</div>
  {/snippet}
  {#snippet mediaFeed()}
    <div style="padding: 12px;">Media Feed</div>
  {/snippet}
</Layout>
