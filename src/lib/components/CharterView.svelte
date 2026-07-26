<script lang="ts">
  /**
   * ZEB-608 — CharterView (spec D3). GENERATES a community's constitution
   * from live governance state: POWER_THRESHOLDS, the member roster, the
   * get_community_governance quorum, and finalized Tier-3 polls. Nothing
   * rendered here is stored charter prose — every number is traceable to
   * real data (spec §5 "no invented data"; tier cards describe REAL
   * mechanics in prose, no invented percentages per §0.3).
   */
  import type { VotingAdapter } from '../voting-adapter';
  import type { Tier3PollSummary } from '../types/voting';
  import type { CommunityMember } from '../types';
  import { POWER_THRESHOLDS } from '../types';
  import { shortAddr } from '../short-addr';
  import PipMeter from './governance/PipMeter.svelte';
  import RoleBadge from './governance/RoleBadge.svelte';

  let {
    communityId,
    communityName,
    members,
    adminQuorum,
    adapter,
    onProposeAmendment,
  }: {
    communityId: string;
    communityName: string;
    members: CommunityMember[];
    /** Current materialized admin quorum (get_community_governance, D1).
     *  `null` = not yet loaded OR the governance fetch failed — the quorum
     *  card then shows a neutral '…' rather than a fake value, keeping the
     *  charter's "live governance state" promise honest (PR #410 CodeRabbit). */
    adminQuorum: number | null;
    adapter: VotingAdapter;
    /** Fired by "Propose amendment" — the parent switches to the
     *  Constitutional tab (spec §0.6: create-form prefill is YAGNI v1). */
    onProposeAmendment: () => void;
  } = $props();

  let joinedMembers = $derived(members.filter((m) => m.status === 'joined'));
  let adminCount = $derived(
    joinedMembers.filter((m) => m.power >= POWER_THRESHOLDS.setPower).length,
  );

  // Finalized Tier-3 polls = the real amendment record (spec §0.4).
  // null = not yet loaded OR load failed — the header pill shows a neutral
  // '…' (never a fake zero) and Article III renders without the list.
  let polls = $state<Tier3PollSummary[] | null>(null);

  $effect(() => {
    const cid = communityId;
    // Per-run cancellation flag: a bare `cid !== communityId` compare misses
    // the A→B→A case (same id returned), letting a late first-A fetch clobber
    // the re-entered A. The cleanup fires on every re-run, so only the latest
    // visit's result is applied (PR #410 Qodo).
    let cancelled = false;
    polls = null;
    void adapter
      .listTier3Polls(cid)
      .then((list) => {
        if (cancelled) return;
        polls = list;
      })
      .catch(() => {
        if (cancelled) return;
        polls = null;
      });
    return () => {
      cancelled = true;
    };
  });

  // A Tier-3 poll ALWAYS carries a synthetic "status quo" candidate that
  // advances to the STAR runoff; when it wins, the poll still finalizes
  // (stage 'fi') with winnerText === STATUS_QUO_TEXT — the mini-public
  // considered an amendment and UPHELD the status quo (backend:
  // synthesize_status_quo + the list-IPC winnerText fallback, lib.rs). Those
  // are ratified DECISIONS but NOT adopted amendments: they must not inflate
  // the "ratified amendments" count, and the raw sentinel must never leak
  // into the constitution. (Final-review I-1; spec §0.4 finalized ≠ adopted.)
  const STATUS_QUO_TEXT = '<status quo>';

  let finalized = $derived(
    (polls ?? [])
      .filter((p) => p.stage === 'fi')
      .slice()
      .sort((a, b) => a.pollCreateHlcMs - b.pollCreateHlcMs),
  );
  function isUpheld(p: Tier3PollSummary): boolean {
    return p.winnerText === STATUS_QUO_TEXT;
  }
  // An adopted amendment is a finalized poll that has an actual winner text
  // and did not uphold the status quo. Guarding on winnerText (which the DTO
  // types as nullable) keeps the pill count consistent with the record, whose
  // "Ratified: …" row already renders only when winnerText is present
  // (PR #410 CodeRabbit).
  function isAdopted(p: Tier3PollSummary): boolean {
    return !!p.winnerText && !isUpheld(p);
  }
  let adoptedCount = $derived(finalized.filter(isAdopted).length);
  let ratifiedPillText = $derived(
    polls === null
      ? '✓ …'
      : adoptedCount === 0
        ? '✓ No amendments yet'
        : `✓ ${adoptedCount} ratified amendment${adoptedCount === 1 ? '' : 's'}`,
  );

  // pollCreateHlcMs is CREATION time — the finalization HLC is not in the
  // summary (spec §0.4), so the record honestly labels the date "proposed".
  function proposedDate(hlcMs: number): string {
    return new Date(hlcMs).toISOString().slice(0, 10);
  }

  // The quorum caption is derived, not hardcoded: with a quorum of 1 a single
  // admin genuinely can act alone, so the "no single admin can act alone"
  // claim only holds at quorum ≥ 2 (PR #410 CodeRabbit).
  let quorumCaption = $derived(
    adminQuorum === null
      ? ''
      : adminQuorum <= 1
        ? 'Any single admin can enact admin actions on their own.'
        : `${adminQuorum} of ${adminCount} admins must co-sign admin actions. No single admin can act alone.`,
  );

  // Capability matrix (spec D3 Article I): derived from the REAL consumer
  // checks — invite ≥ invite (0, i.e. any joined member; backend verify at
  // community_membership.rs:3159); channel manage/moderate & kick ≥ kick (50);
  // set-roles/kick-admin/change-quorum ≥ setPower (100). Join-vouch is
  // deliberately absent: enforcement is member-level (rs:246) while the
  // moderation UI surfaces requests at ≥50 — a row would mislead either way.
  // ● = can, — = cannot.
  const MATRIX_ROWS: Array<{ action: string; member: boolean; mod: boolean; admin: boolean }> = [
    { action: 'Post, vote, propose & invite', member: true, mod: true, admin: true },
    { action: 'Delegate & recall', member: true, mod: true, admin: true },
    { action: 'Fork the community', member: true, mod: true, admin: true },
    { action: 'Manage & moderate channels', member: false, mod: true, admin: true },
    { action: 'Remove & ban members', member: false, mod: true, admin: true },
    { action: 'Set roles · change decision rules', member: false, mod: false, admin: true },
  ];
</script>

<article class="charter-view" aria-label={`${communityName} charter`}>
  <div class="doc-column">
    <header class="charter-header">
      <div class="header-main">
        <h1 class="charter-title">📜 {communityName} Charter</h1>
        <div class="meta-row">
          <span class="ratified-pill">{ratifiedPillText}</span>
          <span class="members-bound"
            >{joinedMembers.length} member{joinedMembers.length === 1 ? '' : 's'} bound</span
          >
        </div>
      </div>
      <button type="button" class="propose-btn" onclick={onProposeAmendment}>
        Propose amendment
      </button>
    </header>

    <section class="charter-section" aria-label="Preamble">
      <h2 class="eyebrow">Preamble</h2>
      <p class="preamble">
        This charter is {communityName}'s constitution, generated from its live governance
        state. Every clause below reflects the rules as they are enforced today, and every
        clause can be changed by the members it governs.
      </p>
    </section>

    <section class="charter-section" aria-label="Article I — Membership and roles">
      <h2 class="eyebrow">Article I · Membership &amp; roles</h2>
      <p class="lede">
        Roles are earned, granted, and revoked as a numeric power level. Three named bands:
      </p>
      <div class="role-cards">
        <div class="role-card">
          <RoleBadge role="member" />
          <span class="power-req">power {POWER_THRESHOLDS.invite}</span>
          <p class="role-desc">
            Full civic standing: posts, votes, proposes, invites, delegates — and can fork.
          </p>
        </div>
        <div class="role-card">
          <RoleBadge role="mod" />
          <span class="power-req">power ≥ {POWER_THRESHOLDS.kick}</span>
          <p class="role-desc">
            Stewards the day-to-day space: channels and join requests.
          </p>
        </div>
        <div class="role-card">
          <RoleBadge role="admin" />
          <span class="power-req">power ≥ {POWER_THRESHOLDS.setPower}</span>
          <p class="role-desc">
            Holds the keys that change the rules — always under quorum.
          </p>
        </div>
      </div>
      <table class="capability-matrix">
        <thead>
          <tr>
            <th class="action-col">Capability</th>
            <th>Member</th>
            <th>Mod</th>
            <th>Admin</th>
          </tr>
        </thead>
        <tbody>
          {#each MATRIX_ROWS as row (row.action)}
            <tr>
              <td class="action-col">{row.action}</td>
              <td class="cap" class:can={row.member} aria-label={row.member ? 'Can' : 'Cannot'}>
                <span aria-hidden="true">{row.member ? '●' : '—'}</span>
              </td>
              <td class="cap" class:can={row.mod} aria-label={row.mod ? 'Can' : 'Cannot'}>
                <span aria-hidden="true">{row.mod ? '●' : '—'}</span>
              </td>
              <td class="cap" class:can={row.admin} aria-label={row.admin ? 'Can' : 'Cannot'}>
                <span aria-hidden="true">{row.admin ? '●' : '—'}</span>
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      <p class="matrix-footnote">Thresholds are platform-wide in v1.</p>
    </section>

    <section class="charter-section" aria-label="Article II — How we decide">
      <h2 class="eyebrow">Article II · How we decide</h2>
      <p class="lede">Proposals move through three tiers. Higher stakes, higher bar.</p>
      <div class="tier-cards">
        <div class="tier-card">
          <h3 class="tier-name">Tier 1 · Poll</h3>
          <p class="tier-desc">
            Multi-option approval polls. Options, window and eligibility are set per poll.
            Non-binding sentiment.
          </p>
        </div>
        <div class="tier-card">
          <h3 class="tier-name">Tier 2 · Motion</h3>
          <p class="tier-desc">
            Binding conviction votes. Support accumulates over time (7-day half-life by
            default) toward a dynamic threshold; delegable, recallable.
          </p>
        </div>
        <div class="tier-card">
          <h3 class="tier-name">Tier 3 · Charter</h3>
          <p class="tier-desc">
            Amends how the community works. A sortition-selected mini-public deliberates,
            drafts and ratifies by STAR ballot.
          </p>
        </div>
      </div>
      <div class="quorum-card">
        <h3 class="quorum-heading">Admin quorum</h3>
        {#if adminQuorum === null}
          <span class="quorum-value">…</span>
          <p class="quorum-caption">Loading current quorum…</p>
        {:else}
          <span class="quorum-value">{adminQuorum} of {adminCount}</span>
          <PipMeter filled={adminQuorum} total={adminCount} label="Admin quorum meter" />
          <p class="quorum-caption">{quorumCaption}</p>
        {/if}
      </div>
    </section>

    <section class="charter-section" aria-label="Article III — Amendment">
      <h2 class="eyebrow">Article III · Amendment</h2>
      <div class="amend-callout">
        <p class="amend-text">
          ✎ No clause here is permanent. Any member may open a Tier-3 proposal to amend how
          {communityName} works; if it ratifies, the change is signed by the mini-public and
          recorded. Every ratified decision stays on the record.
        </p>
      </div>
      {#if finalized.length > 0}
        <section class="on-record" aria-label="On the record">
          <h3 class="or-heading">On the record</h3>
          <ul class="amendment-list">
            {#each finalized as p (p.pollId)}
              <li class="amendment-row">
                <span class="amendment-date">{proposedDate(p.pollCreateHlcMs)} · proposed</span>
                <span class="amendment-title">{p.proposalText}</span>
                {#if isUpheld(p)}
                  <span class="amendment-outcome upheld">Upheld: status quo</span>
                {:else if p.winnerText}
                  <span class="amendment-outcome">Ratified: {p.winnerText}</span>
                {/if}
                <span class="amendment-proposer">{shortAddr(p.proposer)}</span>
              </li>
            {/each}
          </ul>
        </section>
      {/if}
    </section>
  </div>
</article>

<style>
  /* ZEB-772: keep clipped content reachable. `overflow-y: auto` scrolls only
     the block axis — the inline axis stayed `visible`, so when a competing
     panel squeezed this column the overflowing controls ("Propose amendment",
     the role cards) were not merely cramped but unreachable, with no scroll
     affordance anywhere. The grid floor in Layout.svelte stops the squeeze
     being severe; this makes the residue reachable rather than lost, which is
     the part that must hold at ANY width. `min-width: 0` stays: it is what
     lets long unbroken strings wrap instead of forcing the column wider. */
  .charter-view {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    overflow-x: auto;
    padding: 24px 20px 48px;
    background: var(--bg-primary);
  }
  .doc-column {
    max-width: 780px;
    margin: 0 auto;
    display: flex;
    flex-direction: column;
    gap: 26px;
  }
  .charter-header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
  }
  .charter-title {
    margin: 0 0 6px;
    font-family: var(--font-display);
    font-weight: 500;
    font-size: 2rem;
    line-height: 1.15;
    color: var(--text-primary);
  }
  .meta-row {
    display: flex;
    align-items: center;
    gap: 10px;
    font-family: var(--font-mono);
    font-size: 12px;
    color: var(--text-muted);
  }
  .ratified-pill {
    color: var(--primary-deep);
    background: var(--primary-soft);
    padding: 2px 10px;
    border-radius: 20px;
    font-weight: 600;
    white-space: nowrap;
  }
  .propose-btn {
    background: var(--gov-clay-soft);
    color: var(--gov-clay-deep);
    border: 1px solid color-mix(in srgb, var(--gov-clay) 35%, var(--surface-raised));
    border-radius: 7px;
    padding: 7px 14px;
    font: inherit;
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
  }
  .propose-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .eyebrow {
    margin: 0 0 10px;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .preamble {
    margin: 0;
    font-family: var(--font-display);
    font-style: italic;
    font-size: 15.5px;
    line-height: 1.65;
    color: var(--text-primary);
  }
  .lede {
    margin: 0 0 12px;
    font-size: 0.85rem;
    line-height: 1.5;
    color: var(--text-secondary);
  }
  .role-cards {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 10px;
    margin-bottom: 14px;
  }
  .role-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-start;
  }
  .power-req {
    font-family: var(--font-mono);
    font-size: 0.7rem;
    color: var(--text-muted);
  }
  .role-desc {
    margin: 0;
    font-size: 0.75rem;
    line-height: 1.45;
    color: var(--text-secondary);
  }
  .capability-matrix {
    width: 100%;
    border-collapse: collapse;
    table-layout: fixed;
    font-size: 0.8rem;
  }
  .capability-matrix th,
  .capability-matrix td {
    border: 1px solid var(--border);
    padding: 6px 10px;
    text-align: center;
  }
  .capability-matrix th {
    background: var(--bg-secondary);
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .capability-matrix th:not(.action-col),
  .capability-matrix td:not(.action-col) {
    width: 92px;
  }
  .capability-matrix .action-col {
    text-align: left;
    color: var(--text-primary);
  }
  .cap {
    color: var(--vote-abstain);
    font-family: var(--font-mono);
  }
  .cap.can {
    color: var(--vote-for);
  }
  .matrix-footnote {
    margin: 8px 0 0;
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--text-muted);
  }
  .tier-cards {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-bottom: 14px;
  }
  .tier-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-left: 3px solid var(--gov-clay);
    border-radius: 8px;
    padding: 10px 14px;
  }
  .tier-name {
    margin: 0 0 4px;
    font-size: 0.85rem;
    font-weight: 600;
    color: var(--text-primary);
  }
  .tier-desc {
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.5;
    color: var(--text-secondary);
  }
  .quorum-card {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: var(--shadow-e1);
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .quorum-heading {
    margin: 0;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .quorum-value {
    font-family: var(--font-mono);
    font-weight: 600;
    font-size: 0.9rem;
    color: var(--text-primary);
  }
  .quorum-caption {
    margin: 0;
    font-size: 0.72rem;
    line-height: 1.45;
    color: var(--text-muted);
  }
  .amend-callout {
    background: var(--primary-soft);
    border: 1px solid var(--primary-border);
    border-radius: 10px;
    padding: 12px 15px;
  }
  .amend-text {
    margin: 0;
    font-size: 0.85rem;
    line-height: 1.55;
    color: var(--primary-deep);
  }
  .on-record {
    margin-top: 14px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .or-heading {
    margin: 0;
    font-size: 0.68rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .amendment-list {
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .amendment-row {
    background: var(--surface-raised);
    border: 1px solid var(--border);
    border-radius: 8px;
    padding: 8px 12px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .amendment-date {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--text-muted);
  }
  .amendment-title {
    font-weight: 600;
    font-size: 0.82rem;
    color: var(--text-primary);
  }
  .amendment-outcome {
    font-size: 0.78rem;
    color: var(--vote-for);
  }
  .amendment-outcome.upheld {
    color: var(--text-muted);
  }
  .amendment-proposer {
    font-family: var(--font-mono);
    font-size: 0.68rem;
    color: var(--text-muted);
  }
</style>
