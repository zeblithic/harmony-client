import {
  validateAvatarInput,
  parseImageHeaderDims,
  assertDecodedDimsOk,
} from './avatar-normalize';

/**
 * Output square edge in px for a custom reaction emoji. Smaller than an avatar
 * (256) — emoji render inline at chip size, so 128 keeps the served bytes tiny.
 */
export const EMOJI_EDGE = 128;

/**
 * Normalize an image File to a PNG byte array sized to fit within
 * {@link EMOJI_EDGE}×{@link EMOJI_EDGE}, **contain-fit**: the whole image is
 * preserved (aspect ratio kept, NO crop — unlike {@link normalizeAvatar}'s
 * cover-crop) and centered on a transparent square canvas. Downscale only —
 * a small input is never upscaled.
 *
 * Reuses the avatar decode-bomb guards: the input gate (image type +
 * {@link AVATAR_MAX_INPUT_BYTES} byte cap), the pre-decode header-dimension
 * guard, and the post-decode bitmap-dimension guard. Frontend-side so there is
 * no Rust image dependency and the served bytes are hard-bounded. Returns the
 * PNG bytes ready for `ingest_channel_artifact_bytes`.
 */
export async function normalizeEmoji(file: File): Promise<Uint8Array> {
  validateAvatarInput(file);
  // Bound the decode BEFORE createImageBitmap allocates the decoded bitmap.
  // `parseImageHeaderDims` reads only the PNG/JPEG header; for an untrusted emoji
  // upload we REJECT any format whose header we can't parse rather than let it
  // decode unbounded — `assertHeaderDimsOk` silently passes unparseable formats,
  // so a non-PNG/JPEG decompression bomb (e.g. a huge WebP/GIF) would otherwise
  // reach createImageBitmap before the post-decode guard. Custom emoji are
  // re-encoded to PNG anyway, so a PNG/JPEG source is sufficient. (The file is
  // already ≤ AVATAR_MAX_INPUT_BYTES via validateAvatarInput above.)
  const headerDims = parseImageHeaderDims(new Uint8Array(await file.arrayBuffer()));
  if (!headerDims) {
    throw new Error('unsupported emoji image format; use PNG or JPEG');
  }
  assertDecodedDimsOk(headerDims.width, headerDims.height);
  const bitmap = await createImageBitmap(file);
  try {
    // Defense in depth: the actual decoded bitmap must also be within bounds,
    // in case a header under-declares its true decoded dimensions.
    assertDecodedDimsOk(bitmap.width, bitmap.height);
    const canvas = document.createElement('canvas');
    canvas.width = EMOJI_EDGE;
    canvas.height = EMOJI_EDGE;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('2d canvas context unavailable');
    // Contain: scale to fit entirely within the square, preserving aspect, and
    // center on the transparent canvas (no crop). Clamp at 1 so we never upscale
    // a small input (which would only add blur + bytes).
    const scale = Math.min(1, EMOJI_EDGE / bitmap.width, EMOJI_EDGE / bitmap.height);
    const dw = bitmap.width * scale;
    const dh = bitmap.height * scale;
    ctx.drawImage(bitmap, (EMOJI_EDGE - dw) / 2, (EMOJI_EDGE - dh) / 2, dw, dh);
    const blob: Blob = await new Promise((resolve, reject) =>
      canvas.toBlob(
        (b) => (b ? resolve(b) : reject(new Error('toBlob produced null'))),
        'image/png',
      ),
    );
    return new Uint8Array(await blob.arrayBuffer());
  } finally {
    bitmap.close();
  }
}
