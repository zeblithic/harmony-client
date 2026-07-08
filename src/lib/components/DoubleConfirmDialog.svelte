<script lang="ts">
  import Modal from './Modal.svelte';
  import ConfirmDialogContent from './ConfirmDialogContent.svelte';

  let {
    title,
    firstMessage,
    secondMessage,
    confirmLabel,
    destructive = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    firstMessage: string;
    secondMessage: string;
    confirmLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  let gate = $state(1);
  const titleId = `dialog-title-${Math.random().toString(36).slice(2)}`;
  let contentEl: HTMLElement;

  // Re-focus first button when gate transitions 1→2 — trapFocus only acts
  // on mount/unmount, not on internal state changes that swap the visible
  // button set. On mount this no-ops because trapFocus already focused the
  // first button. Cancel is first (the safe default for a double-confirm).
  $effect(() => {
    void gate;
    contentEl?.querySelector<HTMLElement>('button')?.focus();
  });
</script>

<Modal onCancel={onCancel} ariaLabelledby={titleId}>
  <div bind:this={contentEl}>
    {#if gate === 1}
      <ConfirmDialogContent
        {title}
        {titleId}
        message={firstMessage}
        confirmLabel="Continue"
        onConfirm={() => (gate = 2)}
        {onCancel}
      />
    {:else}
      <ConfirmDialogContent
        {title}
        {titleId}
        message={secondMessage}
        {confirmLabel}
        {destructive}
        {onConfirm}
        {onCancel}
      />
    {/if}
  </div>
</Modal>
