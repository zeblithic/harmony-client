<script lang="ts">
  import { untrack } from 'svelte';
  import type { ChannelMessageService } from '../channel-message-service';
  import { parseImageHeaderDims, assertDecodedDimsOk } from '../avatar-normalize';

  // Custom (CAS-backed) reaction emoji image (ZEB-541). Resolves a reaction
  // emoji CID to a displayable blob URL via `previewReactionEmoji`, applying the
  // SAME blob-URL lifecycle discipline as ZEB-540's MessageAttachments preview:
  // per-cid fetch, `isLive` post-await guards, revoke on unmount + on cid change,
  // decode-bomb guards (header dims pre-decode, decoded dims post-decode), and a
  // neutral placeholder while pending / on error.
  let { communityId, channelId, cid, channelMessageService }: {
    communityId?: string;
    channelId?: string;
    cid: string;
    channelMessageService: ChannelMessageService;
  } = $props();

  let url = $state<string | null>(null);
  let failed = $state(false);

  // Set true by the unmount cleanup so fetch work that resolves AFTER teardown
  // never creates or stores a blob URL on a dead component (mirrors
  // AvatarResolver/MessageAttachments `destroyed` re-checks after the async gap).
  let destroyed = false;
  // The fetched bytes are only still wanted if the component is mounted AND the
  // cid prop hasn't changed mid-fetch (Svelte may reuse this instance
  // positionally under an unkeyed {#each}, so the prop can swap during the IPC).
  function isLive(forCid: string): boolean {
    return !destroyed && forCid === cid;
  }

  function revoke() {
    if (url) {
      URL.revokeObjectURL(url);
      url = null;
    }
  }

  async function load(forCid: string) {
    try {
      const bytes = communityId && channelId
        ? await channelMessageService.previewReactionEmoji(communityId, channelId, forCid)
        : await channelMessageService.previewNamedEmoji(forCid);
      // Bail if torn down / cid swapped during the IPC — nothing created yet.
      if (!isLive(forCid)) return;
      // Decode-bomb guards (ZEB-540/541 parity): these bytes are UNTRUSTED —
      // they come from a remote peer's signed React over the wire. Reject any
      // format whose header we can't parse BEFORE createImageBitmap allocates
      // the decoded bitmap. `assertHeaderDimsOk` silently passes unparseable
      // formats, so a non-PNG/JPEG decompression bomb (a huge WebP/GIF) would
      // otherwise reach decode before the post-decode guard. Emoji are minted as
      // PNG, so a PNG/JPEG-only header parse is sufficient.
      const header = parseImageHeaderDims(bytes);
      if (!header) throw new Error('unsupported emoji image format; use PNG or JPEG');
      assertDecodedDimsOk(header.width, header.height);
      // Label the blob with the format the header parser actually detected, not
      // a hardcoded image/png — the verifier accepts any image/* so a peer's
      // emoji could be JPEG; a contradictory MIME hint to createImageBitmap is
      // avoided (Greptile PR #320).
      const blob = new Blob([bytes], {
        type: header.format === 'jpeg' ? 'image/jpeg' : 'image/png',
      });
      const bmp = await createImageBitmap(blob);
      try {
        assertDecodedDimsOk(bmp.width, bmp.height);
      } finally {
        bmp.close();
      }
      // Re-check after the decode await, BEFORE creating the blob URL, so a
      // late-resolving fetch can't leak a URL onto a dead/stale component.
      if (!isLive(forCid)) return;
      revoke();
      url = URL.createObjectURL(blob);
      failed = false;
    } catch {
      if (!isLive(forCid)) return;
      revoke();
      failed = true;
    }
  }

  // (Re)fetch whenever the cid changes; revoke the previous blob URL first so a
  // cid swap on a reused instance never leaks. Depend ONLY on `cid` — read/write
  // the blob-URL state under untrack so revoke()/load() (which touch `url`) don't
  // register `url` as a dependency and re-trigger this effect into a loop.
  $effect(() => {
    const forCid = cid;
    untrack(() => {
      revoke();
      failed = false;
      void load(forCid);
    });
  });

  // Leak safety net: revoke the blob URL when this component unmounts.
  $effect(() => {
    return () => {
      destroyed = true;
      revoke();
    };
  });
</script>

{#if url}
  <img class="reaction-emoji-img" src={url} alt="custom emoji" />
{:else if failed}
  <!-- Distinguish failed (muted red ⚠) from still-loading (muted grey 🖼) both
       visually and to assistive tech — the bare glyph was aria-hidden and gave
       no hint whether to retry or wait. -->
  <span
    class="reaction-emoji-fallback reaction-emoji-failed"
    role="img"
    aria-label="Emoji failed to load"
    title="Emoji failed to load"
  >{'\u{26A0}'}</span>
{:else}
  <span
    class="reaction-emoji-fallback reaction-emoji-pending"
    role="img"
    aria-label="Emoji loading"
    title="Emoji loading…"
  >{'\u{1F5BC}'}</span>
{/if}

<style>
  .reaction-emoji-img {
    height: 1.4em;
    width: auto;
    max-width: 1.4em;
    object-fit: contain;
    vertical-align: middle;
  }
  .reaction-emoji-fallback {
    display: inline-block;
    height: 1.4em;
    line-height: 1.4em;
    vertical-align: middle;
    opacity: 0.6;
  }
  /* Pending reads as a neutral, dimmer placeholder; failed reads as an alert. */
  .reaction-emoji-pending {
    opacity: 0.45;
  }
  .reaction-emoji-failed {
    color: #d83c3e;
    opacity: 0.85;
  }
</style>
