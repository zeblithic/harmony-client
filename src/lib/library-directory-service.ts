import type { TauriAdapter } from './zenoh-service';

/**
 * Hlc payload as published by the backend `Hlc` struct. Note: the wire
 * keys are single-char (`w` / `l` / `d`) for canonical-CBOR field-length
 * parity (see `owner_state_types::Hlc`). For the directory IPCs we treat
 * the Hlc as opaque metadata and never read it on the frontend — the
 * type is kept loose to avoid coupling to the wire-key choice.
 */
export type Hlc = Record<string, unknown>;

/** Mirrors `library_directory::LibraryInfo` IPC return shape. */
export interface LibraryInfo {
  /** Hex-encoded OwnerAddr (32 chars). */
  address: string;
  added_at: Hlc;
  entry_count: number;
}

/** Mirrors `library_directory::DirectoryEntryDTO` IPC return shape. */
export interface DirectoryEntry {
  /** Hex-encoded SpaceId (32 chars). */
  community_id: string;
  /** Hex-encoded OwnerAddr (32 chars) derived from the admin identity. */
  community_addr: string;
  name: string;
  description: string;
  topics: string[];
  invite_url: string;
  listed_by_count: number;
  listed_at: Hlc;
}

/**
 * Thin IPC wrapper for the Sub-D library-directory IPCs. Mirrors the
 * `community-service.ts` shape: constructor takes a `TauriAdapter`, each
 * method translates JS-side camelCase arg names to the Rust snake_case
 * IPC parameter names that Tauri rewrites at the boundary.
 */
export class LibraryDirectoryService {
  constructor(private adapter: TauriAdapter) {}

  /** Snapshot of effective LibraryEntry rows (sorted by address). */
  async list(): Promise<LibraryInfo[]> {
    return (await this.adapter.invoke('list_libraries', {})) as LibraryInfo[];
  }

  /** LWW-add a library to owner_state.libraries + subscribe. */
  async add(libraryAddr: string): Promise<void> {
    await this.adapter.invoke('add_library', { libraryAddr });
  }

  /** LWW-tombstone a library + unsubscribe + evict its contributions. */
  async remove(libraryAddr: string): Promise<void> {
    await this.adapter.invoke('remove_library', { libraryAddr });
  }

  /** Snapshot of the aggregation map. `null` aggregates across all
   *  libraries; passing a single addr filters to one library. */
  async browse(libraryAddr?: string | null): Promise<DirectoryEntry[]> {
    return (await this.adapter.invoke('browse_library', {
      libraryAddr: libraryAddr ?? null,
    })) as DirectoryEntry[];
  }
}
