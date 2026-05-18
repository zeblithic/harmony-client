import { describe, it, expect, vi } from 'vitest';
import { FileManagerService, type MoveContentResult } from './file-manager-service';
import type { TauriAdapter } from './zenoh-service';
import { createMockAdapter } from './test-utils';

/**
 * ZEB-162 — moveContent service-layer wire shape and refresh behaviour.
 *
 * The case dispatch lives on the backend; the service is responsible
 * for serialising the right arg shape over IPC (Tauri auto-converts
 * camelCase → snake_case). These tests pin one example per case (A/B/C/D)
 * and verify that on success, the service triggers the same root-refresh
 * pattern as createFolder.
 */
describe('FileManagerService.moveContent', () => {
  function makeMoveResult(overrides: Partial<MoveContentResult> = {}): MoveContentResult {
    return {
      srcNewCid: 'aa'.repeat(32),
      dstSidecarId: 'mock-sidecar-dst',
      dstNewCid: 'bb'.repeat(32),
      ...overrides,
    };
  }

  function installMoveInvoke(adapter: TauriAdapter, moveResult: MoveContentResult) {
    // Per-command dispatcher: list_content returns [] (so the
    // refetchRoot after connectAdapter / after moveContent succeeds
    // without touching mock state), move_content returns the configured
    // wire shape, everything else falls back to undefined.
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'move_content') return Promise.resolve(moveResult);
      return Promise.resolve(undefined);
    });
  }

  it('throws when no adapter is connected', async () => {
    const svc = new FileManagerService();
    await expect(
      svc.moveContent({
        srcSidecarId: 'sid-src',
        srcPath: ['aa'.repeat(32)],
        srcChildCid: 'cc'.repeat(32),
        dstSidecarId: 'sid-dst',
        dstPath: ['dd'.repeat(32)],
      }),
    ).rejects.toThrow('adapter not connected');
  });

  // ── Case A: same top-level ─────────────────────────────────────────

  it('Case A: emits move_content with same src/dst sidecar and same top-level CID', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    const moveResult = makeMoveResult({
      srcNewCid: '11'.repeat(32),
      dstSidecarId: 'sid-T',
      dstNewCid: '11'.repeat(32), // same top-level → src_new === dst_new
    });
    installMoveInvoke(adapter, moveResult);
    await svc.connectAdapter(adapter);

    const out = await svc.moveContent({
      srcSidecarId: 'sid-T',
      srcPath: ['top'.padEnd(64, '0'), 'parA'.padEnd(64, '0')],
      srcChildCid: 'child'.padEnd(64, '0'),
      dstSidecarId: 'sid-T',
      dstPath: ['top'.padEnd(64, '0'), 'parB'.padEnd(64, '0')],
    });

    expect(adapter.invoke).toHaveBeenCalledWith('move_content', {
      srcSidecarId: 'sid-T',
      srcPath: ['top'.padEnd(64, '0'), 'parA'.padEnd(64, '0')],
      srcChildCid: 'child'.padEnd(64, '0'),
      dstSidecarId: 'sid-T',
      dstPath: ['top'.padEnd(64, '0'), 'parB'.padEnd(64, '0')],
      newName: null,
    });
    expect(out).toEqual(moveResult);
  });

  // ── Case B: across two top-levels ──────────────────────────────────

  it('Case B: emits move_content with different src/dst sidecar ids', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    installMoveInvoke(adapter, makeMoveResult({ dstSidecarId: 'sid-T2' }));
    await svc.connectAdapter(adapter);

    await svc.moveContent({
      srcSidecarId: 'sid-T1',
      srcPath: ['t1'.padEnd(64, '0'), 'p'.padEnd(64, '0')],
      srcChildCid: 'c'.padEnd(64, '0'),
      dstSidecarId: 'sid-T2',
      dstPath: ['t2'.padEnd(64, '0')],
    });

    expect(adapter.invoke).toHaveBeenCalledWith(
      'move_content',
      expect.objectContaining({
        srcSidecarId: 'sid-T1',
        dstSidecarId: 'sid-T2',
        srcPath: ['t1'.padEnd(64, '0'), 'p'.padEnd(64, '0')],
        dstPath: ['t2'.padEnd(64, '0')],
        newName: null,
      }),
    );
  });

  // ── Case C: root → nested ──────────────────────────────────────────

  it('Case C: emits move_content with srcPath of length 1 equal to srcChildCid', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    installMoveInvoke(adapter, makeMoveResult({ srcNewCid: null }));
    await svc.connectAdapter(adapter);

    const topCid = 'aa'.repeat(32);
    await svc.moveContent({
      srcSidecarId: 'sid-leaf-as-top',
      srcPath: [topCid],
      srcChildCid: topCid,
      dstSidecarId: 'sid-dst',
      dstPath: ['dd'.repeat(32)],
    });

    expect(adapter.invoke).toHaveBeenCalledWith(
      'move_content',
      expect.objectContaining({
        srcSidecarId: 'sid-leaf-as-top',
        srcPath: [topCid],
        srcChildCid: topCid,
        dstSidecarId: 'sid-dst',
        dstPath: ['dd'.repeat(32)],
        newName: null,
      }),
    );
  });

  // ── Case D: nested → root ──────────────────────────────────────────

  it('Case D: emits move_content with dstSidecarId=null and dstPath=[]', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    installMoveInvoke(adapter, makeMoveResult({ dstSidecarId: 'sid-newly-minted' }));
    await svc.connectAdapter(adapter);

    await svc.moveContent({
      srcSidecarId: 'sid-T',
      srcPath: ['top'.padEnd(64, '0'), 'parent'.padEnd(64, '0')],
      srcChildCid: 'child'.padEnd(64, '0'),
      dstSidecarId: null,
      dstPath: [],
    });

    expect(adapter.invoke).toHaveBeenCalledWith(
      'move_content',
      expect.objectContaining({
        dstSidecarId: null,
        dstPath: [],
        newName: null,
      }),
    );
  });

  // ── Refresh-on-success ─────────────────────────────────────────────

  it('refreshes the root listing after a successful move (refetchRoot fires onChange)', async () => {
    const svc = new FileManagerService();
    const { adapter } = createMockAdapter();
    installMoveInvoke(adapter, makeMoveResult());
    await svc.connectAdapter(adapter);

    // Reset the spy so we count only post-connect invocations and
    // detect the post-move refetch precisely.
    adapter.invoke = vi.fn().mockImplementation((cmd: string) => {
      if (cmd === 'list_content') return Promise.resolve([]);
      if (cmd === 'move_content') return Promise.resolve(makeMoveResult());
      return Promise.resolve(undefined);
    });
    const onChange = vi.fn();
    svc.onChange = onChange;

    await svc.moveContent({
      srcSidecarId: 'sid-T',
      srcPath: ['t'.padEnd(64, '0'), 'p'.padEnd(64, '0')],
      srcChildCid: 'c'.padEnd(64, '0'),
      dstSidecarId: 'sid-T',
      dstPath: ['t'.padEnd(64, '0'), 'q'.padEnd(64, '0')],
    });

    // The service called move_content AND list_content (the refresh).
    expect(adapter.invoke).toHaveBeenCalledWith('move_content', expect.any(Object));
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
      if (cmd === 'move_content') {
        return Promise.reject(new Error("destination already has an entry named 'foo.txt'"));
      }
      return Promise.resolve(undefined);
    });
    await svc.connectAdapter(adapter);

    await expect(
      svc.moveContent({
        srcSidecarId: 'sid-T',
        srcPath: ['t'.padEnd(64, '0'), 'p'.padEnd(64, '0')],
        srcChildCid: 'c'.padEnd(64, '0'),
        dstSidecarId: 'sid-T',
        dstPath: ['t'.padEnd(64, '0'), 'q'.padEnd(64, '0')],
      }),
    ).rejects.toThrow("destination already has an entry named 'foo.txt'");
  });
});
