// src/lib/stq8-service.ts
import type { FlashcardLevel, Challenge, LevelInfo } from './flashcard-types';

/** Subset of WasmPipeline methods used by flashcard UI. */
export interface WasmPipelineApi {
  generate_challenge(level: number, rng_bytes: Uint8Array): string;
  validate_row(expected_bytes: Uint8Array, heard_nibbles: Uint8Array): string;
  format_box_q8(data: Uint8Array, bytes_per_row: number): string;
  format_flat_q8(data: Uint8Array, bytes_per_row: number): string;
  level_info(level: number): string;
  process(pcm: Float32Array): string;
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

export class Stq8Service {
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
}
