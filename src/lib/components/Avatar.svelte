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

{#snippet avatarContent()}
  {#if avatarUrl}
    <img src={avatarUrl} alt={displayName} width={size} height={size} />
  {:else}
    {@html identiconSvg}
  {/if}
{/snippet}

{#if onclick}
  <button
    class="avatar interactive"
    style="width: {size}px; height: {size}px;"
    title={displayName}
    onclick={(e) => onclick?.(e)}
    type="button"
  >
    {@render avatarContent()}
  </button>
{:else}
  <span
    class="avatar"
    style="width: {size}px; height: {size}px;"
    title={displayName}
  >
    {@render avatarContent()}
  </span>
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
    cursor: default;
  }

  .avatar.interactive {
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
