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

  it('same-owner reconnect is a no-op (shared-instance guard, ZEB-666)', () => {
    const s = new LocalStorageUnreadCursorStore();
    s.connectOwner('owner-a');
    s.set('c1', 'ch1', HLC);
    // Wipe the persisted blob: a reload would lose the live map, so a
    // surviving cursor proves the same-owner reconnect didn't reparse.
    localStorage.clear();
    s.connectOwner('owner-a');
    expect(s.get('c1', 'ch1')).toEqual(HLC);
    // A real owner change still reloads.
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

  it('drops mis-shaped entries (valid JSON, wrong cursor shape) but keeps valid ones', () => {
    localStorage.setItem(
      'harmony-unread:owner-owner-a',
      JSON.stringify({
        'c1:bad-string': 'not-an-hlc',
        'c1:bad-partial': { wallMs: 5 }, // missing logical/deviceId
        'c1:bad-types': { wallMs: '5', logical: 0, deviceId: 'd' }, // wallMs not a number
        'c1:good': HLC,
      }),
    );
    const s = new LocalStorageUnreadCursorStore();
    s.connectOwner('owner-a');
    expect(s.get('c1', 'bad-string')).toBeNull();
    expect(s.get('c1', 'bad-partial')).toBeNull();
    expect(s.get('c1', 'bad-types')).toBeNull();
    expect(s.get('c1', 'good')).toEqual(HLC);
  });

  it('does not throw when localStorage.setItem fails (e.g. quota exceeded)', () => {
    const s = new LocalStorageUnreadCursorStore();
    s.connectOwner('owner-a');
    const original = localStorage.setItem.bind(localStorage);
    localStorage.setItem = () => {
      throw new Error('quota exceeded');
    };
    try {
      expect(() => s.set('c1', 'ch1', HLC)).not.toThrow();
      expect(s.get('c1', 'ch1')).toEqual(HLC); // in-memory map still updated
    } finally {
      localStorage.setItem = original;
    }
  });
});
