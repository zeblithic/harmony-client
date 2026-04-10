/**
 * Comfort noise generator for voice playback gaps.
 *
 * Produces low-level white noise to mask missing frames instead of
 * hard silence. Uses mulberry32 PRNG for deterministic, testable output.
 */

/**
 * Mulberry32 PRNG — fast, deterministic, 32-bit state.
 * Returns a float in [0, 1).
 */
function mulberry32(state: { seed: number }): number {
  state.seed = (state.seed + 0x6d2b79f5) | 0;
  let t = state.seed;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}

/**
 * Stateful comfort noise generator with deterministic PRNG.
 * Use for testing or when you need reproducible output.
 */
export class ComfortNoiseGenerator {
  private state: { seed: number };

  constructor(seed: number) {
    this.state = { seed };
  }

  /**
   * Generate `samples` of white noise at the given amplitude level.
   * Output range: [-level, +level].
   */
  generate(samples: number, level: number): Float32Array {
    const out = new Float32Array(samples);
    for (let i = 0; i < samples; i++) {
      // Map [0, 1) to [-1, 1) then scale by level
      out[i] = (mulberry32(this.state) * 2 - 1) * level;
    }
    return out;
  }
}

/** Module-level generator for production use (non-deterministic across runs). */
const defaultGenerator = new ComfortNoiseGenerator(Date.now() | 0);

/**
 * Generate comfort noise using the module-level generator.
 * Convenient for production — use ComfortNoiseGenerator directly for tests.
 *
 * @param samples  Number of PCM samples to generate.
 * @param level    Amplitude level (default 0.005 ≈ -46dB).
 */
export function generateComfortNoise(
  samples: number,
  level = 0.005,
): Float32Array {
  return defaultGenerator.generate(samples, level);
}
