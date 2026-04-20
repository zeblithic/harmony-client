// src/lib/stq8-service.test.ts
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { Stq8Service } from './stq8-service';

function createMockWasm() {
  return {
    generate_challenge: vi.fn().mockReturnValue(JSON.stringify({
      level: 'Novice',
      data: [0x42],
      rows: [[0x42]],
    })),
    validate_row: vi.fn().mockReturnValue(JSON.stringify({
      matched: true,
      expected: [{ consonant: "'", vowel: 'O' }, { consonant: "'", vowel: 'O' }],
      heard: [{ consonant: "'", vowel: 'O' }, { consonant: "'", vowel: 'O' }],
    })),
    format_box_q8: vi.fn().mockReturnValue('A A\nO O'),
    format_flat_q8: vi.fn().mockReturnValue("'O'O"),
    level_info: vi.fn().mockReturnValue(JSON.stringify({
      total_bytes: 1,
      bytes_per_row: 1,
      num_rows: 1,
      total_bits: 8,
    })),
    process: vi.fn().mockReturnValue(JSON.stringify({
      syllables: [],
    })),
    add_calibration_sample: vi.fn(),
    finalize_calibration: vi.fn(),
    is_calibrated: vi.fn().mockReturnValue(false),
    export_profile: vi.fn().mockReturnValue('{"centroids":{},"created":0}'),
    import_profile: vi.fn(),
    set_created_epoch_secs: vi.fn(),
  };
}

describe('Stq8Service', () => {
  let mockWasm: ReturnType<typeof createMockWasm>;
  let service: Stq8Service;

  beforeEach(() => {
    mockWasm = createMockWasm();
    service = new Stq8Service(mockWasm);
  });

  it('generateChallenge calls WASM and parses JSON', () => {
    const challenge = service.generateChallenge(0);
    expect(mockWasm.generate_challenge).toHaveBeenCalledWith(
      0,
      expect.any(Uint8Array),
    );
    expect(challenge.data).toEqual([0x42]);
    expect(challenge.rows).toEqual([[0x42]]);
  });

  it('generateChallenge passes rng_bytes of correct length', () => {
    mockWasm.level_info.mockReturnValue(JSON.stringify({
      total_bytes: 32, bytes_per_row: 4, num_rows: 8, total_bits: 256,
    }));
    service.generateChallenge(4);
    const call = mockWasm.generate_challenge.mock.calls[0];
    expect(call[1]).toBeInstanceOf(Uint8Array);
    expect(call[1].length).toBe(32);
  });

  it('validateRow calls WASM with expected_bytes and heard_nibbles', () => {
    const result = service.validateRow([0x00], [0, 0]);
    expect(mockWasm.validate_row).toHaveBeenCalledWith(
      new Uint8Array([0x00]),
      new Uint8Array([0, 0]),
    );
    expect(result.matched).toBe(true);
  });

  it('getLevelInfo returns parsed level metadata', () => {
    const info = service.getLevelInfo(0);
    expect(info.total_bytes).toBe(1);
    expect(info.bytes_per_row).toBe(1);
    expect(info.num_rows).toBe(1);
    expect(info.total_bits).toBe(8);
  });

  it('isReady returns true when wasm is provided', () => {
    expect(service.isReady()).toBe(true);
  });

  it('isReady returns false when wasm is null', () => {
    const unloaded = new Stq8Service(null);
    expect(unloaded.isReady()).toBe(false);
  });

  it('addCalibrationSample forwards syllable index and pcm to WASM', () => {
    const pcm = new Float32Array([0.1, 0.2, 0.3]);
    service.addCalibrationSample(7, pcm);
    expect(mockWasm.add_calibration_sample).toHaveBeenCalledWith(7, pcm);
  });

  it('finalizeCalibration delegates to WASM', () => {
    service.finalizeCalibration();
    expect(mockWasm.finalize_calibration).toHaveBeenCalledOnce();
  });

  it('isCalibrated reflects WASM state', () => {
    mockWasm.is_calibrated.mockReturnValue(true);
    expect(service.isCalibrated()).toBe(true);
    mockWasm.is_calibrated.mockReturnValue(false);
    expect(service.isCalibrated()).toBe(false);
  });

  it('isCalibrated returns false when wasm is null (no throw)', () => {
    const unloaded = new Stq8Service(null);
    expect(unloaded.isCalibrated()).toBe(false);
  });

  it('exportProfile returns the WASM-serialized JSON', () => {
    const json = service.exportProfile();
    expect(json).toBe('{"centroids":{},"created":0}');
  });

  it('importProfile forwards JSON to WASM', () => {
    service.importProfile('{"centroids":{"0":[1,2,3]}}');
    expect(mockWasm.import_profile).toHaveBeenCalledWith('{"centroids":{"0":[1,2,3]}}');
  });

  it('setCreatedEpochSecs forwards bigint to WASM', () => {
    service.setCreatedEpochSecs(1234567890n);
    expect(mockWasm.set_created_epoch_secs).toHaveBeenCalledWith(1234567890n);
  });
});
