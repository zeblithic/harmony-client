import { describe, it, expect } from 'vitest';
import { generateIdenticon } from './identicon';

describe('generateIdenticon', () => {
  it('returns an SVG string', () => {
    const svg = generateIdenticon('a1b2c3d4');
    expect(svg).toContain('<svg');
    expect(svg).toContain('</svg>');
  });

  it('is deterministic — same address produces same output', () => {
    const svg1 = generateIdenticon('a1b2c3d4');
    const svg2 = generateIdenticon('a1b2c3d4');
    expect(svg1).toBe(svg2);
  });

  it('produces different output for different addresses', () => {
    const svg1 = generateIdenticon('a1b2c3d4');
    const svg2 = generateIdenticon('e5f6g7h8');
    expect(svg1).not.toBe(svg2);
  });

  it('generates a symmetric 5x5 grid pattern', () => {
    const svg = generateIdenticon('a1b2c3d4');
    const rects = svg.match(/<rect /g);
    expect(rects).toBeTruthy();
    expect(rects!.length).toBeGreaterThan(0);
    expect(rects!.length).toBeLessThanOrEqual(25);
  });

  it('produces horizontally symmetric patterns', () => {
    const svg = generateIdenticon('test-address');
    const rectMatches = [...svg.matchAll(/x="(\d+)" y="(\d+)"/g)];
    const cells = rectMatches.map((m) => ({ x: Number(m[1]), y: Number(m[2]) }));

    if (cells.length > 0) {
      const maxX = Math.max(...cells.map((c) => c.x));
      for (const cell of cells) {
        const mirrorX = maxX - cell.x;
        const hasMirror = cells.some((c) => c.x === mirrorX && c.y === cell.y);
        expect(hasMirror).toBe(true);
      }
    }
  });

  it('respects custom size parameter', () => {
    const svg = generateIdenticon('a1b2c3d4', 100);
    expect(svg).toContain('width="100"');
    expect(svg).toContain('height="100"');
  });

  it('uses default size of 64', () => {
    const svg = generateIdenticon('a1b2c3d4');
    expect(svg).toContain('width="64"');
    expect(svg).toContain('height="64"');
  });
});
