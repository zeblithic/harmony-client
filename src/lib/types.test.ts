import { describe, it, expect } from 'vitest';
import type {
  AppMode,
  ReplicationTier,
  ContentItem,
  ContentDetail,
  QuotaStatus,
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

  it('ContentDetail is an alias of ContentItem (ZEB-612 S3: mock peer fields gone)', () => {
    const detail: ContentDetail = {
      sidecarId: 'sidecar-abc',
      cid: 'abc123',
      name: 'photo.jpg',
      category: 'image',
      sensitivity: 'private',
      sizeBytes: 1024,
      storedAt: 1000,
      replicationTier: 'default',
      replicaCount: 3,
      pinned: false,
      licensed: false,
      parentCid: null,
      isFolder: false,
    };
    expect(detail.replicaCount).toBe(3);
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
});
