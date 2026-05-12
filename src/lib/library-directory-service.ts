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

/**
 * Mirrors `library_directory::DiscoveredLibraryInfo` IPC return shape
 * (ZEB-279 Sub-D Phase 2). Surfaces libraries the user has *discovered*
 * via the `harmony/discovery/library/announce` topic but has NOT yet
 * added to their trust set. The Rust DTO uses
 * `#[serde(rename_all = "camelCase")]`, so wire keys are camelCase
 * (unlike `LibraryInfo`, which is snake_case on the wire). `listedAt`
 * is a base-10 string of `listed_at.wall_ms` for display only — callers
 * MUST NOT use this for HLC ordering decisions.
 */
export interface DiscoveredLibraryInfo {
  /** Hex-encoded 16-byte library OwnerAddr (32 hex chars). */
  libraryAddr: string;
  name: string;
  description: string;
  /** `listed_at.wall_ms` as base-10 string for display only. */
  listedAt: string;
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
  /**
   * Count of libraries with valid attestation for this entry
   * (`attested_by.len()` in the Rust aggregation). Includes Phase 1
   * unwrapped contributions via fallback to entry.listed_by.
   */
  listed_by_count: number;
  /**
   * Sub-D Phase 3 (ZEB-280): true if at least one broadcasting
   * library's wrapping sig failed to verify (Rust:
   * `!unattested_by.is_empty()`). Drives the inline "⚠ Unattested"
   * badge in `LibraryDirectoryBrowser.svelte`.
   */
  unattested: boolean;
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

  /**
   * Sub-D Phase 2 (ZEB-279): snapshot of libraries the user has
   * discovered via the announce topic but has NOT yet added. The Rust
   * handler filters out already-added (non-tombstoned) libraries, so
   * a row migrates from this list to `list()` after a successful
   * `add()` + refetch.
   */
  async listDiscovered(): Promise<DiscoveredLibraryInfo[]> {
    return (await this.adapter.invoke(
      'list_discovered_libraries',
      {},
    )) as DiscoveredLibraryInfo[];
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
