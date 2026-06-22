import { describe, it, expect } from 'vitest';
import {
  PREVIEW_MAX_BYTES,
  isPreviewable,
  isImage,
  isText,
  decodeTextHead,
} from '../artifact-preview';
import type { ChannelAttachmentDto } from '../channel-message-service';

function att(p: Partial<ChannelAttachmentDto>): ChannelAttachmentDto {
  return { cid: 'aa', mime: 'text/plain', name: 'f', size: 10, encrypted: false, ...p };
}

describe('artifact-preview', () => {
  it('PREVIEW_MAX_BYTES is 4 MiB', () => {
    expect(PREVIEW_MAX_BYTES).toBe(4 * 1024 * 1024);
  });

  it('isPreviewable: image/text under cap true; over cap / other mime / empty false', () => {
    expect(isPreviewable(att({ mime: 'image/png', size: 1000 }))).toBe(true);
    expect(isPreviewable(att({ mime: 'text/plain', size: 1000 }))).toBe(true);
    expect(isPreviewable(att({ mime: 'image/png', size: PREVIEW_MAX_BYTES + 1 }))).toBe(false);
    expect(isPreviewable(att({ mime: 'application/zip', size: 10 }))).toBe(false);
    expect(isPreviewable(att({ mime: 'image/png', size: 0 }))).toBe(false);
    expect(isPreviewable(att({ mime: 'image/png', size: PREVIEW_MAX_BYTES }))).toBe(true); // boundary
  });

  it('isImage / isText classify by mime prefix', () => {
    expect(isImage(att({ mime: 'image/jpeg' }))).toBe(true);
    expect(isImage(att({ mime: 'text/plain' }))).toBe(false);
    expect(isText(att({ mime: 'text/markdown' }))).toBe(true);
    expect(isText(att({ mime: 'image/png' }))).toBe(false);
  });

  it('excludes SVG from image preview (download-only)', () => {
    expect(isImage(att({ mime: 'image/svg+xml' }))).toBe(false);
    expect(isImage(att({ mime: 'image/svg+xml; charset=utf-8' }))).toBe(false);
    expect(isPreviewable(att({ mime: 'image/svg+xml', size: 1000 }))).toBe(false);
    // raster images are still previewable
    expect(isImage(att({ mime: 'image/png' }))).toBe(true);
    expect(isImage(att({ mime: 'image/webp' }))).toBe(true);
  });

  it('decodeTextHead returns head + full + truncated flag', () => {
    const lines = Array.from({ length: 100 }, (_, i) => `line ${i}`).join('\n');
    const bytes = new TextEncoder().encode(lines);
    const r = decodeTextHead(bytes, 40, 100000);
    expect(r.truncated).toBe(true);
    expect(r.head.split('\n').length).toBe(40);
    expect(r.full).toBe(lines);

    const small = new TextEncoder().encode('a\nb\nc');
    const r2 = decodeTextHead(small, 40, 100000);
    expect(r2.truncated).toBe(false);
    expect(r2.head).toBe('a\nb\nc');
  });

  it('decodeTextHead truncates on maxChars even within line budget', () => {
    const bytes = new TextEncoder().encode('x'.repeat(5000));
    const r = decodeTextHead(bytes, 40, 4000);
    expect(r.truncated).toBe(true);
    expect(r.head.length).toBe(4000);
  });
});
