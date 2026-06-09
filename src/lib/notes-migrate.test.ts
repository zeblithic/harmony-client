import { describe, it, expect, vi, beforeEach } from 'vitest';
import { migrateLocalNotes } from './notes-migrate';

describe('migrateLocalNotes (ZEB-417 one-time import)', () => {
  beforeEach(() => localStorage.clear());

  it('imports legacy notes once, idempotently', async () => {
    localStorage.setItem('harmony-notes:owner-abc', JSON.stringify([
      { id: 'a', text: 'one', timestamp: 1 },
      { id: 'b', text: 'two', timestamp: 2 },
    ]));
    const inv = vi.fn().mockResolvedValue({ id: 'x', text: 'x', timestamp: 0 });
    await migrateLocalNotes('owner-abc', inv);
    expect(inv).toHaveBeenCalledTimes(2);
    expect(inv).toHaveBeenCalledWith('notes_upsert', { id: undefined, text: 'one' });
    expect(localStorage.getItem('harmony-notes-migrated:owner-abc')).toBe('1');
    inv.mockClear();
    await migrateLocalNotes('owner-abc', inv); // second run: no-op
    expect(inv).not.toHaveBeenCalled();
  });

  it('is a no-op with no legacy notes but still marks migrated', async () => {
    const inv = vi.fn();
    await migrateLocalNotes('owner-xyz', inv);
    expect(inv).not.toHaveBeenCalled();
    expect(localStorage.getItem('harmony-notes-migrated:owner-xyz')).toBe('1');
  });

  it('skips blank/whitespace and malformed legacy entries', async () => {
    localStorage.setItem('harmony-notes:o', JSON.stringify([
      { text: '  ' }, { text: 'keep' }, { notext: true }, null, 'bogus',
    ]));
    const inv = vi.fn().mockResolvedValue({});
    await migrateLocalNotes('o', inv);
    expect(inv).toHaveBeenCalledTimes(1);
    expect(inv).toHaveBeenCalledWith('notes_upsert', { id: undefined, text: 'keep' });
  });

  it('does nothing for an empty ownerId', async () => {
    const inv = vi.fn();
    await migrateLocalNotes('', inv);
    expect(inv).not.toHaveBeenCalled();
  });

  it('tolerates corrupt JSON', async () => {
    localStorage.setItem('harmony-notes:o', '{not json');
    const inv = vi.fn();
    await migrateLocalNotes('o', inv);
    expect(inv).not.toHaveBeenCalled();
    expect(localStorage.getItem('harmony-notes-migrated:o')).toBe('1');
  });
});
