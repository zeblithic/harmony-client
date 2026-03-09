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
} from './types';
import {
  mockPrivateContent,
  mockPublishedContent,
  mockCleanupRecommendations,
  mockStorageBuddies,
  mockPeers,
} from './mock-file-data';

export class FileManagerService {
  readonly settings: FileManagerSettings;
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

  /** Returns private content. With no args returns all; with parentCid filters by parent. */
  getContents(parentCid?: string | null): ContentItem[] {
    if (parentCid === undefined) {
      return this.privateContent;
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
      storageBuddies: [mockPeers[2]],
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

  /** Returns cleanup recommendations, filtering out burned items, sorted by confidence desc. */
  getCleanupRecommendations(): CleanupRecommendation[] {
    const activeCids = new Set(this.privateContent.map((i) => i.cid));
    return this.cleanupRecommendations
      .filter((r) => activeCids.has(r.cid))
      .sort((a, b) => b.confidence - a.confidence);
  }

  /** Returns storage buddies. */
  getStorageBuddies(): StorageBuddy[] {
    return this.storageBuddies;
  }

  /** Returns published content. */
  getPublishedContent(): PublishedItem[] {
    return this.publishedContent;
  }

  /** Permanently removes content items and frees their quota. */
  burn(cids: string[]): void {
    const cidSet = new Set(cids);
    this.privateContent = this.privateContent.filter((i) => !cidSet.has(i.cid));
  }

  /** Archive stub — no-op for now. */
  archive(_cids: string[]): void {
    // Future: move to cold storage tier
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
  }

  /** Clears the pinned flag on a content item. */
  unpin(cid: string): void {
    const item = this.privateContent.find((i) => i.cid === cid);
    if (item) item.pinned = false;
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

  /** Export stub — no-op for now. */
  exportToDevice(_cids: string[]): void {
    // Future: trigger Tauri file save dialog
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
