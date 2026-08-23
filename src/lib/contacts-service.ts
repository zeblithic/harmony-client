import type { TauriAdapter } from './zenoh-service';
import { nonEmpty } from './display-label';

/**
 * ZEB-977: frontend contacts service — owner-private petname + notes for ANY
 * identity (not just friends), backed by the fleet-synced contacts dataset.
 * Mirrors `friend-service.ts` (adapter-based, `connectAdapter` / private
 * `invoke` / `destroy`, event-listener wiring) so it's unit-testable against
 * a mock `TauriAdapter`.
 *
 * IPCs:
 *   - `contacts_list`        → live ContactView rows
 *   - `set_contact_petname`  → set/clear the local petname (null clears)
 *   - `set_contact_notes`    → set/clear the local private notes (null clears)
 *
 * Events:
 *   - `contacts-changed` → re-fetch (fired on local writes AND when a
 *     fleet-sync merge from one of the owner's other devices lands)
 *
 * Privacy: everything here is local to the owner (synced only across their
 * own devices, encrypted); nothing is ever shown to the annotated peer.
 */

/** Mirrors `ContactView` in src-tauri/src/contacts_commands.rs
 *  (`#[serde(rename_all = "camelCase")]`). */
export interface ContactView {
  /** The annotated identity's 16-byte master owner_id, hex (32 chars, lowercase). */
  ownerIdHex: string;
  /** Local petname — the name YOU assigned. Absent when unset. */
  petname?: string | null;
  /** Local private notes. Absent when unset. */
  notes?: string | null;
  /** Local wall-clock ms when this entry was first created on any of the
   *  owner's devices. */
  firstSeenMs: number;
  /** Wall-clock ms of the last write. */
  updatedMs: number;
}

/**
 * The petname map behind `resolveNickname`, keyed by lowercased owner hex.
 * Only non-blank petnames are kept (`nonEmpty`, same guard as the display
 * ladder) so a whitespace-only value can never enter resolver state.
 */
export function petnameMapFromContacts(contacts: ContactView[]): Map<string, string> {
  const map = new Map<string, string>();
  for (const c of contacts) {
    const petname = nonEmpty(c.petname);
    if (petname !== undefined) map.set(c.ownerIdHex.toLowerCase(), petname);
  }
  return map;
}

export class ContactsService {
  /** Listeners notified when the backend emits `contacts-changed` (a local
   *  write, or a fleet-sync merge from another of the owner's devices).
   *  A registry (not a single slot) so multiple consumers can subscribe
   *  without stomping one another — same pattern as FriendService. */
  private contactsChangedListeners = new Set<() => void>();

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];

  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return;
    // Claim the slot synchronously (duplicate-init guard for concurrent
    // callers), but RELEASE it if listener registration fails — otherwise the
    // service would sit falsely connected forever, never receiving
    // contacts-changed, and no retry could fix it.
    this.adapter = adapter;
    try {
      const unlistenChanged = await adapter.listen('contacts-changed', () => {
        // Snapshot before iterating so a listener that unsubscribes itself
        // during notification doesn't mutate the live set mid-loop.
        for (const cb of [...this.contactsChangedListeners]) cb();
      });
      this.unlisteners.push(unlistenChanged);
    } catch (e) {
      this.adapter = null;
      throw e instanceof Error ? e : new Error(String(e));
    }
  }

  /**
   * Register a callback fired when the contacts set changes. Receivers should
   * re-fetch `list()`. Returns an unsubscribe function; multiple subscribers
   * are supported.
   */
  onContactsChanged(cb: () => void): () => void {
    this.contactsChangedListeners.add(cb);
    return () => {
      this.contactsChangedListeners.delete(cb);
    };
  }

  /** List all live contact annotations. */
  async list(): Promise<ContactView[]> {
    return this.invoke<ContactView[]>('contacts_list', {});
  }

  /**
   * Set (or clear, with `null`/blank) the LOCAL petname for any identity by
   * 16-byte master owner_id hex — no friend relationship required. Returns
   * the entry after the write, or `null` when the write left no live entry
   * (clearing the last field removes the record). The backend emits
   * `contacts-changed` + `friend-list-changed` on a real change.
   */
  async setPetname(ownerIdHex: string, petname: string | null): Promise<ContactView | null> {
    return this.invoke<ContactView | null>('set_contact_petname', { ownerIdHex, petname });
  }

  /** Set (or clear, with `null`/blank) the LOCAL private notes for any
   *  identity. Same contract as {@link setPetname}. */
  async setNotes(ownerIdHex: string, notes: string | null): Promise<ContactView | null> {
    return this.invoke<ContactView | null>('set_contact_notes', { ownerIdHex, notes });
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
    this.contactsChangedListeners.clear();
    // Null the adapter so connectAdapter's duplicate-init guard doesn't no-op
    // on reconnect after destroy() (mirrors FriendService.destroy()).
    this.adapter = null;
  }

  private async invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
    if (!this.adapter) throw new Error(`ContactsService.${cmd}: adapter not connected`);
    try {
      return (await this.adapter.invoke(cmd, args)) as T;
    } catch (e) {
      // Normalize both production (string) + test (Error) rejection shapes
      // (CLAUDE.md "Tauri IPC error extraction").
      throw new Error(e instanceof Error ? e.message : String(e));
    }
  }
}
