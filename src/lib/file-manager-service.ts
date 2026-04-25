import type { TauriAdapter } from './zenoh-service';
import type {
  ContentItem,
  ContentDetail,
  QuotaStatus,
  CleanupRecommendation,
  StorageBuddy,
  PublishedItem,
  FileManagerSettings,
  ReplicationTier,
  ContentCategory,
  PeerRef,
} from './types';
import {
  mockPrivateContent,
  mockPublishedContent,
  mockCleanupRecommendations,
  mockStorageBuddies,
  mockPeers,
} from './mock-file-data';

/** Wire format for content availability announcements from the Rust backend. */
export interface ContentAnnouncementEvent {
  cid: string;
  sizeBytes: number;
}

/** Wire format returned by the ingest_content Tauri command. */
interface IngestResult {
  cid: string;
  fileName: string;
  sizeBytes: number;
}

/** Wire format returned by the create_folder Tauri command. */
export interface CreateFolderResult {
  sidecarId: string;
  cid: string;
}

/** Wire format for entries returned by the list_content Tauri command. */
interface ContentItemWire {
  sidecarId: string;
  cid: string;
  name: string;
  sizeBytes: number;
  storedAt: number;
  sensitivity: 'private' | 'confidential' | 'public';
  replicationTier: ReplicationTier;
  pinned: boolean;
  licensed: boolean;
  archived: boolean;
  /** Source-of-truth node type from the backend. */
  kind: 'leaf' | 'folder';
}

const MUSIC_EXTS = ['mp3', 'flac', 'wav', 'ogg', 'aac', 'm4a', 'opus', 'wma'];
const VIDEO_EXTS = ['mp4', 'mkv', 'avi', 'mov', 'webm', 'flv', 'wmv'];
const IMAGE_EXTS = ['jpg', 'jpeg', 'png', 'gif', 'svg', 'webp', 'bmp', 'ico', 'tiff'];
const SOFTWARE_EXTS = ['exe', 'app', 'dmg', 'deb', 'rpm', 'msi', 'wasm', 'tar', 'gz', 'zip', 'rar', '7z'];
const DATASET_EXTS = ['csv', 'parquet', 'arrow', 'jsonl', 'ndjson', 'tsv', 'sqlite', 'db'];

/** Infer a content category from a file name's extension. */
export function inferCategory(fileName: string): ContentCategory {
  const ext = fileName.split('.').pop()?.toLowerCase() ?? '';
  if (MUSIC_EXTS.includes(ext)) return 'music';
  if (VIDEO_EXTS.includes(ext)) return 'video';
  if (IMAGE_EXTS.includes(ext)) return 'image';
  if (SOFTWARE_EXTS.includes(ext)) return 'software';
  if (DATASET_EXTS.includes(ext)) return 'dataset';
  return 'text';
}

function wireToContentItem(wire: ContentItemWire): ContentItem {
  // `kind` is the source of truth on the wire; `isFolder` is a derived
  // client-side convenience kept for back-compat with existing
  // filter/sort predicates in FileBrowser and FileList.
  return {
    sidecarId: wire.sidecarId,
    cid: wire.cid,
    name: wire.name,
    category: inferCategory(wire.name),
    sensitivity: wire.sensitivity,
    sizeBytes: wire.sizeBytes,
    storedAt: wire.storedAt,
    lastAccessed: wire.storedAt,
    accessCount: 0,
    stalenessScore: 0,
    replicationTier: wire.replicationTier,
    replicaCount: 1,
    pinned: wire.pinned,
    licensed: wire.licensed,
    archived: wire.archived,
    parentCid: null,
    isFolder: wire.kind === 'folder',
  };
}

export class FileManagerService {
  readonly settings: FileManagerSettings;
  /** Called whenever content state changes so the UI can re-render. */
  onChange?: () => void;
  /** CIDs announced on the mesh (real network data). */
  announcedCids = new Map<string, { sizeBytes: number; firstSeen: number }>();

  private adapter: TauriAdapter | null = null;
  private unlisteners: Array<() => void> = [];
  private privateContent: ContentItem[];
  private publishedContent: PublishedItem[];
  private cleanupRecommendations: CleanupRecommendation[];
  private storageBuddies: StorageBuddy[];

  constructor(overrides?: Partial<FileManagerSettings>) {
    this.settings = {
      defaultReplicationTier: 'default',
      quotaBytes: 10_000_000_000,
      defaultViewMode: 'list',
      confirmationOverrides: {},
      ...overrides,
    };

    // Each instance gets its own deep copy so mutations are isolated
    this.privateContent = structuredClone(mockPrivateContent);
    this.publishedContent = structuredClone(mockPublishedContent);
    this.cleanupRecommendations = structuredClone(mockCleanupRecommendations);
    this.storageBuddies = structuredClone(mockStorageBuddies);
  }

  /** Connect a Tauri adapter and start listening for content announcements. */
  async connectAdapter(adapter: TauriAdapter): Promise<void> {
    if (this.adapter) return; // already wired; prevent duplicate listeners

    // Do the full bootstrap (list + listener registration) before committing
    // `this.adapter`. If either step fails the service stays disconnected so
    // a later retry isn't blocked by the guard above.
    let raw: ContentItemWire[] | null | undefined;
    let unlisten: () => void;
    try {
      raw = (await adapter.invoke('list_content', { folderCid: null })) as ContentItemWire[] | null | undefined;
      unlisten = await adapter.listen(
        'content-announced',
        (event) => {
          const wire = event.payload as ContentAnnouncementEvent;
          if (this.announcedCids.has(wire.cid)) return;
          this.announcedCids = new Map([
            ...this.announcedCids,
            [wire.cid, { sizeBytes: wire.sizeBytes, firstSeen: Date.now() }],
          ]);
          this.onChange?.();
        },
      );
    } catch (err) {
      // Leave this.adapter null so the caller (or a retry path) can try again.
      throw err;
    }

    this.adapter = adapter;
    // Per ZEB-146, the file-manager UI must never mix mocks with real state
    // once the backend adapter is connected. Archived items stay in the
    // sidecar but are hidden from the active list (spec: archive = UI-hide).
    this.privateContent = Array.isArray(raw)
      ? raw.filter((w) => !w.archived).map(wireToContentItem)
      : [];
    this.unlisteners.push(unlisten);
    this.onChange?.();
  }

  /** Returns private content. With no args returns a copy of all; with parentCid filters by parent. */
  getContents(parentCid?: string | null): ContentItem[] {
    if (parentCid === undefined) {
      return [...this.privateContent];
    }
    return this.privateContent.filter((item) => item.parentCid === parentCid);
  }

  /** Returns extended detail for a single content item, or undefined if not found. */
  getContentDetail(cid: string): ContentDetail | undefined {
    const item = this.privateContent.find((i) => i.cid === cid);
    if (!item) return undefined;

    return {
      ...item,
      sharedWith: [mockPeers[0], mockPeers[1]],
      storageBuddies: [mockPeers[0]],
      origin: 'self-created',
    };
  }

  /** Computes quota status from current private content. */
  getQuotaStatus(): QuotaStatus {
    const byCategory: Partial<Record<ContentCategory, number>> = {};
    let usedBytes = 0;

    for (const item of this.privateContent) {
      usedBytes += item.sizeBytes;
      byCategory[item.category] = (byCategory[item.category] ?? 0) + item.sizeBytes;
    }

    return {
      usedBytes,
      totalBytes: this.settings.quotaBytes,
      byCategory,
    };
  }

  /** Returns cleanup recommendations, filtering out burned items, sorted by confidence desc.
   *  TODO: Re-evaluate recommendation reasons dynamically (e.g., drop 'over-replicated'
   *  after tier change) once real replication backends are wired in. */
  getCleanupRecommendations(): CleanupRecommendation[] {
    const activeCids = new Map(this.privateContent.map((i) => [i.cid, i]));
    return this.cleanupRecommendations
      .filter((r) => activeCids.has(r.cid))
      .map((r) => ({ ...r, sensitivity: activeCids.get(r.cid)!.sensitivity }))
      .sort((a, b) => b.confidence - a.confidence);
  }

  /** Returns storage buddies. */
  getStorageBuddies(): StorageBuddy[] {
    return [...this.storageBuddies];
  }

  /** Returns published content. */
  getPublishedContent(): PublishedItem[] {
    return [...this.publishedContent];
  }

  /** Returns available peers for sharing/buddy assignment. */
  getAvailablePeers(): PeerRef[] {
    return [...mockPeers];
  }

  /** Permanently removes content items and frees their quota. */
  async burn(sidecarIds: string[]): Promise<void> {
    if (!this.adapter) {
      // Offline-only path: still mutate local state so tests/Storybook work.
      const idSet = new Set(sidecarIds);
      this.privateContent = this.privateContent.filter((i) => !idSet.has(i.sidecarId));
      this.onChange?.();
      return;
    }
    const results = await Promise.allSettled(
      sidecarIds.map((sidecarId) => this.adapter!.invoke('burn_content', { sidecarId })),
    );
    const succeeded = new Set(
      sidecarIds.filter((_, i) => {
        const r = results[i];
        return r.status === 'fulfilled' && r.value === true;
      }),
    );
    this.privateContent = this.privateContent.filter((i) => !succeeded.has(i.sidecarId));
    this.onChange?.();
  }

  /** Move content to cold storage (archive tier). Items are removed from
   *  the active file list and the backend is notified to migrate the data. */
  async archive(sidecarIds: string[]): Promise<void> {
    if (!this.adapter) {
      const idSet = new Set(sidecarIds);
      this.privateContent = this.privateContent.filter((i) => !idSet.has(i.sidecarId));
      this.onChange?.();
      return;
    }
    const results = await Promise.allSettled(
      sidecarIds.map((sidecarId) => this.adapter!.invoke('archive_content', { sidecarId })),
    );
    const succeeded = new Set(
      sidecarIds.filter((_, i) => {
        const r = results[i];
        return r.status === 'fulfilled' && r.value === true;
      }),
    );
    this.privateContent = this.privateContent.filter((i) => !succeeded.has(i.sidecarId));
    this.onChange?.();
  }

  /** Moves content from private to published with durable publish mode. */
  publish(cids: string[]): void {
    this.moveToPublished(cids, 'durable');
  }

  /** Moves content from private to published with ephemeral publish mode. */
  release(cids: string[]): void {
    this.moveToPublished(cids, 'ephemeral');
  }

  /** Sets the pinned flag on a content item. */
  async pin(sidecarId: string): Promise<void> {
    if (!this.adapter) {
      // Offline-only path: mutate local state for Storybook/test contexts.
      const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
      if (item) item.pinned = true;
      this.onChange?.();
      return;
    }
    const ok = (await this.adapter.invoke('pin_content', { sidecarId })) as boolean;
    if (ok === false) {
      throw new Error('pin quota exhausted');
    }
    const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
    if (item) item.pinned = true;
    this.onChange?.();
  }

  /** Clears the pinned flag on a content item. */
  async unpin(sidecarId: string): Promise<void> {
    if (!this.adapter) {
      const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
      if (item) item.pinned = false;
      this.onChange?.();
      return;
    }
    await this.adapter.invoke('unpin_content', { sidecarId });
    const item = this.privateContent.find((i) => i.sidecarId === sidecarId);
    if (item) item.pinned = false;
    this.onChange?.();
  }

  /** Updates the replication tier for specified items. */
  async setReplicationTier(sidecarIds: string[], tier: ReplicationTier): Promise<void> {
    if (this.adapter) {
      await this.adapter.invoke('set_replication_tier', { sidecarIds, tier });
    }
    const idSet = new Set(sidecarIds);
    for (const item of this.privateContent) {
      if (idSet.has(item.sidecarId)) {
        item.replicationTier = tier;
      }
    }
    this.onChange?.();
  }

  /** Export content to the local filesystem via a native save dialog.
   *  Each CID triggers a separate save dialog on the Rust backend. */
  async exportToDevice(cids: string[]): Promise<void> {
    if (!this.adapter) return;
    for (const cid of cids) {
      const item = this.privateContent.find((i) => i.cid === cid);
      const fileName = item?.name ?? cid;
      await this.adapter.invoke('export_content', { cid, fileName });
    }
  }

  /** Open a file picker, ingest the selected file into the content store,
   *  and add it to the private content list.
   *  Returns the new ContentItem, or undefined if the user cancels or no adapter. */
  async ingest(parentCid?: string | null): Promise<ContentItem | undefined> {
    if (!this.adapter) return undefined;
    const result = (await this.adapter.invoke('ingest_content')) as IngestResult;
    // Deduplicate: if this CID already exists in private content, skip.
    if (this.privateContent.some((i) => i.cid === result.cid)) return undefined;
    const item: ContentItem = {
      sidecarId: '',
      cid: result.cid,
      name: result.fileName,
      category: inferCategory(result.fileName),
      sensitivity: 'private',
      sizeBytes: result.sizeBytes,
      storedAt: Date.now(),
      lastAccessed: Date.now(),
      accessCount: 0,
      stalenessScore: 0,
      replicationTier: this.settings.defaultReplicationTier,
      replicaCount: 1,
      pinned: false,
      licensed: false,
      archived: false,
      parentCid: parentCid ?? null,
      isFolder: false,
    };
    this.privateContent.push(item);
    return item;
  }

  /**
   * Load the contents of a specific folder via the backend's
   * list_content(folder_cid) path. Returns the live contents WITHOUT
   * caching — callers are responsible for binding to state.
   */
  async listFolderContents(folderCid: string): Promise<ContentItem[]> {
    if (!this.adapter) return [];
    // Let errors propagate. The backend distinguishes transient states
    // (bundle evicted from cache) from permanent corruption (manifest/bundle
    // mismatch, malformed manifest). Swallowing both as `[]` hides the
    // latter — a corrupted folder looks indistinguishable from an empty
    // one with only a console log. Callers decide how to surface the
    // error to users.
    const raw = (await this.adapter.invoke('list_content', { folderCid })) as
      | ContentItemWire[]
      | null
      | undefined;
    return Array.isArray(raw)
      ? raw.filter((w) => !w.archived).map(wireToContentItem)
      : [];
  }

  /**
   * Create a new folder via the backend.
   *
   * @param name              folder display name
   * @param parentSidecarId   the top-level sidecar entry's id (root entry
   *                          owning the cascade), or null for root creation
   * @param parentPath        CID chain from top-level root (inclusive) down
   *                          to the immediate parent; empty for root creation
   *
   * Returns `{ sidecarId, cid }`. For nested creation, `sidecarId` is the
   * unchanged top-level entry's id; `cid` is the new top-level root CID
   * after the ancestor cascade.
   *
   * Refetches the root listing and emits onChange. Callers navigating
   * inside a folder at the time of creation should also refetch the
   * folder contents.
   */
  async createFolder(
    name: string,
    parentSidecarId: string | null,
    parentPath: string[],
  ): Promise<CreateFolderResult> {
    if (!this.adapter) throw new Error('adapter not connected');
    const result = (await this.adapter.invoke('create_folder', {
      name,
      parentSidecarId,
      parentPath,
    })) as CreateFolderResult;
    try {
      await this.refetchRoot();
    } catch (err) {
      console.warn(
        'createFolder: refetchRoot failed (folder was created); UI may show stale list:',
        err,
      );
    }
    return result;
  }

  /** Register an external unlisten handle so it gets cleaned up alongside the service. */
  addUnlisten(fn: () => void): void {
    this.unlisteners.push(fn);
  }

  destroy(): void {
    for (const fn of this.unlisteners) fn();
    this.unlisteners = [];
  }

  // ── Private helpers ─────────────────────────────────────────────────

  private async refetchRoot(): Promise<void> {
    if (!this.adapter) return;
    const raw = (await this.adapter.invoke('list_content', { folderCid: null })) as
      | ContentItemWire[]
      | null
      | undefined;
    this.privateContent = Array.isArray(raw)
      ? raw.filter((w) => !w.archived).map(wireToContentItem)
      : [];
    this.onChange?.();
  }

  private moveToPublished(cids: string[], publishMode: 'durable' | 'ephemeral'): void {
    const cidSet = new Set(cids);
    const toMove = this.privateContent.filter((i) => cidSet.has(i.cid));

    for (const item of toMove) {
      this.publishedContent.push({
        cid: item.cid,
        name: item.name,
        category: item.category,
        sizeBytes: item.sizeBytes,
        publishedAt: Date.now(),
        publishMode,
      });
    }

    this.privateContent = this.privateContent.filter((i) => !cidSet.has(i.cid));
  }
}
