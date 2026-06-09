import { describe, it, expect, vi, beforeEach } from 'vitest';
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
import { invoke } from '@tauri-apps/api/core';
import { NotesService } from './notes-service';

// ZEB-417: the self-notes store is now IPC-backed (Rust `NotesDoc` dataset,
// synced across devices). The consumer contract — entries, onChange,
// getEntries, load, append — is preserved from the prior localStorage shape.
describe('NotesService — IPC-backed (ZEB-417)', () => {
  beforeEach(() => vi.mocked(invoke).mockReset());

  it('append calls notes_upsert and appends the created entry', async () => {
    vi.mocked(invoke).mockResolvedValueOnce({ id: 'n1', text: 'hi', timestamp: 5 });
    const svc = new NotesService();
    const entry = await svc.append('owner-abc', '  hi  ');
    expect(invoke).toHaveBeenCalledWith('notes_upsert', { id: undefined, text: '  hi  ' });
    expect(entry).toEqual({ id: 'n1', text: 'hi', timestamp: 5 });
    expect(svc.entries).toHaveLength(1);
  });

  it('getEntries calls notes_list', async () => {
    vi.mocked(invoke).mockResolvedValueOnce([{ id: 'n1', text: 'hi', timestamp: 5 }]);
    const svc = new NotesService();
    expect(await svc.getEntries('owner-abc')).toHaveLength(1);
    expect(invoke).toHaveBeenCalledWith('notes_list', {});
  });

  it('append rejects blank text without invoking', async () => {
    const svc = new NotesService();
    expect(await svc.append('owner-abc', '   ')).toBeNull();
    expect(invoke).not.toHaveBeenCalled();
  });

  it('load hydrates entries and fires onChange', async () => {
    vi.mocked(invoke).mockResolvedValueOnce([{ id: 'n1', text: 'hi', timestamp: 5 }]);
    const svc = new NotesService();
    const spy = vi.fn(); svc.onChange = spy;
    await svc.load('owner-abc');
    expect(svc.entries).toHaveLength(1);
    expect(spy).toHaveBeenCalled();
  });

  it('getEntries returns [] on invoke rejection (no throw)', async () => {
    vi.mocked(invoke).mockRejectedValueOnce('boom');
    const svc = new NotesService();
    expect(await svc.getEntries('owner-abc')).toEqual([]);
  });
});
