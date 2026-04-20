import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { resolve } from 'path';

// stq8-web WASM bindings live in the sibling `harmony-stq8` clone.
// Developers run `scripts/build-wasm.sh` there to produce `stq8-web/pkg/`
// (gitignored), which this alias points at. Spellbook falls back to a
// friendly "not built yet" state if the import fails, so a fresh clone
// without the harmony-stq8 sibling still boots cleanly.
const stq8WebPkg = resolve(__dirname, '../harmony-stq8/stq8-web/pkg');

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  resolve: {
    alias: {
      'harmony-stq8': stq8WebPkg,
    },
  },
  server: {
    port: 5173,
    strictPort: true,
    fs: {
      // Vite's default deny-outside-root guard blocks the sibling pkg.
      allow: [resolve(__dirname), stq8WebPkg],
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
