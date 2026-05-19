<!--
  ZEB-166: inline retry block for folder-load failures. Rendered in
  place of FileList/FileGrid when FileBrowser's folderLoadError is
  set and matches the current cid. Replaces the legacy window.alert
  path.
-->
<script lang="ts">
  let {
    message,
    onRetry,
  }: {
    message: string;
    onRetry: () => void;
  } = $props();
</script>

<div class="folder-load-error" role="alert">
  <span class="folder-load-error-icon" aria-hidden="true">⚠</span>
  <div class="folder-load-error-title">Could not load folder contents:</div>
  <p class="folder-load-error-message">{message}</p>
  <button class="folder-load-error-retry" onclick={onRetry}>Retry</button>
</div>

<style>
  .folder-load-error {
    display: flex;
    flex: 1;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    padding: 32px;
    text-align: center;
  }

  .folder-load-error-icon {
    font-size: 2rem;
    color: var(--text-muted, #949ba4);
  }

  .folder-load-error-title {
    color: var(--text-muted, #949ba4);
    font-size: 0.9rem;
  }

  .folder-load-error-message {
    margin: 0;
    max-width: 480px;
    overflow-wrap: anywhere;
    color: var(--text-primary, #f2f3f5);
    font-size: 0.85rem;
  }

  .folder-load-error-retry {
    margin-top: 8px;
    padding: 4px 10px;
    font: inherit;
    font-size: 0.85rem;
    border: 1px solid var(--border, #3f4147);
    background: transparent;
    color: var(--text-muted, #949ba4);
    cursor: pointer;
    border-radius: 4px;
  }

  .folder-load-error-retry:hover {
    background: var(--bg-tertiary, #232428);
  }

  .folder-load-error-retry:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 1px;
  }
</style>
