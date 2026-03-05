<script lang="ts">
  let { address, size = 24, displayName = '' }: {
    address: string;
    size?: number;
    displayName?: string;
  } = $props();

  let hue = $derived(
    address.split('').reduce((acc, ch) => acc + ch.charCodeAt(0), 0) % 360
  );
  let bgColor = $derived(`hsl(${hue}, 45%, 45%)`);
  let initial = $derived(displayName.charAt(0).toUpperCase() || '?');
</script>

<div
  class="avatar"
  style="width: {size}px; height: {size}px; background: {bgColor}; font-size: {Math.round(size * 0.45)}px;"
  title={displayName}
>
  {initial}
</div>

<style>
  .avatar {
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    color: white;
    font-weight: 600;
    flex-shrink: 0;
    user-select: none;
  }
</style>
