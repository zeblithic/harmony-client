<script lang="ts">
  import './app.css';
  import Layout from './lib/components/Layout.svelte';
  import NavPanel from './lib/components/NavPanel.svelte';
  import TextFeed from './lib/components/TextFeed.svelte';
  import MediaFeed from './lib/components/MediaFeed.svelte';
  import NotificationSettingsPanel from './lib/components/NotificationSettingsPanel.svelte';
  import { NotificationService } from './lib/notification-service';
  import { messages, navNodes } from './lib/mock-data';
  import type { MessagePriority } from './lib/types';

  let innerWidth = $state(window.innerWidth);
  let collapsed = $derived(innerWidth <= 768);
  let showSettings = $state(false);

  const notificationService = new NotificationService();

  // Mock per-peer override to demonstrate settings
  notificationService.setPeerPolicy('q7r8s9t0', { quiet: 'silent' });

  let allMessages = $state([...messages]);

  function scrollToMedia(mediaId: string) {
    document.getElementById(`media-${mediaId}`)?.scrollIntoView({
      behavior: 'smooth',
      block: 'center',
    });
  }

  function scrollToMessage(messageId: string) {
    let el = document.getElementById(`msg-${messageId}`);
    if (!el) {
      document.dispatchEvent(new CustomEvent('reveal-message', { detail: messageId }));
      requestAnimationFrame(() => {
        el = document.getElementById(`msg-${messageId}`);
        if (el) {
          el.scrollIntoView({ behavior: 'smooth', block: 'center' });
          el.classList.add('highlight');
          setTimeout(() => el.classList.remove('highlight'), 1500);
        }
      });
      return;
    }
    el.scrollIntoView({ behavior: 'smooth', block: 'center' });
    el.classList.add('highlight');
    setTimeout(() => el.classList.remove('highlight'), 1500);
  }

  function handleSend(text: string, priority: MessagePriority) {
    const newMsg = {
      id: `msg-${Date.now()}`,
      sender: { address: 'self', displayName: 'You' },
      text,
      timestamp: Date.now(),
      media: [],
      priority,
    };
    allMessages = [...allMessages, newMsg];
  }

  // Extract community nodes (folders) for settings panel
  let communities = $derived(navNodes.filter((n) => n.type === 'folder'));

  // Collect known peers from messages and nav nodes (DMs)
  let knownPeers = $derived.by(() => {
    const peerMap = new Map(
      allMessages
        .filter((m) => m.sender.address !== 'self')
        .map((m) => [m.sender.address, m.sender])
    );
    for (const node of navNodes) {
      if (node.peer && !peerMap.has(node.peer.address)) {
        peerMap.set(node.peer.address, node.peer);
      }
    }
    return [...peerMap.values()];
  });
</script>

<svelte:window bind:innerWidth />

<Layout {collapsed} {showSettings}>
  {#snippet nav()}
    <NavPanel nodes={navNodes} {collapsed} onSettingsClick={() => { showSettings = !showSettings; }} />
  {/snippet}
  {#snippet textFeed()}
    <TextFeed messages={allMessages} {collapsed} onMediaClick={scrollToMedia} onSend={handleSend} />
  {/snippet}
  {#snippet mediaFeed()}
    <MediaFeed messages={allMessages} onLinkBack={scrollToMessage} />
  {/snippet}
  {#snippet settingsPanel()}
    <NotificationSettingsPanel
      service={notificationService}
      peers={knownPeers}
      {communities}
      onClose={() => { showSettings = false; }}
    />
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
