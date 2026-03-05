<script lang="ts">
  import { generateIdenticon } from '../identicon';

  let { address, size = 24, displayName = '', avatarUrl, onclick }: {
    address: string;
    size?: number;
    displayName?: string;
    avatarUrl?: string;
    onclick?: (e: MouseEvent) => void;
  } = $props();

  let identiconSvg = $derived(generateIdenticon(address, size));
</script>

{#if avatarUrl}
  <button
    class="avatar"
    style="width: {size}px; height: {size}px;"
    title={displayName}
    onclick={(e) => onclick?.(e)}
    type="button"
  >
    <img src={avatarUrl} alt={displayName} width={size} height={size} />
  </button>
{:else}
  <button
    class="avatar"
    style="width: {size}px; height: {size}px;"
    title={displayName}
    onclick={(e) => onclick?.(e)}
    type="button"
  >
    {@html identiconSvg}
  </button>
{/if}

<style>
  .avatar {
    border-radius: 50%;
    overflow: hidden;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    user-select: none;
    border: none;
    background: none;
    padding: 0;
    cursor: pointer;
  }

  .avatar :global(svg) {
    display: block;
    border-radius: 50%;
  }

  .avatar img {
    display: block;
    border-radius: 50%;
    object-fit: cover;
  }
</style>
