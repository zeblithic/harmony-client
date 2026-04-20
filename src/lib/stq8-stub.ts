// Type-compatible stand-in for the `harmony-stq8` WASM module emitted by
// wasm-pack, so `vite build` succeeds on machines without the sibling
// `../harmony-stq8/stq8-web/pkg/` directory (fresh clones, CI, Tauri
// packaging on a machine that never ran `scripts/build-wasm.sh` in the
// harmony-stq8 repo).
//
// The alias in `vite.config.ts` points at the real pkg when it exists,
// otherwise at this stub. App.svelte's WASM-init IIFE wraps the dynamic
// `import('harmony-stq8')` + `default()` call in try/catch, so this
// stub's throws surface as the same "[harmony-client] stq8 WASM not
// loaded" info log — Spellbook stays in its "Loading Q8 engine..."
// placeholder state instead of crashing the bundler at build time.
//
// Keep this file's exported shape in sync with `stq8-web/pkg/stq8_web.d.ts`
// so TypeScript is happy via `tsconfig.json`'s `paths` alias regardless
// of whether the sibling clone exists.

const STUB_ERROR = 'stq8 WASM stub — sibling harmony-stq8 clone not found. Run scripts/build-wasm.sh in ../harmony-stq8 to enable Spellbook.';

export default async function init(): Promise<never> {
  throw new Error(STUB_ERROR);
}

export class WasmPipeline {
  constructor() {
    throw new Error(STUB_ERROR);
  }

  free(): void {}

  process(_pcm: Float32Array): string {
    throw new Error(STUB_ERROR);
  }

  static generate_challenge(_level: number, _rng_bytes: Uint8Array): string {
    throw new Error(STUB_ERROR);
  }

  static validate_row(_expected_bytes: Uint8Array, _heard_nibbles: Uint8Array): string {
    throw new Error(STUB_ERROR);
  }

  static format_box_q8(_data: Uint8Array, _bytes_per_row: number): string {
    throw new Error(STUB_ERROR);
  }

  static format_flat_q8(_data: Uint8Array, _bytes_per_row: number): string {
    throw new Error(STUB_ERROR);
  }

  static level_info(_level: number): string {
    throw new Error(STUB_ERROR);
  }
}
