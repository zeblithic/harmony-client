import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

// Hoist mocks ahead of static imports.
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

import { invoke } from '@tauri-apps/api/core';
import NotesView from '../NotesView.svelte';
import { NotesService } from '../../notes-service';

// ZEB-417: NotesView renders the IPC-backed self-notes stream + a compose box.
// It must render existing notes with the configured display name, surface the
// private-space framing, and append on Enter via the async IPC service.
describe('NotesView — ZEB-417 IPC-backed self-notes', () => {
  beforeEach(() => vi.mocked(invoke).mockReset());

  it('renders existing notes for the owner, labelled with the display name', async () => {
    vi.mocked(invoke).mockResolvedValue([{ id: 'n1', text: 'remember the milk', timestamp: 1 }]);
    const svc = new NotesService();
    render(NotesView, { notesService: svc, ownerId: 'owner-1', displayName: 'Jake' });
    await waitFor(() => expect(screen.getByText('remember the milk')).toBeInTheDocument());
    expect(screen.getByText('Jake')).toBeInTheDocument();
  });

  it('makes clear the space is private', () => {
    vi.mocked(invoke).mockResolvedValue([]);
    const svc = new NotesService();
    render(NotesView, { notesService: svc, ownerId: 'owner-1', displayName: 'Jake' });
    expect(screen.getByTestId('notes-view')).toHaveTextContent(/private/i);
  });

  it('appends a note on Enter and renders it', async () => {
    // First call (getEntries in $effect) returns empty; second (getEntries after
    // append) returns the new entry.
    vi.mocked(invoke)
      .mockResolvedValueOnce([]) // notes_list on $effect load
      .mockResolvedValueOnce({ id: 'n1', text: 'new thought', timestamp: 5 }) // notes_upsert
      .mockResolvedValueOnce([{ id: 'n1', text: 'new thought', timestamp: 5 }]); // notes_list after append
    const svc = new NotesService();
    render(NotesView, { notesService: svc, ownerId: 'owner-1', displayName: 'Jake' });
    const box = screen.getByTestId('notes-compose');
    await fireEvent.input(box, { target: { value: 'new thought' } });
    await fireEvent.keyDown(box, { key: 'Enter' });
    await waitFor(() => expect(screen.getByText('new thought')).toBeInTheDocument());
  });

  it('does not append blank/whitespace-only notes', async () => {
    vi.mocked(invoke).mockResolvedValue([]);
    const svc = new NotesService();
    render(NotesView, { notesService: svc, ownerId: 'owner-1', displayName: 'Jake' });
    const box = screen.getByTestId('notes-compose');
    await fireEvent.input(box, { target: { value: '   ' } });
    await fireEvent.keyDown(box, { key: 'Enter' });
    // notes_upsert should NOT be called (blank text guard fires first)
    await waitFor(() => {
      const upsertCalls = vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === 'notes_upsert');
      expect(upsertCalls).toHaveLength(0);
    });
  });

  it('keeps the compose text when the note cannot be saved (append returns null)', async () => {
    // With an empty ownerId, append still gets called but text.trim() is non-empty,
    // so notes_upsert is invoked. Simulate a backend failure → append returns null
    // → compose text must not be cleared.
    vi.mocked(invoke)
      .mockResolvedValueOnce([]) // notes_list on $effect load
      .mockRejectedValueOnce('backend error'); // notes_upsert failure
    const svc = new NotesService();
    render(NotesView, { notesService: svc, ownerId: '', displayName: 'Jake' });
    const box = screen.getByTestId('notes-compose') as HTMLTextAreaElement;
    await fireEvent.input(box, { target: { value: 'unsaved thought' } });
    await fireEvent.keyDown(box, { key: 'Enter' });
    // After the async append fails, compose text must remain (not cleared).
    await waitFor(() => expect(box.value).toBe('unsaved thought'));
  });
});
