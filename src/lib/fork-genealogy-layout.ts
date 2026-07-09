/**
 * ZEB-649: pure, deterministic layout for the fork genealogy graph
 * (`ForkGenealogyGraph.svelte`). No d3, no simulation — the honest
 * topology our DTOs can know is a "caterpillar": a vertical chain of
 * ancestors (root → … → immediate parent) down to self, then self's
 * direct descendants fanned horizontally below. Sibling branches
 * (descendants of ancestors) are unknowable — `list_community_forks`
 * is Joined-gated — so the layout renders exactly what's real.
 *
 * Extracted from the component so coordinate math is unit-testable
 * without a DOM (the DelegationGraph lesson: pin structure, not pixels
 * from a simulation — here the pixels ARE deterministic, so we pin them).
 */

import type { CommunityLineageDto, ForkDescendantDto } from './types';

export const GENEALOGY_CARD_W = 230;
export const GENEALOGY_CARD_H = 64;
/** Vertical distance between a card's bottom and the next level's top. */
export const GENEALOGY_V_GAP = 56;
/** Horizontal gap between fanned descendant cards. */
export const GENEALOGY_FAN_GAP = 24;
/** Stage padding on every side. */
export const GENEALOGY_MARGIN = 32;

export interface GenealogyNode {
  spaceId: string;
  kind: 'ancestor' | 'self' | 'descendant';
  /** Frozen display name; '' for descendants (the component resolves via
   *  resolveLocalName / truncated-hex, same ladder as ForkLineageTree). */
  name: string;
  /** wall_ms this node forked from its predecessor; null for the root
   *  and unknown hops. */
  forkedAtWallMs: number | null;
  /** Why this node forked from its predecessor; null when unknown. */
  reason: string | null;
  /** Only meaningful for descendants (clickability gate). Self and
   *  locally-known ancestors are resolved by the component. */
  locallyKnown: boolean;
  /** Card top-left corner, in stage coordinates. */
  x: number;
  y: number;
}

export interface GenealogyEdge {
  /** SVG path (elbow: `M … V … H … V …`), parent bottom → child top. */
  path: string;
  /** Label chip anchor (centered on the elbow's midpoint). */
  labelX: number;
  labelY: number;
  /** The child's fork metadata — the component formats the chip text. */
  forkedAtWallMs: number | null;
  reason: string | null;
  childSpaceId: string;
}

export interface GenealogyLayout {
  nodes: GenealogyNode[];
  edges: GenealogyEdge[];
  width: number;
  height: number;
}

export function layoutGenealogy(
  lineage: CommunityLineageDto,
  descendants: ForkDescendantDto[],
): GenealogyLayout {
  const chainLevels = lineage.parentLineage.length + 1; // ancestors + self
  const hasFan = descendants.length > 0;
  const levels = chainLevels + (hasFan ? 1 : 0);

  const fanWidth =
    descendants.length > 0
      ? descendants.length * GENEALOGY_CARD_W +
        (descendants.length - 1) * GENEALOGY_FAN_GAP
      : 0;
  const contentWidth = Math.max(GENEALOGY_CARD_W, fanWidth);
  const width = contentWidth + 2 * GENEALOGY_MARGIN;
  const height =
    levels * GENEALOGY_CARD_H + (levels - 1) * GENEALOGY_V_GAP + 2 * GENEALOGY_MARGIN;

  const centerX = width / 2;
  const chainX = centerX - GENEALOGY_CARD_W / 2;
  const levelY = (level: number) =>
    GENEALOGY_MARGIN + level * (GENEALOGY_CARD_H + GENEALOGY_V_GAP);

  const nodes: GenealogyNode[] = [];
  const edges: GenealogyEdge[] = [];

  lineage.parentLineage.forEach((entry, i) => {
    nodes.push({
      spaceId: entry.spaceId,
      kind: 'ancestor',
      name: entry.name,
      forkedAtWallMs: entry.forkedAtWallMs,
      reason: entry.reason,
      locallyKnown: false, // resolved by the component via localNavIds
      x: chainX,
      y: levelY(i),
    });
  });

  const selfLevel = lineage.parentLineage.length;
  nodes.push({
    spaceId: lineage.selfSpaceId,
    kind: 'self',
    name: lineage.selfName,
    forkedAtWallMs: lineage.forkedAtWallMs,
    reason: lineage.forkReason,
    locallyKnown: true,
    x: chainX,
    y: levelY(selfLevel),
  });

  // Chain edges: straight vertical drops, bottom-center → top-center.
  // Edge i connects chain node i → chain node i+1; the CHILD's fork
  // metadata labels the edge (parallel to ParentLineageEntry semantics).
  for (let i = 0; i < chainLevels - 1; i++) {
    const child = nodes[i + 1];
    const topY = levelY(i) + GENEALOGY_CARD_H;
    const bottomY = levelY(i + 1);
    edges.push({
      path: `M ${centerX} ${topY} V ${bottomY}`,
      labelX: centerX,
      labelY: (topY + bottomY) / 2,
      forkedAtWallMs: child.forkedAtWallMs,
      reason: child.reason,
      childSpaceId: child.spaceId,
    });
  }

  // Descendant fan: a row below self, elbow connectors from self's
  // bottom-center down to the midpoint, across, and into each child.
  if (hasFan) {
    const fanY = levelY(selfLevel + 1);
    const fanStartX = centerX - fanWidth / 2;
    const selfBottomY = levelY(selfLevel) + GENEALOGY_CARD_H;
    const midY = (selfBottomY + fanY) / 2;

    descendants.forEach((desc, i) => {
      const x = fanStartX + i * (GENEALOGY_CARD_W + GENEALOGY_FAN_GAP);
      const childCenterX = x + GENEALOGY_CARD_W / 2;
      nodes.push({
        spaceId: desc.forkSpaceId,
        kind: 'descendant',
        name: '',
        forkedAtWallMs: desc.forkedAtWallMs,
        reason: desc.reason,
        locallyKnown: desc.locallyKnown,
        x,
        y: fanY,
      });
      edges.push({
        path: `M ${centerX} ${selfBottomY} V ${midY} H ${childCenterX} V ${fanY}`,
        labelX: childCenterX,
        labelY: (midY + fanY) / 2,
        forkedAtWallMs: desc.forkedAtWallMs,
        reason: desc.reason,
        childSpaceId: desc.forkSpaceId,
      });
    });
  }

  return { nodes, edges, width, height };
}
