import { describe, it, expect, vi } from 'vitest';
import { LibraryDirectoryService } from '../library-directory-service';
import type { TauriAdapter } from '../zenoh-service';

function mockAdapter(invokeImpl: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>): TauriAdapter {
  return {
    invoke: vi.fn(invokeImpl),
    listen: vi.fn(async () => () => {}),
  } as unknown as TauriAdapter;
}

describe('LibraryDirectoryService', () => {
  it('list() invokes list_libraries with empty args', async () => {
    const adapter = mockAdapter(async (cmd) => {
      expect(cmd).toBe('list_libraries');
      return [];
    });
    const svc = new LibraryDirectoryService(adapter);
    await svc.list();
    expect(adapter.invoke).toHaveBeenCalledWith('list_libraries', {});
  });

  it('add() forwards libraryAddr (camelCase at boundary)', async () => {
    const adapter = mockAdapter(async () => undefined);
    const svc = new LibraryDirectoryService(adapter);
    await svc.add('aabbccddeeff00112233445566778899');
    expect(adapter.invoke).toHaveBeenCalledWith('add_library', {
      libraryAddr: 'aabbccddeeff00112233445566778899',
    });
  });

  it('remove() forwards libraryAddr', async () => {
    const adapter = mockAdapter(async () => undefined);
    const svc = new LibraryDirectoryService(adapter);
    await svc.remove('aabbccddeeff00112233445566778899');
    expect(adapter.invoke).toHaveBeenCalledWith('remove_library', {
      libraryAddr: 'aabbccddeeff00112233445566778899',
    });
  });

  it('browse() with no arg sends null (aggregate across all)', async () => {
    const adapter = mockAdapter(async () => []);
    const svc = new LibraryDirectoryService(adapter);
    await svc.browse();
    expect(adapter.invoke).toHaveBeenCalledWith('browse_library', {
      libraryAddr: null,
    });
  });

  it('browse(addr) filters to that library', async () => {
    const adapter = mockAdapter(async () => []);
    const svc = new LibraryDirectoryService(adapter);
    await svc.browse('aabbccddeeff00112233445566778899');
    expect(adapter.invoke).toHaveBeenCalledWith('browse_library', {
      libraryAddr: 'aabbccddeeff00112233445566778899',
    });
  });
});
