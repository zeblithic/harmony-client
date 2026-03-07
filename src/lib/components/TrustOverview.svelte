<script lang="ts">
  import type { TrustScore, TrustEdge } from '../trust-score';
  import {
    getIdentity,
    getCompliance,
    getAssociation,
    getEndorsement,
    trustScoreColor,
  } from '../trust-score';
  import TrustBadge from './TrustBadge.svelte';

  interface PeerInfo {
    address: string;
    displayName: string;
  }

  type SortKey = 'name' | 'myScore' | 'theirScore' | 'identity' | 'compliance' | 'association' | 'endorsement';

  let {
    localAddress,
    peers,
    edges,
  }: {
    localAddress: string;
    peers: PeerInfo[];
    edges: TrustEdge[];
  } = $props();

  let sortKey: SortKey = $state('name');
  let sortAsc: boolean = $state(true);

  interface PeerRow {
    address: string;
    displayName: string;
    myScore: TrustScore | null;
    theirScore: TrustScore | null;
  }

  let rows = $derived.by(() => {
    const myEdges = new Map(
      edges.filter((e) => e.source === localAddress).map((e) => [e.target, e.score]),
    );
    const theirEdges = new Map(
      edges.filter((e) => e.target === localAddress).map((e) => [e.source, e.score]),
    );
    return peers.map((p) => ({
      address: p.address,
      displayName: p.displayName,
      myScore: myEdges.get(p.address) ?? null,
      theirScore: theirEdges.get(p.address) ?? null,
    }));
  });

  let sortedRows = $derived.by(() => {
    const copy = [...rows];
    copy.sort((a, b) => {
      let cmp = 0;
      switch (sortKey) {
        case 'name':
          cmp = a.displayName.localeCompare(b.displayName);
          break;
        case 'myScore':
          cmp = (a.myScore ?? -1) - (b.myScore ?? -1);
          break;
        case 'theirScore':
          cmp = (a.theirScore ?? -1) - (b.theirScore ?? -1);
          break;
        case 'identity':
          cmp = (a.myScore !== null ? getIdentity(a.myScore) : -1) -
                (b.myScore !== null ? getIdentity(b.myScore) : -1);
          break;
        case 'compliance':
          cmp = (a.myScore !== null ? getCompliance(a.myScore) : -1) -
                (b.myScore !== null ? getCompliance(b.myScore) : -1);
          break;
        case 'association':
          cmp = (a.myScore !== null ? getAssociation(a.myScore) : -1) -
                (b.myScore !== null ? getAssociation(b.myScore) : -1);
          break;
        case 'endorsement':
          cmp = (a.myScore !== null ? getEndorsement(a.myScore) : -1) -
                (b.myScore !== null ? getEndorsement(b.myScore) : -1);
          break;
      }
      return sortAsc ? cmp : -cmp;
    });
    return copy;
  });

  function toggleSort(key: SortKey) {
    if (sortKey === key) {
      sortAsc = !sortAsc;
    } else {
      sortKey = key;
      sortAsc = false;
    }
  }

  function sortIndicator(key: SortKey): string {
    if (sortKey !== key) return '';
    return sortAsc ? ' \u25B2' : ' \u25BC';
  }

  function formatScore(score: TrustScore | null): string {
    if (score === null) return '\u2014';
    return `0x${score.toString(16).padStart(2, '0')}`;
  }

  function dimValue(score: TrustScore | null, extractor: (s: TrustScore) => number): string {
    if (score === null) return '\u2014';
    return String(extractor(score));
  }

  const columns: { key: SortKey; label: string }[] = [
    { key: 'name', label: 'Name' },
    { key: 'myScore', label: 'My Score' },
    { key: 'theirScore', label: 'Their Score' },
    { key: 'identity', label: 'Id' },
    { key: 'compliance', label: 'Comp' },
    { key: 'association', label: 'Assoc' },
    { key: 'endorsement', label: 'Endorse' },
  ];
</script>

<table class="trust-table" role="grid">
  <thead>
    <tr>
      {#each columns as col}
        <th scope="col" aria-sort={sortKey === col.key ? (sortAsc ? 'ascending' : 'descending') : 'none'}>
          <button
            class="sort-button"
            onclick={() => toggleSort(col.key)}
            aria-label="Sort by {col.label}"
          >
            {col.label}{sortIndicator(col.key)}
          </button>
        </th>
      {/each}
    </tr>
  </thead>
  <tbody>
    {#each sortedRows as row (row.address)}
      <tr class="trust-row">
        <td class="name-cell">
          <TrustBadge score={row.myScore} />
          {row.displayName}
        </td>
        <td style="color: {trustScoreColor(row.myScore)}">{formatScore(row.myScore)}</td>
        <td style="color: {trustScoreColor(row.theirScore)}">{formatScore(row.theirScore)}</td>
        <td>{dimValue(row.myScore, getIdentity)}</td>
        <td>{dimValue(row.myScore, getCompliance)}</td>
        <td>{dimValue(row.myScore, getAssociation)}</td>
        <td>{dimValue(row.myScore, getEndorsement)}</td>
      </tr>
    {/each}
  </tbody>
</table>

<style>
  .trust-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
    color: var(--text-primary, #dcddde);
    background: var(--bg-primary, #1e1f22);
  }

  thead {
    background: var(--bg-secondary, #2f3136);
    position: sticky;
    top: 0;
    z-index: 1;
  }

  th {
    text-align: left;
    padding: 0;
    border-bottom: 1px solid var(--bg-tertiary, #40444b);
    font-weight: 600;
    color: var(--text-secondary, #b9bbbe);
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }

  .sort-button {
    display: flex;
    align-items: center;
    gap: 4px;
    width: 100%;
    padding: 8px 12px;
    border: none;
    background: none;
    color: inherit;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
    text-align: left;
    white-space: nowrap;
  }

  .sort-button:hover {
    color: var(--text-primary, #dcddde);
    background: var(--bg-tertiary, #40444b);
  }

  .sort-button:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
    border-radius: 2px;
  }

  .trust-row {
    border-bottom: 1px solid var(--bg-secondary, #2f3136);
  }

  .trust-row:hover {
    background: var(--bg-secondary, #2f3136);
  }

  td {
    padding: 6px 12px;
    white-space: nowrap;
    color: var(--text-primary, #dcddde);
  }

  .name-cell {
    font-weight: 500;
    display: flex;
    align-items: center;
    gap: 8px;
  }
</style>
