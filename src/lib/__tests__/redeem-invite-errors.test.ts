import { describe, it, expect } from 'vitest';
import {
  mapRedeemInviteError,
  redeemInviteCopy,
  toRedeemInviteError,
  type RedeemInviteErrorCode,
} from '../redeem-invite-errors';

// ZEB-885: the mapper now switches on the backend's structured error CODE, not
// on regex-matching raw prose. These tests feed codes (the real wire shape),
// unlike the pre-885 suite which fed fabricated CamelCase strings the backend
// never actually emitted.

const ALL_CODES: RedeemInviteErrorCode[] = [
  'bootstrap_missing',
  'bootstrap_actor_mismatch',
  'bootstrap_community_mismatch',
  'bootstrap_signature_invalid',
  'bootstrap_kind_invalid',
  'invite_url_malformed',
  'inviter_enrollment_invalid',
  'invite_token_missing',
  'missing_admin_identity_pub',
  'inviter_unreachable',
  'relays_warming_up',
  'node_not_ready',
  'generation_changed',
  'engine_insert_failed',
  'join_failed',
  'internal',
  'unknown',
];

describe('mapRedeemInviteError', () => {
  it('maps a known code to its copy and preserves the raw message', () => {
    const r = mapRedeemInviteError({
      code: 'bootstrap_signature_invalid',
      message: 'redeem_invite: admin_bootstrap signature verify failed',
    });
    expect(r.summary).toBe('Invite link signature is invalid.');
    expect(r.code).toBe('bootstrap_signature_invalid');
    expect(r.raw).toBe('redeem_invite: admin_bootstrap signature verify failed');
    expect(r.hint).toBeTruthy();
  });

  it('gives every known code a non-fallback summary', () => {
    const fallback = redeemInviteCopy('unknown').summary;
    for (const code of ALL_CODES) {
      const r = mapRedeemInviteError({ code, message: 'x' });
      expect(r.code).toBe(code);
      if (code !== 'unknown') {
        expect(r.summary, `code ${code} should have its own copy`).not.toBe(fallback);
      }
    }
  });

  it('maps the pkarr relay warm-up code to an actionable, non-misleading message (ZEB-879)', () => {
    const r = mapRedeemInviteError({
      code: 'relays_warming_up',
      message: 'no relays available (all on cooldown or unreachable)',
    });
    expect(r.code).toBe('relays_warming_up');
    expect(r.summary).toBe('The network is still warming up.');
    expect(r.hint).toMatch(/try again/i);
  });

  it('bootstrap failures now surface their specific copy (repairs the pre-885 dead-regex bug)', () => {
    // Pre-885 these fell through to the generic network_failure fallback
    // because the Display prose never contained the CamelCase variant name.
    const r = mapRedeemInviteError({
      code: 'bootstrap_missing',
      message: 'redeem_invite: invite-only payload missing admin bootstrap',
    });
    expect(r.summary).toContain('incomplete');
    expect(r.summary).not.toBe(redeemInviteCopy('unknown').summary);
  });

  it('falls back to the unknown copy for a code the frontend has not learned, keeping the raw code', () => {
    const r = mapRedeemInviteError({
      code: 'some_future_code' as RedeemInviteErrorCode,
      message: 'brand new backend failure',
    });
    expect(r.summary).toBe(redeemInviteCopy('unknown').summary);
    // the original code is preserved for the disclosure / bug report
    expect(r.code).toBe('some_future_code');
    expect(r.raw).toBe('brand new backend failure');
  });
});

describe('redeemInviteCopy', () => {
  it('returns the unknown fallback for an unrecognized code', () => {
    expect(redeemInviteCopy('not_a_real_code')).toEqual(redeemInviteCopy('unknown'));
  });
});

describe('toRedeemInviteError', () => {
  it('passes through a structured { code, message } rejection', () => {
    const e = toRedeemInviteError({ code: 'inviter_unreachable', message: 'pkarr resolve failed' });
    expect(e.code).toBe('inviter_unreachable');
    expect(e.message).toBe('pkarr resolve failed');
  });

  it('degrades an Error to the unknown code, keeping its message', () => {
    const e = toRedeemInviteError(new Error('Error: node stopping; operation rejected'));
    expect(e.code).toBe('unknown');
    expect(e.message).toBe('Error: node stopping; operation rejected');
  });

  it('degrades a bare string to the unknown code', () => {
    const e = toRedeemInviteError('some raw string rejection');
    expect(e.code).toBe('unknown');
    expect(e.message).toBe('some raw string rejection');
  });

  it('handles a coded object missing its message field', () => {
    const e = toRedeemInviteError({ code: 'internal' });
    expect(e.code).toBe('internal');
    expect(typeof e.message).toBe('string');
  });
});
