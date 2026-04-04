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
    this.adapter = adapter;
    const unlisten = await adapter.listen(
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
    this.unlisteners.push(unlisten);
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
  burn(cids: string[]): void {
    const cidSet = new Set(cids);
    this.privateContent = this.privateContent.filter((i) => !cidSet.has(i.cid));
    if (this.adapter) {
      for (const cid of cids) {
        this.adapter.invoke('burn_content', { cid }).catch(() => {});
      }
    }
  }

  /** Move content to cold storage (archive tier). Items are removed from
   *  the active file list and the backend is notified to migrate the data. */
  archive(cids: string[]): void {
    const cidSet = new Set(cids);
    this.privateContent = this.privateContent.filter((i) => !cidSet.has(i.cid));
    if (this.adapter) {
      for (const cid of cids) {
        this.adapter.invoke('archive_content', { cid }).catch(() => {});
      }
    }
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
  pin(cid: string): void {
    const item = this.privateContent.find((i) => i.cid === cid);
    if (item) item.pinned = true;
    if (this.adapter) {
      this.adapter.invoke('pin_content', { cid }).catch(() => {});
    }
  }

  /** Clears the pinned flag on a content item. */
  unpin(cid: string): void {
    const item = this.privateContent.find((i) => i.cid === cid);
    if (item) item.pinned = false;
    if (this.adapter) {
      this.adapter.invoke('unpin_content', { cid }).catch(() => {});
    }
  }

  /** Updates the replication tier for specified items. */
  setReplicationTier(cids: string[], tier: ReplicationTier): void {
    const cidSet = new Set(cids);
    for (const item of this.privateContent) {
      if (cidSet.has(item.cid)) {
        item.replicationTier = tier;
      }
    }
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
      parentCid: parentCid ?? null,
      isFolder: false,
    };
    this.privateContent.push(item);
    return item;
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
