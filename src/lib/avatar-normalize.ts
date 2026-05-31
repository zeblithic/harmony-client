/** Max accepted input file size before downscale (10 MB). */
export const AVATAR_MAX_INPUT_BYTES = 10 * 1024 * 1024;
/** Output square edge in px. */
export const AVATAR_EDGE = 256;

/** Throw if `file` is not an acceptable avatar input. */
export function validateAvatarInput(file: File): void {
  if (!file.type.startsWith('image/')) {
    throw new Error(`not an image: ${file.type || 'unknown type'}`);
  }
  if (file.size > AVATAR_MAX_INPUT_BYTES) {
    throw new Error(`image too large: ${file.size} > ${AVATAR_MAX_INPUT_BYTES}`);
  }
}

/**
 * Normalize an image File to a 256x256 PNG byte array, center-cropped (cover).
 * Frontend-side so there is no Rust image dependency and served bytes are
 * hard-bounded. Returns the PNG bytes ready for `ingest_avatar_bytes`.
 */
export async function normalizeAvatar(file: File): Promise<Uint8Array> {
  validateAvatarInput(file);
  const bitmap = await createImageBitmap(file);
  try {
    const canvas = document.createElement('canvas');
    canvas.width = AVATAR_EDGE;
    canvas.height = AVATAR_EDGE;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('2d canvas context unavailable');
    // Cover: scale to fill, center-crop the overflow.
    const scale = Math.max(AVATAR_EDGE / bitmap.width, AVATAR_EDGE / bitmap.height);
    const dw = bitmap.width * scale;
    const dh = bitmap.height * scale;
    ctx.drawImage(bitmap, (AVATAR_EDGE - dw) / 2, (AVATAR_EDGE - dh) / 2, dw, dh);
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
