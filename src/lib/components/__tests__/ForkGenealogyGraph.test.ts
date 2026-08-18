import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import ForkGenealogyGraph from '../ForkGenealogyGraph.svelte';
import type { CommunityLineageDto, ForkDescendantDto } from '../../types';

// DelegationGraph-style tests: pin structure (counts, classes, selection,
// navigation), never pixel positions — those live in
// fork-genealogy-layout.test.ts where the math is pure.

function threeDeepLineage(): CommunityLineageDto {
  return {
    forkedFrom: '22'.repeat(16),
    forkedAtWallMs: 1_710_000_000_000,
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
}

function descendants(): ForkDescendantDto[] {
  return [
    {
      forkSpaceId: '33'.repeat(16),
      forkerAddr: 'ab'.repeat(16),
      forkerDisplayName: null,
      forkedAtWallMs: 1_716_000_000_000,
      locallyKnown: true,
      reason: 'Child wanted its own charter',
    },
    {
      forkSpaceId: '44'.repeat(16),
      forkerAddr: 'cd'.repeat(16),
      forkerDisplayName: null,
      forkedAtWallMs: 1_717_000_000_000,
      locallyKnown: false,
      reason: null,
    },
  ];
}

describe('ForkGenealogyGraph', () => {
  it('renders one card button per node, SVG connectors per edge, self selected by default', () => {
    const { container } = render(ForkGenealogyGraph, {
      props: {
        lineage: threeDeepLineage(),
        descendants: descendants(),
        localNavIds: new Set(['33'.repeat(16)]),
      },
    });
    // 2 ancestors + self + 2 descendants.
    expect(container.querySelectorAll('button.genealogy-card')).toHaveLength(5);
    // 2 chain edges + 2 fan edges.
    expect(container.querySelectorAll('path.genealogy-edge')).toHaveLength(4);
    // Self card exists and is the default selection shown in the panel.
    const self = container.querySelector('.genealogy-card.card-self');
    expect(self?.getAttribute('aria-pressed')).toBe('true');
    const inspect = container.querySelector('.genealogy-inspect');
    expect(inspect?.textContent).toContain('The Fork');
    expect(inspect?.textContent).toContain('Direct forks');
    expect(inspect?.textContent).toContain('2');
  });

  it('self inspect shows the fork-reason quote and per-fork reason cards', () => {
    const { container } = render(ForkGenealogyGraph, {
      props: {
        lineage: threeDeepLineage(),
        descendants: descendants(),
        localNavIds: new Set<string>(),
      },
    });
    const inspect = container.querySelector('.genealogy-inspect')!;
    expect(inspect.textContent).toContain('“Treasury split”');
    // One reason card per descendant; only the with-reason one quotes.
    expect(inspect.querySelectorAll('.inspect-fork-card')).toHaveLength(2);
    expect(inspect.textContent).toContain('“Child wanted its own charter”');
    expect(inspect.querySelectorAll('.fork-card-reason')).toHaveLength(1);
    expect(inspect.textContent).toContain('an unknown member');
  });

  it('clicking a known descendant selects it; Open fires onNavigate with the hex id', async () => {
    const onNavigate = vi.fn();
    const resolveLocalName = vi.fn((id: string) =>
      id === '33'.repeat(16) ? 'Charter Fork' : null,
    );
    const { container, getByText } = render(ForkGenealogyGraph, {
      props: {
        lineage: threeDeepLineage(),
        descendants: descendants(),
        localNavIds: new Set(['33'.repeat(16)]),
        resolveLocalName,
        onNavigate,
      },
    });
    // The resolved name also appears in the self-inspect fork list —
    // click the CARD (button) specifically.
    const card = Array.from(container.querySelectorAll('button.genealogy-card')).find(
      (b) => b.textContent?.includes('Charter Fork'),
    )!;
    await fireEvent.click(card);
    const inspect = container.querySelector('.genealogy-inspect')!;
    expect(inspect.textContent).toContain('Charter Fork');
    expect(inspect.textContent).toContain('“Child wanted its own charter”');
    await fireEvent.click(getByText(/open community/i));
    expect(onNavigate).toHaveBeenCalledWith('33'.repeat(16));
  });

  it('unknown node inspect shows the not-a-member line and no Open action', async () => {
    const { container, queryByText } = render(ForkGenealogyGraph, {
      props: {
        lineage: threeDeepLineage(),
        descendants: descendants(),
        localNavIds: new Set<string>(),
      },
    });
    // The unknown descendant renders as truncated hex; the same text also
    // appears in the self-inspect fork list, so target the card button.
    const card = Array.from(container.querySelectorAll('button.genealogy-card')).find(
      (b) => b.textContent?.includes('0x44444444…'),
    )!;
    await fireEvent.click(card);
    expect(container.querySelector('.genealogy-inspect')?.textContent).toContain(
      "You're not a member",
    );
    expect(queryByText(/open community/i)).toBeNull();
  });

  it('edge chips carry the clay fork glyph and reason snippets', () => {
    const { container } = render(ForkGenealogyGraph, {
      props: {
        lineage: threeDeepLineage(),
        descendants: descendants(),
        localNavIds: new Set<string>(),
      },
    });
    const chips = Array.from(container.querySelectorAll('.genealogy-edge-chip')).map(
      (c) => c.textContent ?? '',
    );
    expect(chips).toHaveLength(4);
    expect(chips.every((t) => t.includes('⑂'))).toBe(true);
    expect(chips.some((t) => t.includes('Mid split from Root'))).toBe(true);
    expect(chips.some((t) => t.includes('Treasury split'))).toBe(true);
  });

  // ZEB-962: the component dropped its copy-pasted local `nonEmpty` for the
  // canonical `display-label` helper. This pins the guard the dedup must keep:
  // a whitespace-only node name resolves to the truncated-hex placeholder, never
  // a blank card. (Present names stay covered by the rendering tests above.)
  it('renders the truncated-hex placeholder for a whitespace-only node name', () => {
    const { container } = render(ForkGenealogyGraph, {
      props: {
        lineage: {
          forkedFrom: '22'.repeat(16),
          forkedAtWallMs: 1_710_000_000_000,
          parentLineage: [
            { spaceId: '11'.repeat(16), name: '   ', forkedAtWallMs: null, reason: null },
          ],
          selfSpaceId: '00'.repeat(16),
          selfName: 'The Fork',
          forkReason: null,
        },
        descendants: [],
        localNavIds: new Set<string>(),
      },
    });
    const names = Array.from(container.querySelectorAll('.card-name')).map((n) => n.textContent);
    expect(names).toContain('0x11111111…');
    expect(names).not.toContain('   ');
  });

  it('root community with no forks renders a single self card and no edges', () => {
    const { container } = render(ForkGenealogyGraph, {
      props: {
        lineage: {
          forkedFrom: null,
          forkedAtWallMs: null,
          parentLineage: [],
          selfSpaceId: '00'.repeat(16),
          selfName: 'Root Community',
          forkReason: null,
        },
        descendants: [],
        localNavIds: new Set<string>(),
      },
    });
    expect(container.querySelectorAll('button.genealogy-card')).toHaveLength(1);
    expect(container.querySelectorAll('path.genealogy-edge')).toHaveLength(0);
    expect(container.textContent).toContain('You are here');
  });
});
