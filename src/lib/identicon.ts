/**
 * Generate a deterministic SVG identicon from a peer address string.
 * Produces a 5x5 symmetric grid pattern with a foreground color derived from the address hash.
 */
export function generateIdenticon(address: string, size = 64): string {
  // Sanitize inputs: strip non-alphanumeric chars from address, clamp size to safe integer
  const safeAddress = address.replace(/[^a-zA-Z0-9_\-]/g, '');
  const safeSize = Math.max(1, Math.min(Math.floor(size), 1024));
  const hash = hashAddress(safeAddress || 'default');
  const hue = (hash[0] * 7 + hash[1] * 13) % 360;
  const fg = `hsl(${hue}, 55%, 50%)`;
  // Build 5x5 grid — columns 0-1 mirror columns 4-3, column 2 is center
  const grid: boolean[][] = [];
  for (let row = 0; row < 5; row++) {
    grid[row] = [];
    for (let col = 0; col < 3; col++) {
      const byteIdx = (row * 3 + col) % hash.length;
      grid[row][col] = hash[byteIdx] % 2 === 0;
      if (col < 2) {
        grid[row][4 - col] = grid[row][col];
      }
    }
  }

  // Use a 5x5 viewBox so each cell is exactly 1 unit — guarantees pixel-perfect symmetry
  const rects: string[] = [];
  for (let row = 0; row < 5; row++) {
    for (let col = 0; col < 5; col++) {
      if (grid[row][col]) {
        rects.push(`<rect x="${col}" y="${row}" width="1" height="1" fill="${fg}"/>`);
      }
    }
  }

  return `<svg xmlns="http://www.w3.org/2000/svg" width="${safeSize}" height="${safeSize}" viewBox="0 0 5 5">${rects.join('')}</svg>`;
}

/** Simple hash function — returns an array of pseudo-random bytes from an address string. */
function hashAddress(address: string): number[] {
  const bytes: number[] = [];
  let h = 0x811c9dc5; // FNV offset basis
  for (let i = 0; i < address.length; i++) {
    h ^= address.charCodeAt(i);
    h = Math.imul(h, 0x01000193); // FNV prime
    h = h >>> 0;
  }
  for (let i = 0; i < 15; i++) {
    h ^= h >>> 13;
    h = Math.imul(h, 0x5bd1e995);
    h = h >>> 0;
    bytes.push(h & 0xff);
  }
  return bytes;
}
