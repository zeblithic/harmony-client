import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte({ hot: !process.env.VITEST })],
  resolve: {
    conditions: ['browser'],
  },
  test: {
    environment: 'jsdom',
    environmentOptions: {
      jsdom: {
        url: 'http://localhost',
      },
    },
    // Override Node.js 22+ built-in localStorage with jsdom's full Storage
    // implementation (which supports .clear(), .key(), etc.).
    setupFiles: ['./src/test-setup.ts'],
    globals: true,
    include: ['src/**/*.test.ts'],
  },
});
