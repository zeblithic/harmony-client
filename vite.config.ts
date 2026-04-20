import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { resolve } from 'path';
import { existsSync } from 'fs';

// stq8-web WASM bindings live in the sibling `harmony-stq8` clone.
// Developers run `scripts/build-wasm.sh` there to produce `stq8-web/pkg/`
// (gitignored), which this alias points at when present. On fresh
// clones, CI, and Tauri packaging machines without the sibling, the
// alias falls back to the in-repo stub at `src/lib/stq8-stub.ts` so
// `vite build` still resolves — Spellbook then stays in its placeholder
// "Loading Q8 engine..." state (the stub's init throws, App.svelte's
// IIFE catches, rest of the app boots normally).
const stq8WebPkg = resolve(__dirname, '../harmony-stq8/stq8-web/pkg');
const stq8Stub = resolve(__dirname, 'src/lib/stq8-stub.ts');
const stq8HasRealPkg = existsSync(resolve(stq8WebPkg, 'stq8_web.js'));
const stq8AliasTarget = stq8HasRealPkg ? stq8WebPkg : stq8Stub;

if (!stq8HasRealPkg) {
  console.warn('[harmony-client] harmony-stq8 sibling pkg/ not found — Spellbook will be disabled in this build. Run scripts/build-wasm.sh in ../harmony-stq8 to enable it.');
}

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  resolve: {
    alias: {
      'harmony-stq8': stq8AliasTarget,
    },
  },
  server: {
    port: 5173,
    strictPort: true,
    fs: {
      // Vite's default deny-outside-root guard blocks the sibling pkg.
      // (The stub is in-tree, so it doesn't need an allow entry.)
      allow: stq8HasRealPkg ? [resolve(__dirname), stq8WebPkg] : [resolve(__dirname)],
    },
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: 'esnext',
    minify: !process.env.TAURI_DEBUG ? 'esbuild' : false,
    sourcemap: !!process.env.TAURI_DEBUG,
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        network: resolve(__dirname, 'src/network.html'),
      },
    },
  },
});
