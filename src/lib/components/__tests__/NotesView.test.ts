import { describe, it, expect, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import NotesView from '../NotesView.svelte';
import { NotesService } from '../../notes-service';

// ZEB-334: NotesView renders the local self-notes stream + a compose box,
// backed only by NotesService (no network plumbing). It must render existing
// notes with the configured display name, surface the private-space framing,
// and append on Enter.
describe('NotesView — ZEB-334 self-notes', () => {
  beforeEach(() => localStorage.clear());

  it('renders existing notes for the owner, labelled with the display name', () => {
    const svc = new NotesService();
    svc.append('owner-1', 'remember the milk');
    render(NotesView, { notesService: svc, ownerId: 'owner-1', displayName: 'Jake' });
    expect(screen.getByText('remember the milk')).toBeInTheDocument();
    expect(screen.getByText('Jake')).toBeInTheDocument();
  });

  it('makes clear the space is private', () => {
    const svc = new NotesService();
    render(NotesView, { notesService: svc, ownerId: 'owner-1', displayName: 'Jake' });
    expect(screen.getByTestId('notes-view')).toHaveTextContent(/private/i);
  });

  it('appends a note on Enter, renders it, and persists it via the service', async () => {
    const svc = new NotesService();
    render(NotesView, { notesService: svc, ownerId: 'owner-1', displayName: 'Jake' });
    const box = screen.getByTestId('notes-compose');
    await fireEvent.input(box, { target: { value: 'new thought' } });
    await fireEvent.keyDown(box, { key: 'Enter' });
    expect(screen.getByText('new thought')).toBeInTheDocument();
    expect(svc.getEntries('owner-1').map((e) => e.text)).toContain('new thought');
  });

  it('does not append blank/whitespace-only notes', async () => {
    const svc = new NotesService();
    render(NotesView, { notesService: svc, ownerId: 'owner-1', displayName: 'Jake' });
    const box = screen.getByTestId('notes-compose');
    await fireEvent.input(box, { target: { value: '   ' } });
    await fireEvent.keyDown(box, { key: 'Enter' });
    expect(svc.getEntries('owner-1')).toHaveLength(0);
  });
});
