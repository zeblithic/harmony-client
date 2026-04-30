<script lang="ts">
  import Modal from '../Modal.svelte';

  let { ariaLabelledby, onCancel }: { ariaLabelledby: string; onCancel: () => void } = $props();
</script>

<Modal {onCancel} {ariaLabelledby}>
  <h2 id={ariaLabelledby}>Heading</h2>
  <p>child content</p>
</Modal>
