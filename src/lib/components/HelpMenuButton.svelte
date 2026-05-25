<script lang="ts">
  /**
   * ZEB-331 — Top-right (?) help/feedback button + dropdown (spec §4.3).
   *
   * Mounted in App.svelte at fixed position top-right.
   * Dropdown items in spec order: Submit Feedback / Network Health /
   * About / Documentation. Click-outside, Escape, and item-click all
   * close the dropdown.
   */

  interface Props {
    onSubmitFeedback: () => void;
    onShowAbout: () => void;
    onOpenNetworkHealth: () => void;
    onOpenDocs: () => void;
  }
  const { onSubmitFeedback, onShowAbout, onOpenNetworkHealth, onOpenDocs }: Props =
    $props();

  let dropdownOpen = $state(false);
  let containerEl: HTMLDivElement | undefined;

  function toggleDropdown() {
    dropdownOpen = !dropdownOpen;
  }

  function close() {
    dropdownOpen = false;
  }

  function handleFeedback() {
    close();
    onSubmitFeedback();
  }
  function handleNetwork() {
    close();
    onOpenNetworkHealth();
  }
  function handleAbout() {
    close();
    onShowAbout();
  }
  function handleDocs() {
    close();
    onOpenDocs();
  }

  // Click-outside / Escape listeners — attached only while dropdown open
  // to avoid pollution and to allow other (?)-like buttons to coexist.
  $effect(() => {
    if (!dropdownOpen) return;
    function onMouseDown(e: MouseEvent) {
      if (containerEl && !containerEl.contains(e.target as Node)) {
        close();
      }
    }
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') close();
    }
    document.addEventListener('mousedown', onMouseDown);
    window.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onMouseDown);
      window.removeEventListener('keydown', onKey);
    };
  });
</script>

<div class="help-container" bind:this={containerEl}>
  <button
    type="button"
    class="help-button"
    data-testid="help-menu-button"
    aria-label="Help and feedback"
    aria-haspopup="menu"
    aria-expanded={dropdownOpen}
    onclick={toggleDropdown}
  >
    ?
  </button>
  {#if dropdownOpen}
    <div
      class="help-dropdown"
      data-testid="help-menu-dropdown"
      role="menu"
    >
      <button
        type="button"
        role="menuitem"
        data-testid="help-menu-feedback"
        onclick={handleFeedback}
      >
        Submit Feedback
      </button>
      <button
        type="button"
        role="menuitem"
        data-testid="help-menu-network"
        onclick={handleNetwork}
      >
        Network Health
      </button>
      <button
        type="button"
        role="menuitem"
        data-testid="help-menu-about"
        onclick={handleAbout}
      >
        About
      </button>
      <button
        type="button"
        role="menuitem"
        data-testid="help-menu-docs"
        onclick={handleDocs}
      >
        Documentation
      </button>
    </div>
  {/if}
</div>

<style>
  .help-container {
    position: relative;
    display: inline-block;
  }
  .help-button {
    width: 32px;
    height: 32px;
    border-radius: 50%;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    font-size: 1rem;
    font-weight: bold;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .help-button:hover {
    background: var(--bg-secondary, #2a2a2a);
  }
  .help-button:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 2px;
  }
  .help-dropdown {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    background: var(--bg-secondary, #2a2a2a);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    min-width: 180px;
    z-index: 100;
    display: flex;
    flex-direction: column;
    padding: 4px 0;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
  }
  .help-dropdown button {
    padding: 8px 12px;
    border: none;
    background: transparent;
    color: var(--text-primary, #fff);
    text-align: left;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .help-dropdown button:hover {
    background: var(--bg-tertiary, #1f1f1f);
  }
  .help-dropdown button:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }
</style>
