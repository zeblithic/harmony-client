<script lang="ts">
  /**
   * ZEB-555 — nav-rail "More ▾" overflow menu.
   *
   * Replaces the floating top-right (?) help button. Opens upward from the
   * nav footer with two sections:
   *  - "Go to": the secondary/flag-gated app modes (Mail / Spellbook / Mint /
   *    Network). Only enabled modes are passed in, so in the default alpha this
   *    section is empty and the menu shows just Help.
   *  - "Help": Network Health / Submit Feedback / About / Documentation —
   *    the items that used to live under the (?) glyph.
   *
   * Dropdown a11y mirrors the former HelpMenuButton: role=menu/menuitem,
   * click-outside + Escape + Tab close, ArrowUp/Down roving focus, first-item
   * auto-focus on open, and focus return to the trigger on keyboard close.
   */
  import type { AppMode } from '../types';

  interface SecondaryMode {
    mode: AppMode;
    label: string;
  }

  interface Props {
    secondaryModes?: SecondaryMode[];
    activeMode?: AppMode;
    onSelectMode?: (mode: AppMode) => void;
    onOpenNetworkHealth?: () => void;
    onSubmitFeedback?: () => void;
    onShowAbout?: () => void;
    onOpenDocs?: () => void;
    /** ZEB-555: compact icon trigger for the collapsed (narrow-screen) rail. */
    compact?: boolean;
  }

  const {
    secondaryModes = [],
    activeMode,
    onSelectMode,
    onOpenNetworkHealth,
    onSubmitFeedback,
    onShowAbout,
    onOpenDocs,
    compact = false,
  }: Props = $props();

  let open = $state(false);
  let containerEl: HTMLDivElement | undefined;
  let buttonEl: HTMLButtonElement | undefined;

  function toggle() {
    open = !open;
  }

  /** Keyboard/item close: returns focus to the trigger so focus isn't stranded. */
  function close() {
    if (!open) return;
    open = false;
    buttonEl?.focus();
  }

  function run(cb: (() => void) | undefined) {
    close();
    cb?.();
  }

  // Click-outside / Escape / Tab / Arrow listeners — attached only while open.
  $effect(() => {
    if (!open) return;
    function onMouseDown(e: MouseEvent) {
      if (containerEl && !containerEl.contains(e.target as Node)) {
        open = false;
      }
    }
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') {
        close();
        return;
      }
      if (e.key === 'Tab') {
        // Let Tab move focus naturally; just dismiss the menu.
        open = false;
        return;
      }
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault();
        const items = containerEl?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]');
        if (!items || items.length === 0) return;
        const cur = Array.from(items).findIndex((el) => el === document.activeElement);
        const next =
          e.key === 'ArrowDown'
            ? cur === -1
              ? 0
              : (cur + 1) % items.length
            : cur === -1
              ? items.length - 1
              : (cur - 1 + items.length) % items.length;
        items[next]?.focus();
      }
    }
    document.addEventListener('mousedown', onMouseDown);
    window.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onMouseDown);
      window.removeEventListener('keydown', onKey);
    };
  });

  // Auto-focus the first menu item on open so keyboard users land inside.
  $effect(() => {
    if (open && containerEl) {
      queueMicrotask(() => {
        // The menu may have closed before this microtask runs (rapid
        // open→close); re-check so we never steal focus back into a dismissed
        // menu. (Qodo PR #334.)
        if (!open) return;
        containerEl?.querySelector<HTMLButtonElement>('[role="menuitem"]')?.focus();
      });
    }
  });
</script>

<div class="more-container" bind:this={containerEl}>
  <button
    type="button"
    class:nav-action-btn={!compact}
    class:more-button={!compact}
    class:more-icon-button={compact}
    data-testid="more-menu-button"
    aria-label="More"
    aria-haspopup="menu"
    aria-expanded={open}
    aria-controls="more-menu-list"
    bind:this={buttonEl}
    onclick={toggle}
  >
    {#if compact}
      <span aria-hidden="true">⋯</span>
    {:else}
      <span>More</span>
      <span class="more-caret" aria-hidden="true">▾</span>
    {/if}
  </button>
  {#if open}
    <div id="more-menu-list" class="more-dropdown" data-testid="more-menu" role="menu">
      {#if secondaryModes.length > 0}
        <p class="menu-section" role="presentation">Go to</p>
        {#each secondaryModes as m (m.mode)}
          <button
            type="button"
            role="menuitem"
            class:active={activeMode === m.mode}
            aria-current={activeMode === m.mode ? 'true' : undefined}
            onclick={() => run(() => onSelectMode?.(m.mode))}
          >
            {m.label}
          </button>
        {/each}
      {/if}
      <p class="menu-section" role="presentation">Help</p>
      <button type="button" role="menuitem" data-testid="more-network-health" onclick={() => run(onOpenNetworkHealth)}>
        Network Health
      </button>
      <button type="button" role="menuitem" data-testid="more-feedback" onclick={() => run(onSubmitFeedback)}>
        Submit Feedback
      </button>
      <button type="button" role="menuitem" data-testid="more-about" onclick={() => run(onShowAbout)}>
        About
      </button>
      <button type="button" role="menuitem" data-testid="more-docs" onclick={() => run(onOpenDocs)}>
        Documentation
      </button>
    </div>
  {/if}
</div>

<style>
  .more-container {
    position: relative;
  }
  /* ZEB-769: the expanded-rail variant received no visual treatment and fell
     back to the user-agent default (13.33px / 0 padding / 2px outset black
     border), beside siblings styled at 10.5px / 4px 6px / no border.

     The button DOES carry NavPanel's `.nav-action-btn` class — the class is
     passed down and applied. What fails is the selector: Svelte scopes
     `.nav-action-btn` to NavPanel's component hash, and this element carries
     MoreMenu's, so the rule never matches. Adding the class again would be a
     no-op; the declarations have to live here, where the scope matches.

     Values mirror `.nav-action-btn` (NavPanel.svelte) deliberately. The
     compact sibling `.more-icon-button` below was always styled — only the
     expanded variant was missed. */
  .more-button {
    display: flex;
    align-items: center;
    justify-content: space-between;
    width: 100%;
    padding: 6px 10px;
    border: none;
    border-radius: 4px;
    background: var(--bg-tertiary);
    color: var(--text-muted);
    font-size: 13px;
    cursor: pointer;
  }
  .more-button:hover {
    background: var(--accent);
    color: var(--on-accent);
  }
  .more-icon-button {
    width: 40px;
    height: 40px;
    border: none;
    border-radius: 8px;
    background: var(--bg-tertiary);
    color: var(--text-primary);
    font-size: 16px;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .more-icon-button:hover {
    background: var(--accent);
    color: var(--on-accent);
  }
  .more-caret {
    font-size: 0.7rem;
    margin-left: 4px;
  }
  .more-dropdown {
    position: absolute;
    bottom: calc(100% + 4px);
    left: 0;
    min-width: 180px;
    background: var(--bg-secondary);
    border: 1px solid var(--border);
    border-radius: 4px;
    z-index: 100;
    display: flex;
    flex-direction: column;
    padding: 4px 0;
    box-shadow: 0 4px 12px var(--shadow-mid);
  }
  .menu-section {
    margin: 0;
    padding: 6px 12px 2px;
    color: var(--text-muted);
    font-size: 0.7rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .more-dropdown button {
    padding: 8px 12px;
    border: none;
    background: transparent;
    color: var(--text-primary);
    text-align: left;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .more-dropdown button:hover {
    background: var(--bg-tertiary);
  }
  .more-dropdown button.active {
    color: var(--accent);
    font-weight: 600;
  }
  .more-dropdown button:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
</style>
