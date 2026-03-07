import { describe, it, expect } from 'vitest';
import {
  buildScore,
  getIdentity,
  getCompliance,
  getAssociation,
  getEndorsement,
  trustScoreColor,
  trustScoreLabel,
} from './trust-score';

describe('buildScore', () => {
  it('encodes all zeros as 0x00', () => {
    expect(buildScore(0, 0, 0, 0)).toBe(0x00);
  });

  it('encodes all threes as 0xFF', () => {
    expect(buildScore(3, 3, 3, 3)).toBe(0xFF);
  });

  it('encodes identity in bits 0-1', () => {
    expect(buildScore(2, 0, 0, 0)).toBe(0b00000010);
  });

  it('encodes compliance in bits 2-3', () => {
    expect(buildScore(0, 2, 0, 0)).toBe(0b00001000);
  });

  it('encodes association in bits 4-5', () => {
    expect(buildScore(0, 0, 2, 0)).toBe(0b00100000);
  });

  it('encodes endorsement in bits 6-7', () => {
    expect(buildScore(0, 0, 0, 2)).toBe(0b10000000);
  });

  it('encodes a mixed score correctly', () => {
    // identity=1, compliance=2, association=3, endorsement=0
    // bits 0-1: 01, bits 2-3: 10, bits 4-5: 11, bits 6-7: 00 = 0b00111001 = 0x39
    expect(buildScore(1, 2, 3, 0)).toBe(0b00111001);
  });

  it('clamps values above 3', () => {
    expect(buildScore(5, 0, 0, 0)).toBe(buildScore(3, 0, 0, 0));
  });

  it('clamps negative values to 0', () => {
    expect(buildScore(-1, 0, 0, 0)).toBe(buildScore(0, 0, 0, 0));
  });
});

describe('dimension extractors', () => {
  it('extracts identity from bits 0-1', () => {
    expect(getIdentity(0b00000011)).toBe(3);
    expect(getIdentity(0b00000010)).toBe(2);
    expect(getIdentity(0b00000000)).toBe(0);
  });

  it('extracts compliance from bits 2-3', () => {
    expect(getCompliance(0b00001100)).toBe(3);
    expect(getCompliance(0b00000100)).toBe(1);
  });

  it('extracts association from bits 4-5', () => {
    expect(getAssociation(0b00110000)).toBe(3);
    expect(getAssociation(0b00010000)).toBe(1);
  });

  it('extracts endorsement from bits 6-7', () => {
    expect(getEndorsement(0b11000000)).toBe(3);
    expect(getEndorsement(0b01000000)).toBe(1);
  });

  it('round-trips through buildScore', () => {
    const score = buildScore(1, 2, 3, 0);
    expect(getIdentity(score)).toBe(1);
    expect(getCompliance(score)).toBe(2);
    expect(getAssociation(score)).toBe(3);
    expect(getEndorsement(score)).toBe(0);
  });

  it('round-trips 0xFF', () => {
    expect(getIdentity(0xFF)).toBe(3);
    expect(getCompliance(0xFF)).toBe(3);
    expect(getAssociation(0xFF)).toBe(3);
    expect(getEndorsement(0xFF)).toBe(3);
  });
});

describe('trustScoreColor', () => {
  it('returns gray for null (unscored)', () => {
    expect(trustScoreColor(null)).toBe('#72767d');
  });

  it('returns red for low trust (0-63)', () => {
    expect(trustScoreColor(0)).toBe('#ed4245');
    expect(trustScoreColor(63)).toBe('#ed4245');
  });

  it('returns amber for cautious (64-127)', () => {
    expect(trustScoreColor(64)).toBe('#faa61a');
    expect(trustScoreColor(127)).toBe('#faa61a');
  });

  it('returns green for trusted (128-191)', () => {
    expect(trustScoreColor(128)).toBe('#43b581');
    expect(trustScoreColor(191)).toBe('#43b581');
  });

  it('returns accent blue for highly trusted (192-255)', () => {
    expect(trustScoreColor(192)).toBe('#5865f2');
    expect(trustScoreColor(255)).toBe('#5865f2');
  });
});

describe('trustScoreLabel', () => {
  it('returns "unscored" for null', () => {
    expect(trustScoreLabel(null)).toBe('unscored');
  });

  it('returns "low trust" for 0-63', () => {
    expect(trustScoreLabel(32)).toBe('low trust');
  });

  it('returns "cautious" for 64-127', () => {
    expect(trustScoreLabel(100)).toBe('cautious');
  });

  it('returns "trusted" for 128-191', () => {
    expect(trustScoreLabel(150)).toBe('trusted');
  });

  it('returns "highly trusted" for 192-255', () => {
    expect(trustScoreLabel(255)).toBe('highly trusted');
  });
});
