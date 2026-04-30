import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import {
  MIN_RECOVERY_PASSPHRASE_LEN,
  MAX_RECOVERY_COMMENT_BYTES,
} from './recovery-policy';

// Read the Rust source and assert the integer literals match the TS
// exports. Failure means the two policy modules have drifted — re-sync
// them and re-run.
const here = dirname(fileURLToPath(import.meta.url));
const RUST_PATH = resolve(here, '../../src-tauri/src/recovery_policy.rs');
const rustSource = readFileSync(RUST_PATH, 'utf-8');

describe('recovery-policy: Rust ↔ TS drift detector', () => {
  it('MIN_RECOVERY_PASSPHRASE_LEN matches the Rust source', () => {
    const m = rustSource.match(
      /pub const MIN_RECOVERY_PASSPHRASE_LEN: usize = (\d+);/
    );
    expect(
      m,
      'Could not find MIN_RECOVERY_PASSPHRASE_LEN in recovery_policy.rs'
    ).not.toBeNull();
    expect(
      Number(m![1]),
      'Rust and TS recovery-policy modules disagree on MIN_RECOVERY_PASSPHRASE_LEN'
    ).toBe(MIN_RECOVERY_PASSPHRASE_LEN);
  });

  it('MAX_RECOVERY_COMMENT_BYTES matches the Rust source', () => {
    const m = rustSource.match(
      /pub const MAX_RECOVERY_COMMENT_BYTES: usize = (\d+);/
    );
    expect(
      m,
      'Could not find MAX_RECOVERY_COMMENT_BYTES in recovery_policy.rs'
    ).not.toBeNull();
    expect(
      Number(m![1]),
      'Rust and TS recovery-policy modules disagree on MAX_RECOVERY_COMMENT_BYTES'
    ).toBe(MAX_RECOVERY_COMMENT_BYTES);
  });
});
