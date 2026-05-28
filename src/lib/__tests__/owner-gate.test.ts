import { describe, it, expect } from 'vitest';
import { classifyOwnerIdentity } from '../owner-gate';
import type { StartNodeResponse } from '../types/onboarding';

function resp(over: Partial<StartNodeResponse> = {}): StartNodeResponse {
  // Minimal shape; only hasOwnerIdentity is load-bearing for classification.
  return { hasOwnerIdentity: false, ...over } as StartNodeResponse;
}

describe('classifyOwnerIdentity', () => {
  it('returns "present" when start_node succeeded with an owner identity', () => {
    expect(classifyOwnerIdentity(resp({ hasOwnerIdentity: true }), false)).toBe('present');
  });

  it('returns "missing" when start_node succeeded and explicitly reports no owner', () => {
    expect(classifyOwnerIdentity(resp({ hasOwnerIdentity: false }), false)).toBe('missing');
  });

  it('returns "missing" for an older backend that omits hasOwnerIdentity (forward-compat)', () => {
    expect(classifyOwnerIdentity(resp({ hasOwnerIdentity: undefined }), false)).toBe('missing');
  });

  it('returns "error" when start_node threw — must NOT be treated as missing', () => {
    // This is the convergent-bug guard: a returning user whose node failed to
    // start must never be routed into the mint gate (mint would deadlock).
    expect(classifyOwnerIdentity(null, true)).toBe('error');
  });

  it('returns "error" defensively when there is no response and no explicit failure', () => {
    expect(classifyOwnerIdentity(null, false)).toBe('error');
  });
});
