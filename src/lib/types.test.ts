import { describe, it, expect } from 'vitest';
import type {
  AppMode,
  ReplicationTier,
  ContentItem,
  ContentDetail,
  QuotaStatus,
  CleanupRecommendation,
  StorageBuddy,
  PublishedItem,
} from './types';

describe('File manager types', () => {
  it('AppMode includes files', () => {
    const mode: AppMode = 'files';
    expect(mode).toBe('files');
  });

  it('ReplicationTier has all five levels', () => {
    const tiers: ReplicationTier[] = ['expendable', 'light', 'default', 'high', 'ultra'];
    expect(tiers).toHaveLength(5);
  });

  it('ContentItem has required fields', () => {
    const item: ContentItem = {
      sidecarId: 'sidecar-abc',
      cid: 'abc123',
      name: 'photo.jpg',
      category: 'image',
      sensitivity: 'private',
      sizeBytes: 1024,
      storedAt: 1000,
      lastAccessed: 2000,
      accessCount: 5,
      stalenessScore: 0.3,
      replicationTier: 'default',
      replicaCount: 3,
      pinned: false,
      licensed: false,
      parentCid: null,
      isFolder: false,
    };
    expect(item.cid).toBe('abc123');
    expect(item.isFolder).toBe(false);
  });

  it('ContentDetail extends ContentItem with peer info', () => {
    const detail: ContentDetail = {
      sidecarId: 'sidecar-abc',
      cid: 'abc123',
      name: 'photo.jpg',
      category: 'image',
      sensitivity: 'private',
      sizeBytes: 1024,
      storedAt: 1000,
      lastAccessed: 2000,
      accessCount: 5,
      stalenessScore: 0.3,
      replicationTier: 'default',
      replicaCount: 3,
      pinned: false,
      licensed: false,
      parentCid: null,
      isFolder: false,
      sharedWith: [{ address: 'peer1', displayName: 'Alice' }],
      storageBuddies: [{ address: 'peer2', displayName: 'Bob' }],
      origin: 'self-created',
    };
    expect(detail.sharedWith).toHaveLength(1);
    expect(detail.origin).toBe('self-created');
  });

  it('QuotaStatus has usage fields (ZEB-612 S3: no invented total)', () => {
    const quota: QuotaStatus = {
      usedBytes: 5_000_000_000,
      byCategory: { image: 2_000_000_000, video: 3_000_000_000 },
      pinnedUsedBytes: 1_000_000,
      pinnedBudgetBytes: 50_000_000,
    };
    expect(quota.pinnedUsedBytes).toBeLessThan(quota.pinnedBudgetBytes!);
  });

  it('CleanupRecommendation has action-relevant fields', () => {
    const rec: CleanupRecommendation = {
      sidecarId: 'sidecar-abc',
      cid: 'abc123',
      name: 'old-doc.txt',
      category: 'text',
      sensitivity: 'private',
      sizeBytes: 50_000,
      reason: 'stale',
      stalenessScore: 0.87,
      spaceRecoverable: 150_000,
      confidence: 0.87,
    };
    expect(rec.reason).toBe('stale');
    expect(rec.confidence).toBeGreaterThan(0.8);
  });

  it('StorageBuddy tracks storage used', () => {
    const buddy: StorageBuddy = {
      address: 'peer3',
      displayName: 'Charlie',
      storageUsedBytes: 500_000_000,
      online: true,
    };
    expect(buddy.online).toBe(true);
  });

  it('PublishedItem includes publish mode', () => {
    const item: PublishedItem = {
      cid: 'pub1',
      name: 'my-song.mp3',
      category: 'music',
      sizeBytes: 8_000_000,
      publishedAt: 1000,
      publishMode: 'durable',
    };
    expect(item.publishMode).toBe('durable');
  });
});
