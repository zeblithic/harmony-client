import { describe, it, expect } from 'vitest';
import { mapRedeemInviteError } from '../redeem-invite-errors';

describe('mapRedeemInviteError', () => {
  it('maps BootstrapMissing to "incomplete" summary', () => {
    const r = mapRedeemInviteError('BootstrapMissing: invite-only payload missing admin bootstrap');
    expect(r.summary).toContain('incomplete');
    expect(r.tag).toBe('bootstrap_missing');
  });

  it('maps BootstrapSignatureInvalid to "invalid" summary', () => {
    const r = mapRedeemInviteError('BootstrapSignatureInvalid: ed25519 verify failed');
    expect(r.summary).toContain('signature is invalid');
    expect(r.tag).toBe('bootstrap_signature_invalid');
  });

  it('maps BootstrapAddressMismatch to "malformed" summary', () => {
    const r = mapRedeemInviteError('BootstrapAddressMismatch: identity_pub address != admin_addr');
    expect(r.summary).toContain('malformed');
    expect(r.tag).toBe('bootstrap_address_mismatch');
  });

  it('maps BootstrapActorMismatch', () => {
    const r = mapRedeemInviteError('BootstrapActorMismatch: bootstrap.actor != admin_addr');
    expect(r.tag).toBe('bootstrap_actor_mismatch');
  });

  it('maps BootstrapCommunityMismatch', () => {
    const r = mapRedeemInviteError('BootstrapCommunityMismatch');
    expect(r.summary).toContain('different community');
    expect(r.tag).toBe('bootstrap_community_mismatch');
  });

  it('maps BootstrapKindInvalid', () => {
    const r = mapRedeemInviteError('BootstrapKindInvalid: kind != Join');
    expect(r.tag).toBe('bootstrap_kind_invalid');
  });

  it('maps BootstrapInvalidPubkey', () => {
    const r = mapRedeemInviteError('BootstrapInvalidPubkey: ed25519 from_bytes failed');
    expect(r.tag).toBe('bootstrap_invalid_pubkey');
  });

  it('maps BootstrapInsertFailed', () => {
    const r = mapRedeemInviteError('BootstrapInsertFailed(Foo)');
    expect(r.summary).toContain("Couldn't bootstrap");
    expect(r.tag).toBe('bootstrap_insert_failed');
  });

  it('maps timeout', () => {
    const r = mapRedeemInviteError('redeem_invite timed out after 15s');
    expect(r.summary).toContain('offline');
    expect(r.tag).toBe('inviter_timeout');
  });

  it('maps already-member', () => {
    const r = mapRedeemInviteError('already a member of community aabbccdd');
    expect(r.summary).toContain("already in");
    expect(r.tag).toBe('already_member');
  });

  it('maps malformed URL', () => {
    const r = mapRedeemInviteError('invalid invite URL: missing scheme');
    expect(r.summary).toContain("doesn't look like");
    expect(r.tag).toBe('malformed_url');
  });

  it('falls through to network failure', () => {
    const r = mapRedeemInviteError('network unreachable');
    expect(r.summary).toContain('network');
    expect(r.tag).toBe('network_failure');
  });

  it('maps the pkarr relay warm-up error to an actionable, non-misleading message (ZEB-879)', () => {
    const r = mapRedeemInviteError('no relays available (all on cooldown or unreachable)');
    expect(r.tag).toBe('relays_warming_up');
    expect(r.summary).toBe('The network is still warming up.');
    expect(r.hint).toMatch(/try again/i);
    // must NOT fall through to the generic network-failure fallback
    expect(r.tag).not.toBe('network_failure');
  });
});
