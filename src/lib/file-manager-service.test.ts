import { describe, it, expect } from 'vitest';
import { FileManagerService } from './file-manager-service';

describe('FileManagerService', () => {
  it('constructs with default settings', () => {
    const svc = new FileManagerService();
    expect(svc.settings.defaultReplicationTier).toBe('default');
    expect(svc.settings.quotaBytes).toBe(10_000_000_000);
  });

  it('constructs with custom settings', () => {
    const svc = new FileManagerService({ defaultReplicationTier: 'high', quotaBytes: 5_000_000_000 });
    expect(svc.settings.defaultReplicationTier).toBe('high');
    expect(svc.settings.quotaBytes).toBe(5_000_000_000);
  });

  it('returns private content', () => {
    const svc = new FileManagerService();
    const items = svc.getContents();
    expect(items.length).toBeGreaterThan(0);
    // All items should have a cid
    for (const item of items) {
      expect(item.cid).toBeTruthy();
    }
  });

  it('filters content by parentCid null for root items', () => {
    const svc = new FileManagerService();
    const root = svc.getContents(null);
    for (const item of root) {
      expect(item.parentCid).toBeNull();
    }
    // Should include both folders and files at root
    expect(root.length).toBeGreaterThan(0);
  });

  it('filters content by specific parentCid for children', () => {
    const svc = new FileManagerService();
    const children = svc.getContents('cid-folder-projects');
    expect(children.length).toBeGreaterThan(0);
    for (const item of children) {
      expect(item.parentCid).toBe('cid-folder-projects');
    }
  });

  it('returns empty array for parentCid with no children', () => {
    const svc = new FileManagerService();
    const children = svc.getContents('nonexistent-cid');
    expect(children).toEqual([]);
  });

  it('returns quota status with correct totals', () => {
    const svc = new FileManagerService();
    const quota = svc.getQuotaStatus();
    expect(quota.totalBytes).toBe(10_000_000_000);
    expect(quota.usedBytes).toBeGreaterThan(0);
    expect(quota.usedBytes).toBeLessThan(quota.totalBytes);
    // byCategory should have entries
    expect(Object.keys(quota.byCategory).length).toBeGreaterThan(0);
  });

  it('returns cleanup recommendations sorted by confidence descending', () => {
    const svc = new FileManagerService();
    const recs = svc.getCleanupRecommendations();
    expect(recs.length).toBeGreaterThan(0);
    for (let i = 1; i < recs.length; i++) {
      expect(recs[i - 1].confidence).toBeGreaterThanOrEqual(recs[i].confidence);
    }
  });

  it('returns storage buddies', () => {
    const svc = new FileManagerService();
    const buddies = svc.getStorageBuddies();
    expect(buddies.length).toBe(2);
    expect(buddies[0].displayName).toBeTruthy();
  });

  it('returns published content', () => {
    const svc = new FileManagerService();
    const published = svc.getPublishedContent();
    expect(published.length).toBe(3);
  });

  it('burn removes content and frees quota', () => {
    const svc = new FileManagerService();
    const before = svc.getQuotaStatus();
    const target = svc.getContents().find((i) => i.cid === 'cid-training-data');
    expect(target).toBeDefined();

    svc.burn([target!.cid]);

    const after = svc.getQuotaStatus();
    expect(after.usedBytes).toBe(before.usedBytes - target!.sizeBytes);
    // Item should no longer appear in contents
    expect(svc.getContents().find((i) => i.cid === 'cid-training-data')).toBeUndefined();
  });

  it('burn filters out burned items from cleanup recommendations', () => {
    const svc = new FileManagerService();
    svc.burn(['cid-training-data']);
    const recs = svc.getCleanupRecommendations();
    expect(recs.find((r) => r.cid === 'cid-training-data')).toBeUndefined();
  });

  it('pin toggles pinned state on', () => {
    const svc = new FileManagerService();
    const item = svc.getContents().find((i) => i.cid === 'cid-video-lecture');
    expect(item!.pinned).toBe(false);

    svc.pin('cid-video-lecture');

    const updated = svc.getContents().find((i) => i.cid === 'cid-video-lecture');
    expect(updated!.pinned).toBe(true);
  });

  it('unpin toggles pinned state off', () => {
    const svc = new FileManagerService();
    // cid-song-favorite is pinned by default
    const item = svc.getContents().find((i) => i.cid === 'cid-song-favorite');
    expect(item!.pinned).toBe(true);

    svc.unpin('cid-song-favorite');

    const updated = svc.getContents().find((i) => i.cid === 'cid-song-favorite');
    expect(updated!.pinned).toBe(false);
  });

  it('publish moves content to published as durable and removes from private', () => {
    const svc = new FileManagerService();
    const beforePrivate = svc.getContents().length;
    const beforePublished = svc.getPublishedContent().length;

    svc.publish(['cid-app-build']);

    expect(svc.getContents().length).toBe(beforePrivate - 1);
    expect(svc.getPublishedContent().length).toBe(beforePublished + 1);
    // Should not be in private anymore
    expect(svc.getContents().find((i) => i.cid === 'cid-app-build')).toBeUndefined();
    // Should be in published with durable mode
    const pub = svc.getPublishedContent().find((i) => i.cid === 'cid-app-build');
    expect(pub).toBeDefined();
    expect(pub!.publishMode).toBe('durable');
  });

  it('release moves content to published as ephemeral', () => {
    const svc = new FileManagerService();
    const beforePublished = svc.getPublishedContent().length;

    svc.release(['cid-design-doc']);

    const pub = svc.getPublishedContent().find((i) => i.cid === 'cid-design-doc');
    expect(pub).toBeDefined();
    expect(pub!.publishMode).toBe('ephemeral');
    expect(svc.getPublishedContent().length).toBe(beforePublished + 1);
    expect(svc.getContents().find((i) => i.cid === 'cid-design-doc')).toBeUndefined();
  });

  it('setReplicationTier updates tier for specified items', () => {
    const svc = new FileManagerService();
    svc.setReplicationTier(['cid-video-lecture', 'cid-training-data'], 'high');

    const video = svc.getContents().find((i) => i.cid === 'cid-video-lecture');
    const data = svc.getContents().find((i) => i.cid === 'cid-training-data');
    expect(video!.replicationTier).toBe('high');
    expect(data!.replicationTier).toBe('high');
  });

  it('getContentDetail returns extended info for a known cid', () => {
    const svc = new FileManagerService();
    const detail = svc.getContentDetail('cid-song-favorite');
    expect(detail).toBeDefined();
    expect(detail!.cid).toBe('cid-song-favorite');
    expect(detail!.origin).toBeDefined();
    expect(detail!.sharedWith).toBeDefined();
    expect(detail!.storageBuddies).toBeDefined();
  });

  it('getContentDetail returns undefined for unknown cid', () => {
    const svc = new FileManagerService();
    expect(svc.getContentDetail('nonexistent')).toBeUndefined();
  });

  it('each instance is independent (structuredClone)', () => {
    const svc1 = new FileManagerService();
    const svc2 = new FileManagerService();
    svc1.burn(['cid-training-data']);
    // svc2 should still have the item
    expect(svc2.getContents().find((i) => i.cid === 'cid-training-data')).toBeDefined();
  });

  it('archive is a no-op stub that does not throw', () => {
    const svc = new FileManagerService();
    expect(() => svc.archive(['cid-design-doc'])).not.toThrow();
  });

  it('exportToDevice is a stub that does not throw', () => {
    const svc = new FileManagerService();
    expect(() => svc.exportToDevice(['cid-design-doc'])).not.toThrow();
  });
});
