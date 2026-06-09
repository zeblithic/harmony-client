import { invoke } from '@tauri-apps/api/core';

export interface NoteEntry {
  id: string;
  text: string;
  /** Unix ms when the note was written. */
  timestamp: number;
}

/** IPC-backed self-notes store (ZEB-417). Notes live in the Rust `NotesDoc`
 *  dataset, synced across the owner's devices. `ownerId` is retained for
 *  call-site compatibility; the backend keys notes by the active owner. */
export class NotesService {
  /** Entries for the most recently loaded/appended owner (drives the UI). */
  entries: NoteEntry[] = [];
  /** Called whenever entries change so the view can re-render. */
  onChange?: () => void;

  /** Fetch all notes from the backend via IPC (oldest-first). Pure read:
   *  does not mutate `entries` (use `load` to hydrate the instance). */
  async getEntries(_ownerId: string): Promise<NoteEntry[]> {
    try {
      return await invoke<NoteEntry[]>('notes_list', {});
    } catch (e) {
      console.error('notes_list failed:', e instanceof Error ? e.message : String(e));
      return [];
    }
  }

  /** Hydrate `entries` from the backend for the given owner and notify
   *  observers (the onChange contract: fires whenever `entries` changes). */
  async load(ownerId: string): Promise<void> {
    this.entries = await this.getEntries(ownerId);
    this.onChange?.();
  }

  /** Append a note via IPC, update the local entries mirror, and fire
   *  onChange. Returns the new entry, or null when the text is
   *  empty/whitespace-only (never persists a blank note). */
  async append(_ownerId: string, text: string): Promise<NoteEntry | null> {
    if (!text.trim()) return null;
    try {
      const entry = await invoke<NoteEntry>('notes_upsert', { id: undefined, text });
      this.entries = [...this.entries, entry];
      this.onChange?.();
      return entry;
    } catch (e) {
      console.error('notes_upsert failed:', e instanceof Error ? e.message : String(e));
      return null;
    }
  }
}
