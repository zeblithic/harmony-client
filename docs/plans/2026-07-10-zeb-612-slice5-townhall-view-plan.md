# ZEB-612 Slice 5 — TownHallView Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `TownHallView.svelte` — voice fused with the Assembly — rendered by `CommunityView` for `kind === 'townhall'`: spotlight, in-room grid, speaker queue with invite-to-speak, quorum-aware motion card, backchannel chat, raise-hand + invited-banner self affordances (spec §6, final ZEB-612 slice).

**Architecture:** Frontend-only composition over shipped seams: S4's `VoiceSession` surface (`speakerQueue`, `handRaisedAt`, `invited`, `selfInvited`, `setHand`, `inviteToSpeak`), the ZEB-290/291 `VotingAdapter` (Tier-1 poll create/vote + Tier-2 proposal fallback), the ZEB-607 governance primitives (`TallyBar`, `CountChip`), and `ChannelMessageFeed` as the backchannel. Two new components: `TownHallMotionCard` (self-contained quorum-aware motion flow) and `TownHallView` (assembly). One additive prop on `ChannelMessageFeed` (`composerPlaceholder`).

**Tech Stack:** Svelte 5 runes, vitest + @testing-library/svelte, existing token system (`--gov-clay`, `--tally-track`, `--accent`).

## Global Constraints

- Zero new backend. Every rendered datum comes from a real seam (honesty rule, spec §0/§8).
- Copy verbatim from spec §6: *"Join the assembly — you'll join muted."*, *"No one has the floor."*, *"wants to speak ✋"*, *"Message the room…"*, *"You've been invited to speak — Unmute?"*, *"⚖ Call this to a motion"*. Draft-card copy is REWORDED per Pin 2 below: *"Not enough present for a live vote — open it as an async proposal instead."* (drops the drawn "48-hour").
- Honesty-ledger omissions (spec §8): no transcript quote, elapsed-only timer (no "/ 3:00" cap), no header meeting timer, no agenda line (no channel topic exists — S4 pin 4).
- MOD threshold: `power >= 50`. Grid overflow: 18 visible + mono "+N more". Motion poll: `windowSeconds: 300`, `minPower: 0`, `quorum: adminQuorum`.
- Style-token-guard budget-0: colors only via `var(--…)` tokens.
- Gates per task: `npx tsc --noEmit` + targeted `npx vitest run <files>`; full `npx vitest run` + style-token-guard at the end. No Rust changes → no cargo gates beyond the pre-PR full-suite backstop already green on main.
- `prefers-reduced-motion` honored for the waveform (decorative animation off).
- Tauri error extraction: `e instanceof Error ? e.message : String(e)`.
- Commit per task with trailers (Co-Authored-By + Claude-Session).

## Ground-truth premise pins (verified at plan time)

1. **Motion title transport.** `Tier1PollConfig` has no title field, and the poll-kind chat fanout body is magic byte + poll_id only (`lib.rs` `voting_create_tier1_poll` fanout). The motion title therefore rides in the option labels — `Aye — ${title}` / `Nay — ${title}` (title input max 70 chars; `MAX_OPTION_LABEL_LEN = 80`). Every surface that renders the poll (motion card, backchannel `PollMessage`) shows the motion text with zero new backend.
2. **"48-hour async proposal" is not honestly renderable.** The standard async proposal (Commons D, `CommunityProposalsPanel`) is a Tier-2 *conviction* proposal — default half-life `TIER2_DEFAULT_HALF_LIFE_SECONDS` = 7 days; no 48-hour-window primitive exists anywhere. Draft-card copy reworded to drop the number (Global Constraints above). §8-ledger treatment; flagged in the PR body for adjudication.
3. **Live-vote anatomy reuses `PollMessage`.** The spec's parenthetical names TallyBar + CountChip + vote buttons; `PollMessage` is the canonical Tier-1 renderer and already delivers option vote buttons, live counts, and animated fills on `--tally-track` — and its optimistic-ballot logic absorbed six review rounds of race fixes (PR #130). The motion card embeds `PollMessage` for the ballot area; `CountChip` renders the present/quorum chip; `TallyBar` renders the DRAFT-state quorum bar exactly as drawn. Re-implementing ballot logic would re-introduce the PR #130 bug class.
4. **Dominant speaker + timer are local inference.** The roster carries per-member `speaking: boolean` only. TownHallView records speaking-start timestamps locally (`Map<deviceHex, ms>` stamped at flip-to-true); dominant = active speaker with the earliest start (longest floor hold); fallback = the most recently active speaker still in the roster; the elapsed-only timer ticks off that local stamp. Purely local, honest per §8. The motion-card creation gate is the backend's own `check_eligibility` (Joined + power ≥ minPower(0)) — the UI gates only on being connected; backend errors surface in-card.

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `src/lib/components/ChannelMessageFeed.svelte` | Modify | additive `composerPlaceholder?: string` prop |
| `src/lib/components/__tests__/ChannelMessageFeed.test.ts` | Modify | placeholder prop test |
| `src/lib/components/TownHallMotionCard.svelte` | Create | quorum-aware motion card (idle → live poll / draft → async proposal) |
| `src/lib/components/__tests__/TownHallMotionCard.test.ts` | Create | motion card tests |
| `src/lib/components/TownHallView.svelte` | Create | the assembly view |
| `src/lib/components/__tests__/TownHallView.test.ts` | Create | view tests |
| `src/lib/components/CommunityView.svelte` | Modify | route `kind === 'townhall'` → TownHallView |
| `src/lib/components/__tests__/CommunityView.test.ts` | Modify | townhall routing test |

---

### Task 1: `composerPlaceholder` prop on ChannelMessageFeed

**Files:**
- Modify: `src/lib/components/ChannelMessageFeed.svelte` (props block ~line 30–95; composer input ~line 1170)
- Test: `src/lib/components/__tests__/ChannelMessageFeed.test.ts`

**Interfaces:**
- Produces: `composerPlaceholder?: string` — when set, replaces the default `` `Message #${channelName}` `` placeholder (ingest-in-flight copy still wins).

- [ ] **Step 1: Write the failing test** (append inside the top-level `describe('ChannelMessageFeed', …)`)

```ts
  it('composerPlaceholder overrides the default composer placeholder (ZEB-612 S5)', async () => {
    const { container } = await setup({ composerPlaceholder: 'Message the room…' });
    await waitFor(() => {
      const input = container.querySelector('.compose-bar input, .compose-input, textarea, input[placeholder]');
      expect(input).toBeTruthy();
    });
    expect(container.querySelector('[placeholder="Message the room…"]')).toBeTruthy();
  });
```

- [ ] **Step 2: Run** `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts -t 'composerPlaceholder'` — expect FAIL (placeholder stays `Message #general`).

- [ ] **Step 3: Implement.** In the destructure add `composerPlaceholder,` beside `mentionCandidates`; in the type block add:

```ts
    /** ZEB-612 S5: override for the composer placeholder (TownHallView passes
     *  "Message the room…"). Absent → the long-standing `Message #name`. */
    composerPlaceholder?: string;
```

At the composer input (line ~1170) change:

```svelte
        placeholder={ingesting ? 'Finishing upload…' : (composerPlaceholder ?? `Message #${channelName}`)}
```

- [ ] **Step 4: Run** the same test — expect PASS; then `npx vitest run src/lib/components/__tests__/ChannelMessageFeed.test.ts` (whole file) + `npx tsc --noEmit`.

- [ ] **Step 5: Commit** `feat: ChannelMessageFeed composerPlaceholder prop (ZEB-612 S5)`.

---

### Task 2: TownHallMotionCard

**Files:**
- Create: `src/lib/components/TownHallMotionCard.svelte`
- Test: `src/lib/components/__tests__/TownHallMotionCard.test.ts`

**Interfaces:**
- Consumes: `VotingAdapter.createTier1Poll/getPoll/listActivePolls/subscribePollCreated/createTier2Proposal`; `PollMessage {pollId, meta, adapter}`; `TallyBar {segments, height, label}`; `CountChip {label, value, tone}`.
- Produces: `<TownHallMotionCard {communityId} {channelId} {adapter} {presentCount} {quorum} {canAct} {onOpenProposals} />` — `adapter?: VotingAdapter`, `quorum: number | null`, `canAct: boolean` (viewer is in the room), `onOpenProposals?: () => void`.

- [ ] **Step 1: Write the failing tests**

```ts
import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import TownHallMotionCard from '../TownHallMotionCard.svelte';
import type { PollMeta } from '../../types/voting';

const CID = 'aa'.repeat(16);
const CHID = 'bb'.repeat(16);

function hexToBytes(hex: string): number[] {
  const out: number[] = [];
  for (let i = 0; i < hex.length; i += 2) out.push(parseInt(hex.slice(i, i + 2), 16));
  return out;
}

function pollMeta(pollIdHex: string, overrides: Partial<PollMeta> = {}): PollMeta {
  return {
    poll_id: hexToBytes(pollIdHex),
    community_id: hexToBytes(CID),
    creator: hexToBytes('cc'.repeat(16)),
    tier: 1,
    eligibility: { min_power: 0 },
    lifecycle: 'Open',
    created_at: { w: 1000, l: 0, d: 'd1' },
    opens_at: { w: 1000, l: 0, d: 'd1' },
    closes_at: { w: 301000, l: 0, d: 'd1' },
    channel_id: hexToBytes(CHID),
    ...overrides,
  } as PollMeta;
}

function makeAdapter(over: Record<string, unknown> = {}) {
  const meta = pollMeta('11'.repeat(32));
  return {
    createTier1Poll: vi.fn(async () => '11'.repeat(32)),
    getPoll: vi.fn(async () => ({
      meta,
      tally: { counts: [0, 0], ballot_count: 0 },
      options: ['Aye — Adopt the charter', 'Nay — Adopt the charter'],
    })),
    listActivePolls: vi.fn(async () => []),
    subscribePollCreated: vi.fn(() => () => {}),
    subscribeBallotCast: vi.fn(() => () => {}),
    castTier1Ballot: vi.fn(async () => {}),
    createTier2Proposal: vi.fn(async () => '22'.repeat(32)),
    ...over,
  };
}

const base = { communityId: CID, channelId: CHID, presentCount: 5, quorum: 3, canAct: true };

describe('TownHallMotionCard (ZEB-612 S5)', () => {
  it('idle state shows the call-to-motion affordance with a title input', () => {
    render(TownHallMotionCard, { props: { ...base, adapter: makeAdapter() as never } });
    expect(screen.getByText('⚖ Call this to a motion')).toBeInTheDocument();
    expect(screen.getByLabelText('Motion title')).toBeInTheDocument();
    expect(screen.getByTestId('motion-call')).toBeDisabled(); // empty title
  });

  it('present ≥ quorum: Call creates a 300s Tier-1 poll with Aye/Nay titled options and renders it live', async () => {
    const adapter = makeAdapter();
    render(TownHallMotionCard, { props: { ...base, adapter: adapter as never } });
    await fireEvent.input(screen.getByLabelText('Motion title'), { target: { value: 'Adopt the charter' } });
    await fireEvent.click(screen.getByTestId('motion-call'));
    await waitFor(() => {
      expect(adapter.createTier1Poll).toHaveBeenCalledWith({
        communityId: CID,
        channelId: CHID,
        options: ['Aye — Adopt the charter', 'Nay — Adopt the charter'],
        windowSeconds: 300,
        minPower: 0,
        quorum: 3,
      });
    });
    // Live card: embedded PollMessage renders the titled options.
    await waitFor(() => {
      expect(screen.getByText('Aye — Adopt the charter')).toBeInTheDocument();
    });
    expect(screen.getByTestId('motion-live-meta')).toHaveTextContent('live · 5 present');
  });

  it('present < quorum: Call shows the DRAFT card (no poll created)', async () => {
    const adapter = makeAdapter();
    render(TownHallMotionCard, {
      props: { ...base, presentCount: 1, quorum: 3, adapter: adapter as never },
    });
    await fireEvent.input(screen.getByLabelText('Motion title'), { target: { value: 'Adopt the charter' } });
    await fireEvent.click(screen.getByTestId('motion-call'));
    expect(adapter.createTier1Poll).not.toHaveBeenCalled();
    expect(screen.getByTestId('motion-draft-badge')).toHaveTextContent('DRAFT');
    expect(screen.getByTestId('motion-quorum')).toHaveTextContent('1 / 3');
    expect(
      screen.getByText('Not enough present for a live vote — open it as an async proposal instead.'),
    ).toBeInTheDocument();
  });

  it('DRAFT → Open as async proposal creates the standard Tier-2 proposal and links to proposals', async () => {
    const adapter = makeAdapter();
    const onOpenProposals = vi.fn();
    render(TownHallMotionCard, {
      props: { ...base, presentCount: 1, quorum: 3, adapter: adapter as never, onOpenProposals },
    });
    await fireEvent.input(screen.getByLabelText('Motion title'), { target: { value: 'Adopt the charter' } });
    await fireEvent.click(screen.getByTestId('motion-call'));
    await fireEvent.click(screen.getByTestId('motion-open-async'));
    await waitFor(() => {
      expect(adapter.createTier2Proposal).toHaveBeenCalledWith({
        communityId: CID,
        channelId: CHID,
        proposalText: 'Adopt the charter',
      });
    });
    await fireEvent.click(screen.getByTestId('motion-view-proposals'));
    expect(onOpenProposals).toHaveBeenCalled();
  });

  it('adopts an existing open Tier-1 poll on this channel at mount (peer-created motion)', async () => {
    const meta = pollMeta('11'.repeat(32));
    const adapter = makeAdapter({ listActivePolls: vi.fn(async () => [meta]) });
    render(TownHallMotionCard, { props: { ...base, adapter: adapter as never } });
    await waitFor(() => {
      expect(screen.getByText('Aye — Adopt the charter')).toBeInTheDocument();
    });
  });

  it('surfaces a create failure in-card as an alert', async () => {
    const adapter = makeAdapter({
      createTier1Poll: vi.fn(async () => { throw new Error('creator not eligible'); }),
    });
    render(TownHallMotionCard, { props: { ...base, adapter: adapter as never } });
    await fireEvent.input(screen.getByLabelText('Motion title'), { target: { value: 'Adopt the charter' } });
    await fireEvent.click(screen.getByTestId('motion-call'));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent(/creator not eligible/);
  });
});
```

- [ ] **Step 2: Run** `npx vitest run src/lib/components/__tests__/TownHallMotionCard.test.ts` — expect FAIL (component missing).

- [ ] **Step 3: Implement `src/lib/components/TownHallMotionCard.svelte`:**

```svelte
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
          onclick={() => { phase = { kind: 'idle' }; }}
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
```

- [ ] **Step 4: Run** `npx vitest run src/lib/components/__tests__/TownHallMotionCard.test.ts` — expect PASS (6/6); `npx tsc --noEmit`.

- [ ] **Step 5: Commit** `feat: TownHallMotionCard — quorum-aware live vote / async draft (ZEB-612 S5)`.

---

### Task 3: TownHallView core — join pane, header, spotlight, grid, control bar

**Files:**
- Create: `src/lib/components/TownHallView.svelte` (core; the rail lands in Task 4)
- Test: `src/lib/components/__tests__/TownHallView.test.ts`

**Interfaces:**
- Consumes: `VoiceSession` state store (`phase/roster/muted/deafened/pttMode/pttHeld/selfPower/selfModMuted/selfKicked/selfInvited/channelFull/micBlocked/reconnecting`), methods `join/leave/setMuted/setDeafened/setPttMode/setPttHeld/setHand/moderate/clearChannelFull`; `RosterMember.handRaisedAt/invited/power/speaking`.
- Produces (full prop surface, rail props consumed in Task 4): see the component code below. CommunityView (Task 5) passes them all.

- [ ] **Step 1: Write the failing tests** — create `src/lib/components/__tests__/TownHallView.test.ts`:

```ts
import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { writable } from 'svelte/store';
import TownHallView from '../TownHallView.svelte';
import { ChannelMessageService } from '../../channel-message-service';
import type { TauriAdapter } from '../../zenoh-service';

function fakeSession(state: object) {
  return {
    state: writable({
      phase: 'idle',
      community: null,
      channel: null,
      muted: true,
      deafened: false,
      pttMode: false,
      pttHeld: false,
      roster: [],
      channelFull: false,
      reconnecting: false,
      micBlocked: false,
      selfPower: 0,
      selfModMuted: false,
      selfKicked: false,
      selfInvited: false,
      ...state,
    }),
    join: vi.fn(async () => {}),
    leave: vi.fn(async () => {}),
    setMuted: vi.fn(async () => {}),
    setDeafened: vi.fn(async () => {}),
    setPttMode: vi.fn(async () => {}),
    setPttHeld: vi.fn(),
    setHand: vi.fn(async () => {}),
    inviteToSpeak: vi.fn(async () => {}),
    moderate: vi.fn(async () => {}),
    clearChannelFull: vi.fn(),
  };
}

function member(i: number, over: Record<string, unknown> = {}) {
  return {
    ownerHex: String(i).padStart(2, '0').repeat(16),
    deviceHex: String(i).padStart(2, '0').repeat(32),
    muted: false,
    speaking: false,
    displayName: `User${i}`,
    modMuted: false,
    power: 0,
    handRaisedAt: null,
    invited: false,
    ...over,
  };
}

function makeTauriAdapter(): TauriAdapter {
  return {
    invoke: vi.fn(async (cmd: string) => {
      if (cmd === 'list_channel_messages') return [];
      return undefined;
    }),
    listen: vi.fn(async () => () => {}),
  } as never;
}

async function setup(sessionState: object, propOverrides: Record<string, unknown> = {}) {
  const session = fakeSession(sessionState);
  const channelMessageService = new ChannelMessageService();
  await channelMessageService.connectAdapter(makeTauriAdapter());
  const votingAdapter = {
    createTier1Poll: vi.fn(async () => '11'.repeat(32)),
    getPoll: vi.fn(async () => ({ meta: {}, tally: { counts: [], ballot_count: 0 }, options: [] })),
    listActivePolls: vi.fn(async () => []),
    subscribePollCreated: vi.fn(() => () => {}),
    subscribeBallotCast: vi.fn(() => () => {}),
    createTier2Proposal: vi.fn(async () => '22'.repeat(32)),
  };
  const props = {
    session: session as never,
    channelName: 'assembly',
    communityId: 'aa'.repeat(16),
    channelId: 'bb'.repeat(16),
    ownAddress: 'cc'.repeat(16),
    myPower: 50,
    adminQuorum: 3,
    votingAdapter: votingAdapter as never,
    channelMessageService,
    ...propOverrides,
  };
  const result = render(TownHallView, { props });
  return { session, votingAdapter, ...result };
}

describe('TownHallView (ZEB-612 S5): join pane + header', () => {
  it('idle shows the assembly join pane with the join-muted hint', async () => {
    const { session } = await setup({ phase: 'idle' });
    expect(screen.getByText("Join the assembly — you'll join muted.")).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: /join assembly/i }));
    expect(session.join).toHaveBeenCalledWith('aa'.repeat(16), 'bb'.repeat(16));
  });

  it('header shows LIVE dot and distinct-owner present count', async () => {
    // Two devices of owner 1 + one device of owner 2 → 2 present.
    const twin = member(1, { deviceHex: 'f'.repeat(64) });
    await setup({ phase: 'connected', roster: [member(1), twin, member(2)] });
    expect(screen.getByTestId('th-live-dot')).toBeInTheDocument();
    expect(screen.getByTestId('th-present')).toHaveTextContent('2 present');
  });
});

describe('TownHallView (ZEB-612 S5): spotlight', () => {
  it('empty state when no one is or was speaking', async () => {
    await setup({ phase: 'connected', roster: [member(1)] });
    expect(screen.getByTestId('th-spotlight-empty')).toHaveTextContent('No one has the floor.');
  });

  it('shows the speaking member with 🎙 speaking, a timer, and a MOD pill at power ≥ 50', async () => {
    await setup({
      phase: 'connected',
      roster: [member(1, { speaking: true, power: 60 }), member(2)],
    });
    const spot = screen.getByTestId('th-spotlight');
    expect(spot).toHaveTextContent('User1');
    expect(spot).toHaveTextContent(/🎙 speaking/);
    expect(screen.getByTestId('th-mod-pill')).toHaveTextContent('MOD');
    expect(screen.getByTestId('th-timer')).toBeInTheDocument();
    expect(screen.getByTestId('th-waveform')).toBeInTheDocument();
  });
});

describe('TownHallView (ZEB-612 S5): in-room grid', () => {
  it('renders hand and mute badges from real roster data', async () => {
    await setup({
      phase: 'connected',
      roster: [member(1, { handRaisedAt: 1000 }), member(2, { muted: true })],
    });
    expect(screen.getByTestId('th-hand-badge')).toBeInTheDocument();
    expect(screen.getByLabelText('muted')).toBeInTheDocument();
  });

  it('caps the grid at 18 tiles with a mono +N more overflow tile', async () => {
    const roster = Array.from({ length: 21 }, (_, i) => member(i));
    await setup({ phase: 'connected', roster });
    expect(screen.getAllByTestId('th-tile')).toHaveLength(18);
    expect(screen.getByTestId('th-overflow')).toHaveTextContent('+3 more');
  });
});

describe('TownHallView (ZEB-612 S5): control bar + raise hand', () => {
  it('raise-hand toggle calls setHand(true) then reads Lower hand', async () => {
    const { session } = await setup({ phase: 'connected', roster: [member(1)] });
    const btn = screen.getByTestId('th-raise-hand');
    expect(btn).toHaveTextContent('✋ Raise hand');
    await fireEvent.click(btn);
    expect(session.setHand).toHaveBeenCalledWith(true);
    await waitFor(() => expect(btn).toHaveTextContent('✋ Lower hand'));
  });

  it('keeps the voice controls (mute toggle, deafen, leave)', async () => {
    const { session } = await setup({ phase: 'connected', muted: true, roster: [member(1)] });
    await fireEvent.click(screen.getByRole('button', { name: /unmute|muted/i }));
    expect(session.setMuted).toHaveBeenCalledWith(false);
    await fireEvent.click(screen.getByRole('button', { name: /leave/i }));
    expect(session.leave).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run** `npx vitest run src/lib/components/__tests__/TownHallView.test.ts` — expect FAIL (component missing).

- [ ] **Step 3: Implement `src/lib/components/TownHallView.svelte`** (core; the `<!-- RAIL (Task 4) -->` placeholder is replaced in Task 4):

```svelte
<script lang="ts">
  /**
   * ZEB-612 S5 — TownHallView: voice fused with the Assembly (spec §6, TH
   * frame A). Rendered by CommunityView for kind === 'townhall'. Reuses the
   * app-lifetime VoiceSession (join/PTT/mute/deafen/mod) plus S4's hand +
   * invite surface.
   *
   * Honesty ledger (§8): no transcript quote (no transcription), elapsed-only
   * speaking timer (no speak-limit policy), no header meeting timer (no
   * room-start record), no agenda line (no channel topic exists — S4 pin 4).
   *
   * Dominant speaker is LOCAL inference (plan Pin 4): speaking-start stamps
   * recorded per device at flip-to-true; dominant = earliest active start
   * (longest floor hold); fallback = most recent speaker still in the room.
   */
  import { onMount } from 'svelte';
  import type { VoiceSession, RosterMember } from '../voice-session';
  import { speakerQueue } from '../voice-session';
  import type { VotingAdapter } from '../voting-adapter';
  import type { ChannelMessageService } from '../channel-message-service';
  import type { ChannelMessageDto } from '../channel-message-service';
  import type { ResolvedCard } from '../member-card-service';
  import type { MentionCandidate } from '../mention-compose';
  import ChannelMessageFeed from './ChannelMessageFeed.svelte';
  import TownHallMotionCard from './TownHallMotionCard.svelte';

  let {
    session,
    channelName,
    communityId,
    channelId,
    onBeforeJoin,
    ownAddress,
    myPower,
    adminQuorum,
    votingAdapter,
    channelMessageService,
    resolveCard,
    resolveNickname,
    onOpenCard,
    mentionCandidates = [],
    snapshotMessages = [],
    originalCommunityName = '',
    forkedAtMs = 0,
    forkReason = null,
    onOpenProposals,
  }: {
    session: VoiceSession;
    channelName: string;
    communityId: string;
    channelId: string;
    onBeforeJoin?: () => Promise<void>;
    ownAddress: string;
    myPower: number;
    /** Community admin quorum for the motion card; null while loading. */
    adminQuorum: number | null;
    votingAdapter?: VotingAdapter;
    channelMessageService: ChannelMessageService;
    resolveCard?: (ownerIdHex: string) => ResolvedCard | undefined;
    resolveNickname?: (ownerIdHex: string) => string | undefined;
    onOpenCard?: (
      payload: {
        ownerIdHex: string;
        displayName: string;
        statusText: string;
        power?: number;
        membershipStatus?: string;
        avatarUrl?: string;
      },
      ev: MouseEvent,
    ) => void;
    mentionCandidates?: MentionCandidate[];
    snapshotMessages?: ChannelMessageDto[];
    originalCommunityName?: string;
    forkedAtMs?: number;
    forkReason?: string | null;
    /** Deep-link to the community proposals view (motion-card async path). */
    onOpenProposals?: () => void;
  } = $props();

  // svelte-ignore state_referenced_locally
  const voiceState = session.state;

  let joining = $state(false);
  let error = $state<string | null>(null);

  // channelFull lives on the app-wide singleton; clear on mount/channel switch
  // (same discipline as VoiceChannelView).
  $effect(() => {
    channelId;
    session.clearChannelFull();
  });

  const GRID_VISIBLE = 18;
  const MOD_POWER = 50;

  async function onJoin() {
    joining = true;
    error = null;
    try {
      await onBeforeJoin?.();
      await session.join(communityId, channelId);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      joining = false;
    }
  }

  const swallow = (p: unknown) => { void Promise.resolve(p).catch(() => {}); };
  const toggleMute = () => swallow(session.setMuted(!$voiceState.muted));
  const toggleDeafen = () => swallow(session.setDeafened(!$voiceState.deafened));
  const togglePtt = () => swallow(session.setPttMode(!$voiceState.pttMode));
  const onLeave = () => swallow(session.leave());
  const pttDown = () => session.setPttHeld(true);
  const pttUp = () => session.setPttHeld(false);

  const silenced = $derived($voiceState.selfModMuted || $voiceState.selfKicked);

  // ── Raise hand (per-device beacon; optimistic local toggle) ─────────────
  let myHandRaised = $state(false);
  function toggleHand() {
    const next = !myHandRaised;
    myHandRaised = next;
    void session.setHand(next).catch(() => {
      myHandRaised = !next;
    });
  }
  function lowerHand() {
    myHandRaised = false;
    swallow(session.setHand(false));
  }
  // Leaving the room lowers the local toggle (the beacon dies with the session).
  $effect(() => {
    if ($voiceState.phase === 'idle') myHandRaised = false;
  });

  // ── Invited banner (Task 4 renders it; state lives here) ────────────────
  let inviteHandled = $state(false);
  $effect(() => {
    if (!$voiceState.selfInvited) inviteHandled = false;
  });
  function acceptInvite() {
    swallow(session.setMuted(false));
    lowerHand();
    inviteHandled = true;
  }
  function dismissInvite() {
    lowerHand();
    inviteHandled = true;
  }

  // ── Presence & queue ─────────────────────────────────────────────────────
  let presentCount = $derived(new Set($voiceState.roster.map((m) => m.ownerHex)).size);
  let queue = $derived(speakerQueue($voiceState.roster));
  let visibleRoster = $derived($voiceState.roster.slice(0, GRID_VISIBLE));
  let overflowCount = $derived(Math.max(0, $voiceState.roster.length - GRID_VISIBLE));

  // ── Dominant-speaker inference (plan Pin 4) ─────────────────────────────
  // Non-reactive map: deviceHex → wall-clock ms when speaking flipped true.
  const speakStart = new Map<string, number>();
  let dominantKey = $state<string | null>(null);
  let lastSpeakerKey = $state<string | null>(null);
  let nowTick = $state(Date.now());

  $effect(() => {
    const roster = $voiceState.roster;
    const now = Date.now();
    const active = new Set<string>();
    for (const m of roster) {
      if (m.speaking) {
        active.add(m.deviceHex);
        if (!speakStart.has(m.deviceHex)) speakStart.set(m.deviceHex, now);
        lastSpeakerKey = m.deviceHex;
      }
    }
    for (const k of [...speakStart.keys()]) {
      if (!active.has(k)) speakStart.delete(k);
    }
    let best: string | null = null;
    let bestT = Infinity;
    for (const [k, t] of speakStart) {
      if (t < bestT) {
        bestT = t;
        best = k;
      }
    }
    dominantKey = best;
  });

  onMount(() => {
    const t = setInterval(() => {
      nowTick = Date.now();
    }, 1000);
    return () => clearInterval(t);
  });

  let spotlight = $derived.by((): RosterMember | null => {
    const roster = $voiceState.roster;
    if (dominantKey) return roster.find((m) => m.deviceHex === dominantKey) ?? null;
    if (lastSpeakerKey) return roster.find((m) => m.deviceHex === lastSpeakerKey) ?? null;
    return null;
  });

  function fmtElapsed(ms: number): string {
    const s = Math.max(0, Math.floor(ms / 1000));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  }
  let spotlightElapsed = $derived.by(() => {
    if (!spotlight || !spotlight.speaking || !dominantKey) return null;
    const start = speakStart.get(dominantKey);
    return start === undefined ? null : fmtElapsed(nowTick - start);
  });

  // ── Moderation (grid hover controls — capability parity with voice) ─────
  const canModerate = (m: RosterMember): boolean =>
    $voiceState.selfPower >= MOD_POWER && $voiceState.selfPower > m.power;
  const modMute = (m: RosterMember) =>
    swallow(session.moderate(m.ownerHex, m.modMuted ? 'unmute' : 'mute'));
  let confirmingKick = $state<string | null>(null);
  const askKick = (m: RosterMember) => { confirmingKick = m.deviceHex; };
  const doKick = (m: RosterMember) => {
    confirmingKick = null;
    swallow(session.moderate(m.ownerHex, 'kick'));
  };
  function onWindowClick(e: MouseEvent) {
    if (!confirmingKick) return;
    const t = e.target as HTMLElement | null;
    if (!t?.closest?.('.mod-controls')) confirmingKick = null;
  }

  // PTT Space hotkey (same rules as VoiceChannelView; the backchannel composer
  // is a typing target so Space there never transmits).
  function isTypingTarget(t: EventTarget | null): boolean {
    const el = t as HTMLElement | null;
    if (!el || !el.tagName) return false;
    return el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable;
  }
  function onKeyDown(e: KeyboardEvent) {
    if (!$voiceState.pttMode || e.code !== 'Space' || e.repeat) return;
    if (isTypingTarget(e.target)) return;
    e.preventDefault();
    session.setPttHeld(true);
  }
  function onKeyUp(e: KeyboardEvent) {
    if (e.code !== 'Space' || isTypingTarget(e.target)) return;
    session.setPttHeld(false);
  }
  const onWindowBlur = () => session.setPttHeld(false);

  function label(m: Pick<RosterMember, 'displayName' | 'ownerHex'>): string {
    return m.displayName ?? `${m.ownerHex.slice(0, 6)}…`;
  }
</script>

<svelte:window onkeydown={onKeyDown} onkeyup={onKeyUp} onblur={onWindowBlur} onclick={onWindowClick} />

<section class="townhall-view" aria-label="Town hall">
  <header class="th-header">
    <span class="th-glyph" aria-hidden="true">⚖</span>
    <span class="th-title">{channelName}</span>
    {#if $voiceState.roster.length > 0}
      <span class="th-live-dot" data-testid="th-live-dot" aria-label="live"></span>
    {/if}
    <span class="th-present" data-testid="th-present">· {presentCount} present</span>
  </header>

  {#if $voiceState.channelFull}
    <div class="th-full-note" role="alert">Assembly full — try again later.</div>
  {:else if error}
    <div class="th-error" role="alert">{error}</div>
  {/if}
  {#if $voiceState.micBlocked}
    <div class="th-mic-blocked" role="status">🎤 Mic blocked — listening only</div>
  {/if}

  {#if $voiceState.phase === 'idle'}
    <div class="th-join-pane">
      <span class="join-glyph" aria-hidden="true">⚖</span>
      <span class="join-name">{channelName}</span>
      <button class="btn-primary" onclick={onJoin} disabled={joining}>
        {joining ? 'Joining…' : 'Join Assembly'}
      </button>
      <p class="hint">Join the assembly — you'll join muted.</p>
    </div>
  {:else}
    <div class="th-body">
      <div class="th-main">
        <section class="th-spotlight-box" aria-label="On the floor">
          <h3 class="th-section-label">On the floor</h3>
          {#if spotlight}
            <div class="th-spotlight" data-testid="th-spotlight">
              {#if spotlight.avatarUrl}
                <img class="sp-avatar" src={spotlight.avatarUrl} alt="" />
              {:else}
                <div class="sp-avatar sp-avatar-fallback" aria-hidden="true"></div>
              {/if}
              <div class="sp-info">
                <span class="sp-name">
                  {label(spotlight)}
                  {#if spotlight.power >= MOD_POWER}
                    <span class="mod-pill" data-testid="th-mod-pill">MOD</span>
                  {/if}
                </span>
                {#if spotlight.speaking}
                  <span class="sp-status">
                    🎙 speaking
                    {#if spotlightElapsed}
                      · <span class="sp-timer" data-testid="th-timer">{spotlightElapsed}</span>
                    {/if}
                  </span>
                {/if}
              </div>
              <div
                class="waveform"
                class:live={spotlight.speaking}
                data-testid="th-waveform"
                aria-hidden="true"
              >
                {#each Array(7) as _, i (i)}
                  <span class="wf-bar" style="animation-delay: {-(i * 0.13)}s"></span>
                {/each}
              </div>
            </div>
          {:else}
            <p class="th-spotlight-empty" data-testid="th-spotlight-empty">No one has the floor.</p>
          {/if}
        </section>

        <section class="th-room" aria-label="In the room">
          <h3 class="th-section-label">In the room · {$voiceState.roster.length}</h3>
          <div class="th-grid">
            {#each visibleRoster as m (m.deviceHex)}
              <div class="th-tile" class:speaking={m.speaking} data-testid="th-tile">
                {#if m.avatarUrl}
                  <img class="avatar" src={m.avatarUrl} alt="" />
                {:else}
                  <div class="avatar avatar-fallback" aria-hidden="true"></div>
                {/if}
                {#if m.handRaisedAt !== null}
                  <span class="hand-badge" data-testid="th-hand-badge" aria-label="hand raised">✋</span>
                {/if}
                {#if m.muted && !m.modMuted}
                  <span class="mute-glyph" aria-label="muted">🔇</span>
                {/if}
                {#if m.modMuted}
                  <span class="mod-badge" title="Muted by a moderator" aria-label="muted by a moderator">🛡</span>
                {/if}
                <span class="name">{label(m)}</span>
                {#if canModerate(m)}
                  <div class="mod-controls">
                    <button class="mod-btn" data-testid="mod-mute" onclick={() => modMute(m)}
                      aria-label={m.modMuted ? 'Unmute (moderator)' : 'Mute (moderator)'}>
                      {m.modMuted ? 'Unmute' : 'Mute'}
                    </button>
                    {#if confirmingKick === m.deviceHex}
                      <button class="mod-btn danger" data-testid="mod-remove-confirm" onclick={() => doKick(m)}
                        aria-label="Confirm remove">Confirm</button>
                    {:else}
                      <button class="mod-btn" data-testid="mod-remove" onclick={() => askKick(m)}
                        aria-label="Remove from voice">Remove</button>
                    {/if}
                  </div>
                {/if}
              </div>
            {/each}
            {#if overflowCount > 0}
              <div class="th-tile th-overflow" data-testid="th-overflow">+{overflowCount} more</div>
            {/if}
          </div>
        </section>
      </div>

      <!-- RAIL (Task 4) -->
    </div>

    {#if $voiceState.selfModMuted}
      <div class="th-mod-note" role="status" data-testid="self-mod-muted">
        🛡 You've been muted by a moderator. Your talk controls are disabled until they unmute you.
      </div>
    {/if}
    {#if $voiceState.selfKicked}
      <div class="th-error" role="alert" data-testid="self-kicked">
        🛡️ You were removed from voice by a moderator.
      </div>
    {/if}

    <div class="th-controls">
      {#if $voiceState.reconnecting}
        <span class="reconnecting" role="status">Reconnecting…</span>
      {/if}
      {#if $voiceState.pttMode}
        <button
          class="ctrl ptt-hold"
          class:active={$voiceState.pttHeld}
          aria-pressed={$voiceState.pttHeld}
          onpointerdown={pttDown}
          onpointerup={pttUp}
          onpointerleave={pttUp}
          onpointercancel={pttUp}
          aria-label="Hold to talk (or hold Space)"
          disabled={silenced}
        >
          {$voiceState.pttHeld ? '🎙 Transmitting… (hold Space)' : '🎙 Hold to Talk'}
        </button>
      {:else}
        <button
          class="ctrl"
          class:active={!$voiceState.muted}
          class:restrictive={$voiceState.muted}
          aria-pressed={!$voiceState.muted}
          onclick={toggleMute}
          aria-label={$voiceState.muted ? 'Unmute' : 'Mute'}
          disabled={silenced}
        >
          {$voiceState.muted ? '🔇 Muted' : '🎙 Live'}
        </button>
      {/if}
      <button
        class="ctrl"
        class:active={$voiceState.pttMode}
        aria-pressed={$voiceState.pttMode}
        onclick={togglePtt}
        aria-label="Push to talk mode"
        disabled={silenced}
      >PTT</button>
      <button
        class="ctrl"
        class:restrictive={$voiceState.deafened}
        aria-pressed={$voiceState.deafened}
        onclick={toggleDeafen}
        aria-label="Deafen"
      >
        {$voiceState.deafened ? '🔕 Deafened' : '🎧 Deafen'}
      </button>
      <button
        class="ctrl"
        class:active={myHandRaised}
        aria-pressed={myHandRaised}
        data-testid="th-raise-hand"
        onclick={toggleHand}
        aria-label={myHandRaised ? 'Lower hand' : 'Raise hand'}
      >
        {myHandRaised ? '✋ Lower hand' : '✋ Raise hand'}
      </button>
      <button class="btn-danger" onclick={onLeave} aria-label="Leave assembly">Leave</button>
    </div>
  {/if}
</section>

<style>
  .townhall-view {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    height: 100%;
  }
  .th-header {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--border);
  }
  .th-glyph { font-size: 0.95rem; }
  .th-title {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .th-live-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--accent);
    flex-shrink: 0;
  }
  .th-present {
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--text-secondary);
    white-space: nowrap;
  }
  .th-error {
    background: var(--bg-tertiary);
    border: 1px solid var(--danger);
    color: var(--danger);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }
  .th-full-note {
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    color: var(--gov-clay-deep);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }
  .th-mic-blocked {
    background: color-mix(in srgb, var(--warning) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    color: var(--warning);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 8px 16px;
    font-size: 0.85rem;
  }
  .th-join-pane {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    color: var(--text-secondary);
  }
  .join-glyph { font-size: 2rem; line-height: 1; }
  .join-name {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary);
    margin-bottom: 4px;
  }
  .btn-primary {
    border: none;
    padding: 8px 22px;
    border-radius: 5px;
    background: var(--accent);
    color: var(--on-accent);
    font-size: 0.9rem;
    cursor: pointer;
  }
  .btn-primary:hover:not(:disabled) { background: var(--accent-hover); }
  .btn-primary:disabled { opacity: 0.6; cursor: not-allowed; }
  .hint { font-size: 0.85rem; margin: 0; color: var(--text-muted); }

  .th-body {
    display: flex;
    flex: 1;
    min-height: 0;
  }
  .th-main {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 1rem;
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }
  .th-section-label {
    margin: 0 0 0.5rem;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }

  /* ── Spotlight ── */
  .th-spotlight {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 14px;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-secondary);
  }
  .sp-avatar {
    width: 64px;
    height: 64px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
    flex-shrink: 0;
    /* Double ring per TH frame A: paper gap then accent. */
    box-shadow:
      0 0 0 3px var(--bg-primary),
      0 0 0 6px var(--accent);
  }
  .sp-avatar-fallback { background: var(--bg-tertiary); border: 1px solid var(--border); }
  .sp-info { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
  .sp-name {
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-primary);
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .mod-pill {
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.06em;
    color: var(--gov-clay-deep);
    background: var(--gov-clay-soft);
    border-radius: 4px;
    padding: 1px 5px;
  }
  .sp-status { font-size: 0.82rem; color: var(--accent); }
  .sp-timer { font-family: var(--font-mono); color: var(--text-secondary); }
  .th-spotlight-empty {
    margin: 0;
    padding: 14px;
    border: 1px dashed var(--border);
    border-radius: 10px;
    color: var(--text-muted);
    font-size: 0.85rem;
  }

  /* Decorative waveform — animates only while the floor-holder is speaking. */
  .waveform {
    margin-left: auto;
    display: flex;
    align-items: flex-end;
    gap: 3px;
    height: 28px;
    flex-shrink: 0;
  }
  .wf-bar {
    width: 4px;
    height: 30%;
    border-radius: 2px;
    background: color-mix(in srgb, var(--accent) 45%, transparent);
  }
  .waveform.live .wf-bar {
    background: var(--accent);
    animation: wf-bounce 0.9s ease-in-out infinite alternate;
  }
  @keyframes wf-bounce {
    from { height: 20%; }
    to { height: 100%; }
  }
  @media (prefers-reduced-motion: reduce) {
    .wf-bar { animation: none !important; }
  }

  /* ── In-room grid ── */
  .th-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(104px, 1fr));
    gap: 0.65rem;
  }
  .th-tile {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.35rem;
    padding: 0.65rem 0.4rem;
    border-radius: 8px;
    background: var(--bg-secondary);
    position: relative;
  }
  .th-tile .avatar {
    width: 42px;
    height: 42px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
  }
  .avatar-fallback { background: var(--bg-tertiary); border: 1px solid var(--border); }
  .th-tile .name {
    font-size: 0.75rem;
    color: var(--text-primary);
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .th-tile.speaking {
    box-shadow:
      0 0 0 2.5px var(--bg-primary),
      0 0 0 5px var(--accent);
  }
  .hand-badge {
    position: absolute;
    top: 6px;
    left: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .mute-glyph {
    position: absolute;
    top: 6px;
    right: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--bg-tertiary);
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .mod-badge {
    position: absolute;
    top: 26px;
    right: 6px;
    font-size: 0.7rem;
    line-height: 1;
    background: var(--status-recalled-bg);
    border: 1px solid color-mix(in srgb, var(--danger) 30%, transparent);
    border-radius: 999px;
    padding: 2px 4px;
  }
  .th-tile:has(.mute-glyph) .name { color: var(--text-muted); }
  .th-overflow {
    justify-content: center;
    font-family: var(--font-mono);
    font-size: 0.8rem;
    color: var(--text-secondary);
    min-height: 84px;
  }
  .mod-controls {
    display: flex;
    gap: 4px;
    margin-top: 4px;
    opacity: 0;
    pointer-events: none;
    transition: opacity 0.12s ease;
  }
  .th-tile:hover .mod-controls,
  .th-tile:focus-within .mod-controls {
    opacity: 1;
    pointer-events: auto;
  }
  .mod-btn {
    border: 1px solid var(--border);
    background: var(--bg-tertiary);
    color: var(--text-secondary);
    font-size: 0.7rem;
    padding: 2px 6px;
    border-radius: 3px;
    cursor: pointer;
  }
  .mod-btn:hover { color: var(--text-primary); }
  .mod-btn.danger { color: var(--danger); border-color: var(--danger); }

  .th-mod-note {
    background: var(--status-recalled-bg);
    border: 1px solid color-mix(in srgb, var(--danger) 35%, transparent);
    color: var(--danger);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 0 16px 8px;
    font-size: 0.85rem;
  }

  /* ── Control bar ── */
  .th-controls {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    border-top: 1px solid var(--border);
    flex-wrap: wrap;
  }
  .ctrl {
    border: 1px solid var(--border);
    background: var(--bg-secondary);
    color: var(--text-secondary);
    padding: 6px 14px;
    border-radius: 5px;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .ctrl:hover { background: var(--bg-tertiary); color: var(--text-primary); }
  .ctrl.active {
    background: var(--accent);
    border-color: var(--accent);
    color: var(--on-accent);
  }
  .ctrl.restrictive {
    background: var(--gov-clay);
    border-color: var(--gov-clay);
    color: var(--text-primary);
  }
  .reconnecting {
    font-size: 0.78rem;
    color: var(--warning);
    background: color-mix(in srgb, var(--warning) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--warning) 40%, transparent);
    padding: 3px 9px;
    border-radius: 999px;
    white-space: nowrap;
  }
  .ptt-hold { touch-action: none; user-select: none; flex: 1; }
  .ptt-hold.active {
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 35%, transparent);
  }
  .btn-danger {
    margin-left: auto;
    border: none;
    background: var(--danger);
    color: var(--on-accent);
    padding: 6px 16px;
    border-radius: 5px;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .btn-danger:hover { filter: brightness(1.1); }
</style>
```

Note: `ChannelMessageFeed`, `TownHallMotionCard`, `speakerQueue`, `queue`, `presentCount` rail consumers and the invite-banner markup land in Task 4 — the imports and state above already compile (unused-import lint doesn't apply to Svelte script; if `svelte-check`/tsc flags unused, keep the imports in Task 4's commit instead).

- [ ] **Step 4: Run** `npx vitest run src/lib/components/__tests__/TownHallView.test.ts` — expect PASS; `npx tsc --noEmit`.

- [ ] **Step 5: Commit** `feat: TownHallView core — spotlight, grid, raise-hand control bar (ZEB-612 S5)`.

---

### Task 4: TownHallView rail — speaker queue, motion card, backchannel + invited banner

**Files:**
- Modify: `src/lib/components/TownHallView.svelte` (replace the `<!-- RAIL (Task 4) -->` placeholder; add rail styles)
- Test: `src/lib/components/__tests__/TownHallView.test.ts` (append describes)

**Interfaces:**
- Consumes: `speakerQueue(roster)`, `session.inviteToSpeak(ownerHex)`, `$voiceState.selfInvited`, `TownHallMotionCard`, `ChannelMessageFeed` (+ Task 1's `composerPlaceholder`).

- [ ] **Step 1: Write the failing tests** (append to `TownHallView.test.ts`):

```ts
describe('TownHallView (ZEB-612 S5): speaker queue', () => {
  it('orders raised hands by handRaisedAt and numbers the rows', async () => {
    await setup({
      phase: 'connected',
      roster: [
        member(1, { handRaisedAt: 2000 }),
        member(2, { handRaisedAt: 1000 }),
        member(3),
      ],
    });
    const rows = screen.getAllByTestId('th-queue-row');
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent('User2');
    expect(rows[0]).toHaveTextContent('wants to speak ✋');
    expect(rows[1]).toHaveTextContent('User1');
  });

  it('Invite is visible to all but disabled below power 50; click delegates to inviteToSpeak', async () => {
    const { session } = await setup({
      phase: 'connected',
      selfPower: 0,
      roster: [member(1, { handRaisedAt: 1000 })],
    });
    expect(screen.getByTestId('th-invite')).toBeDisabled();
    session.state.update((s: never) => ({ ...(s as object), selfPower: 60 }) as never);
    await waitFor(() => expect(screen.getByTestId('th-invite')).toBeEnabled());
    await fireEvent.click(screen.getByTestId('th-invite'));
    expect(session.inviteToSpeak).toHaveBeenCalledWith(member(1).ownerHex);
  });

  it('invited entries dim with an "invited" label instead of the button', async () => {
    await setup({
      phase: 'connected',
      roster: [member(1, { handRaisedAt: 1000, invited: true })],
    });
    expect(screen.queryByTestId('th-invite')).not.toBeInTheDocument();
    expect(screen.getByTestId('th-queue-row')).toHaveTextContent('invited');
  });

  it('shows an empty-queue note when no hands are raised', async () => {
    await setup({ phase: 'connected', roster: [member(1)] });
    expect(screen.getByText('No raised hands yet.')).toBeInTheDocument();
  });
});

describe('TownHallView (ZEB-612 S5): invited banner', () => {
  it('selfInvited shows the banner; Unmute accepts (unmute + lower hand)', async () => {
    const { session } = await setup({ phase: 'connected', selfInvited: true, roster: [member(1)] });
    const banner = screen.getByTestId('th-invited-banner');
    expect(banner).toHaveTextContent("You've been invited to speak — Unmute?");
    await fireEvent.click(screen.getByTestId('th-invite-accept'));
    expect(session.setMuted).toHaveBeenCalledWith(false);
    expect(session.setHand).toHaveBeenCalledWith(false);
    await waitFor(() => {
      expect(screen.queryByTestId('th-invited-banner')).not.toBeInTheDocument();
    });
  });

  it('Dismiss lowers the hand without unmuting', async () => {
    const { session } = await setup({ phase: 'connected', selfInvited: true, roster: [member(1)] });
    await fireEvent.click(screen.getByTestId('th-invite-dismiss'));
    expect(session.setHand).toHaveBeenCalledWith(false);
    expect(session.setMuted).not.toHaveBeenCalled();
    await waitFor(() => {
      expect(screen.queryByTestId('th-invited-banner')).not.toBeInTheDocument();
    });
  });
});

describe('TownHallView (ZEB-612 S5): rail — motion card + backchannel', () => {
  it('renders the motion card and the backchannel with the room composer placeholder', async () => {
    await setup({ phase: 'connected', roster: [member(1)] });
    expect(screen.getByText('⚖ Call this to a motion')).toBeInTheDocument();
    await waitFor(() => {
      expect(document.querySelector('[placeholder="Message the room…"]')).toBeTruthy();
    });
  });
});
```

- [ ] **Step 2: Run** the file — new describes FAIL (no rail markup).

- [ ] **Step 3: Implement.** Replace `<!-- RAIL (Task 4) -->` with:

```svelte
      <aside class="th-rail" aria-label="Floor">
        <section class="th-queue" aria-label="Speaker queue">
          <h3 class="th-section-label">Speaker queue</h3>
          {#if queue.length === 0}
            <p class="th-queue-empty">No raised hands yet.</p>
          {:else}
            <ol class="th-queue-list">
              {#each queue as m, i (m.deviceHex)}
                <li class="th-queue-row" class:dimmed={m.invited} data-testid="th-queue-row">
                  <span class="q-num" class:first={i === 0}>{i + 1}</span>
                  {#if m.avatarUrl}
                    <img class="q-avatar" src={m.avatarUrl} alt="" />
                  {:else}
                    <div class="q-avatar q-avatar-fallback" aria-hidden="true"></div>
                  {/if}
                  <span class="q-text">
                    <span class="q-name">{label(m)}</span>
                    <span class="q-wants">wants to speak ✋</span>
                  </span>
                  {#if m.invited}
                    <span class="q-invited">invited</span>
                  {:else}
                    <button
                      type="button"
                      class="q-invite"
                      class:primary={i === 0}
                      data-testid="th-invite"
                      disabled={$voiceState.selfPower < MOD_POWER}
                      title={$voiceState.selfPower < MOD_POWER ? 'Moderators can invite to speak' : ''}
                      onclick={() => swallow(session.inviteToSpeak(m.ownerHex))}
                    >Invite</button>
                  {/if}
                </li>
              {/each}
            </ol>
          {/if}
        </section>

        <TownHallMotionCard
          {communityId}
          {channelId}
          adapter={votingAdapter}
          {presentCount}
          quorum={adminQuorum}
          canAct={$voiceState.phase === 'connected'}
          {onOpenProposals}
        />

        <section class="th-backchannel" aria-label="Backchannel">
          <h3 class="th-section-label">Backchannel</h3>
          <div class="th-backchannel-feed">
            <ChannelMessageFeed
              {communityId}
              {channelId}
              {channelName}
              {channelMessageService}
              {votingAdapter}
              {ownAddress}
              {myPower}
              {snapshotMessages}
              {originalCommunityName}
              {forkedAtMs}
              {forkReason}
              {resolveCard}
              {resolveNickname}
              {onOpenCard}
              {mentionCandidates}
              composerPlaceholder="Message the room…"
            />
          </div>
        </section>
      </aside>
```

Insert the invited banner directly above the `{#if $voiceState.selfModMuted}` block:

```svelte
    {#if $voiceState.selfInvited && !inviteHandled}
      <div class="th-invited-banner" role="status" data-testid="th-invited-banner">
        <span>You've been invited to speak — Unmute?</span>
        <button type="button" class="btn-primary" data-testid="th-invite-accept" onclick={acceptInvite}>Unmute</button>
        <button type="button" class="ctrl" data-testid="th-invite-dismiss" onclick={dismissInvite}>Dismiss</button>
      </div>
    {/if}
```

Append rail styles inside `<style>`:

```css
  /* ── Right rail ── */
  .th-rail {
    width: 340px;
    flex-shrink: 0;
    border-left: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 1rem 0.75rem;
    min-height: 0;
    overflow-y: auto;
  }
  .th-queue-empty { margin: 0; font-size: 0.8rem; color: var(--text-muted); }
  .th-queue-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .th-queue-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    border-radius: 6px;
    background: var(--bg-secondary);
  }
  .th-queue-row.dimmed { opacity: 0.55; }
  .q-num {
    font-family: var(--font-mono);
    font-size: 0.78rem;
    color: var(--text-muted);
    width: 16px;
    text-align: center;
    flex-shrink: 0;
  }
  .q-num.first { color: var(--gov-clay); font-weight: 700; }
  .q-avatar {
    width: 26px;
    height: 26px;
    border-radius: 50%;
    object-fit: cover;
    display: block;
    flex-shrink: 0;
  }
  .q-avatar-fallback { background: var(--bg-tertiary); border: 1px solid var(--border); }
  .q-text { display: flex; flex-direction: column; min-width: 0; flex: 1; }
  .q-name {
    font-size: 0.82rem;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .q-wants { font-size: 0.7rem; color: var(--text-muted); }
  .q-invited {
    font-size: 0.72rem;
    color: var(--text-muted);
    font-style: italic;
  }
  .q-invite {
    border: 1px solid var(--accent);
    background: transparent;
    color: var(--accent);
    padding: 3px 10px;
    border-radius: 5px;
    font-size: 0.75rem;
    cursor: pointer;
    flex-shrink: 0;
  }
  .q-invite.primary {
    background: var(--accent);
    color: var(--on-accent);
  }
  .q-invite:hover:not(:disabled) { filter: brightness(1.08); }
  .q-invite:disabled { opacity: 0.5; cursor: not-allowed; }
  .th-backchannel {
    flex: 1;
    min-height: 220px;
    display: flex;
    flex-direction: column;
  }
  .th-backchannel-feed {
    flex: 1;
    min-height: 0;
    display: flex;
    border: 1px solid var(--border);
    border-radius: 8px;
    overflow: hidden;
  }
  .th-invited-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--gov-clay-soft);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 45%, transparent);
    color: var(--gov-clay-deep);
    padding: 8px 12px;
    border-radius: 8px;
    margin: 0 16px 8px;
    font-size: 0.85rem;
  }
```

- [ ] **Step 4: Run** `npx vitest run src/lib/components/__tests__/TownHallView.test.ts` — all describes PASS; `npx tsc --noEmit`.

- [ ] **Step 5: Commit** `feat: TownHallView rail — speaker queue, motion card, backchannel, invited banner (ZEB-612 S5)`.

---

### Task 5: CommunityView routes townhall → TownHallView

**Files:**
- Modify: `src/lib/components/CommunityView.svelte` (import + the `kind` branch at ~line 441)
- Test: `src/lib/components/__tests__/CommunityView.test.ts`

- [ ] **Step 1: Write the failing test** (append; add a `townhallFloor` channel fixture beside `voiceLounge` with `kind: 'townhall'`):

```ts
  it('routes a townhall channel to TownHallView (ZEB-612 S5)', async () => {
    const { container, getByText } = await setup([townhallFloor], {
      votingAdapter: makeVotingAdapterStub(),
    });
    await waitFor(() => {
      expect(container.querySelector('.townhall-view')).toBeTruthy();
    });
    expect(container.querySelector('.voice-view')).toBeNull();
    expect(getByText("Join the assembly — you'll join muted.")).toBeTruthy();
  });
```

- [ ] **Step 2: Run** `npx vitest run src/lib/components/__tests__/CommunityView.test.ts -t townhall` — FAIL (renders `.voice-view`).

- [ ] **Step 3: Implement.** Add `import TownHallView from './TownHallView.svelte';` beside the VoiceChannelView import. Replace the S4-interim branch:

```svelte
      {#if activeChannel.kind === 'townhall'}
        <!-- ZEB-612 S5: townhall channels render the assembly view. -->
        {#if voiceSession}
          <TownHallView
            session={voiceSession}
            channelName={activeChannel.name}
            {communityId}
            channelId={activeChannel.channelId}
            onBeforeJoin={onBeforeVoiceJoin}
            {ownAddress}
            {myPower}
            adminQuorum={governance?.adminQuorum ?? null}
            {votingAdapter}
            {channelMessageService}
            {resolveCard}
            {resolveNickname}
            {onOpenCard}
            snapshotMessages={preForkSnapshot?.channelLog?.[activeChannel.channelId] ?? []}
            originalCommunityName={preForkSnapshot?.originalCommunityName ?? ''}
            forkedAtMs={preForkSnapshot?.forkedAtMs ?? 0}
            forkReason={preForkSnapshot?.forkReason ?? null}
            mentionCandidates={members
              .filter((m) => m.status === 'joined')
              .map((m) => ({
                ownerId: m.address,
                label: resolveMentionLabel(m.address, resolveNickname, resolveCard),
              }))}
            onOpenProposals={() => { activeView = 'proposals'; }}
          />
        {/if}
      {:else if activeChannel.kind === 'voice'}
        {#if voiceSession}
          <VoiceChannelView
            session={voiceSession}
            channelName={activeChannel.name}
            {communityId}
            channelId={activeChannel.channelId}
            onBeforeJoin={onBeforeVoiceJoin}
          />
        {/if}
      {:else}
```

(The trailing `{:else}` keeps the existing `ChannelMessageFeed` branch unchanged.)

- [ ] **Step 4: Run** `npx vitest run src/lib/components/__tests__/CommunityView.test.ts` (whole file — the voice/text routing pins must stay green); `npx tsc --noEmit`.

- [ ] **Step 5: Commit** `feat: CommunityView routes townhall channels to TownHallView (ZEB-612 S5)`.

---

### Task 6: Final gates

- [ ] **Step 1:** `npx tsc --noEmit` — clean.
- [ ] **Step 2:** `npx vitest run` — full frontend suite green.
- [ ] **Step 3:** `npx vitest run src/style-token-guard.test.ts` — budget-0 (no raw colors added).
- [ ] **Step 4:** No Rust changes this slice; run `cd src-tauri && cargo fmt --all -- --check` as a no-op sanity (CI runs it regardless).
- [ ] **Step 5: Commit** anything outstanding; open the PR with the premise pins (1–4 above) leading the body.

## Self-review notes

- Spec §6 coverage: header (LIVE dot + present) ✅ / agenda omitted ✅ / spotlight incl. MOD pill, elapsed timer, waveform, reduced-motion, empty state ✅ / grid badges + 18-cap overflow ✅ / queue (numbered, clay #1, 26px avatar, wants-to-speak, invite gating, invited dim) ✅ / motion card (idle → live 300s Tier-1 in-card, DRAFT card with quorum bar + async fallback + proposals link) ✅ / backchannel (same channelId, "Message the room…") ✅ / self affordances (raise-hand toggle, invited banner accept/dismiss) ✅ / not-joined pane ✅.
- Type consistency: `speakerQueue`/`RosterMember.handRaisedAt/invited`/`selfInvited`/`setHand`/`inviteToSpeak` verified against `voice-session.ts` on main; `CreateTier1PollArgs`, `PollMeta.channel_id?: number[]`, `Hlc {w,l,d}` verified against `types/voting.ts`; `CountChip {label,value,tone}` / `TallyBar {segments,height,label}` verified.
- The `VotingPollCreatedPayload` (camelCase `channelId`/`pollId` strings) matches the Rust `#[serde(rename_all = "camelCase")]` payload struct.
