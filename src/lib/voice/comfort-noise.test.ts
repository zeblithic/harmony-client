import { describe, it, expect } from 'vitest';
import { generateComfortNoise, ComfortNoiseGenerator } from './comfort-noise';

describe('generateComfortNoise', () => {
  it('returns Float32Array of requested length', () => {
    const noise = generateComfortNoise(320, 0.005);
    expect(noise).toBeInstanceOf(Float32Array);
    expect(noise.length).toBe(320);
  });

  it('produces non-zero samples', () => {
    const noise = generateComfortNoise(320, 0.005);
    const hasNonZero = noise.some((s) => s !== 0);
    expect(hasNonZero).toBe(true);
  });

  it('all samples are within [-level, +level]', () => {
    const level = 0.01;
    const noise = generateComfortNoise(1000, level);
    for (let i = 0; i < noise.length; i++) {
      expect(Math.abs(noise[i])).toBeLessThanOrEqual(level);
    }
  });

  it('different calls produce different output (stateful PRNG)', () => {
    const a = generateComfortNoise(160, 0.005);
    const b = generateComfortNoise(160, 0.005);
    // Not identical — PRNG state advances between calls
    const same = a.every((v, i) => v === b[i]);
    expect(same).toBe(false);
  });
});

describe('ComfortNoiseGenerator', () => {
  it('produces deterministic output for same seed', () => {
    const gen1 = new ComfortNoiseGenerator(42);
    const gen2 = new ComfortNoiseGenerator(42);
    const a = gen1.generate(160, 0.005);
    const b = gen2.generate(160, 0.005);
    expect(a).toEqual(b);
  });

  it('different seeds produce different output', () => {
    const gen1 = new ComfortNoiseGenerator(42);
    const gen2 = new ComfortNoiseGenerator(99);
    const a = gen1.generate(160, 0.005);
    const b = gen2.generate(160, 0.005);
    const same = a.every((v, i) => v === b[i]);
    expect(same).toBe(false);
  });

  it('generates correct number of samples', () => {
    const gen = new ComfortNoiseGenerator(1);
    expect(gen.generate(160, 0.005).length).toBe(160);
    expect(gen.generate(320, 0.005).length).toBe(320);
  });
});
