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
    map['cafe'] = 'blob:late-url'; // resolver now has it (simulating a completed fetch)
    svc.onAvatarsRefreshed();
    expect(svc.resolve('cc'.repeat(16))?.avatarUrl).toBe('blob:late-url');
    expect(updates).toBeGreaterThan(0);
  });
});
