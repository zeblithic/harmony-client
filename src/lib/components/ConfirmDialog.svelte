<script lang="ts">
  import Modal from './Modal.svelte';
  import ConfirmDialogContent from './ConfirmDialogContent.svelte';

  let {
    title,
    message,
    confirmLabel,
    destructive = false,
    onConfirm,
    onCancel,
  }: {
    title: string;
    message: string;
    confirmLabel: string;
    destructive?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  const titleId = `dialog-title-${Math.random().toString(36).slice(2)}`;
</script>

<Modal onCancel={onCancel} ariaLabelledby={titleId}>
  <ConfirmDialogContent
    {title}
    {titleId}
    {message}
    {confirmLabel}
    {destructive}
    {onConfirm}
    {onCancel}
  />
</Modal>
