/**
 * ZEB-648 — shared Tier-2 governance-surface formatting.
 *
 * Extracted so the ConvictionProposalCard header pill and the
 * CommunityProposalsPanel doc-column breadcrumb pill render one proposal
 * with ONE lifecycle label/variant (they previously diverged: the card
 * passed descriptive labels while the breadcrumb fell back to StatusPill's
 * glyphed defaults), and so half-life copy is derived one way everywhere.
 */
import type { StatusPillVariant } from './status-pill-variant';
import type { Tier2ProposalExport } from '../../types/voting';

type Tier2Lifecycle = Tier2ProposalExport['lifecycle'];

/**
 * Canonical Tier-2 lifecycle → Commons pill {variant, label}. The single
 * source of the lifecycle→variant mapping (was duplicated as a `switch` in
 * the card and a nested ternary in the panel) and of the human label
 * (descriptive copy: the ThresholdReached label spells out the 24h
 * contestability window per spec §5).
 */
export function tier2LifecyclePill(lifecycle: Tier2Lifecycle): {
  variant: StatusPillVariant;
  label: string;
} {
  switch (lifecycle) {
    case 'Open':
      return { variant: 'open', label: 'Open' };
    case 'ThresholdReached':
      return { variant: 'passing', label: 'Threshold reached — 24h window' };
    case 'Finalized':
      return { variant: 'passed', label: 'Finalized' };
    case 'Archived':
      return { variant: 'archived', label: 'Archived' };
    default:
      // Exhaustive today; keep a graceful fallback if the Rust enum grows a
      // variant before this map is updated (label = the raw string).
      return { variant: 'archived', label: lifecycle };
  }
}

/**
 * Whole-number threshold percent for display. Floors (does not round) so a
 * proposal still below threshold never displays "100% reached" next to an
 * "Open" lifecycle pill — threshold-reached fires only at an exact 100, and
 * `toFixed(0)` would round anything in [99.5, 100) up to a premature "100".
 * Input is already clamped to [0, 100] by `convictionPercent`; the clamp
 * here is defensive.
 */
export function thresholdPercent(pct: number): number {
  if (!Number.isFinite(pct)) return 0;
  return Math.max(0, Math.min(100, Math.floor(pct)));
}

/**
 * Human half-life for a conviction proposal. `Math.round(s / 86_400)` alone
 * printed "0d" for any half-life under ~12h (ZEB-648 item 3); step down to
 * hours, then minutes, so a real duration always shows.
 */
export function formatHalfLife(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return '0m';
  const days = seconds / 86_400;
  if (days >= 1) return `${Math.round(days)}d`;
  const hours = seconds / 3_600;
  if (hours >= 1) return `${Math.round(hours)}h`;
  const minutes = seconds / 60;
  return `${Math.max(1, Math.round(minutes))}m`;
}
