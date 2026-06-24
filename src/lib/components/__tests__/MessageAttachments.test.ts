import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import MessageAttachments from '../MessageAttachments.svelte';
import type { ChannelAttachmentDto } from '../../channel-message-service';

// vi.mock is hoisted to the top of the file; vi.hoisted makes the spy
// available at factory-call time (repo pattern — see WelcomeModal.test.ts).
const { saveMock } = vi.hoisted(() => ({ saveMock: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => ({
  save: saveMock,
  open: vi.fn(),
}));

function att(over: Partial<ChannelAttachmentDto> = {}): ChannelAttachmentDto {
  return { cid: 'cid1', mime: 'text/plain', name: 'log.txt', size: 2048, encrypted: true, ...over };
}
function makeService(
  downloadArtifact: any = vi.fn().mockResolvedValue(2048),
  previewArtifact: any = vi.fn().mockResolvedValue(new Uint8Array()),
) {
  return { downloadArtifact, previewArtifact } as any;
}
function props(over: Record<string, unknown> = {}) {
  return { communityId: 'c', channelId: 'ch', attachments: [att()], channelMessageService: makeService(), ...over };
}

// PNG magic bytes so the header-dim guard recognises the format.
// A real minimal PNG header (8-byte signature + a 16x16 IHDR) so the preview
// path's content-validation (`parseImageHeaderDims`, which requires a genuine
// PNG/JPEG header) accepts it. The decoder itself (`createImageBitmap`) is
// mocked, so no actual pixel data is needed.
const PNG_BYTES = new Uint8Array([
  0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG signature
  0x00, 0x00, 0x00, 0x0d, // IHDR chunk length = 13
  0x49, 0x48, 0x44, 0x52, // "IHDR"
  0x00, 0x00, 0x00, 0x10, // width = 16
  0x00, 0x00, 0x00, 0x10, // height = 16
]);

describe('MessageAttachments', () => {
  beforeEach(() => { saveMock.mockReset(); });

  it('renders a chip per attachment with name, size, icon, lock', () => {
    const { container } = render(MessageAttachments, { props: props() });
    expect(container.textContent).toContain('log.txt');
    expect(container.textContent).toContain('2.0 KB');
    expect(container.querySelector('.att-lock')).not.toBeNull();
    expect(container.querySelectorAll('.attachment-chip').length).toBe(1);
  });

  it('omits the lock badge when not encrypted', () => {
    const { container } = render(MessageAttachments, { props: props({ attachments: [att({ encrypted: false })] }) });
    expect(container.querySelector('.att-lock')).toBeNull();
  });

  it('renders one chip and does not throw for duplicate CIDs', () => {
    const dup = att({ cid: 'dup' });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [dup, { ...dup }], channelMessageService: makeService() }),
    });
    expect(container.querySelectorAll('.attachment-chip').length).toBe(1);
  });

  it('download: save → downloadArtifact called with chosen path', async () => {
    saveMock.mockResolvedValue('/tmp/out.txt');
    const service = makeService();
    const a = att();
    const { container } = render(MessageAttachments, { props: props({ attachments: [a], channelMessageService: service }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => {
      expect(service.downloadArtifact).toHaveBeenCalledWith('c', 'ch', a, '/tmp/out.txt');
    });
  });

  it('cancel (save → null) does not call downloadArtifact', async () => {
    saveMock.mockResolvedValue(null);
    const service = makeService();
    const { container } = render(MessageAttachments, { props: props({ channelMessageService: service }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await Promise.resolve();
    expect(service.downloadArtifact).not.toHaveBeenCalled();
  });

  it('sanitizes a path-traversal name to a basename for the save dialog', async () => {
    saveMock.mockResolvedValue(null); // cancel — we only assert the dialog args
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [att({ name: '../../etc/passwd' })], channelMessageService: makeService() }),
    });
    await fireEvent.click(container.querySelector('.att-download')!);
    expect(saveMock).toHaveBeenCalledWith(expect.objectContaining({ defaultPath: 'passwd' }));
  });

  it('download error → error message + retry re-invokes', async () => {
    saveMock.mockResolvedValue('/tmp/out.txt');
    const downloadArtifact = vi.fn()
      .mockRejectedValueOnce(new Error('peer offline'))
      .mockResolvedValueOnce(2048);
    const { container } = render(MessageAttachments, { props: props({ channelMessageService: makeService(downloadArtifact) }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => expect(container.querySelector('.att-error')?.textContent).toContain('peer offline'));
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => expect(downloadArtifact).toHaveBeenCalledTimes(2));
  });
});

describe('MessageAttachments — inline preview (ZEB-540)', () => {
  let createImageBitmapMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    saveMock.mockReset();
    createImageBitmapMock = vi.fn().mockResolvedValue({ width: 800, height: 600, close: vi.fn() });
    vi.stubGlobal('createImageBitmap', createImageBitmapMock);
    // jsdom lacks URL.createObjectURL / revokeObjectURL.
    vi.stubGlobal('URL', {
      ...URL,
      createObjectURL: vi.fn(() => 'blob:mock'),
      revokeObjectURL: vi.fn(),
    });
  });
  afterEach(() => { vi.unstubAllGlobals(); });

  function previewService(previewArtifact: any) {
    return makeService(vi.fn().mockResolvedValue(0), previewArtifact);
  }

  it('shows a Preview button only for previewable (image/text ≤ cap) attachments', () => {
    const img = att({ cid: 'img', mime: 'image/png', name: 'a.png', size: 1000 });
    const zip = att({ cid: 'zip', mime: 'application/zip', name: 'a.zip', size: 1000 });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [img, zip], channelMessageService: makeService() }),
    });
    const buttons = container.querySelectorAll('.att-preview-btn');
    expect(buttons.length).toBe(1);
  });

  it('hides Preview for an over-cap previewable attachment (download chip only)', () => {
    const huge = att({ cid: 'big', mime: 'image/png', name: 'a.png', size: 5 * 1024 * 1024 });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [huge], channelMessageService: makeService() }),
    });
    expect(container.querySelector('.att-preview-btn')).toBeNull();
    expect(container.querySelector('.att-download')).not.toBeNull();
  });

  it('previews an image: click fetches, creates a blob URL, renders <img>', async () => {
    const previewArtifact = vi.fn().mockResolvedValue(PNG_BYTES);
    const img = att({ cid: 'img', mime: 'image/png', name: 'a.png', size: 1000 });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [img], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => {
      expect(previewArtifact).toHaveBeenCalledWith('c', 'ch', img);
      expect(URL.createObjectURL).toHaveBeenCalled();
      const el = container.querySelector('img.att-preview-img') as HTMLImageElement | null;
      expect(el).not.toBeNull();
      expect(el!.getAttribute('src')).toBe('blob:mock');
      expect(el!.getAttribute('alt')).toBe('a.png');
    });
  });

  it('refuses to preview an extension-spoofed non-PNG/JPEG image (decode-bomb safety)', async () => {
    // A WebP (RIFF) payload mislabeled image/jpeg: detect_mime classifies by the
    // sender-controlled extension, but createImageBitmap decodes by CONTENT, so
    // the preview path must content-validate. parseImageHeaderDims rejects a
    // non-PNG/JPEG header → preview is refused and the malicious bytes never
    // reach the decoder. (CodeAnt, PR #329.)
    const webp = new Uint8Array([0x52, 0x49, 0x46, 0x46, 0, 0, 0, 0, 0x57, 0x45, 0x42, 0x50]);
    const previewArtifact = vi.fn().mockResolvedValue(webp);
    const spoofed = att({ cid: 'sp', mime: 'image/jpeg', name: 'evil.jpg', size: webp.length });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [spoofed], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() =>
      expect(container.querySelector('.att-error')?.textContent).toContain('Not a previewable image'),
    );
    // The unguarded decoder was never invoked on the spoofed bytes.
    expect(createImageBitmapMock).not.toHaveBeenCalled();
    expect(URL.createObjectURL).not.toHaveBeenCalled();
  });

  it('previews text: click renders the decoded head in a <pre>', async () => {
    const previewArtifact = vi.fn().mockResolvedValue(new TextEncoder().encode('hello\nworld'));
    const txt = att({ cid: 'txt', mime: 'text/plain', name: 'l.txt', size: 11 });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [txt], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => {
      const pre = container.querySelector('pre.att-preview-text');
      expect(pre).not.toBeNull();
      expect(pre!.textContent).toContain('hello');
    });
  });

  it('collapse revokes the image blob URL and hides the <img>', async () => {
    const previewArtifact = vi.fn().mockResolvedValue(PNG_BYTES);
    const img = att({ cid: 'img', mime: 'image/png', name: 'a.png', size: 1000 });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [img], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => expect(container.querySelector('img.att-preview-img')).not.toBeNull());
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => {
      expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:mock');
      expect(container.querySelector('img.att-preview-img')).toBeNull();
    });
  });

  it('surfaces a preview error with a retry', async () => {
    const previewArtifact = vi.fn()
      .mockRejectedValueOnce(new Error('peer offline'))
      .mockResolvedValueOnce(new TextEncoder().encode('ok'));
    const txt = att({ cid: 'txt', mime: 'text/plain', name: 'l.txt', size: 2 });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [txt], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => expect(container.querySelector('.att-error')?.textContent).toContain('peer offline'));
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => expect(previewArtifact).toHaveBeenCalledTimes(2));
  });

  it('rejects an over-dimension image (decode bomb) without rendering <img>', async () => {
    createImageBitmapMock.mockResolvedValue({ width: 9000, height: 9000, close: vi.fn() });
    const previewArtifact = vi.fn().mockResolvedValue(PNG_BYTES);
    const img = att({ cid: 'img', mime: 'image/png', name: 'a.png', size: 1000 });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [img], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => expect(container.querySelector('.att-error')).not.toBeNull());
    expect(container.querySelector('img.att-preview-img')).toBeNull();
  });

  it('revokes blob URLs on unmount', async () => {
    const previewArtifact = vi.fn().mockResolvedValue(PNG_BYTES);
    const img = att({ cid: 'img', mime: 'image/png', name: 'a.png', size: 1000 });
    const { container, unmount } = render(MessageAttachments, {
      props: props({ attachments: [img], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => expect(container.querySelector('img.att-preview-img')).not.toBeNull());
    unmount();
    expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:mock');
  });

  it('revokes an orphaned blob URL when the attachments prop changes (positional reuse)', async () => {
    // The feed renders MessageAttachments under an unkeyed {#each}, so a channel
    // switch reuses this instance with a DIFFERENT attachments prop instead of
    // unmounting it. An open image preview's blob URL must not leak.
    const previewArtifact = vi.fn().mockResolvedValue(PNG_BYTES);
    const img = att({ cid: 'img', mime: 'image/png', name: 'a.png', size: 1000 });
    const { container, rerender } = render(MessageAttachments, {
      props: props({ attachments: [img], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    await waitFor(() => expect(container.querySelector('img.att-preview-img')).not.toBeNull());

    const other = att({ cid: 'other', mime: 'text/plain', name: 'b.txt', size: 5 });
    await rerender(props({ attachments: [other], channelMessageService: previewService(previewArtifact) }));

    await waitFor(() => expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:mock'));
    // The orphaned image preview is gone (instance reused for the new attachment).
    expect(container.querySelector('img.att-preview-img')).toBeNull();
  });

  it('does not create a blob URL when torn down during the preview fetch (teardown race)', async () => {
    // Qodo/CodeAnt teardown race: the IPC resolves AFTER unmount. The post-await
    // liveness guard must prevent creating a blob URL on a dead component.
    let resolveFetch!: (value: Uint8Array | PromiseLike<Uint8Array>) => void;
    const previewArtifact = vi.fn(() => new Promise<Uint8Array>((r) => { resolveFetch = r; }));
    const img = att({ cid: 'img', mime: 'image/png', name: 'a.png', size: 1000 });
    const { container, unmount } = render(MessageAttachments, {
      props: props({ attachments: [img], channelMessageService: previewService(previewArtifact) }),
    });
    await fireEvent.click(container.querySelector('.att-preview-btn')!);
    expect(previewArtifact).toHaveBeenCalled();
    unmount(); // teardown while the fetch is in flight
    resolveFetch(PNG_BYTES);
    await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
    expect(URL.createObjectURL).not.toHaveBeenCalled();
  });
});
