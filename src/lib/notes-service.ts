/** A single self-note entry in the local scratchpad. */
export interface NoteEntry {
  id: string;
  text: string;
  /** Unix ms when the note was written. */
  timestamp: number;
}

const KEY_PREFIX = 'harmony-notes:';

function storageKey(ownerId: string): string {
  return KEY_PREFIX + ownerId;
}

/**
 * Local-only, per-owner-identity self-notes store (ZEB-334).
 *
 * Backs the always-present "Notes" space — a private scratchpad the user has
 * with themselves, shown as the default when no community is joined. Persists
 * to localStorage keyed by owner id and holds no network state. The interface
 * is deliberately thin so the persistence layer can later be swapped for a
 * synced substrate (the deferred owner→device sync) without touching callers.
 *
 * Notes are keyed strictly by owner id. The WelcomeModal hard gate (ZEB-338)
 * guarantees an identity exists before Notes is reachable, so there is no
 * pre-identity bucket; if an owner id is somehow absent the store is a safe
 * no-op rather than writing to an un-keyed bucket.
 */
export class NotesService {
  /** Entries for the most recently loaded/appended owner (drives the UI). */
  entries: NoteEntry[] = [];
  /** Called whenever entries change so the view can re-render. */
  onChange?: () => void;

  /** Read all notes for an owner from storage (oldest-first). Returns an
   *  empty list for unknown owners or when no owner id is available. Pure read:
   *  does not mutate `entries` (use `load` to hydrate the instance). */
  getEntries(ownerId: string): NoteEntry[] {
    return ownerId ? readNotes(ownerId) : [];
  }

  /** Hydrate `entries` from storage for the given owner and notify observers
   *  (the onChange contract: fires whenever `entries` changes). */
  load(ownerId: string): void {
    this.entries = ownerId ? readNotes(ownerId) : [];
    this.onChange?.();
  }

  /** Append a note for an owner, persist it, and fire onChange. Returns the
   *  new entry, or null when no owner id is available (never writes to an
   *  un-keyed bucket) or the text is empty/whitespace-only (never persists a
   *  blank note — CodeAnt, PR #180). */
  append(ownerId: string, text: string): NoteEntry | null {
    if (!ownerId) return null;
    const trimmed = text.trim();
    if (!trimmed) return null;
    const entry: NoteEntry = {
      id: crypto.randomUUID(),
      text: trimmed,
      timestamp: Date.now(),
    };
    const list = readNotes(ownerId);
    list.push(entry);
    writeNotes(ownerId, list);
    this.entries = list;
    this.onChange?.();
    return entry;
  }
}

function readNotes(ownerId: string): NoteEntry[] {
  try {
    const stored = localStorage.getItem(storageKey(ownerId));
    if (!stored) return [];
    const parsed = JSON.parse(stored);
    if (!Array.isArray(parsed)) return [];
    // Tolerate partial/corrupt persisted data — keep only well-formed entries.
    return parsed.filter(
      (e): e is NoteEntry =>
        e != null &&
        typeof e.id === 'string' &&
        typeof e.text === 'string' &&
        typeof e.timestamp === 'number',
    );
  } catch {
    // Corrupt JSON or localStorage unavailable (SSR, private browsing).
    return [];
  }
}

function writeNotes(ownerId: string, list: NoteEntry[]): void {
  try {
    localStorage.setItem(storageKey(ownerId), JSON.stringify(list));
  } catch {
    // localStorage may be unavailable (SSR, private browsing quota).
  }
}
