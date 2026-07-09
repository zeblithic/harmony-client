import { describe, it, expect, beforeEach } from 'vitest';
import { LocalStorageUnreadCursorStore } from './unread-cursor-store';

const HLC = { wallMs: 100, logical: 1, deviceId: 'd1' };

describe('LocalStorageUnreadCursorStore (ZEB-665)', () => {
  beforeEach(() => localStorage.clear());

  it('pre-owner: get returns null and set is a no-op (ZEB-586 guard)', () => {
    const s = new LocalStorageUnreadCursorStore();
    s.set('c1', 'ch1', HLC);
    expect(s.get('c1', 'ch1')).toBeNull();
    expect(localStorage.length).toBe(0); // nothing leaked to a shared key
  });

  it('round-trips a cursor after connectOwner, keyed per owner', () => {
    const s = new LocalStorageUnreadCursorStore();
    s.connectOwner('owner-a');
    s.set('c1', 'ch1', HLC);
    expect(s.get('c1', 'ch1')).toEqual(HLC);
    expect(localStorage.getItem('harmony-unread:owner-owner-a')).toContain('"c1:ch1"');
  });

  it('isolates owners: owner B does not see owner A cursors', () => {
    const s = new LocalStorageUnreadCursorStore();
    s.connectOwner('owner-a');
    s.set('c1', 'ch1', HLC);
    s.connectOwner('owner-b');
    expect(s.get('c1', 'ch1')).toBeNull();
  });

  it('persists across instances for the same owner', () => {
    const a = new LocalStorageUnreadCursorStore();
    a.connectOwner('owner-a');
    a.set('c1', 'ch1', HLC);
    const b = new LocalStorageUnreadCursorStore();
    b.connectOwner('owner-a');
    expect(b.get('c1', 'ch1')).toEqual(HLC);
  });

  it('degrades a corrupt blob to an empty map instead of throwing', () => {
    localStorage.setItem('harmony-unread:owner-owner-a', '{not json');
    const s = new LocalStorageUnreadCursorStore();
    expect(() => s.connectOwner('owner-a')).not.toThrow();
    expect(s.get('c1', 'ch1')).toBeNull();
    s.set('c1', 'ch1', HLC); // and recovers on next write
    expect(s.get('c1', 'ch1')).toEqual(HLC);
  });
});
