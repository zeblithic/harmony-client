// src/lib/stq8-service.ts
import type { FlashcardLevel, Challenge, LevelInfo } from './flashcard-types';

/**
 * Subset of WasmPipeline methods used by flashcard UI.
 *
 * Mixes wasm-bindgen static methods (challenge/validate/format/level_info)
 * and instance methods (process + calibration/profile). App.svelte's
 * adapter flattens both onto one object; tests pass a plain mock.
 */
export interface WasmPipelineApi {
  generate_challenge(level: number, rng_bytes: Uint8Array): string;
  validate_row(expected_bytes: Uint8Array, heard_nibbles: Uint8Array): string;
  format_box_q8(data: Uint8Array, bytes_per_row: number): string;
  format_flat_q8(data: Uint8Array, bytes_per_row: number): string;
  level_info(level: number): string;
  process(pcm: Float32Array): string;
  add_calibration_sample(syllable_index: number, pcm: Float32Array): void;
  finalize_calibration(): void;
  is_calibrated(): boolean;
  export_profile(): string;
  import_profile(json: string): void;
  set_created_epoch_secs(secs: bigint): void;
}

/** Row validation result from WASM. */
export interface WasmRowResult {
  matched: boolean;
  expected: Array<{ consonant: string; vowel: string }>;
  heard: Array<{ consonant: string; vowel: string }>;
}

/** Utterance result from WASM pipeline.process(). */
export interface UtteranceResult {
  syllables: Array<{ nibble: number; consonant: string; vowel: string }>;
}

/**
 * Public surface the UI consumes. Declared as an interface so tests and
 * other components can accept a structurally-compatible mock without
 * depending on the concrete class's private fields (which would
 * otherwise nominally brand the type).
 */
export interface Stq8ServiceLike {
  isReady(): boolean;
  isCalibrated(): boolean;
  getLevelInfo(level: FlashcardLevel): LevelInfo;
  generateChallenge(level: FlashcardLevel): Challenge;
  validateRow(expectedBytes: number[], heardNibbles: number[]): WasmRowResult;
  processPcm(pcm: Float32Array): UtteranceResult;
  addCalibrationSample(syllableIndex: number, pcm: Float32Array): void;
  finalizeCalibration(): void;
  exportProfile(): string;
  importProfile(json: string): void;
  setCreatedEpochSecs(secs: bigint): void;
}

export class Stq8Service implements Stq8ServiceLike {
  private wasm: WasmPipelineApi | null;

  constructor(wasm: WasmPipelineApi | null) {
    this.wasm = wasm;
  }

  isReady(): boolean {
    return this.wasm !== null;
  }

  getLevelInfo(level: FlashcardLevel): LevelInfo {
    if (!this.wasm) throw new Error('WASM not loaded');
    return JSON.parse(this.wasm.level_info(level));
  }

  generateChallenge(level: FlashcardLevel): Challenge {
    if (!this.wasm) throw new Error('WASM not loaded');
    const info = this.getLevelInfo(level);
    const rngBytes = new Uint8Array(info.total_bytes);
    crypto.getRandomValues(rngBytes);
    return JSON.parse(this.wasm.generate_challenge(level, rngBytes));
  }

  validateRow(expectedBytes: number[], heardNibbles: number[]): WasmRowResult {
    if (!this.wasm) throw new Error('WASM not loaded');
    return JSON.parse(
      this.wasm.validate_row(
        new Uint8Array(expectedBytes),
        new Uint8Array(heardNibbles),
      ),
    );
  }

  processPcm(pcm: Float32Array): UtteranceResult {
    if (!this.wasm) throw new Error('WASM not loaded');
    return JSON.parse(this.wasm.process(pcm));
  }

  /**
   * Feed a calibration sample for one syllable.
   *
   * syllableIndex is 0–15 mapped as: 0='O, 1='U, 2='E, 3='I, 4=JO, 5=JU,
   * 6=JE, 7=JI, 8=KO, 9=KU, 10=KE, 11=KI, 12=VO, 13=VU, 14=VE, 15=VI.
   * `pcm` should be raw mono 16 kHz f32 samples — the underlying MFCC
   * extractor doesn't care about exact duration, but the capture UI
   * targets ~1–2 s per sample for a usable centroid.
   */
  addCalibrationSample(syllableIndex: number, pcm: Float32Array): void {
    if (!this.wasm) throw new Error('WASM not loaded');
    this.wasm.add_calibration_sample(syllableIndex, pcm);
  }

  /**
   * Lock in the base classifier after all 16 syllables have been sampled.
   * Idempotent — safe to call again after a full re-sample.
   */
  finalizeCalibration(): void {
    if (!this.wasm) throw new Error('WASM not loaded');
    this.wasm.finalize_calibration();
  }

  /** True once finalize_calibration has produced a usable classifier. */
  isCalibrated(): boolean {
    if (!this.wasm) return false;
    return this.wasm.is_calibrated();
  }

  /** Serialize the full voice profile (centroids + metadata) to a JSON string. */
  exportProfile(): string {
    if (!this.wasm) throw new Error('WASM not loaded');
    return this.wasm.export_profile();
  }

  /** Restore a previously-exported profile on boot. */
  importProfile(json: string): void {
    if (!this.wasm) throw new Error('WASM not loaded');
    this.wasm.import_profile(json);
  }

  /** Stamp the profile with a creation time (seconds since epoch) before exporting. */
  setCreatedEpochSecs(secs: bigint): void {
    if (!this.wasm) throw new Error('WASM not loaded');
    this.wasm.set_created_epoch_secs(secs);
  }
}
