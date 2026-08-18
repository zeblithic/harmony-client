<script lang="ts">
  import { generateIdenticon } from '../identicon';
  import { nonEmpty } from '../display-label';
  import { shortId } from '../short-addr';

  let { address, size = 24, displayName = '', avatarUrl, onclick }: {
    address: string;
    size?: number;
    displayName?: string;
    avatarUrl?: string;
    onclick?: (e: MouseEvent) => void;
  } = $props();

  let identiconSvg = $derived(generateIdenticon(address, size));
  // ZEB-962: the shared accessible label. A peer can broadcast a blank/whitespace
  // name, so never render an empty `alt`/`title`; fall back to the identity floor.
  let label = $derived(nonEmpty(displayName) ?? shortId(address));
</script>

{#snippet avatarContent()}
  {#if avatarUrl}
    <img src={avatarUrl} alt={label} width={size} height={size} />
  {:else}
    {@html identiconSvg}
  {/if}
{/snippet}

{#if onclick}
  <button
    class="avatar interactive"
    style="width: {size}px; height: {size}px;"
    title={label}
    onclick={(e) => onclick?.(e)}
    type="button"
  >
    {@render avatarContent()}
  </button>
{:else}
  <span
    class="avatar"
    style="width: {size}px; height: {size}px;"
    title={label}
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
