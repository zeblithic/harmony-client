import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { AvatarResolver } from '../avatar-resolver';
import type { TauriAdapter } from '../zenoh-service';

// PNG magic bytes so the resolver's detectImageMime returns image/png.
const PNG_BYTES = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

function makeAdapter(): TauriAdapter {
  return { invoke: vi.fn().mockResolvedValue(PNG_BYTES) } as unknown as TauriAdapter;
}

let createImageBitmapMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  createImageBitmapMock = vi.fn().mockResolvedValue({ width: 256, height: 256, close: vi.fn() });
  vi.stubGlobal('createImageBitmap', createImageBitmapMock);
  // jsdom lacks URL.createObjectURL / revokeObjectURL.
  vi.stubGlobal('URL', {
    ...URL,
    createObjectURL: vi.fn(() => 'blob:mock-url'),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('AvatarResolver — receive-side decode guard (ZEB-344)', () => {
  it('fetches via fetch_avatar and caches a URL for an in-bounds image', async () => {
    const resolver = new AvatarResolver();
    const adapter = makeAdapter();
    resolver.connectAdapter(adapter);

    await (resolver as unknown as { fetchCid(cid: string): Promise<void> }).fetchCid('aa');

    expect(adapter.invoke).toHaveBeenCalledWith('fetch_avatar', { cid: 'aa' });
    expect(resolver.resolve('aa')).toBe('blob:mock-url');
  });

  it('rejects an over-dimension (decode-bomb) image and falls back (no cached URL)', async () => {
    createImageBitmapMock.mockResolvedValue({ width: 9000, height: 9000, close: vi.fn() });
    const resolver = new AvatarResolver();
    const adapter = makeAdapter();
    resolver.connectAdapter(adapter);

    await (resolver as unknown as { fetchCid(cid: string): Promise<void> }).fetchCid('bb');

    expect(resolver.resolve('bb')).toBeUndefined();
  });

  it('does not cache a URL when destroy() runs during the decode await', async () => {
    const resolver = new AvatarResolver();
    const adapter = makeAdapter();
    resolver.connectAdapter(adapter);
    // Tear down WHILE the createImageBitmap await is in flight.
    createImageBitmapMock.mockImplementation(async () => {
      resolver.destroy();
      return { width: 256, height: 256, close: vi.fn() };
    });

    await (resolver as unknown as { fetchCid(cid: string): Promise<void> }).fetchCid('cc');

    // No blob URL created on the torn-down resolver, nothing cached.
    expect(URL.createObjectURL).not.toHaveBeenCalled();
    expect((resolver as unknown as { cache: Map<string, string> }).cache.size).toBe(0);
  });
});
