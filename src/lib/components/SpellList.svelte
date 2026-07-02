<script lang="ts">
  // v1: Empty state. Future: displays bookmarked Q8 page addresses.
</script>

<div class="spell-list-empty">
  <p class="empty-title">No spells yet</p>
  <p class="empty-hint">Try the Practice tab to start learning Q8 pronunciation.</p>
</div>

<style>
  .spell-list-empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 100%;
    padding: 32px;
    text-align: center;
  }

  .empty-title {
    font-size: 1.125rem;
    color: var(--text-primary);
    margin: 0 0 8px;
  }

  .empty-hint {
    font-size: 0.875rem;
    color: var(--text-muted);
    margin: 0;
  }
</style>
