import { describe, it, expect } from 'vitest';
import { MemberCardService } from '../member-card-service';

describe('MemberCardService self-seed', () => {
  it('resolves the self owner_id to the local profile name/status synchronously', () => {
    const svc = new MemberCardService();
    svc.seedSelf('685e4ba76a8fde38ecbd2ff5c138df8c', { displayName: 'Jake (Koya Dev)', statusText: 'building' });
    expect(svc.resolve('685e4ba76a8fde38ecbd2ff5c138df8c')).toEqual({ displayName: 'Jake (Koya Dev)', statusText: 'building' });
  });
  it('returns undefined for an unknown owner_id (caller falls back to hash prefix)', () => {
    const svc = new MemberCardService();
    expect(svc.resolve('deadbeefdeadbeefdeadbeefdeadbeef')).toBeUndefined();
  });
  it('seedSelf overwrites the same owner_id on re-seed (profile edited)', () => {
    const svc = new MemberCardService();
    svc.seedSelf('aa'.repeat(16), { displayName: 'old', statusText: '' });
    svc.seedSelf('aa'.repeat(16), { displayName: 'new', statusText: 'hi' });
    expect(svc.resolve('aa'.repeat(16))).toEqual({ displayName: 'new', statusText: 'hi' });
  });
});
