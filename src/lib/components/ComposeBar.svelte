<script lang="ts">
  let draft = $state('');
</script>

<div class="compose-bar">
  <input
    type="text"
    class="compose-input"
    placeholder="Message #general"
    bind:value={draft}
  />
</div>

<style>
  .compose-bar {
    padding: 12px 16px;
    border-top: 1px solid var(--border);
    flex-shrink: 0;
  }

  .compose-input {
    width: 100%;
    padding: 10px 12px;
    border: none;
    border-radius: 8px;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-size: 14px;
    outline: none;
  }

  .compose-input::placeholder {
    color: var(--text-muted);
  }

  .compose-input:focus {
    box-shadow: 0 0 0 2px var(--accent);
  }
</style>
