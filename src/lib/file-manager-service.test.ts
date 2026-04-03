import { describe, it, expect, vi } from 'vitest';
import { FileManagerService, type ContentAnnouncementEvent } from './file-manager-service';
import { createMockAdapter } from './test-utils';

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

  it('archive removes content from private and frees quota', () => {
    const svc = new FileManagerService();
    const before = svc.getQuotaStatus();
    const target = svc.getContents().find((i) => i.cid === 'cid-design-doc');
    expect(target).toBeDefined();

    svc.archive([target!.cid]);

    const after = svc.getQuotaStatus();
    expect(after.usedBytes).toBe(before.usedBytes - target!.sizeBytes);
    expect(svc.getContents().find((i) => i.cid === 'cid-design-doc')).toBeUndefined();
  });

  it('archive invokes archive_content on the adapter for each cid', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.archive(['cid-design-doc', 'cid-video-lecture']);
    expect(adapter.invoke).toHaveBeenCalledWith('archive_content', { cid: 'cid-design-doc' });
    expect(adapter.invoke).toHaveBeenCalledWith('archive_content', { cid: 'cid-video-lecture' });
  });

  it('exportToDevice invokes export_content with cid and file name', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.exportToDevice(['cid-design-doc']);
    expect(adapter.invoke).toHaveBeenCalledWith('export_content', {
      cid: 'cid-design-doc',
      fileName: 'mesh-design.md',
    });
  });

  it('exportToDevice uses cid as fallback name for unknown items', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    await svc.exportToDevice(['unknown-cid']);
    expect(adapter.invoke).toHaveBeenCalledWith('export_content', {
      cid: 'unknown-cid',
      fileName: 'unknown-cid',
    });
  });

  it('exportToDevice is a no-op without adapter', async () => {
    const svc = new FileManagerService();
    // Should not throw when adapter is null
    await expect(svc.exportToDevice(['cid-design-doc'])).resolves.toBeUndefined();
  });

  // ── connectAdapter ────────────────────────────────────────────────

  it('registers a content-announced listener', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    expect(adapter.listen).toHaveBeenCalledWith('content-announced', expect.any(Function));
  });

  it('idempotent — second connectAdapter is a no-op', async () => {
    const svc = new FileManagerService();
    const { adapter: a1 } = createMockAdapter();
    const { adapter: a2 } = createMockAdapter();
    await svc.connectAdapter(a1);
    await svc.connectAdapter(a2);
    expect(a2.listen).not.toHaveBeenCalled();
  });

  it('tracks announced CIDs from network events', async () => {
    const svc = new FileManagerService();
    const { adapter, emit } = createMockAdapter();
    await svc.connectAdapter(adapter);
    emit('content-announced', { cid: 'abc123', sizeBytes: 4096 } satisfies ContentAnnouncementEvent);
    expect(svc.announcedCids.has('abc123')).toBe(true);
    expect(svc.announcedCids.get('abc123')!.sizeBytes).toBe(4096);
  });

  it('deduplicates announced CIDs', async () => {
    const svc = new FileManagerService();
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    emit('content-announced', { cid: 'dup1', sizeBytes: 100 } satisfies ContentAnnouncementEvent);
    emit('content-announced', { cid: 'dup1', sizeBytes: 200 } satisfies ContentAnnouncementEvent);
    expect(svc.announcedCids.get('dup1')!.sizeBytes).toBe(100); // first wins
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  it('calls onChange on new content announcement', async () => {
    const svc = new FileManagerService();
    const { adapter, emit } = createMockAdapter();
    svc.onChange = vi.fn();
    await svc.connectAdapter(adapter);
    emit('content-announced', { cid: 'new1', sizeBytes: 512 } satisfies ContentAnnouncementEvent);
    expect(svc.onChange).toHaveBeenCalledOnce();
  });

  // ── adapter invoke on mutations ───────────────────────────────────

  it('burn invokes burn_content on the adapter for each cid', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.burn(['cid-training-data', 'cid-video-lecture']);
    expect(adapter.invoke).toHaveBeenCalledWith('burn_content', { cid: 'cid-training-data' });
    expect(adapter.invoke).toHaveBeenCalledWith('burn_content', { cid: 'cid-video-lecture' });
  });

  it('pin invokes pin_content on the adapter', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.pin('cid-video-lecture');
    expect(adapter.invoke).toHaveBeenCalledWith('pin_content', { cid: 'cid-video-lecture' });
  });

  it('unpin invokes unpin_content on the adapter', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.unpin('cid-song-favorite');
    expect(adapter.invoke).toHaveBeenCalledWith('unpin_content', { cid: 'cid-song-favorite' });
  });

  // ── destroy / addUnlisten ─────────────────────────────────────────

  it('destroy calls all registered unlisteners', async () => {
    const svc = new FileManagerService();
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    const external = vi.fn();
    svc.addUnlisten(external);
    svc.destroy();
    expect(unlisten).toHaveBeenCalledOnce();
    expect(external).toHaveBeenCalledOnce();
  });

  it('destroy is safe to call twice', async () => {
    const svc = new FileManagerService();
    const { adapter, unlisten } = createMockAdapter();
    await svc.connectAdapter(adapter);
    svc.destroy();
    svc.destroy();
    expect(unlisten).toHaveBeenCalledOnce();
  });
});
