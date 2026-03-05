<script lang="ts">
  import './app.css';
  import Layout from './lib/components/Layout.svelte';
  import NavPanel from './lib/components/NavPanel.svelte';
  import TextFeed from './lib/components/TextFeed.svelte';
  import MediaFeed from './lib/components/MediaFeed.svelte';
  import { messages, navNodes } from './lib/mock-data';

  let innerWidth = $state(window.innerWidth);
  let collapsed = $derived(innerWidth <= 768);

  function scrollToMedia(mediaId: string) {
    document.getElementById(`media-${mediaId}`)?.scrollIntoView({
      behavior: 'smooth',
      block: 'center',
    });
  }

  function scrollToMessage(messageId: string) {
    const el = document.getElementById(`msg-${messageId}`);
    if (el) {
      el.scrollIntoView({ behavior: 'smooth', block: 'center' });
      el.classList.add('highlight');
      setTimeout(() => el.classList.remove('highlight'), 1500);
    }
  }
</script>

<svelte:window bind:innerWidth />

<Layout {collapsed}>
  {#snippet nav()}
    <NavPanel nodes={navNodes} {collapsed} />
  {/snippet}
  {#snippet textFeed()}
    <TextFeed {messages} {collapsed} onMediaClick={scrollToMedia} />
  {/snippet}
  {#snippet mediaFeed()}
    <MediaFeed {messages} onLinkBack={scrollToMessage} />
  {/snippet}
</Layout>

<style>
  :global(.text-message) {
    transition: background 0.3s ease;
  }

  :global(.text-message.highlight) {
    background: rgba(88, 101, 242, 0.15) !important;
  }
</style>
