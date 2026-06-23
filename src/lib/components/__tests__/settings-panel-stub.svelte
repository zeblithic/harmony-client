<script lang="ts">
  // Stub for SettingsPanel.test.ts. The real settings panels each fire Tauri
  // IPC / construct services on mount; mocking all six to this no-op isolates
  // the container test to *tab routing* rather than the panels' own behavior.
  // Accept-and-ignore whatever props the container forwards (the rest sink
  // keeps Svelte from warning about unknown props during mount).
  // eslint-disable-next-line @typescript-eslint/no-unused-vars
  const { ...rest } = $props();
</script>

<div data-testid="panel-stub"></div>
