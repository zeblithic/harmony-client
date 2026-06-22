import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, waitFor } from '@testing-library/svelte';
import ReactionEmojiImage from '../ReactionEmojiImage.svelte';

// PNG magic bytes so the header-dim guard recognises the format.
const PNG_BYTES = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

function makeService(previewReactionEmoji: any) {
  return { previewReactionEmoji } as any;
}

function props(over: Record<string, unknown> = {}) {
  return {
    communityId: 'c',
    channelId: 'ch',
    cid: 'cid1',
    channelMessageService: makeService(vi.fn().mockResolvedValue(PNG_BYTES)),
    ...over,
  };
}

describe('ReactionEmojiImage (ZEB-541)', () => {
  let createImageBitmapMock: ReturnType<typeof vi.fn>;
  let createObjectURLMock: ReturnType<typeof vi.fn>;
  let revokeObjectURLMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    createImageBitmapMock = vi.fn().mockResolvedValue({ width: 128, height: 128, close: vi.fn() });
    vi.stubGlobal('createImageBitmap', createImageBitmapMock);
    // jsdom lacks URL.createObjectURL / revokeObjectURL.
    let n = 0;
    createObjectURLMock = vi.fn(() => `blob:mock${++n}`);
    revokeObjectURLMock = vi.fn();
    vi.stubGlobal('URL', {
      ...URL,
      createObjectURL: createObjectURLMock,
      revokeObjectURL: revokeObjectURLMock,
    });
  });
  afterEach(() => { vi.unstubAllGlobals(); });

  it('fetches the cid and renders an <img> with the blob URL', async () => {
    const previewReactionEmoji = vi.fn().mockResolvedValue(PNG_BYTES);
    const { container } = render(ReactionEmojiImage, {
      props: props({ channelMessageService: makeService(previewReactionEmoji) }),
    });
    await waitFor(() => {
      expect(previewReactionEmoji).toHaveBeenCalledWith('c', 'ch', 'cid1');
      const img = container.querySelector('img.reaction-emoji-img') as HTMLImageElement | null;
      expect(img).not.toBeNull();
      expect(img!.getAttribute('src')).toBe('blob:mock1');
    });
  });

  it('shows a neutral placeholder until the blob resolves', async () => {
    let resolveFetch!: (v: Uint8Array) => void;
    const previewReactionEmoji = vi.fn(() => new Promise<Uint8Array>((r) => { resolveFetch = r; }));
    const { container } = render(ReactionEmojiImage, {
      props: props({ channelMessageService: makeService(previewReactionEmoji) }),
    });
    // Before resolution: a fallback glyph, no <img>.
    expect(container.querySelector('img.reaction-emoji-img')).toBeNull();
    expect(container.querySelector('.reaction-emoji-fallback')).not.toBeNull();
    resolveFetch(PNG_BYTES);
    await waitFor(() => expect(container.querySelector('img.reaction-emoji-img')).not.toBeNull());
  });

  it('shows the error placeholder when the fetch fails (no <img>)', async () => {
    const previewReactionEmoji = vi.fn().mockRejectedValue(new Error('unauthorized'));
    const { container } = render(ReactionEmojiImage, {
      props: props({ channelMessageService: makeService(previewReactionEmoji) }),
    });
    await waitFor(() => {
      expect(container.querySelector('.reaction-emoji-fallback')).not.toBeNull();
    });
    expect(container.querySelector('img.reaction-emoji-img')).toBeNull();
    expect(createObjectURLMock).not.toHaveBeenCalled();
  });

  it('rejects an over-dimension decode bomb without rendering <img>', async () => {
    createImageBitmapMock.mockResolvedValue({ width: 9000, height: 9000, close: vi.fn() });
    const previewReactionEmoji = vi.fn().mockResolvedValue(PNG_BYTES);
    const { container } = render(ReactionEmojiImage, {
      props: props({ channelMessageService: makeService(previewReactionEmoji) }),
    });
    await waitFor(() => expect(container.querySelector('.reaction-emoji-fallback')).not.toBeNull());
    expect(container.querySelector('img.reaction-emoji-img')).toBeNull();
    expect(createObjectURLMock).not.toHaveBeenCalled();
  });

  it('revokes the blob URL on unmount', async () => {
    const previewReactionEmoji = vi.fn().mockResolvedValue(PNG_BYTES);
    const { container, unmount } = render(ReactionEmojiImage, {
      props: props({ channelMessageService: makeService(previewReactionEmoji) }),
    });
    await waitFor(() => expect(container.querySelector('img.reaction-emoji-img')).not.toBeNull());
    unmount();
    expect(revokeObjectURLMock).toHaveBeenCalledWith('blob:mock1');
  });

  it('revokes the old URL and refetches when the cid prop changes', async () => {
    const previewReactionEmoji = vi.fn().mockResolvedValue(PNG_BYTES);
    const { container, rerender } = render(ReactionEmojiImage, {
      props: props({ channelMessageService: makeService(previewReactionEmoji) }),
    });
    await waitFor(() => {
      expect(container.querySelector('img.reaction-emoji-img')?.getAttribute('src')).toBe('blob:mock1');
    });
    await rerender(props({ cid: 'cid2', channelMessageService: makeService(previewReactionEmoji) }));
    await waitFor(() => {
      expect(revokeObjectURLMock).toHaveBeenCalledWith('blob:mock1');
      expect(previewReactionEmoji).toHaveBeenLastCalledWith('c', 'ch', 'cid2');
      expect(container.querySelector('img.reaction-emoji-img')?.getAttribute('src')).toBe('blob:mock2');
    });
  });

  it('does not create a blob URL when torn down during the fetch (teardown race)', async () => {
    let resolveFetch!: (v: Uint8Array) => void;
    const previewReactionEmoji = vi.fn(() => new Promise<Uint8Array>((r) => { resolveFetch = r; }));
    const { unmount } = render(ReactionEmojiImage, {
      props: props({ channelMessageService: makeService(previewReactionEmoji) }),
    });
    expect(previewReactionEmoji).toHaveBeenCalled();
    unmount(); // teardown while the fetch is in flight
    resolveFetch(PNG_BYTES);
    await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
    expect(createObjectURLMock).not.toHaveBeenCalled();
  });

  it('does not create a blob URL for a stale cid that resolves after a cid swap', async () => {
    // cid swap mid-fetch: the first (now-stale) fetch must NOT create a URL when
    // it finally resolves — only the current cid's fetch may.
    let resolveFirst!: (v: Uint8Array) => void;
    const previewReactionEmoji = vi
      .fn()
      .mockImplementationOnce(() => new Promise<Uint8Array>((r) => { resolveFirst = r; }))
      .mockResolvedValue(PNG_BYTES);
    const { container, rerender } = render(ReactionEmojiImage, {
      props: props({ channelMessageService: makeService(previewReactionEmoji) }),
    });
    // Swap the cid before the first fetch resolves.
    await rerender(props({ cid: 'cid2', channelMessageService: makeService(previewReactionEmoji) }));
    await waitFor(() => {
      expect(container.querySelector('img.reaction-emoji-img')?.getAttribute('src')).toBe('blob:mock1');
    });
    const callsBefore = createObjectURLMock.mock.calls.length;
    // Now resolve the STALE first fetch — it must be ignored.
    resolveFirst(PNG_BYTES);
    await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
    expect(createObjectURLMock.mock.calls.length).toBe(callsBefore);
  });
});
