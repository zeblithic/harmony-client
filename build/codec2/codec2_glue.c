/*
 * Thin C wrapper for codec2 — exports the minimal surface needed
 * by the TypeScript Codec2Codec wrapper.
 *
 * Build with emscripten:
 *   emcc codec2_glue.c -I codec2/src -L codec2/build/src -lcodec2 \
 *     -O2 -s MODULARIZE=1 -s EXPORT_ES6=1 \
 *     -s EXPORTED_FUNCTIONS='["_codec2_create","_codec2_destroy","_codec2_encode","_codec2_decode","_codec2_samples_per_frame","_codec2_bits_per_frame","_malloc","_free"]' \
 *     -s EXPORTED_RUNTIME_METHODS='["HEAP16","HEAPU8"]' \
 *     -o ../../src/lib/voice/codec2-wasm.js
 */

#include "codec2/codec2.h"
