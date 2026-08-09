<script lang="ts">
  import { untrack, onMount, onDestroy } from 'svelte';
  import Modal from './Modal.svelte';
  import {
    mapRedeemInviteError,
    redeemInviteCopy,
    toRedeemInviteError,
    type RedeemInviteError,
  } from '../redeem-invite-errors';
  import {
    redeemInviteIroh,
    onResolutionProgress,
    previewInvite,
  } from '../connectivity-adapter';
  import type { RedemptionStage, InvitePreviewDto } from '../types/connectivity';

  let {
    onSubmit,
    onCancel,
    error = null,
    pending = false,
    initialUrl = '',
    joinedDismissDelayMs = 1200,
  }: {
    onSubmit: (url: string) => void;
    onCancel: () => void;
    error?: RedeemInviteError | null;
    pending?: boolean;
    initialUrl?: string;
    /**
     * ZEB-325 Phase 2c: how long to keep the dialog open displaying
     * "Joined ✓" before auto-dismissing via `onCancel`. Tests override
     * this to 0 to avoid timer flake.
     */
    joinedDismissDelayMs?: number;
  } = $props();

  let url = $state(untrack(() => initialUrl));
  let canSubmit = $derived(
    url.trim().startsWith('harmony://invite/') && !pending && !irohPending && !previewExpired,
  );
  let mapped = $derived(error ? mapRedeemInviteError(error) : null);
  const titleId = `redeem-invite-title-${Math.random().toString(36).slice(2)}`;

  // ── Iroh / pkarr path state ──────────────────────────────────────────────
  let irohPending = $state(false);
  let irohStage = $state<RedemptionStage | null>(null);
  let irohError = $state<string | null>(null);
  // ZEB-885: structured diagnostics (code + raw message) for an iroh HARD-error
  // catch, surfaced in a disclosure like the LAN banner. Null for the Ok-side
  // status branches (plain-language copy, no code to show).
  let irohErrorDetail = $state<{ code: string; raw: string } | null>(null);
  let showFallbackButton = $state(false);
  /**
   * ZEB-325 Phase 2c: handle for the post-`joined` dismiss timer so it can
   * be cancelled if the dialog is torn down before it fires.
   */
  let joinedDismissTimer: ReturnType<typeof setTimeout> | null = null;

  const STAGE_LABELS: Record<RedemptionStage, string> = {
    resolving: 'Looking up inviter…',
    connecting: 'Connecting…',
    sending: 'Sending join request…',
    awaiting_countersig: 'Waiting for confirmation…',
    joined: 'Joined ✓',
  };

  let stopProgressListener: (() => void) | null = null;

  // ── ZEB-650 slice 3: debounced pure-local invite preview ────────────────
  // `preview_invite` decodes + verifies + expiry-evaluates the pasted URL
  // locally (no join, no network), so firing it on keystroke settle is safe.
  // The TOCTOU rule doesn't apply: preview and redeem both derive
  // deterministically from the same immutable URL string.
  let preview = $state<InvitePreviewDto | null>(null);
  let previewInvalid = $state(false);
  let previewExpired = $derived(preview?.expired === true);
  let previewTimer: ReturnType<typeof setTimeout> | null = null;
  /** Monotonic guard: a resolution whose seq no longer matches is stale
   *  (URL changed or dialog torn down mid-flight) and must be discarded. */
  let previewSeq = 0;
  const PREVIEW_DEBOUNCE_MS = 300;

  $effect(() => {
    const trimmed = url.trim();
    if (previewTimer !== null) clearTimeout(previewTimer);
    previewTimer = null;
    previewSeq += 1;
    const seq = previewSeq;
    if (!trimmed.startsWith('harmony://invite/')) {
      preview = null;
      previewInvalid = false;
      return;
    }
    // Invalidate the previous URL's preview immediately: the old card (and
    // its expired/verified verdicts) must not survive into the new URL's
    // debounce window (Qodo/CodeRabbit PR #438). The preview stays advisory —
    // submit is not gated while the new verdict is pending; expiry is
    // enforced authoritatively by the join handshake itself.
    preview = null;
    previewInvalid = false;
    previewTimer = setTimeout(() => {
      previewTimer = null;
      void (async () => {
        try {
          const dto = await previewInvite(trimmed);
          if (seq !== previewSeq) return;
          preview = dto ?? null;
          previewInvalid = false;
        } catch {
          if (seq !== previewSeq) return;
          preview = null;
          previewInvalid = true;
        }
      })();
    }, PREVIEW_DEBOUNCE_MS);
  });

  onMount(() => {
    stopProgressListener = onResolutionProgress((ev) => {
      irohStage = ev.stage;
    });
  });

  onDestroy(() => {
    stopProgressListener?.();
    if (joinedDismissTimer !== null) {
      clearTimeout(joinedDismissTimer);
      joinedDismissTimer = null;
    }
    if (previewTimer !== null) {
      clearTimeout(previewTimer);
      previewTimer = null;
    }
    previewSeq += 1; // discard any in-flight preview resolution
  });

  // ZEB-885: codes for which the Reticulum LAN fallback could actually help.
  // Local/internal failures (engine_insert_failed, node_not_ready,
  // generation_changed, internal, join_failed, and invite-defect codes) rerun
  // the same machinery / carry the same bad invite, so offering the fallback
  // there is misleading — suppress it.
  const IROH_LAN_RECOVERABLE_CODES = new Set([
    'inviter_unreachable',
    'relays_warming_up',
    'missing_admin_identity_pub',
    'unknown',
  ]);

  async function handleIrohRedeem() {
    const trimmed = url.trim();
    if (!trimmed.startsWith('harmony://invite/') || irohPending || pending || previewExpired)
      return;
    irohPending = true;
    irohStage = 'resolving';
    irohError = null;
    irohErrorDetail = null;
    showFallbackButton = false;
    let suppressFinally = false;
    try {
      const outcome = await redeemInviteIroh(trimmed);
      if (outcome.status === 'joined') {
        // ZEB-325 Phase 2c: full handshake completed (pkarr resolve →
        // iroh connect → PendingJoin → counter-signed Join applied). The
        // backend has already mutated community state and (per PR #159
        // R3-1) emits the same `nav-updated` event the Reticulum
        // `redeem_invite` IPC emits, so the join is visible in the
        // sidebar without any frontend-side refresh. Show the
        // "Joined ✓" success label briefly, then dismiss the dialog
        // via `onCancel` — same end state as the Reticulum path,
        // which closes via the parent's `onSubmit` handler after
        // `communityService.redeemInvite` resolves (see App.svelte
        // ~line 1762).
        //
        // Keep `irohPending` true across the display window so the
        // progress row (`{#if irohPending && irohStage}`) stays visible
        // and Cancel stays disabled; the timer clears both before firing
        // `onCancel`.
        irohStage = 'joined';
        suppressFinally = true;
        joinedDismissTimer = setTimeout(() => {
          joinedDismissTimer = null;
          irohPending = false;
          onCancel();
        }, joinedDismissDelayMs);
        return;
      }
      if (outcome.status === 'pkarr_resolved_no_handshake') {
        // Defensive fallback: Phase 2c should now always return `joined`
        // on the pkarr-resolved branch, but if `redeem_invite_inner` fails
        // after a successful pkarr seed the backend still surfaces this
        // status. Surface the LAN fallback so the user has a recovery path.
        irohStage = null;
        irohError =
          'Found the inviter on the network, but the full join handshake did not complete. ' +
          'Use the local network fallback to join now.';
        showFallbackButton = true;
      } else if (outcome.status === 'inviter_unreachable') {
        irohStage = null;
        irohError =
          "Couldn't reach the inviter through the network right now. They may be offline; try again later.";
        showFallbackButton = true;
      } else if (outcome.status === 'join_failed') {
        // ZEB-325 PR #159 R1: the inviter WAS reached and we received a
        // valid JoinCountersign, but the local insert/commit failed
        // (engine insert error, fence violation, commit rollback, etc.).
        // The Reticulum LAN fallback won't help — it runs the same
        // `redeem_invite_inner` against the same local engine state — so
        // suppress the fallback button. The community_id is included so
        // bug reports can correlate. Suggest restart-and-retry as the
        // first remediation; persistent failure is a bug.
        irohStage = null;
        const communityHint = outcome.communityId
          ? ` (community ${outcome.communityId.slice(0, 12)}…)`
          : '';
        irohError =
          'Reached the inviter but couldn’t complete the join locally' +
          communityHint +
          '. Try restarting the app and redeeming again; if it keeps failing, please report it as a bug.';
        showFallbackButton = false;
      } else {
        // missing_admin_identity_pub, fallback_reticulum, or other — hand off to LAN path
        irohStage = null;
        showFallbackButton = true;
      }
    } catch (e) {
      irohStage = null;
      // ZEB-885: the iroh command rejects with a structured { code, message }.
      // Route copy through the shared table (summary + actionable hint) instead
      // of dumping the raw backend message, and only offer the LAN fallback when
      // it could actually help (see IROH_LAN_RECOVERABLE_CODES).
      const err = toRedeemInviteError(e);
      const copy = redeemInviteCopy(err.code);
      irohError = copy.hint ? `${copy.summary} ${copy.hint}` : copy.summary;
      // Keep the code + raw message for the diagnostic disclosure / bug reports.
      irohErrorDetail = { code: err.code, raw: err.message };
      showFallbackButton = IROH_LAN_RECOVERABLE_CODES.has(err.code);
    } finally {
      // The `joined` branch defers `irohPending = false` to the dismiss
      // timer so the "Joined ✓" label stays visible across the display
      // window. Every other branch clears it here.
      if (!suppressFinally) {
        irohPending = false;
      }
    }
  }

  function handleSubmit() {
    if (!canSubmit) return;
    // Clear the iroh-path banner before handing off to the LAN fallback: a
    // LAN-redeem failure is surfaced by the parent via the `error` prop (the
    // `mapped` banner), which would otherwise stack on top of the still-visible
    // iroh banner — two error banners for one redeem attempt (finding 12).
    irohError = null;
    irohErrorDetail = null;
    onSubmit(url.trim());
  }
</script>

<Modal {onCancel} canCancel={!pending && !irohPending} ariaLabelledby={titleId}>
  <h3 class="dialog-title" id={titleId}>Redeem invite link</h3>

  {#if mapped}
    <div class="error-banner">
      <p class="summary">{mapped.summary}</p>
      {#if mapped.hint}<p class="hint">{mapped.hint}</p>{/if}
      <details>
        <summary>Show details</summary>
        <div class="diagnostic">
          <div>Code: <code>{mapped.code}</code></div>
          <div>Raw error: <code>{mapped.raw}</code></div>
        </div>
      </details>
    </div>
  {/if}

  {#if irohError}
    <div class="error-banner" data-testid="iroh-error-banner">
      <p class="summary">{irohError}</p>
      {#if irohErrorDetail}
        <details>
          <summary>Show details</summary>
          <div class="diagnostic">
            <div>Code: <code>{irohErrorDetail.code}</code></div>
            <div>Raw error: <code>{irohErrorDetail.raw}</code></div>
          </div>
        </details>
      {/if}
    </div>
  {/if}

  <textarea
    placeholder="harmony://invite/v1?..."
    bind:value={url}
    class="url-input"
    rows="3"
    disabled={pending || irohPending}
    aria-label="Invite link URL"
  ></textarea>

  {#if irohPending && irohStage}
    <div class="pending-row" data-testid="iroh-progress">
      <div class="spinner" role="status" aria-label="Connecting via network"></div>
      <span data-testid="iroh-stage-label">{STAGE_LABELS[irohStage]}</span>
    </div>
  {/if}

  {#if pending}
    <div class="pending-row">
      <div class="spinner" role="status" aria-label="Verifying invite"></div>
      <span>Verifying invite...</span>
    </div>
  {/if}

  {#if preview !== null}
    <!-- ZEB-650 slice 3: honest preview — name + inviter authorization only.
         Never member/channel counts: the token signature doesn't commit to
         the payload's snapshot (ZEB-610 §0.4). -->
    <div class="preview-card" data-testid="redeem-preview-card">
      <div class="preview-headline">
        <span class="preview-name">{preview.communityName}</span>
        {#if preview.isInviteOnly}
          <span class="invite-only-chip" data-testid="preview-invite-only-chip">🔒 Invite-only</span>
        {/if}
      </div>
      {#if preview.inviterVerified}
        <div class="preview-verified" data-testid="preview-verified">
          ✓ invite signature verified{#if preview.inviterDisplayName ?? preview.inviterFingerprint}&nbsp;— from {preview.inviterDisplayName ?? preview.inviterFingerprint}{/if}
        </div>
      {:else}
        <div class="preview-unverified" data-testid="preview-unverified">signature not verifiable</div>
      {/if}
      {#if preview.expired}
        <div class="preview-expired" data-testid="preview-expired">This invite has expired.</div>
      {/if}
    </div>
  {:else if previewInvalid}
    <div class="preview-card" data-testid="redeem-preview-invalid">
      <span class="preview-unverified">This invite link looks invalid</span>
    </div>
  {/if}

  <div class="dialog-actions">
    <button class="cancel-btn" onclick={onCancel} disabled={pending || irohPending}>Cancel</button>
    {#if showFallbackButton}
      <button
        class="fallback-btn"
        onclick={handleSubmit}
        disabled={!url.trim().startsWith('harmony://invite/') || pending || previewExpired}
        data-testid="fallback-lan-btn"
      >
        Try via local network
      </button>
    {:else}
      <button
        class="iroh-btn"
        onclick={handleIrohRedeem}
        disabled={!url.trim().startsWith('harmony://invite/') ||
          pending ||
          irohPending ||
          previewExpired}
        data-testid="iroh-redeem-btn"
      >
        Redeem
      </button>
    {/if}
  </div>
</Modal>

<style>
  .dialog-title {
    color: var(--text-primary);
    font-family: var(--font-display);
    font-size: 1.35rem;
    font-weight: 600;
    letter-spacing: -0.01em;
    margin: 0 0 16px;
  }
  .url-input {
    width: 100%;
    padding: 8px 12px;
    background: var(--bg-tertiary);
    border: 1px solid var(--border-default);
    border-radius: 6px;
    color: var(--text-primary);
    font-family: var(--font-mono);
    font-size: 0.75rem;
    margin-bottom: 12px;
    box-sizing: border-box;
    resize: vertical;
    transition: border-color 0.15s ease, box-shadow 0.15s ease;
  }
  .url-input:focus {
    /* Keep the focus ring visible under forced-colors / High Contrast,
       where box-shadow is dropped (Qodo #412; see ForkConfirmDialog #411). */
    outline: 2px solid transparent;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent);
  }
  .error-banner {
    background: var(--bg-tertiary);
    border: 1px solid var(--danger-muted);
    padding: 10px 12px;
    border-radius: 6px;
    margin-bottom: 12px;
  }
  .error-banner .summary {
    margin: 0 0 4px 0;
    color: var(--danger-muted);
    font-size: 0.85rem;
  }
  .error-banner .hint {
    margin: 0 0 8px 0;
    color: var(--text-secondary);
    font-size: 0.8rem;
  }
  .error-banner details {
    font-size: 0.75rem;
    color: var(--text-secondary);
  }
  .error-banner details summary {
    cursor: pointer;
  }
  .diagnostic {
    padding: 8px 0 0 0;
    font-family: var(--font-mono);
  }
  .diagnostic code {
    color: var(--text-primary);
  }
  .pending-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 12px;
    color: var(--text-secondary);
    font-size: 0.8rem;
  }
  .spinner {
    width: 12px;
    height: 12px;
    border: 2px solid var(--accent);
    border-top-color: transparent;
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .cancel-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border-default);
    padding: 8px 16px;
    border-radius: 6px;
    cursor: pointer;
    font-family: var(--font-ui);
    font-size: 0.875rem;
  }
  .cancel-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .cancel-btn:focus-visible,
  .iroh-btn:focus-visible,
  .fallback-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .iroh-btn {
    background: var(--accent);
    color: var(--on-accent);
    border: 1px solid var(--accent);
    padding: 8px 16px;
    border-radius: 6px;
    cursor: pointer;
    font-family: var(--font-ui);
    font-size: 0.875rem;
  }
  .iroh-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .fallback-btn {
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    border: 1px solid var(--border-default);
    padding: 8px 16px;
    border-radius: 6px;
    cursor: pointer;
    font-family: var(--font-ui);
    font-size: 0.875rem;
  }
  .fallback-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  /* ZEB-650 slice 3: preview card — .error-banner anatomy, neutral border. */
  .preview-card {
    background: var(--bg-tertiary);
    border: 1px solid var(--border-default);
    border-radius: 6px;
    padding: 10px 12px;
    margin-bottom: 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .preview-headline {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .preview-name {
    color: var(--text-primary);
    font-weight: 600;
    font-size: 0.95rem;
  }
  .invite-only-chip {
    padding: 2px 10px;
    border-radius: 20px;
    font-size: 11px;
    font-weight: 600;
    color: var(--text-secondary);
    background: color-mix(in srgb, currentColor 16%, transparent);
    white-space: nowrap;
  }
  .preview-verified {
    color: var(--success);
    font-family: var(--font-mono);
    font-size: 0.75rem;
  }
  .preview-unverified {
    color: var(--text-secondary);
    font-size: 0.8rem;
  }
  .preview-expired {
    color: var(--danger-muted);
    font-size: 0.8rem;
  }
</style>
