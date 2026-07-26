<script lang="ts">
  import type { Snippet } from 'svelte';
  import type { AppMode } from '../types';
  import {
    MEDIA_PANEL_MIN_WIDTH,
    MEDIA_PANEL_MAX_WIDTH,
    MEDIA_PANEL_DEFAULT_WIDTH,
  } from '../media-panel-prefs';

  let {
    nav,
    textFeed,
    mediaFeed,
    vineFeed,
    fileBrowser,
    fileDetailPanel,
    spellbookContent,
    spellbookDetail,
    mailInbox,
    mailDetail,
    mintLedger,
    networkPanel,
    settingsPanel,
    collapsed = false,
    showSettings = false,
    mode = 'messages',
    mailSelected = false,
    mediaPanelOpen = $bindable(false),
    mediaPanelWidth = $bindable(MEDIA_PANEL_DEFAULT_WIDTH),
  }: {
    nav: Snippet;
    textFeed: Snippet;
    mediaFeed: Snippet;
    vineFeed?: Snippet;
    fileBrowser?: Snippet;
    fileDetailPanel?: Snippet;
    spellbookContent?: Snippet;
    spellbookDetail?: Snippet;
    mailInbox?: Snippet;
    mailDetail?: Snippet;
    mintLedger?: Snippet;
    networkPanel?: Snippet;
    settingsPanel?: Snippet;
    collapsed?: boolean;
    showSettings?: boolean;
    mode?: AppMode;
    mailSelected?: boolean;
    mediaPanelOpen?: boolean;
    mediaPanelWidth?: number;
  } = $props();

  // ZEB-405 (WS-C): the messages-mode right column has three states —
  //   • responsive-collapsed (narrow screen): the existing `.collapsed` rule wins
  //   • visible: media feed (resizable) OR the Settings panel (which shares the cell)
  //   • closed: a slim edge rail to reveal the media feed
  // Settings keeps the column whenever it's open, even if the media feed is hidden.
  const isMessages = $derived(mode === 'messages');
  const wantSettings = $derived(isMessages && showSettings && !!settingsPanel);
  const rightColumnVisible = $derived(
    isMessages && !collapsed && (wantSettings || mediaPanelOpen),
  );
  const showingSettings = $derived(rightColumnVisible && wantSettings);
  const showingMedia = $derived(rightColumnVisible && !wantSettings);
  const railVisible = $derived(isMessages && !collapsed && !rightColumnVisible);

  // Live width during a drag, committed back to the bound prop on pointer-up so
  // the parent persists once per resize, not once per animation frame.
  const clampWidth = (px: number) =>
    Math.min(MEDIA_PANEL_MAX_WIDTH, Math.max(MEDIA_PANEL_MIN_WIDTH, Math.round(px)));
  const RESIZE_STEP = 16;

  let dragging = $state(false);
  let liveWidth = $state(mediaPanelWidth);
  let dragStartX = 0;
  let dragStartWidth = 0;

  // If the splitter unmounts mid-drag (panel hidden or Settings opens before
  // pointerup fires), the pointerup is lost and `dragging` would stick true —
  // clear it so the width re-syncs instead of freezing at the interrupted value.
  $effect(() => {
    if (!showingMedia && dragging) dragging = false;
  });
  // Mirror the prop into liveWidth whenever we're not mid-drag.
  $effect(() => {
    if (!dragging) liveWidth = mediaPanelWidth;
  });

  function onResizeStart(e: PointerEvent) {
    dragging = true;
    dragStartX = e.clientX;
    dragStartWidth = liveWidth;
    try {
      (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    } catch {
      // setPointerCapture can throw for an invalid/already-released pointerId.
    }
    e.preventDefault();
  }
  function onResizeMove(e: PointerEvent) {
    if (!dragging) return;
    // The panel sits on the right edge: dragging the handle left widens it.
    liveWidth = clampWidth(dragStartWidth + (dragStartX - e.clientX));
  }
  function onResizeEnd(e: PointerEvent) {
    if (!dragging) return;
    dragging = false;
    mediaPanelWidth = clampWidth(liveWidth);
    try {
      (e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId);
    } catch {
      // Pointer capture may already be released — non-fatal.
    }
  }
  function onResizeKey(e: KeyboardEvent) {
    if (e.key === 'ArrowLeft') {
      mediaPanelWidth = clampWidth(mediaPanelWidth + RESIZE_STEP);
      e.preventDefault();
    } else if (e.key === 'ArrowRight') {
      mediaPanelWidth = clampWidth(mediaPanelWidth - RESIZE_STEP);
      e.preventDefault();
    }
  }
</script>

<div
  class="layout"
  class:collapsed
  class:files-mode={mode === 'files' && fileBrowser}
  class:vine-mode={mode === 'vines' && vineFeed}
  class:spellbook-mode={mode === 'spellbook' && spellbookContent}
  class:mail-mode={mode === 'mail' && mailInbox}
  class:mint-mode={mode === 'mint' && mintLedger}
  class:network-mode={mode === 'network' && networkPanel}
  class:msg-media-open={showingMedia}
  class:msg-media-closed={railVisible}
  style="--media-width: {liveWidth}px"
>
  <aside class="nav-area">
    {@render nav()}
  </aside>
  {#if mode === 'mail' && mailInbox}
    {#if collapsed && mailSelected && mailDetail}
      <main class="mail-list-area">
        {@render mailDetail()}
      </main>
    {:else}
      <main class="mail-list-area">
        {@render mailInbox()}
      </main>
      {#if !collapsed && mailDetail}
        <section class="detail-area">
          {@render mailDetail()}
        </section>
      {/if}
    {/if}
  {:else if mode === 'files' && fileBrowser}
    <main class="files-area">
      {@render fileBrowser()}
    </main>
    {#if !collapsed && fileDetailPanel}
      <section class="detail-area">
        {@render fileDetailPanel()}
      </section>
    {/if}
  {:else if mode === 'vines' && vineFeed}
    <main class="vine-area">
      {@render vineFeed()}
    </main>
  {:else if mode === 'spellbook' && spellbookContent}
    <main class="spellbook-area">
      {@render spellbookContent()}
    </main>
    {#if !collapsed && spellbookDetail}
      <section class="detail-area">
        {@render spellbookDetail()}
      </section>
    {/if}
  {:else if mode === 'mint' && mintLedger}
    <main class="mint-area">
      {@render mintLedger()}
    </main>
  {:else if mode === 'network' && networkPanel}
    <main class="network-area">
      {@render networkPanel()}
    </main>
  {:else}
    <main class="text-area">
      {@render textFeed()}
    </main>
    {#if rightColumnVisible}
      <section class="media-area">
        {#if showingSettings}
          {@render settingsPanel?.()}
        {:else}
          {@render mediaFeed()}
        {/if}
      </section>
      {#if showingMedia}
        <!--
          ARIA "window splitter": a focusable separator with aria-valuenow +
          arrow-key resize. This role IS interactive, but Svelte's a11y lint
          doesn't model it, so suppress its two false positives here.
        -->
        <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
        <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
        <div
          class="media-resizer"
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize media panel"
          aria-valuemin={MEDIA_PANEL_MIN_WIDTH}
          aria-valuemax={MEDIA_PANEL_MAX_WIDTH}
          aria-valuenow={liveWidth}
          tabindex="0"
          onpointerdown={onResizeStart}
          onpointermove={onResizeMove}
          onpointerup={onResizeEnd}
          onpointercancel={onResizeEnd}
          onkeydown={onResizeKey}
        ></div>
        <!-- Collapse control: a chevron tab on the panel's left edge, vertically
             centered so it clears the app's top chrome (backup banner + Help
             button), mirroring the reveal rail (‹ opens → › closes). -->
        <button
          type="button"
          class="media-close-tab"
          aria-label="Hide media panel"
          title="Hide media panel"
          onclick={() => (mediaPanelOpen = false)}
        >›</button>
      {/if}
    {:else if railVisible}
      <button
        type="button"
        class="media-rail"
        aria-label="Show media panel"
        title="Show media panel"
        onclick={() => (mediaPanelOpen = true)}
      >‹</button>
    {/if}
  {/if}
</div>

<style>
  .layout {
    display: grid;
    /* ZEB-772: floor the text column. This base 3-column form is the variant
       that applies while Settings is open (`.msg-media-open`/`.msg-media-closed`
       override it otherwise), and a bare `1fr` let Settings starve the content
       column — measured 990px -> 504px here, and 694px -> 232px on a narrower
       nav, clipping the charter's "Propose amendment" to a 4px sliver.

       A `1fr` track is `minmax(auto, 1fr)` and would normally refuse to shrink
       below its content's min-content size; `.text-area`'s `overflow-y: auto`
       silently opts it out, because an element with non-visible overflow has an
       automatic minimum size of zero. So the floor has to be stated explicitly.

       360px only binds between 769px and ~960px of viewport (below 769px the
       `.collapsed` rule drops to two columns), so wide windows are unaffected
       and the narrow band trades a little Settings width for a readable
       document column. */
    grid-template-columns: var(--nav-width) minmax(360px, 1fr) 1fr;
    grid-template-areas: "nav text media";
    /* ZEB-406: fill the .app-shell column (banner reserves its own height above)
       instead of pinning to 100vh, which used to force the layout under a
       fixed-overlay banner. */
    flex: 1 1 auto;
    min-height: 0;
    overflow: hidden;
    position: relative;
  }

  .layout.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav text";
  }

  /* ZEB-405 (WS-C): messages-mode media-column states. The base grid above is
     the wide "settings/open" 3-column layout; these narrow or hide the media
     column. The `:not(.collapsed)` guard lets the responsive `.collapsed` rule
     win on narrow screens. */
  .layout.msg-media-open:not(.collapsed) {
    grid-template-columns: var(--nav-width) 1fr var(--media-width, 340px);
    grid-template-areas: "nav text media";
  }
  .layout.msg-media-closed:not(.collapsed) {
    grid-template-columns: var(--nav-width) 1fr 18px;
    grid-template-areas: "nav text rail";
  }

  .nav-area {
    grid-area: nav;
    background: var(--bg-secondary);
    border-right: 1px solid var(--border);
    overflow-y: auto;
  }

  .text-area {
    grid-area: text;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .media-area {
    grid-area: media;
    background: var(--bg-secondary);
    border-left: 1px solid var(--border);
    overflow-y: auto;
    padding: 12px;
  }

  /* Drag splitter — a direct child of `.layout` (never the scrolling cell), so
     it stays pinned to the media column's left edge at any scroll position. */
  .media-resizer {
    position: absolute;
    top: 0;
    bottom: 0;
    right: calc(var(--media-width, 340px) - 4px);
    width: 8px;
    cursor: col-resize;
    z-index: 5;
    background: transparent;
    touch-action: none;
  }
  .media-resizer:hover,
  .media-resizer:focus-visible {
    background: var(--accent);
    outline: none;
  }

  /* Collapse tab — straddles the panel's left edge, vertically centered (above
     the splitter) so it never collides with the top banner or Help button. */
  .media-close-tab {
    position: absolute;
    top: 50%;
    transform: translateY(-50%);
    right: calc(var(--media-width, 340px) - 11px);
    width: 22px;
    height: 48px;
    z-index: 6;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 6px 0 0 6px;
    background: var(--bg-secondary);
    color: var(--text-muted);
    cursor: pointer;
    font-size: 0.95rem;
    line-height: 1;
  }
  .media-close-tab:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  /* Slim reveal rail shown when the media feed is collapsed. */
  .media-rail {
    grid-area: rail;
    appearance: none;
    border: none;
    border-left: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-muted);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    font-size: 0.9rem;
  }
  .media-rail:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  .layout.files-mode {
    grid-template-columns: var(--nav-width) 1fr 320px;
    grid-template-areas: "nav files detail";
  }
  .layout.files-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav files";
  }
  .files-area {
    grid-area: files;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }
  .detail-area {
    grid-area: detail;
    background: var(--bg-secondary);
    overflow-y: auto;
    border-left: 1px solid var(--border);
  }

  .layout.vine-mode {
    grid-template-columns: var(--nav-width) 1fr;
    grid-template-areas: "nav vine";
  }

  .layout.vine-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav vine";
  }

  .vine-area {
    grid-area: vine;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .layout.spellbook-mode {
    grid-template-columns: var(--nav-width) 1fr 320px;
    grid-template-areas: "nav spellbook detail";
  }
  .layout.spellbook-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav spellbook";
  }
  .spellbook-area {
    grid-area: spellbook;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .layout.mail-mode {
    grid-template-columns: var(--nav-width) 1fr 1fr;
    grid-template-areas: "nav mail-list detail";
  }
  .layout.mail-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav mail-list";
  }
  .mail-list-area {
    grid-area: mail-list;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .layout.mint-mode {
    grid-template-columns: var(--nav-width) 1fr;
    grid-template-areas: "nav mint";
  }
  .layout.mint-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav mint";
  }
  .mint-area {
    grid-area: mint;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }

  .layout.network-mode {
    grid-template-columns: var(--nav-width) 1fr;
    grid-template-areas: "nav network";
  }
  .layout.network-mode.collapsed {
    grid-template-columns: var(--nav-width-collapsed) 1fr;
    grid-template-areas: "nav network";
  }
  .network-area {
    grid-area: network;
    background: var(--bg-primary);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }
</style>
