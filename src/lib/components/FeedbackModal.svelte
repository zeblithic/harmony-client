<script lang="ts">
  /**
   * ZEB-331 — Submit Feedback modal (spec §4.2).
   *
   * Description textarea + optional "Attach network diagnostics" toggle
   * → opens browser to pre-filled GitHub new-issue URL via shell.open.
   *
   * Privacy invariant: diagnostics path goes through
   * network_health_export_payload(includeFullIds=false) — server-side
   * redactor from ZEB-329 R3. No new code path can leak full Ed25519 hex.
   *
   * Stale-response guard: `latestRequest` is a plain `let` (NOT $state)
   * to avoid effect_update_depth_exceeded. Matches the
   * DiagnosticExportModal pattern from ZEB-329 R1.
   */
  import { invoke } from '@tauri-apps/api/core';
  import { open as shellOpen } from '@tauri-apps/plugin-shell';
  import { collectEnvironment, buildGitHubIssueUrl } from '../onboarding-env';
  import type { FeedbackPayload } from '../types/onboarding';

  interface Props {
    open: boolean;
    onDismiss: () => void;
  }
  const { open, onDismiss }: Props = $props();

  const MIN_DESCRIPTION_LEN = 10;

  let description = $state('');
  let attachDiagnostics = $state(false);
  let submitting = $state(false);
  let diagnosticsPreview = $state<string | null>(null);
  let diagnosticsError = $state<string | null>(null);
  let toastMsg = $state<string | null>(null);

  // Plain `let` — NOT $state. This is internal control-flow bookkeeping
  // for the stale-response guard, not UI state. Wrapping in $state would
  // cause the $effect below to re-fire on every load() and produce
  // effect_update_depth_exceeded. See DiagnosticExportModal.svelte:43 for
  // the same pattern (ZEB-329 R1).
  let latestRequest = 0;

  let submitDisabled = $derived(description.length < MIN_DESCRIPTION_LEN || submitting);

  async function loadDiagnostics() {
    const requestId = ++latestRequest;
    diagnosticsPreview = null;
    diagnosticsError = null;
    try {
      const result = (await invoke('network_health_export_payload', {
        includeFullIds: false,
      })) as string;
      if (requestId !== latestRequest) return;
      diagnosticsPreview = result;
    } catch (e) {
      if (requestId !== latestRequest) return;
      diagnosticsError = 'Diagnostics unavailable — submit without?';
      // Underlying error captured for console only:
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[zeb-331] network_health_export_payload failed:', msg);
    }
  }

  // Fire load when the toggle flips ON. Bumping latestRequest in the
  // OFF branch ensures any in-flight ON request can't sneak its
  // result into the now-hidden preview pane.
  $effect(() => {
    if (attachDiagnostics) {
      void loadDiagnostics();
    } else {
      latestRequest++;
      diagnosticsPreview = null;
      diagnosticsError = null;
    }
  });

  async function handleSubmit() {
    if (submitDisabled) return;
    submitting = true;
    toastMsg = null;
    try {
      const env = await collectEnvironment();
      const payload: FeedbackPayload = {
        description,
        env,
        ...(attachDiagnostics && diagnosticsPreview
          ? { diagnostics: diagnosticsPreview }
          : {}),
      };
      const url = buildGitHubIssueUrl(payload);
      try {
        await shellOpen(url);
        onDismiss();
      } catch (e) {
        // shell.open failure → clipboard fallback
        const msg = e instanceof Error ? e.message : String(e);
        console.warn('[zeb-331] shell.open failed:', msg);
        try {
          await navigator.clipboard.writeText(url);
          toastMsg =
            "Couldn't open browser. URL copied to clipboard — paste it in your browser.";
        } catch (clipErr) {
          toastMsg = `Couldn't open browser or copy: ${
            clipErr instanceof Error ? clipErr.message : String(clipErr)
          }`;
        }
      }
    } finally {
      submitting = false;
    }
  }

  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) {
      onDismiss();
    }
  }

  // Spec §6.7: feedback drafts do NOT persist across modal dismissal.
  // Reset all form state when modal closes so reopen shows a clean form.
  // Gate on !submitting so an in-flight handleSubmit (whose awaits could
  // straddle a parent-driven close) can finish reading description /
  // diagnosticsPreview before they're wiped. The finally block in
  // handleSubmit clears `submitting`, which re-fires this effect and
  // performs the reset on the next tick.
  $effect(() => {
    if (!open && !submitting) {
      description = '';
      attachDiagnostics = false;
      diagnosticsPreview = null;
      diagnosticsError = null;
      toastMsg = null;
      latestRequest++; // invalidate any in-flight diagnostics request
    }
  });

  // Esc dismiss while open.
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
    data-testid="feedback-modal-backdrop"
    onclick={handleBackdropClick}
    role="presentation"
  >
    <div
      class="modal-content"
      data-testid="feedback-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="feedback-title"
    >
      <h2 id="feedback-title">Submit feedback</h2>

      <label class="field-label" for="feedback-description">
        Describe what happened, what you expected, and what you saw.
      </label>
      <textarea
        id="feedback-description"
        data-testid="feedback-description"
        rows="6"
        bind:value={description}
        placeholder="Steps to reproduce, expected behavior, actual behavior…"
      ></textarea>

      <label class="toggle-row">
        <input
          type="checkbox"
          data-testid="feedback-attach-toggle"
          bind:checked={attachDiagnostics}
        />
        Attach network diagnostics (redacted — no full identifiers)
      </label>

      {#if attachDiagnostics}
        {#if diagnosticsPreview !== null}
          <pre
            class="diagnostics-preview"
            data-testid="feedback-diagnostics-preview"
          >{diagnosticsPreview}</pre>
        {:else if diagnosticsError !== null}
          <p class="error" data-testid="feedback-diagnostics-error">
            {diagnosticsError}
          </p>
        {:else}
          <p class="loading">Loading diagnostics…</p>
        {/if}
      {/if}

      {#if toastMsg !== null}
        <p class="toast" data-testid="feedback-toast">{toastMsg}</p>
      {/if}

      <div class="actions">
        <button
          type="button"
          data-testid="feedback-cancel"
          onclick={onDismiss}
          disabled={submitting}
        >
          Cancel
        </button>
        <button
          type="button"
          class="primary"
          data-testid="feedback-submit"
          onclick={handleSubmit}
          disabled={submitDisabled}
        >
          {submitting ? 'Submitting…' : 'Submit'}
        </button>
      </div>
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
    max-width: 640px;
    width: 90%;
    max-height: 80vh;
    overflow-y: auto;
  }
  .modal-content h2 {
    margin: 0 0 1rem;
    font-size: 1.25rem;
  }
  .field-label {
    display: block;
    margin: 0 0 0.5rem;
    font-size: 0.9rem;
  }
  textarea {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem;
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    font-family: monospace;
    font-size: 0.85rem;
    resize: vertical;
  }
  .toggle-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 1rem 0;
    font-size: 0.9rem;
  }
  .diagnostics-preview {
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    padding: 0.75rem;
    border-radius: 4px;
    max-height: 240px;
    overflow-y: auto;
    white-space: pre-wrap;
    font-size: 0.75rem;
  }
  .error {
    color: crimson;
    margin: 0.5rem 0;
    font-size: 0.85rem;
  }
  .loading {
    font-size: 0.85rem;
    color: var(--text-secondary, #aaa);
    margin: 0.5rem 0;
  }
  .toast {
    background: var(--bg-tertiary, #1f1f1f);
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
    margin-top: 0.5rem;
    font-size: 0.85rem;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    margin-top: 1rem;
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
  .actions button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
