<script lang="ts">
  // V1 scaffold (ZEB-349). Audio capture/relay + live roster arrive in
  // V2/V3 (ZEB-350/351); this view only proves the routing surface so a
  // 🔊 voice channel renders something coherent instead of a message feed.
  let { channelName }: { channelName: string } = $props();
</script>

<section class="voice-channel" aria-label="Voice channel">
  <header class="voice-header">
    <span class="voice-glyph" aria-hidden="true">🔊</span>
    <h2 class="voice-title">{channelName}</h2>
  </header>

  <div class="voice-body">
    <p class="voice-roster-placeholder">No one is here yet.</p>
    <button type="button" class="voice-join" disabled aria-describedby="voice-coming-soon">Join</button>
    <p id="voice-coming-soon" class="voice-note">Voice chat is coming soon.</p>
  </div>
</section>

<style>
  .voice-channel {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    height: 100%;
  }
  .voice-header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--border);
  }
  .voice-glyph { font-size: 0.95rem; }
  .voice-title {
    font-size: 1rem;
    margin: 0;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .voice-body {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.75rem;
    color: var(--text-secondary);
  }
  .voice-roster-placeholder { margin: 0; }
  .voice-join {
    border: none;
    padding: 8px 20px;
    border-radius: 4px;
    background: var(--accent);
    color: var(--text-primary);
    font-size: 0.9rem;
    cursor: pointer;
  }
  .voice-join:disabled { opacity: 0.5; cursor: not-allowed; }
  .voice-note { font-size: 0.85rem; margin: 0; color: var(--text-tertiary, var(--text-secondary)); }
</style>
