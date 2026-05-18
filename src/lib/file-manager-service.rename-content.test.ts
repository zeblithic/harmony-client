import { describe, it, expect, vi } from 'vitest';
import { FileManagerService, type RenameContentResult } from './file-manager-service';
import type { TauriAdapter } from './zenoh-service';
import { createMockAdapter } from './test-utils';

/**
 * ZEB-299 — renameContent service-layer wire shape and refresh behaviour.
 *
 * Mirrors file-manager-service.move-content.test.ts. The case dispatch
 * (top-level vs nested) lives on the backend; the service is
 * responsible for serialising the right arg shape over IPC (Tauri
 * auto-converts camelCase → snake_case). These tests pin one example
 * per case and verify that on success the service triggers the same
 * root-refresh pattern as createFolder / moveContent.
 */
describe('FileManagerService.renameContent', () => {
  function makeRenameResult(
    overrides: Partial<RenameContentResult> = {},
  ): RenameContentResult {
    return {
      srcNewCid: null,
      ...overrides,
    };
  }

  function installRenameInvoke(
    adapter: TauriAdapter,
    renameResult: RenameContentResult,
  ) {
    // Per-command dispatcher: list_content returns [] (so refetchRoot
    // after connectAdapter and after renameContent succeeds without
    // touching mock state), rename_content returns the configured wire
    // shape, everything else falls back to undefined.
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'rename_content') return Promise.resolve(renameResult);
      return Promise.resolve(undefined);
    });
  }

  it('throws when no adapter is connected', async () => {
    const svc = new FileManagerService();
    await expect(
      svc.renameContent({
        srcSidecarId: 'sid-src',
        srcPath: ['aa'.repeat(32)],
        srcChildCid: 'aa'.repeat(32),
        srcChildName: 'old.txt',
        newName: 'new.txt',
      }),
    ).rejects.toThrow('adapter not connected');
  });

  // ── Top-level rename ─────────────────────────────────────────────

  it('top-level: emits rename_content with srcPath length 1 equal to srcChildCid, returns srcNewCid=null', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    const renameResult = makeRenameResult({ srcNewCid: null });
    installRenameInvoke(adapter, renameResult);
    await svc.connectAdapter(adapter);

    const topCid = 'aa'.repeat(32);
    const out = await svc.renameContent({
      srcSidecarId: 'sid-top',
      srcPath: [topCid],
      srcChildCid: topCid,
      srcChildName: 'hello.txt',
      newName: 'world.txt',
    });

    expect(adapter.invoke).toHaveBeenCalledWith('rename_content', {
      srcSidecarId: 'sid-top',
      srcPath: [topCid],
      srcChildCid: topCid,
      srcChildName: 'hello.txt',
      newName: 'world.txt',
    });
    expect(out).toEqual(renameResult);
    expect(out.srcNewCid).toBeNull();
  });

  // ── Nested rename ────────────────────────────────────────────────

  it('nested: emits rename_content with srcPath length >= 1, returns srcNewCid=<hex>', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    const renameResult = makeRenameResult({ srcNewCid: 'bb'.repeat(32) });
    installRenameInvoke(adapter, renameResult);
    await svc.connectAdapter(adapter);

    const topCid = 'cc'.repeat(32);
    const parentCid = 'dd'.repeat(32);
    const childCid = 'ee'.repeat(32);
    const out = await svc.renameContent({
      srcSidecarId: 'sid-T',
      srcPath: [topCid, parentCid],
      srcChildCid: childCid,
      srcChildName: 'old-name',
      newName: 'new-name',
    });

    expect(adapter.invoke).toHaveBeenCalledWith(
      'rename_content',
      expect.objectContaining({
        srcSidecarId: 'sid-T',
        srcPath: [topCid, parentCid],
        srcChildCid: childCid,
        srcChildName: 'old-name',
        newName: 'new-name',
      }),
    );
    expect(out.srcNewCid).toBe('bb'.repeat(32));
  });

  // ── Refresh-on-success ───────────────────────────────────────────

  it('refreshes the root listing after a successful rename (refetchRoot fires onChange)', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    installRenameInvoke(adapter, makeRenameResult());
    await svc.connectAdapter(adapter);

    // Reset the spy so we count only post-connect invocations and
    // detect the post-rename refetch precisely.
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'rename_content') return Promise.resolve(makeRenameResult());
      return Promise.resolve(undefined);
    });
    const onChange = vi.fn();
    svc.onChange = onChange;

    await svc.renameContent({
      srcSidecarId: 'sid-T',
      srcPath: ['t'.padEnd(64, '0'), 'p'.padEnd(64, '0')],
      srcChildCid: 'c'.padEnd(64, '0'),
      srcChildName: 'old.txt',
      newName: 'new.txt',
    });

    expect(adapter.invoke).toHaveBeenCalledWith('rename_content', expect.any(Object));
    expect(adapter.invoke).toHaveBeenCalledWith('list_content', { folderCid: null });
    // refetchRoot fires onChange so the parent App.svelte can bump its
    // version counter (which in turn re-runs FileBrowser's folder-fetch
    // effect, keeping the in-folder view consistent).
    expect(onChange).toHaveBeenCalled();
  });

  it('propagates backend errors to the caller without swallowing them', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'rename_content') {
        return Promise.reject(
          new Error("parent folder already has an entry named 'foo.txt'"),
        );
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    await expect(
      svc.renameContent({
        srcSidecarId: 'sid-T',
        srcPath: ['t'.padEnd(64, '0'), 'p'.padEnd(64, '0')],
        srcChildCid: 'c'.padEnd(64, '0'),
        srcChildName: 'bar.txt',
        newName: 'foo.txt',
      }),
    ).rejects.toThrow("parent folder already has an entry named 'foo.txt'");
  });
});
