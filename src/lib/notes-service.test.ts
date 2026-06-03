import { describe, it, expect, vi, beforeEach } from 'vitest';
import { NotesService } from './notes-service';

// ZEB-334: the self-notes store is a local-only, per-owner-identity
// scratchpad backing the always-present "Notes" space. It must persist
// across cold starts (localStorage), isolate notes per owner id, and be a
// safe no-op when no owner id is available (the WelcomeModal hard gate
// normally guarantees one, but the store must never write to an un-keyed
// bucket).
describe('NotesService — ZEB-334 local self-notes store', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('persists an appended note and reloads it in a fresh instance', () => {
    const a = new NotesService();
    a.append('owner-abc', 'first note');
    // A brand-new instance (simulating a cold app start) must see it.
    const b = new NotesService();
    const entries = b.getEntries('owner-abc');
    expect(entries).toHaveLength(1);
    expect(entries[0].text).toBe('first note');
    expect(typeof entries[0].id).toBe('string');
    expect(typeof entries[0].timestamp).toBe('number');
  });

  it('keeps notes isolated per owner id', () => {
    const svc = new NotesService();
    svc.append('owner-abc', 'mine');
    expect(svc.getEntries('owner-def')).toEqual([]);
    expect(svc.getEntries('owner-abc')).toHaveLength(1);
  });

  it('returns an empty list for an owner with no notes', () => {
    const svc = new NotesService();
    expect(svc.getEntries('nobody')).toEqual([]);
  });

  it('is a safe no-op when ownerId is absent', () => {
    const svc = new NotesService();
    expect(svc.getEntries('')).toEqual([]);
    expect(svc.append('', 'into the void')).toBeNull();
    // Nothing was persisted under any key.
    expect(svc.getEntries('')).toEqual([]);
  });

  it('rejects blank / whitespace-only note text (never persists an empty note)', () => {
    const svc = new NotesService();
    expect(svc.append('owner-abc', '')).toBeNull();
    expect(svc.append('owner-abc', '   \n\t ')).toBeNull();
    expect(svc.getEntries('owner-abc')).toEqual([]);
  });

  it('trims surrounding whitespace from a stored note', () => {
    const svc = new NotesService();
    const entry = svc.append('owner-abc', '  padded note  ');
    expect(entry?.text).toBe('padded note');
  });

  it('fires onChange and updates entries on append', () => {
    const svc = new NotesService();
    svc.onChange = vi.fn();
    svc.load('owner-abc');
    const entry = svc.append('owner-abc', 'hello');
    expect(entry).not.toBeNull();
    expect(svc.onChange).toHaveBeenCalled();
    expect(svc.entries).toHaveLength(1);
    expect(svc.entries[0].text).toBe('hello');
  });
});
