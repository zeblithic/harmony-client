import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ForkLineageTree from '../ForkLineageTree.svelte';
import type { CommunityLineageDto, ForkDescendantDto } from '../../types';

function emptyLineage(selfName = 'My Community'): CommunityLineageDto {
  return {
    forkedFrom: null,
    forkedAtWallMs: null,
    parentLineage: [],
    selfSpaceId: '00'.repeat(16),
    selfName,
    forkReason: null,
  };
}

describe('ForkLineageTree', () => {
  it('renders_non_fork_no_descendants_minimally', () => {
    const { getByText } = render(ForkLineageTree, {
      props: {
        lineage: emptyLineage(),
        descendants: [],
        localNavIds: new Set<string>(),
      },
    });

    expect(getByText(/You are here/)).toBeTruthy();
    expect(getByText(/My Community/)).toBeTruthy();
    expect(getByText(/no forks yet/i)).toBeTruthy();
  });

  it('renders_ancestors_only_for_leaf_fork', () => {
    const lineage: CommunityLineageDto = {
      ...emptyLineage('Leaf'),
      forkedFrom: 'aa'.repeat(16),
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [
        { spaceId: '11'.repeat(16), name: 'Root C', forkedAtWallMs: null, reason: null },
        { spaceId: '22'.repeat(16), name: 'Middle B', forkedAtWallMs: 1_650_000_000_000, reason: null },
      ],
    };

    const { getByText } = render(ForkLineageTree, {
      props: { lineage, descendants: [], localNavIds: new Set() },
    });

    expect(getByText(/Root C/)).toBeTruthy();
    expect(getByText(/Middle B/)).toBeTruthy();
    expect(getByText(/Leaf/)).toBeTruthy();
  });

  it('falls back to the truncated space-id when a lineage name is blank (PR #411 Qodo)', () => {
    // Legacy/corrupted snapshots can carry an empty frozen name; the card must
    // show the real space-id, never a blank label (team nonEmpty() idiom).
    const lineage: CommunityLineageDto = {
      ...emptyLineage('Leaf'),
      forkedFrom: 'ab'.repeat(16),
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [{ spaceId: 'ab'.repeat(16), name: '', forkedAtWallMs: null, reason: null }],
    };
    const { getByText, container } = render(ForkLineageTree, {
      props: { lineage, descendants: [], localNavIds: new Set() },
    });
    expect(getByText(/0xabababab…/)).toBeTruthy();
    // The avatar initial must derive from the SAME resolved display as the
    // label (matching the descendant rows), so a blank name yields '0' from
    // the 0x… id — never the raw-name '⑂' placeholder (PR #411 Qodo re-flag).
    expect(
      container.querySelector('.lineage-ancestor .node-avatar')?.textContent?.trim(),
    ).toBe('0');
  });

  it('renders_descendants_only_for_root_with_forks', () => {
    const descendants: ForkDescendantDto[] = [
      {
        forkSpaceId: '33'.repeat(16),
        forkerAddr: 'ab'.repeat(16),
        forkerDisplayName: 'Maya',
        forkedAtWallMs: 1_715_000_000_000,
        locallyKnown: true,
        reason: null,
      },
      {
        forkSpaceId: '44'.repeat(16),
        forkerAddr: 'cd'.repeat(16),
        forkerDisplayName: null,
        forkedAtWallMs: 1_716_000_000_000,
        locallyKnown: false,
        reason: null,
      },
    ];

    const { getByText } = render(ForkLineageTree, {
      props: { lineage: emptyLineage(), descendants, localNavIds: new Set() },
    });

    // Maya is rendered as the forker (descendant row shows "by {forker}").
    expect(getByText(/Maya/)).toBeTruthy();
    expect(getByText(/an unknown member/i)).toBeTruthy();
  });

  it('renders_full_tree_three_deep_two_descendants', () => {
    const lineage: CommunityLineageDto = {
      forkedFrom: '22'.repeat(16),
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [
        { spaceId: '11'.repeat(16), name: 'C', forkedAtWallMs: null, reason: null },
        { spaceId: '22'.repeat(16), name: 'B', forkedAtWallMs: 1_650_000_000_000, reason: null },
      ],
      selfSpaceId: '33'.repeat(16),
      selfName: 'A',
      forkReason: null,
    };
    const descendants: ForkDescendantDto[] = [
      {
        forkSpaceId: '44'.repeat(16),
        forkerAddr: 'ab'.repeat(16),
        forkerDisplayName: 'Maya',
        forkedAtWallMs: 1_715_000_000_000,
        locallyKnown: true,
        reason: null,
      },
      {
        forkSpaceId: '55'.repeat(16),
        forkerAddr: 'cd'.repeat(16),
        forkerDisplayName: 'Sam',
        forkedAtWallMs: 1_716_000_000_000,
        locallyKnown: true,
        reason: null,
      },
    ];

    const { getAllByRole } = render(ForkLineageTree, {
      props: {
        lineage,
        descendants,
        localNavIds: new Set([
          '11'.repeat(16),
          '22'.repeat(16),
          '44'.repeat(16),
          '55'.repeat(16),
        ]),
      },
    });

    const treeitems = getAllByRole('treeitem');
    // 2 ancestors + 1 self + 2 descendants = 5
    expect(treeitems.length).toBe(5);
  });

  it('renders_truncation_marker_for_overlong_lineage', () => {
    const overlongLineage = Array.from({ length: 18 }, (_, i) => ({
      spaceId: i.toString(16).padStart(2, '0').repeat(16),
      name: `ancestor_${i}`,
      forkedAtWallMs: i === 0 ? null : i,
      reason: null,
    }));

    const lineage: CommunityLineageDto = {
      ...emptyLineage('Deep Leaf'),
      forkedFrom: 'ff'.repeat(16),
      forkedAtWallMs: 999_999,
      parentLineage: overlongLineage,
    };

    const { getByText } = render(ForkLineageTree, {
      props: { lineage, descendants: [], localNavIds: new Set() },
    });

    expect(getByText(/and 2 earlier ancestors/i)).toBeTruthy();
  });

  it('click_navigates_to_locally_known_community', async () => {
    const navigateSpy = vi.fn();
    const lineage: CommunityLineageDto = {
      ...emptyLineage('A'),
      forkedFrom: '22'.repeat(16),
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [
        { spaceId: '11'.repeat(16), name: 'Cool C', forkedAtWallMs: null, reason: null },
      ],
    };

    const { getByText } = render(ForkLineageTree, {
      props: {
        lineage,
        descendants: [],
        localNavIds: new Set(['11'.repeat(16)]),
        onNavigate: navigateSpy,
      },
    });

    await fireEvent.click(getByText(/Cool C/));
    expect(navigateSpy).toHaveBeenCalledWith('11'.repeat(16));
  });

  it('non_clickable_for_unknown_community', () => {
    const lineage: CommunityLineageDto = {
      ...emptyLineage('A'),
      forkedFrom: '22'.repeat(16),
      forkedAtWallMs: 1_700_000_000_000,
      parentLineage: [
        { spaceId: '11'.repeat(16), name: 'Cool C', forkedAtWallMs: null, reason: null },
      ],
    };

    const { container } = render(ForkLineageTree, {
      props: {
        lineage,
        descendants: [],
        localNavIds: new Set(), // empty — Cool C is NOT locally known
      },
    });

    // The ancestor row should NOT be a <button> when not locally known.
    // Self row is non-clickable; only locally-known rows are buttons.
    // Empty localNavIds → NO ancestor/descendant rows are buttons.
    const buttons = container.querySelectorAll('button');
    expect(buttons.length).toBe(0);
  });

  it('renders_phase1_fork_immediate_parent_row_from_forked_from', () => {
    // R1-1 regression: for Phase 1 forks the backend `get_community_lineage`
    // IPC synthesizes a single ParentLineageEntry from the immediate-parent
    // pointer when CommunityState.parent_lineage is empty but forked_from
    // is set. The component just iterates `parent_lineage`, so this test
    // pins the rendering behaviour from the frontend's perspective: when
    // the DTO carries that one synthesized entry, the ancestor row renders
    // AND the "(no forks yet)" empty hint is suppressed.
    const lineage: CommunityLineageDto = {
      forkedFrom: 'aa'.repeat(16),
      forkedAtWallMs: null, // Phase 1: forked_at_wall_ms not set
      parentLineage: [
        {
          spaceId: 'aa'.repeat(16),
          name: 'Parent',
          forkedAtWallMs: null,
          reason: null,
        },
      ],
      selfSpaceId: '33'.repeat(16),
      selfName: 'Phase 1 Leaf',
      forkReason: null,
    };

    const { getByText, queryByText } = render(ForkLineageTree, {
      props: { lineage, descendants: [], localNavIds: new Set() },
    });

    expect(getByText(/Parent/)).toBeTruthy();
    expect(getByText(/Phase 1 Leaf/)).toBeTruthy();
    // hasAnyForks must be true → empty hint suppressed.
    expect(queryByText(/no forks yet/i)).toBeNull();
  });

  it('renders_descendant_name_when_locally_known', () => {
    // R3-1 regression: descendants with `locally_known: true` AND a hit in
    // localNavIds should render via the injected resolveLocalName resolver
    // — NOT a truncated hex string. Per spec §5.2.
    const descendants: ForkDescendantDto[] = [
      {
        forkSpaceId: '33'.repeat(16),
        forkerAddr: 'ab'.repeat(16),
        forkerDisplayName: 'Maya',
        forkedAtWallMs: 1_715_000_000_000,
        locallyKnown: true,
        reason: null,
      },
      {
        forkSpaceId: '44'.repeat(16),
        forkerAddr: 'cd'.repeat(16),
        forkerDisplayName: 'Sam',
        forkedAtWallMs: 1_716_000_000_000,
        locallyKnown: false, // not locally known — should stay as truncated hex
        reason: null,
      },
    ];
    const resolver = vi.fn((hex: string) =>
      hex === '33'.repeat(16) ? 'Resolved Fork Name' : null,
    );

    const { getByText, queryByText } = render(ForkLineageTree, {
      props: {
        lineage: emptyLineage('Root'),
        descendants,
        // Both forks "potentially known", but only the first has locallyKnown:true
        localNavIds: new Set(['33'.repeat(16), '44'.repeat(16)]),
        resolveLocalName: resolver,
      },
    });

    // Known + resolver hit → real name shown
    expect(getByText(/Resolved Fork Name/)).toBeTruthy();
    // Not-known descendant → still truncated hex (locallyKnown: false suppresses resolution)
    expect(getByText(/0x44444444…/)).toBeTruthy();
    // The full hex for the known fork must NOT appear (spec §5.2 — names not hex when known)
    // R4-2: previous regex `/33{16}/` meant "3 then exactly 16 more 3s" (17 total),
    // not the intended 32-char hex string. Use the explicit repetition count.
    expect(queryByText(/3{32}/)).toBeNull();
    // Resolver was called only for the locally-known descendant
    expect(resolver).toHaveBeenCalledWith('33'.repeat(16));
  });

  it('renders_descendant_truncated_hex_when_resolver_returns_null', () => {
    // R3-1 defense-in-depth: even when locallyKnown + localNavIds both
    // gate to "known", if the resolver returns null we fall back to
    // truncated hex (shouldn't happen in production but the fallback
    // keeps the row legible if NavService races the gating set).
    const descendants: ForkDescendantDto[] = [
      {
        forkSpaceId: '77'.repeat(16),
        forkerAddr: 'ee'.repeat(16),
        forkerDisplayName: 'Ari',
        forkedAtWallMs: 1_717_000_000_000,
        locallyKnown: true,
        reason: null,
      },
    ];

    const { getByText } = render(ForkLineageTree, {
      props: {
        lineage: emptyLineage('Root'),
        descendants,
        localNavIds: new Set(['77'.repeat(16)]),
        resolveLocalName: () => null,
      },
    });

    expect(getByText(/0x77777777…/)).toBeTruthy();
  });

  it('aria_current_page_on_self_row', () => {
    const { container } = render(ForkLineageTree, {
      props: {
        lineage: emptyLineage('My Self'),
        descendants: [],
        localNavIds: new Set(),
      },
    });

    const selfRow = container.querySelector('[aria-current="page"]');
    expect(selfRow).toBeTruthy();
    expect(selfRow?.textContent).toMatch(/My Self/);
  });

  it('shows the sage "You are here" badge on the self row', () => {
    const { container } = render(ForkLineageTree, {
      props: { lineage: emptyLineage('Root'), descendants: [], localNavIds: new Set() },
    });
    const selfRow = container.querySelector('[aria-current="page"]')!;
    const badge = selfRow.querySelector('.lineage-badge.you-here');
    expect(badge).toBeTruthy();
    expect(badge!.textContent).toMatch(/You are here/);
  });

  it('badges a locally-known descendant "Member" and an unknown one "not joined"', () => {
    const descendants: ForkDescendantDto[] = [
      { forkSpaceId: '33'.repeat(16), forkerAddr: 'ab'.repeat(16), forkerDisplayName: 'Maya', forkedAtWallMs: 1_715_000_000_000, locallyKnown: true, reason: null },
      { forkSpaceId: '44'.repeat(16), forkerAddr: 'cd'.repeat(16), forkerDisplayName: null, forkedAtWallMs: 1_716_000_000_000, locallyKnown: false, reason: null },
    ];
    const { container } = render(ForkLineageTree, {
      props: {
        lineage: emptyLineage('Root'),
        descendants,
        localNavIds: new Set(['33'.repeat(16)]),
        resolveLocalName: (hex: string) => (hex === '33'.repeat(16) ? 'Maya Fork' : null),
      },
    });
    const rows = container.querySelectorAll('.lineage-descendant');
    expect(rows.length).toBe(2);
    // Locally-known → sage Member badge, and it is a button.
    expect(rows[0].querySelector('.lineage-badge.member')?.textContent).toMatch(/Member/);
    expect(rows[0].querySelector('button')).toBeTruthy();
    // Unknown → muted "not joined", and zero buttons.
    expect(rows[1].querySelector('.lineage-badge.not-joined')?.textContent).toMatch(/not joined/);
    expect(rows[1].querySelector('button')).toBeNull();
  });

  it('ZEB-649: renders quoted reasons on self, ancestor, and descendant cards; omits when null', () => {
    const lineage: CommunityLineageDto = {
      forkedFrom: 'aa'.repeat(16),
      forkedAtWallMs: 1_715_000_000_000,
      parentLineage: [
        { spaceId: '11'.repeat(16), name: 'Root', forkedAtWallMs: null, reason: null },
        {
          spaceId: '22'.repeat(16),
          name: 'Mid',
          forkedAtWallMs: 1_650_000_000_000,
          reason: 'Mid split from Root',
        },
      ],
      selfSpaceId: '00'.repeat(16),
      selfName: 'The Fork',
      forkReason: 'Treasury split',
    };
    const descendants: ForkDescendantDto[] = [
      {
        forkSpaceId: '33'.repeat(16),
        forkerAddr: 'ab'.repeat(16),
        forkerDisplayName: null,
        forkedAtWallMs: 1_716_000_000_000,
        locallyKnown: false,
        reason: 'Child wanted its own charter',
      },
      {
        forkSpaceId: '44'.repeat(16),
        forkerAddr: 'cd'.repeat(16),
        forkerDisplayName: null,
        forkedAtWallMs: 1_717_000_000_000,
        locallyKnown: false,
        reason: null, // pre-ZEB-649 event — no quote line
      },
    ];
    const { container, getByText } = render(ForkLineageTree, {
      props: { lineage, descendants, localNavIds: new Set<string>() },
    });

    expect(getByText('\u201CTreasury split\u201D')).toBeTruthy();
    expect(getByText('\u201CMid split from Root\u201D')).toBeTruthy();
    expect(getByText('\u201CChild wanted its own charter\u201D')).toBeTruthy();
    // Exactly 3 reason lines: root ancestor + null-reason descendant render none.
    expect(container.querySelectorAll('.node-reason')).toHaveLength(3);
  });
});
