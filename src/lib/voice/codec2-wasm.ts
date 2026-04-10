/**
 * Placeholder for the emscripten-compiled codec2 WASM module.
 *
 * In production, this file is replaced by the emscripten build output
 * (build/codec2/Makefile generates codec2-wasm.js + codec2-wasm.wasm).
 *
 * The default export is a factory function that resolves to the module.
 */
export default async function createCodec2Module(): Promise<unknown> {
  throw new Error(
    'codec2 WASM not built. Run: make -C build/codec2'
  );
}
