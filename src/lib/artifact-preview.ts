import type { ChannelAttachmentDto } from './channel-message-service';

/**
 * Frontend mirror of the backend `MAX_PREVIEW_BYTES` (src-tauri/src/lib.rs).
 * Used only to decide whether to OFFER a preview — the backend enforces the cap
 * authoritatively. Keep the two in sync (4 MiB).
 */
export const PREVIEW_MAX_BYTES = 4 * 1024 * 1024;

export function isImage(att: ChannelAttachmentDto): boolean {
  return att.mime.toLowerCase().startsWith('image/');
}

export function isText(att: ChannelAttachmentDto): boolean {
  return att.mime.toLowerCase().startsWith('text/');
}

/** True iff we can render an inline preview: an image or text artifact whose
 *  signed size is in (0, PREVIEW_MAX_BYTES]. Everything else is download-only. */
export function isPreviewable(att: ChannelAttachmentDto): boolean {
  return att.size > 0 && att.size <= PREVIEW_MAX_BYTES && (isImage(att) || isText(att));
}

export interface TextHead {
  head: string;
  full: string;
  truncated: boolean;
}

/** Decode UTF-8 bytes and return the first `maxLines` lines capped at `maxChars`.
 *  `truncated` is true if either bound clipped the text. `full` is the entire
 *  decoded string (we already hold all the bytes), used by the "show more" toggle. */
export function decodeTextHead(
  bytes: Uint8Array,
  maxLines = 40,
  maxChars = 4000,
): TextHead {
  const full = new TextDecoder().decode(bytes);
  const lines = full.split('\n');
  let head = lines.slice(0, maxLines).join('\n');
  let truncated = lines.length > maxLines;
  if (head.length > maxChars) {
    head = head.slice(0, maxChars);
    truncated = true;
  }
  return { head, full, truncated };
}
