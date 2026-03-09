import type {
  PeerRef,
  StorageBuddy,
  ContentItem,
  PublishedItem,
  CleanupRecommendation,
  QuotaStatus,
  ContentCategory,
} from './types';

const day = 86_400_000;
const now = Date.now();

// ── Peers ───────────────────────────────────────────────────────────

export const mockPeers: PeerRef[] = [
  { address: 'a1b2c3d4e5f6a1b2', displayName: 'Alice' },
  { address: 'b2c3d4e5f6a1b2c3', displayName: 'Bob' },
  { address: 'c3d4e5f6a1b2c3d4', displayName: 'Carol' },
  { address: 'd4e5f6a1b2c3d4e5', displayName: 'Dave' },
];

// ── Storage Buddies ─────────────────────────────────────────────────

export const mockStorageBuddies: StorageBuddy[] = [
  {
    address: mockPeers[0].address,
    displayName: mockPeers[0].displayName,
    storageUsedBytes: 1_200_000_000,
    online: true,
  },
  {
    address: mockPeers[1].address,
    displayName: mockPeers[1].displayName,
    storageUsedBytes: 800_000_000,
    online: false,
  },
];

// ── Private Content ─────────────────────────────────────────────────

export const mockPrivateContent: ContentItem[] = [
  // Folder: "Projects" bundle at root
  {
    cid: 'cid-folder-projects',
    name: 'Projects',
    category: 'bundle',
    sensitivity: 'private',
    sizeBytes: 0,
    storedAt: now - 90 * day,
    lastAccessed: now - 1 * day,
    accessCount: 42,
    stalenessScore: 0.05,
    replicationTier: 'default',
    replicaCount: 3,
    pinned: false,
    licensed: false,
    parentCid: null,
    isFolder: true,
  },
  // File inside Projects folder
  {
    cid: 'cid-design-doc',
    name: 'mesh-design.md',
    category: 'text',
    sensitivity: 'private',
    sizeBytes: 45_000,
    storedAt: now - 60 * day,
    lastAccessed: now - 2 * day,
    accessCount: 18,
    stalenessScore: 0.15,
    replicationTier: 'default',
    replicaCount: 3,
    pinned: false,
    licensed: false,
    parentCid: 'cid-folder-projects',
    isFolder: false,
  },
  // File inside Projects folder
  {
    cid: 'cid-arch-diagram',
    name: 'architecture.svg',
    category: 'image',
    sensitivity: 'private',
    sizeBytes: 320_000,
    storedAt: now - 45 * day,
    lastAccessed: now - 10 * day,
    accessCount: 8,
    stalenessScore: 0.35,
    replicationTier: 'default',
    replicaCount: 3,
    pinned: false,
    licensed: false,
    parentCid: 'cid-folder-projects',
    isFolder: false,
  },
  // Root-level music file, pinned
  {
    cid: 'cid-song-favorite',
    name: 'favorite-track.flac',
    category: 'music',
    sensitivity: 'public',
    sizeBytes: 35_000_000,
    storedAt: now - 120 * day,
    lastAccessed: now - 1 * day,
    accessCount: 200,
    stalenessScore: 0.02,
    replicationTier: 'high',
    replicaCount: 5,
    pinned: true,
    licensed: true,
    parentCid: null,
    isFolder: false,
  },
  // Root-level video, large, under-replicated
  {
    cid: 'cid-video-lecture',
    name: 'distributed-systems-lecture.mp4',
    category: 'video',
    sensitivity: 'public',
    sizeBytes: 1_500_000_000,
    storedAt: now - 30 * day,
    lastAccessed: now - 20 * day,
    accessCount: 2,
    stalenessScore: 0.7,
    replicationTier: 'default',
    replicaCount: 1,
    pinned: false,
    licensed: false,
    parentCid: null,
    isFolder: false,
  },
  // Root-level confidential text
  {
    cid: 'cid-private-keys-backup',
    name: 'key-backup.enc',
    category: 'text',
    sensitivity: 'confidential',
    sizeBytes: 2_048,
    storedAt: now - 200 * day,
    lastAccessed: now - 30 * day,
    accessCount: 3,
    stalenessScore: 0.1,
    replicationTier: 'ultra',
    replicaCount: 7,
    pinned: true,
    licensed: false,
    parentCid: null,
    isFolder: false,
  },
  // Root-level dataset
  {
    cid: 'cid-training-data',
    name: 'sensor-readings.parquet',
    category: 'dataset',
    sensitivity: 'private',
    sizeBytes: 250_000_000,
    storedAt: now - 15 * day,
    lastAccessed: now - 14 * day,
    accessCount: 1,
    stalenessScore: 0.85,
    replicationTier: 'expendable',
    replicaCount: 2,
    pinned: false,
    licensed: false,
    parentCid: null,
    isFolder: false,
  },
  // Root-level software bundle
  {
    cid: 'cid-app-build',
    name: 'harmony-client-v0.3.tar.gz',
    category: 'software',
    sensitivity: 'public',
    sizeBytes: 85_000_000,
    storedAt: now - 7 * day,
    lastAccessed: now - 1 * day,
    accessCount: 12,
    stalenessScore: 0.1,
    replicationTier: 'high',
    replicaCount: 4,
    pinned: false,
    licensed: true,
    parentCid: null,
    isFolder: false,
  },
  // Root-level intimate photo
  {
    cid: 'cid-family-photo',
    name: 'family-reunion-2025.jpg',
    category: 'image',
    sensitivity: 'intimate',
    sizeBytes: 8_500_000,
    storedAt: now - 180 * day,
    lastAccessed: now - 60 * day,
    accessCount: 5,
    stalenessScore: 0.55,
    replicationTier: 'light',
    replicaCount: 2,
    pinned: false,
    licensed: false,
    parentCid: null,
    isFolder: false,
  },
];

// ── Published Content ───────────────────────────────────────────────

export const mockPublishedContent: PublishedItem[] = [
  {
    cid: 'cid-pub-blog',
    name: 'decentralized-identity-post.md',
    category: 'text',
    sizeBytes: 12_000,
    publishedAt: now - 14 * day,
    publishMode: 'durable',
  },
  {
    cid: 'cid-pub-album',
    name: 'ambient-loops-vol2.zip',
    category: 'music',
    sizeBytes: 120_000_000,
    publishedAt: now - 7 * day,
    publishMode: 'durable',
  },
  {
    cid: 'cid-pub-status',
    name: 'weekly-status-update.txt',
    category: 'text',
    sizeBytes: 3_500,
    publishedAt: now - 1 * day,
    publishMode: 'ephemeral',
  },
];

// ── Cleanup Recommendations ─────────────────────────────────────────

export const mockCleanupRecommendations: CleanupRecommendation[] = [
  {
    cid: 'cid-training-data',
    name: 'sensor-readings.parquet',
    category: 'dataset',
    sizeBytes: 250_000_000,
    reason: 'stale',
    stalenessScore: 0.85,
    spaceRecoverable: 250_000_000,
    confidence: 0.92,
  },
  {
    cid: 'cid-video-lecture',
    name: 'distributed-systems-lecture.mp4',
    category: 'video',
    sizeBytes: 1_500_000_000,
    reason: 'stale',
    stalenessScore: 0.7,
    spaceRecoverable: 1_500_000_000,
    confidence: 0.78,
  },
  {
    cid: 'cid-family-photo',
    name: 'family-reunion-2025.jpg',
    category: 'image',
    sizeBytes: 8_500_000,
    reason: 'over-replicated',
    stalenessScore: 0.55,
    spaceRecoverable: 8_500_000,
    confidence: 0.65,
  },
];

// ── Quota ────────────────────────────────────────────────────────────

export function mockQuotaStatus(): QuotaStatus {
  const totalBytes = 10_000_000_000; // 10 GB
  const byCategory: Partial<Record<ContentCategory, number>> = {};
  let usedBytes = 0;

  for (const item of mockPrivateContent) {
    usedBytes += item.sizeBytes;
    byCategory[item.category] = (byCategory[item.category] ?? 0) + item.sizeBytes;
  }

  return { usedBytes, totalBytes, byCategory };
}
