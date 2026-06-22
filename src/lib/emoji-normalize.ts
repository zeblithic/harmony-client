import {
  validateAvatarInput,
  assertHeaderDimsOk,
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
  // Reject a decode bomb by its declared header dimensions BEFORE
  // createImageBitmap allocates the decoded bitmap. Bounded read — the file is
  // already ≤ AVATAR_MAX_INPUT_BYTES (validateAvatarInput above). An unparseable
  // header falls through to the post-decode assertDecodedDimsOk guard below.
  assertHeaderDimsOk(new Uint8Array(await file.arrayBuffer()));
  const bitmap = await createImageBitmap(file);
  try {
    // Decompression-bomb guard: reject an absurdly large decoded bitmap before
    // allocating a canvas / drawing (the byte-size check only bounds the file).
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
