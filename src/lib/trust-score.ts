export type TrustScore = number; // 0-255, uint8

export interface TrustEdge {
  /** SHA-256 address of the scorer */
  source: string;
  /** SHA-256 address of the scored peer */
  target: string;
  /** 8-bit trust score */
  score: TrustScore;
  /** When this score was last set (unix ms) */
  timestamp: number;
}

/** Dimension labels for display */
export const DIMENSIONS = ['Identity', 'Compliance', 'Association', 'Endorsement'] as const;
export type TrustDimension = (typeof DIMENSIONS)[number];

function clamp02(v: number): number {
  return Math.max(0, Math.min(3, Math.floor(v)));
}

export function buildScore(
  identity: number,
  compliance: number,
  association: number,
  endorsement: number,
): TrustScore {
  return (
    (clamp02(identity) & 0x3) |
    ((clamp02(compliance) & 0x3) << 2) |
    ((clamp02(association) & 0x3) << 4) |
    ((clamp02(endorsement) & 0x3) << 6)
  );
}

export function getIdentity(score: TrustScore): number {
  return score & 0x3;
}

export function getCompliance(score: TrustScore): number {
  return (score >> 2) & 0x3;
}

export function getAssociation(score: TrustScore): number {
  return (score >> 4) & 0x3;
}

export function getEndorsement(score: TrustScore): number {
  return (score >> 6) & 0x3;
}

export function trustScoreColor(score: TrustScore | null): string {
  if (score === null) return '#72767d';
  if (score < 64) return '#ed4245';
  if (score < 128) return '#faa61a';
  if (score < 192) return '#43b581';
  return '#5865f2';
}

export function trustScoreLabel(score: TrustScore | null): string {
  if (score === null) return 'unscored';
  if (score < 64) return 'low trust';
  if (score < 128) return 'cautious';
  if (score < 192) return 'trusted';
  return 'highly trusted';
}
