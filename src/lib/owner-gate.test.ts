import { describe, expect, it } from 'vitest';
import { classifyOwnerIdentity } from './owner-gate';
import type { StartNodeResponse } from './types/onboarding';

function resp(overrides: Partial<StartNodeResponse> = {}): StartNodeResponse {
  return { nodeAddr: 'iroh:abc', ...overrides };
}

describe('classifyOwnerIdentity (ZEB-668 S2 revoked state)', () => {
  it('classifies selfRevoked before present/missing', () => {
    expect(classifyOwnerIdentity(resp({ hasOwnerIdentity: false, selfRevoked: true }), false)).toBe(
      'revoked',
    );
  });

  it('error still wins when start_node failed', () => {
    expect(classifyOwnerIdentity(null, true)).toBe('error');
  });

  it('existing states are unchanged', () => {
    expect(classifyOwnerIdentity(resp({ hasOwnerIdentity: true }), false)).toBe('present');
    expect(classifyOwnerIdentity(resp({ hasOwnerIdentity: false }), false)).toBe('missing');
    expect(classifyOwnerIdentity(resp(), false)).toBe('missing');
  });

  it('missing selfRevoked field (older backend) never classifies as revoked', () => {
    expect(classifyOwnerIdentity(resp({ hasOwnerIdentity: true, selfRevoked: undefined }), false)).toBe(
      'present',
    );
  });
});

describe('classifyOwnerIdentity (ZEB-836 enrollment-missing state)', () => {
  it('classifies selfEnrollmentMissing before missing (avoids the mint-gate trap)', () => {
    expect(
      classifyOwnerIdentity(
        resp({ hasOwnerIdentity: false, selfEnrollmentMissing: true }),
        false,
      ),
    ).toBe('enrollment-missing');
  });

  it('selfRevoked outranks selfEnrollmentMissing', () => {
    expect(
      classifyOwnerIdentity(
        resp({ hasOwnerIdentity: false, selfRevoked: true, selfEnrollmentMissing: true }),
        false,
      ),
    ).toBe('revoked');
  });

  it('error still wins over selfEnrollmentMissing when start_node failed', () => {
    expect(classifyOwnerIdentity(null, true)).toBe('error');
  });

  it('missing selfEnrollmentMissing field (older backend) never classifies as enrollment-missing', () => {
    expect(
      classifyOwnerIdentity(resp({ hasOwnerIdentity: false, selfEnrollmentMissing: undefined }), false),
    ).toBe('missing');
  });
});
