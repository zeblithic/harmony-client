import { describe, it, expect } from 'vitest';
import { MemberCardService } from '../member-card-service';

function fakeResolver(map: Record<string, string>) {
  return {
    resolve: (cid: string) => map[cid],
  };
}

describe('MemberCardService avatar resolution', () => {
  it('resolves avatarCid to avatarUrl via the AvatarResolver', () => {
    const svc = new MemberCardService();
    svc.setAvatarResolver(fakeResolver({ deadbeef: 'blob:fake-url' }) as any);
    svc.applyCard('AA'.repeat(16), {
      displayName: 'Ann',
      statusText: 'hi',
      avatarCid: 'deadbeef',
    } as any);
    const card = svc.resolve('aa'.repeat(16));
    expect(card?.avatarUrl).toBe('blob:fake-url');
  });

  it('leaves avatarUrl undefined when no avatarCid', () => {
    const svc = new MemberCardService();
    svc.setAvatarResolver(fakeResolver({}) as any);
    svc.applyCard('BB'.repeat(16), { displayName: 'Bo', statusText: '' } as any);
    expect(svc.resolve('bb'.repeat(16))?.avatarUrl).toBeUndefined();
  });

  it('onAvatarsRefreshed re-resolves cards whose avatarCid newly resolved', () => {
    const svc = new MemberCardService();
    const map: Record<string, string> = {}; // resolver returns undefined initially
    svc.setAvatarResolver({ resolve: (cid: string) => map[cid] } as any);
    let updates = 0;
    svc.onUpdate = () => { updates++; };
    svc.applyCard('CC'.repeat(16), { displayName: 'Cy', statusText: '', avatarCid: 'cafe' } as any);
    expect(svc.resolve('cc'.repeat(16))?.avatarUrl).toBeUndefined(); // not resolved yet
    // Capture the count BEFORE the refresh: applyCard already incremented
    // `updates`, so asserting > 0 would pass even if onAvatarsRefreshed did
    // nothing. Assert the DELTA so we know the refresh itself fired onUpdate.
    const before = updates;
    map['cafe'] = 'blob:late-url'; // resolver now has it (simulating a completed fetch)
    svc.onAvatarsRefreshed();
    expect(svc.resolve('cc'.repeat(16))?.avatarUrl).toBe('blob:late-url');
    expect(updates).toBeGreaterThan(before);
  });

  it('seedSelf resolves the self avatarCid via the resolver (reload, no blob url)', () => {
    // After reload the session-local blob: avatarUrl is gone; the self avatar
    // must still resolve from its persisted avatarCid via the AvatarResolver,
    // otherwise the user's own row/messages fall back to an identicon.
    const svc = new MemberCardService();
    svc.setAvatarResolver(fakeResolver({ selfcid: 'blob:self-url' }) as any);
    svc.seedSelf('FF'.repeat(16), {
      displayName: 'Me',
      statusText: '',
      avatarCid: 'selfcid',
    } as any);
    expect(svc.resolve('ff'.repeat(16))?.avatarUrl).toBe('blob:self-url');
  });

  it('seedSelf prefers a live blob avatarUrl, and onAvatarsRefreshed does not clobber it', () => {
    const svc = new MemberCardService();
    // Resolver WOULD return a different URL for the cid — but the live blob must win.
    svc.setAvatarResolver(fakeResolver({ selfcid: 'blob:resolved' }) as any);
    svc.seedSelf('AB'.repeat(16), {
      displayName: 'Me',
      statusText: '',
      avatarUrl: 'blob:live-preview',
      avatarCid: 'selfcid',
    } as any);
    expect(svc.resolve('ab'.repeat(16))?.avatarUrl).toBe('blob:live-preview');
    // A later refresh (e.g. the resolver fetching some other card's avatar) must
    // NOT overwrite the live self preview with the resolver URL.
    svc.onAvatarsRefreshed();
    expect(svc.resolve('ab'.repeat(16))?.avatarUrl).toBe('blob:live-preview');
  });
});

// ── ZEB-345: profilePageRoot threading (mirrors the avatarCid path) ──
describe('MemberCardService profilePageRoot threading', () => {
  it('applyCard threads profilePageRoot through to the resolved card', () => {
    const svc = new MemberCardService();
    svc.applyCard('AA'.repeat(16), {
      displayName: 'Ann',
      statusText: 'hi',
      profilePageRoot: 'cid-page-1',
    } as any);
    expect(svc.resolve('aa'.repeat(16))?.profilePageRoot).toBe('cid-page-1');
  });

  it('applyCard leaves profilePageRoot undefined when the card has no page', () => {
    const svc = new MemberCardService();
    svc.applyCard('BB'.repeat(16), { displayName: 'Bo', statusText: '' } as any);
    expect(svc.resolve('bb'.repeat(16))?.profilePageRoot).toBeUndefined();
  });

  it('applyCard fires onUpdate when only profilePageRoot changed', () => {
    const svc = new MemberCardService();
    let updates = 0;
    svc.onUpdate = () => { updates++; };
    svc.applyCard('CC'.repeat(16), { displayName: 'Cy', statusText: '' } as any);
    const before = updates;
    // Same name/status, but now a profile page root appears → must re-fire.
    svc.applyCard('CC'.repeat(16), {
      displayName: 'Cy',
      statusText: '',
      profilePageRoot: 'cid-new',
    } as any);
    expect(svc.resolve('cc'.repeat(16))?.profilePageRoot).toBe('cid-new');
    expect(updates).toBeGreaterThan(before);
  });

  it('seedSelf threads profilePageRoot through to the self card', () => {
    const svc = new MemberCardService();
    svc.seedSelf('FF'.repeat(16), {
      displayName: 'Me',
      statusText: '',
      profilePageRoot: 'cid-self-page',
    } as any);
    expect(svc.resolve('ff'.repeat(16))?.profilePageRoot).toBe('cid-self-page');
  });
});
