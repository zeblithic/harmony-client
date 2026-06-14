/** Max accepted input file size before downscale (10 MB). */
export const AVATAR_MAX_INPUT_BYTES = 10 * 1024 * 1024;
/** Output square edge in px. */
export const AVATAR_EDGE = 256;
/**
 * Max accepted DECODED dimension (px) on either axis. The byte-size check
 * bounds the compressed file, but a tiny highly-compressed image can decode to
 * an enormous bitmap (a decompression bomb). Reject absurd decoded dimensions
 * before allocating a canvas / drawing.
 */
export const AVATAR_MAX_DECODED_DIM = 8192;

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
 * Throw if a decoded bitmap's dimensions exceed {@link AVATAR_MAX_DECODED_DIM}.
 * Pure helper (no DOM) so it is unit-testable without `createImageBitmap`.
 */
export function assertDecodedDimsOk(width: number, height: number): void {
  if (width > AVATAR_MAX_DECODED_DIM || height > AVATAR_MAX_DECODED_DIM) {
    throw new Error(
      `image dimensions too large: ${width}x${height} (max ${AVATAR_MAX_DECODED_DIM})`,
    );
  }
}

/**
 * Parse the pixel dimensions from a PNG (IHDR) or JPEG (SOFn) header WITHOUT
 * decoding the image. Returns `null` for formats we don't parse (GIF/WebP), an
 * unrecognized signature, or a header too short / malformed to read — callers
 * fall back to the post-decode dimension guard for those.
 *
 * PNG: 8-byte signature, then the IHDR chunk — width is a big-endian u32 at byte
 * offset 16, height at 20. JPEG: SOI (0xFFD8) then marker segments; a
 * Start-Of-Frame marker (0xC0–0xCF, excluding DHT/JPG/DAC) carries
 * `[precision(1)][height(2)][width(2)]` right after its 2-byte length.
 */
export function parseImageHeaderDims(
  bytes: Uint8Array,
): { width: number; height: number } | null {
  // ── PNG ──
  if (
    bytes.length >= 24 &&
    bytes[0] === 0x89 &&
    bytes[1] === 0x50 &&
    bytes[2] === 0x4e &&
    bytes[3] === 0x47 &&
    bytes[4] === 0x0d &&
    bytes[5] === 0x0a &&
    bytes[6] === 0x1a &&
    bytes[7] === 0x0a
  ) {
    // The first chunk of a valid PNG is always IHDR with a 13-byte body. Verify
    // the chunk length (bytes 8–11 == 13) and type (bytes 12–15 == "IHDR")
    // before trusting the width/height at fixed offsets, so a PNG-signature blob
    // with a malformed first chunk falls through (null) instead of yielding
    // garbage dimensions and spuriously rejecting the image.
    const ihdrLenOk =
      bytes[8] === 0x00 && bytes[9] === 0x00 && bytes[10] === 0x00 && bytes[11] === 0x0d;
    const ihdrTypeOk =
      bytes[12] === 0x49 && bytes[13] === 0x48 && bytes[14] === 0x44 && bytes[15] === 0x52;
    if (!ihdrLenOk || !ihdrTypeOk) return null;
    const width = ((bytes[16] << 24) | (bytes[17] << 16) | (bytes[18] << 8) | bytes[19]) >>> 0;
    const height = ((bytes[20] << 24) | (bytes[21] << 16) | (bytes[22] << 8) | bytes[23]) >>> 0;
    return { width, height };
  }

  // ── JPEG ──
  if (bytes.length >= 4 && bytes[0] === 0xff && bytes[1] === 0xd8) {
    let offset = 2;
    while (offset + 1 < bytes.length) {
      if (bytes[offset] !== 0xff) return null; // not on a marker boundary
      let marker = bytes[offset + 1];
      // A run of 0xFF bytes is allowed as fill before a marker code.
      while (marker === 0xff) {
        offset += 1;
        if (offset + 1 >= bytes.length) return null;
        marker = bytes[offset + 1];
      }
      offset += 2; // consume 0xFF + marker code
      // Standalone markers carry no length: TEM (0x01), RSTn/SOI/EOI (0xD0–0xD9).
      if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd9)) continue;
      if (offset + 1 >= bytes.length) return null;
      const segLen = (bytes[offset] << 8) | bytes[offset + 1]; // includes the 2 length bytes
      // A well-formed segment is ≥ 2 bytes (the length field itself) and must
      // fit within the buffer; a lying / truncated segLen → unparseable → null.
      if (segLen < 2 || offset + segLen > bytes.length) return null;
      const isSof =
        marker >= 0xc0 &&
        marker <= 0xcf &&
        marker !== 0xc4 && // DHT
        marker !== 0xc8 && // JPG
        marker !== 0xcc; // DAC
      if (isSof) {
        // The declared segment must actually cover the fields we read —
        // [length(2)][precision(1)][height(2)][width(2)] = 7 bytes — before we
        // trust the dimensions (the offset+segLen≤length check above then
        // guarantees those indices are in-bounds).
        if (segLen < 7) return null;
        const height = (bytes[offset + 3] << 8) | bytes[offset + 4];
        const width = (bytes[offset + 5] << 8) | bytes[offset + 6];
        return { width, height };
      }
      offset += segLen;
    }
    return null;
  }

  return null;
}

/**
 * Reject a decode bomb by its declared header dimensions BEFORE decoding.
 * Parses PNG/JPEG header dims and runs {@link assertDecodedDimsOk} on them; a
 * no-op for formats / headers we can't parse (the post-decode guard still
 * applies there). Pure (no DOM) so it is unit-testable without `createImageBitmap`.
 */
export function assertHeaderDimsOk(bytes: Uint8Array): void {
  const dims = parseImageHeaderDims(bytes);
  if (dims) assertDecodedDimsOk(dims.width, dims.height);
}

/**
 * Normalize an image File to a 256x256 PNG byte array, center-cropped (cover).
 * Frontend-side so there is no Rust image dependency and served bytes are
 * hard-bounded. Returns the PNG bytes ready for `ingest_avatar_bytes`.
 */
export async function normalizeAvatar(file: File): Promise<Uint8Array> {
  validateAvatarInput(file);
  // ZEB-408: reject a decode bomb by its header dimensions BEFORE
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
