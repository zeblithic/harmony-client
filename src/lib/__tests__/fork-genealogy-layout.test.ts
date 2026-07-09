import { describe, it, expect } from 'vitest';
import {
  layoutGenealogy,
  GENEALOGY_CARD_W,
  GENEALOGY_CARD_H,
  GENEALOGY_V_GAP,
  GENEALOGY_FAN_GAP,
  GENEALOGY_MARGIN,
} from '../fork-genealogy-layout';
import type { CommunityLineageDto, ForkDescendantDto } from '../types';

function lineage(overrides: Partial<CommunityLineageDto> = {}): CommunityLineageDto {
  return {
    forkedFrom: null,
    forkedAtWallMs: null,
    parentLineage: [],
    selfSpaceId: '00'.repeat(16),
    selfName: 'Self',
    forkReason: null,
    ...overrides,
  };
}

function desc(idByte: string, reason: string | null = null): ForkDescendantDto {
  return {
    forkSpaceId: idByte.repeat(16),
    forkerAddr: 'ab'.repeat(16),
    forkerDisplayName: null,
    forkedAtWallMs: 1_716_000_000_000,
    locallyKnown: false,
    reason,
  };
}

describe('layoutGenealogy', () => {
  it('root community with no forks: one centered self node, no edges', () => {
    const l = layoutGenealogy(lineage(), []);
    expect(l.nodes).toHaveLength(1);
    expect(l.edges).toHaveLength(0);
    expect(l.nodes[0].kind).toBe('self');
    expect(l.width).toBe(GENEALOGY_CARD_W + 2 * GENEALOGY_MARGIN);
    expect(l.height).toBe(GENEALOGY_CARD_H + 2 * GENEALOGY_MARGIN);
    // Centered: x = (width - CARD_W) / 2 = MARGIN.
    expect(l.nodes[0].x).toBe(GENEALOGY_MARGIN);
    expect(l.nodes[0].y).toBe(GENEALOGY_MARGIN);
  });

  it('chain of 2 ancestors: vertical stack, child metadata labels each edge', () => {
    const l = layoutGenealogy(
      lineage({
        forkedFrom: '22'.repeat(16),
        forkedAtWallMs: 1_710_000_000_000,
        forkReason: 'Self split from Mid',
        parentLineage: [
          { spaceId: '11'.repeat(16), name: 'Root', forkedAtWallMs: null, reason: null },
          {
            spaceId: '22'.repeat(16),
            name: 'Mid',
            forkedAtWallMs: 1_650_000_000_000,
            reason: 'Mid split from Root',
          },
        ],
      }),
      [],
    );
    expect(l.nodes.map((n) => n.kind)).toEqual(['ancestor', 'ancestor', 'self']);
    // Chain shares one x; levels descend by CARD_H + V_GAP.
    const xs = new Set(l.nodes.map((n) => n.x));
    expect(xs.size).toBe(1);
    expect(l.nodes[1].y - l.nodes[0].y).toBe(GENEALOGY_CARD_H + GENEALOGY_V_GAP);
    expect(l.nodes[2].y - l.nodes[1].y).toBe(GENEALOGY_CARD_H + GENEALOGY_V_GAP);
    // Edge 0 (Root→Mid) carries Mid's metadata; edge 1 (Mid→Self) self's.
    expect(l.edges).toHaveLength(2);
    expect(l.edges[0].reason).toBe('Mid split from Root');
    expect(l.edges[0].childSpaceId).toBe('22'.repeat(16));
    expect(l.edges[1].reason).toBe('Self split from Mid');
    expect(l.edges[1].forkedAtWallMs).toBe(1_710_000_000_000);
    // Straight vertical connectors for the chain.
    expect(l.edges[0].path).toMatch(/^M [\d.]+ [\d.]+ V [\d.]+$/);
  });

  it('4-descendant fan: symmetric row below self, elbow connectors', () => {
    const descendants = [desc('33', 'first why'), desc('44'), desc('55'), desc('66')];
    const l = layoutGenealogy(lineage(), descendants);
    const fan = l.nodes.filter((n) => n.kind === 'descendant');
    expect(fan).toHaveLength(4);
    // All at the same y, one level below self.
    const self = l.nodes.find((n) => n.kind === 'self')!;
    expect(new Set(fan.map((n) => n.y)).size).toBe(1);
    expect(fan[0].y - self.y).toBe(GENEALOGY_CARD_H + GENEALOGY_V_GAP);
    // Evenly spaced.
    expect(fan[1].x - fan[0].x).toBe(GENEALOGY_CARD_W + GENEALOGY_FAN_GAP);
    // Fan is symmetric about the stage center.
    const left = fan[0].x - GENEALOGY_MARGIN;
    const right = l.width - GENEALOGY_MARGIN - (fan[3].x + GENEALOGY_CARD_W);
    expect(left).toBe(right);
    // Self stays centered even when the fan is wider than one card.
    expect(self.x + GENEALOGY_CARD_W / 2).toBe(l.width / 2);
    // Elbow connectors (M … V … H … V …) with the child's metadata.
    expect(l.edges).toHaveLength(4);
    expect(l.edges[0].path).toMatch(/^M [\d.]+ [\d.]+ V [\d.]+ H [\d.]+ V [\d.]+$/);
    expect(l.edges[0].reason).toBe('first why');
    expect(l.edges[0].childSpaceId).toBe('33'.repeat(16));
  });

  it('deterministic: same input yields identical output', () => {
    const l = lineage({
      forkedFrom: '11'.repeat(16),
      parentLineage: [
        { spaceId: '11'.repeat(16), name: 'Root', forkedAtWallMs: null, reason: null },
      ],
    });
    const ds = [desc('33'), desc('44')];
    expect(layoutGenealogy(l, ds)).toEqual(layoutGenealogy(l, ds));
  });
});
