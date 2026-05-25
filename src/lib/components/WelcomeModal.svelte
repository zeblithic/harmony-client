<script lang="ts">
  /**
   * ZEB-331 — First-run welcome modal (spec §4.1).
   *
   * Fires when `start_node` returns freshlyCreated=true (Flow 1).
   * Suppressed when a harmony:// deep-link is delivered during boot
   * (Flow 5 — handled by parent setting open=false in the deep-link
   * receiver).
   *
   * Uses the existing extractHarmonyInviteUrl validator from
   * deep-link-router so we don't drift from the canonical URL shape.
   */
  import { onMount } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';
  import { extractHarmonyInviteUrl } from '../deep-link-router';

  interface Props {
    open: boolean;
    onDismiss: () => void;
    onJoinWithInvite: (url: string) => void;
  }
  const { open, onDismiss, onJoinWithInvite }: Props = $props();

  let appVersion = $state<string>('unknown');
  let inviteUrl = $state('');
  let inviteError = $state<string | null>(null);

  onMount(async () => {
    try {
      appVersion = await getVersion();
    } catch (e) {
      // Outside Tauri (dev/browser) — leave appVersion as 'unknown'.
      const msg = e instanceof Error ? e.message : String(e);
      console.debug('[zeb-331] WelcomeModal getVersion failed:', msg);
    }
  });

  function handleJoin() {
    const trimmed = inviteUrl.trim();
    if (trimmed.length === 0) {
      inviteError = 'Paste an invite URL or click Skip for now.';
      return;
    }
    const validated = extractHarmonyInviteUrl([trimmed]);
    if (validated === null) {
      inviteError = "That doesn't look like a harmony:// invite.";
      return;
    }
    inviteError = null;
    onJoinWithInvite(validated);
  }

  function handleBackdropClick(e: MouseEvent) {
    // Only fire when click landed on the backdrop, not on the modal body.
    if (e.target === e.currentTarget) {
      onDismiss();
    }
  }

  // Esc key listener — attached/removed based on `open`.
  $effect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') onDismiss();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
</script>

{#if open}
  <div
    class="modal-backdrop"
    data-testid="welcome-modal-backdrop"
    onclick={handleBackdropClick}
    role="presentation"
  >
    <div
      class="modal-content"
      data-testid="welcome-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="welcome-title"
    >
      <h2 id="welcome-title">Welcome to Harmony alpha</h2>

      <p>
        Harmony is a federated chat where communities are self-governing.
        You're testing v0.1.0-alpha, so expect rough edges — please report
        issues via the <strong>(?)</strong> icon in the top-right.
      </p>

      <p>
        A device identity is ready to use. You can name yourself and
        customize your avatar in <strong>Settings → Profile</strong>
        whenever you like.
      </p>

      <div class="invite-section">
        <label for="welcome-invite-input">
          Have a <code>harmony://</code> invite?
        </label>
        <input
          id="welcome-invite-input"
          data-testid="welcome-invite-input"
          type="text"
          placeholder="harmony://invite/v1?..."
          bind:value={inviteUrl}
          oninput={() => { inviteError = null; }}
        />
        {#if inviteError}
          <p class="error" data-testid="welcome-invite-error">{inviteError}</p>
        {/if}
        <div class="actions">
          <button
            data-testid="welcome-join"
            class="primary"
            onclick={handleJoin}
          >
            Join now
          </button>
          <button data-testid="welcome-skip" onclick={onDismiss}>
            Skip for now
          </button>
        </div>
      </div>

      <footer>
        <span class="version" data-testid="welcome-version">v{appVersion}</span>
        <a
          data-testid="welcome-feedback-link"
          href="https://github.com/zeblithic/harmony-client/blob/main/docs/feedback.md"
          target="_blank"
          rel="noopener noreferrer"
        >
          How to submit feedback →
        </a>
      </footer>
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 520px;
    width: 90%;
  }
  .modal-content h2 {
    margin: 0 0 1rem;
    font-size: 1.25rem;
  }
  .modal-content p {
    margin: 0 0 1rem;
    line-height: 1.5;
  }
  .invite-section {
    margin: 1.5rem 0 1rem;
    padding: 1rem;
    background: var(--bg-tertiary, #1f1f1f);
    border-radius: 4px;
  }
  .invite-section label {
    display: block;
    margin-bottom: 0.5rem;
    font-size: 0.9rem;
  }
  .invite-section input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem;
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    font-family: monospace;
    font-size: 0.85rem;
    margin-bottom: 0.5rem;
  }
  .actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
  .actions button.primary {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
  }
  .error {
    color: crimson;
    font-size: 0.85rem;
    margin: 0 0 0.5rem;
  }
  footer {
    margin-top: 1rem;
    font-size: 0.85rem;
  }
  .version {
    display: inline-block;
    margin-right: 1rem;
    font-size: 0.85rem;
    color: var(--text-secondary, #aaa);
    opacity: 0.7;
  }
  footer a {
    color: var(--accent, #5865f2);
    text-decoration: none;
  }
  footer a:hover {
    text-decoration: underline;
  }
</style>
