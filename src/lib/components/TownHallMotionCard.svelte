<script lang="ts">
  /**
   * ZEB-612 S5 — quorum-aware "Call this to a motion" card (spec §6).
   *
   * idle → title input. On call: present ≥ quorum → create a 300 s Tier-1
   * approval poll on this channel and render it live in-card (embedded
   * PollMessage — plan Pin 3: the canonical Tier-1 renderer already carries
   * the vote buttons + tally fills, and its ballot logic absorbed six review
   * rounds on PR #130). Below quorum → the drawn DRAFT card (TallyBar quorum
   * bar, --gov-clay) whose terminal action opens the STANDARD async proposal
   * (Tier-2 conviction — plan Pin 2: no 48-hour primitive exists, copy
   * reworded accordingly).
   *
   * The motion title rides in the option labels (`Aye — T` / `Nay — T`) —
   * Tier-1 polls have no title field and the chat fanout body is only
   * magic + poll_id (plan Pin 1). TITLE_MAX 70 keeps labels under the
   * backend's MAX_OPTION_LABEL_LEN = 80.
   *
   * Cross-client convergence: on mount (and on voting-poll-created) the card
   * adopts an existing Open Tier-1 poll targeting this channelId, newest
   * first, so every client in the room sees the same live motion.
   */
  import { untrack } from 'svelte';
  import type { VotingAdapter } from '../voting-adapter';
  import type { PollMeta } from '../types/voting';
  import PollMessage from './PollMessage.svelte';
  import TallyBar from './governance/TallyBar.svelte';
  import CountChip from './governance/CountChip.svelte';

  let {
    communityId,
    channelId,
    adapter,
    presentCount,
    quorum,
    canAct = true,
    onOpenProposals,
  }: {
    communityId: string;
    channelId: string;
    /** Optional: absent (pre-connect) renders a quiet unavailable note. */
    adapter?: VotingAdapter;
    /** Distinct owners in the live roster (spec §5's honest "present"). */
    presentCount: number;
    /** adminQuorum, or null while governance hasn't loaded (no quorum gate). */
    quorum: number | null;
    /** Viewer is connected to the room (motions are called from inside). */
    canAct?: boolean;
    onOpenProposals?: () => void;
  } = $props();

  const TITLE_MAX = 70;

  type Phase =
    | { kind: 'idle' }
    | { kind: 'live'; meta: PollMeta }
    | { kind: 'draft'; title: string }
    | { kind: 'proposed' };
  let phase = $state<Phase>({ kind: 'idle' });
  let title = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);

  const bytesToHex = (b: number[]) => b.map((x) => x.toString(16).padStart(2, '0')).join('');

  // Adopt an existing open motion poll on this channel so all clients in the
  // room converge on the same card (a peer's create only reaches us as the
  // poll-kind chat message + this listActivePolls/pollCreated pair).
  $effect(() => {
    if (!adapter) return;
    const a = adapter;
    const cid = communityId;
    const chid = channelId;
    let cancelled = false;
    void (async () => {
      try {
        const polls = await a.listActivePolls(cid);
        if (cancelled) return;
        const open = polls
          .filter(
            (p) =>
              p.tier === 1 &&
              p.lifecycle === 'Open' &&
              p.channel_id != null &&
              bytesToHex(p.channel_id) === chid,
          )
          .sort((x, y) => y.created_at.w - x.created_at.w || y.created_at.l - x.created_at.l);
        if (open.length > 0 && untrack(() => phase).kind === 'idle') {
          phase = { kind: 'live', meta: open[0] };
        }
      } catch {
        // Non-fatal: card stays idle; the backchannel still renders polls.
      }
    })();
    const unsub = a.subscribePollCreated((p) => {
      if (cancelled || p.channelId !== chid) return;
      void (async () => {
        try {
          const st = await a.getPoll(p.pollId);
          if (cancelled) return;
          if (untrack(() => phase).kind !== 'live') phase = { kind: 'live', meta: st.meta };
        } catch {
          // Poll vanished between event and fetch: ignore.
        }
      })();
    });
    return () => {
      cancelled = true;
      unsub();
    };
  });

  async function callMotion() {
    const t = title.trim();
    if (!t || busy || !adapter || !canAct) return;
    error = null;
    // Below quorum: no backend action yet — show the drawn DRAFT card whose
    // terminal action opens the async proposal. quorum === null (governance
    // not loaded) imposes no gate rather than dead-ending the card.
    if (quorum !== null && presentCount < quorum) {
      phase = { kind: 'draft', title: t };
      return;
    }
    busy = true;
    try {
      const pollId = await adapter.createTier1Poll({
        communityId,
        channelId,
        options: [`Aye — ${t}`, `Nay — ${t}`],
        windowSeconds: 300,
        minPower: 0,
        quorum: quorum ?? undefined,
      });
      const st = await adapter.getPoll(pollId);
      phase = { kind: 'live', meta: st.meta };
      title = '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function openAsyncProposal() {
    if (phase.kind !== 'draft' || busy || !adapter) return;
    const t = phase.title;
    busy = true;
    error = null;
    try {
      await adapter.createTier2Proposal({ communityId, channelId, proposalText: t });
      phase = { kind: 'proposed' };
      title = '';
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  let quorumPct = $derived(quorum && quorum > 0 ? (presentCount / quorum) * 100 : 0);
</script>

<section class="motion-card" aria-label="Motion">
  {#if !adapter}
    <p class="motion-unavailable">Motions need a live governance connection.</p>
  {:else if phase.kind === 'live'}
    <div class="motion-live">
      <div class="motion-meta-row">
        <span class="live-dot" aria-hidden="true"></span>
        <span class="motion-meta" data-testid="motion-live-meta">live · {presentCount} present</span>
        {#if quorum !== null}
          <CountChip label="Quorum" value={`${presentCount} / ${quorum}`} tone="clay" />
        {/if}
      </div>
      <PollMessage pollId={phase.meta.poll_id} meta={phase.meta} {adapter} />
    </div>
  {:else if phase.kind === 'draft'}
    <div class="motion-draft">
      <div class="motion-meta-row">
        <span class="draft-badge" data-testid="motion-draft-badge">DRAFT</span>
        <span class="motion-meta">live · {presentCount} present</span>
      </div>
      <p class="motion-title-line">⚖ {phase.title}</p>
      <p class="motion-provenance">Moved by you · just now</p>
      <div class="motion-quorum-row">
        <TallyBar segments={[{ pct: quorumPct, token: '--gov-clay' }]} height={8} label="Quorum progress" />
        <span class="motion-quorum-count" data-testid="motion-quorum">{presentCount} / {quorum}</span>
      </div>
      <p class="motion-draft-copy">
        Not enough present for a live vote — open it as an async proposal instead.
      </p>
      <div class="motion-actions">
        <button
          type="button"
          class="motion-btn primary"
          data-testid="motion-open-async"
          disabled={busy}
          onclick={openAsyncProposal}
        >Open as async proposal</button>
        <button
          type="button"
          class="motion-btn"
          onclick={() => {
            phase = { kind: 'idle' };
          }}
        >Back</button>
      </div>
    </div>
  {:else if phase.kind === 'proposed'}
    <div class="motion-proposed" role="status">
      <p>Opened as an async proposal.</p>
      <button
        type="button"
        class="motion-btn"
        data-testid="motion-view-proposals"
        onclick={() => onOpenProposals?.()}
      >View proposals →</button>
    </div>
  {:else}
    <div class="motion-idle">
      <h4 class="motion-heading">⚖ Call this to a motion</h4>
      <input
        type="text"
        maxlength={TITLE_MAX}
        placeholder="Motion title…"
        aria-label="Motion title"
        bind:value={title}
      />
      <button
        type="button"
        class="motion-btn primary"
        data-testid="motion-call"
        disabled={!title.trim() || busy || !canAct}
        onclick={callMotion}
      >Call to a motion</button>
    </div>
  {/if}
  {#if error}
    <p class="motion-error" role="alert">{error}</p>
  {/if}
</section>

<style>
  .motion-card {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--bg-secondary);
  }
  .motion-unavailable {
    margin: 0;
    font-size: 0.8rem;
    color: var(--text-muted);
  }
  .motion-idle,
  .motion-live,
  .motion-draft,
  .motion-proposed {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .motion-heading {
    margin: 0;
    font-size: 0.85rem;
    color: var(--text-primary);
  }
  .motion-idle input {
    border: 1px solid var(--border);
    background: var(--bg-primary);
    color: var(--text-primary);
    border-radius: 5px;
    padding: 6px 10px;
    font: inherit;
    font-size: 0.85rem;
  }
  .motion-btn {
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    padding: 6px 12px;
    border-radius: 5px;
    font-size: 0.82rem;
    cursor: pointer;
    align-self: flex-start;
  }
  .motion-btn:hover:not(:disabled) {
    background: var(--bg-tertiary);
    color: var(--text-primary);
  }
  .motion-btn.primary {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--on-accent);
  }
  .motion-btn.primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  .motion-btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .motion-meta-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .live-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent);
    flex-shrink: 0;
  }
  .motion-meta {
    font-family: var(--font-mono);
    font-size: 0.72rem;
    color: var(--text-secondary);
  }
  .draft-badge {
    font-size: 0.62rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    color: var(--gov-clay-deep);
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    border-radius: 4px;
    padding: 2px 6px;
  }
  .motion-title-line {
    margin: 0;
    font-size: 0.9rem;
    font-weight: 600;
    color: var(--text-primary);
  }
  .motion-provenance {
    margin: 0;
    font-size: 0.75rem;
    color: var(--text-muted);
  }
  .motion-quorum-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .motion-quorum-row :global(.tally-track) {
    flex: 1;
  }
  .motion-quorum-count {
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--gov-clay-deep);
    white-space: nowrap;
  }
  .motion-draft-copy {
    margin: 0;
    font-size: 0.8rem;
    color: var(--text-secondary);
  }
  .motion-actions {
    display: flex;
    gap: 6px;
  }
  .motion-proposed p {
    margin: 0;
    font-size: 0.85rem;
    color: var(--text-primary);
  }
  .motion-error {
    margin: 0;
    color: var(--danger);
    font-size: 0.8rem;
  }
</style>
