import { defineConfig } from '@playwright/test';

/**
 * Playwright config for the Windows WebView2 CDP harness.
 *
 * This suite does not launch a browser; it attaches to an already-running
 * `tauri dev` instance via CDP. See e2e/README.md for the two-terminal
 * workflow. Runs are local-only, Windows-only, human-triggered.
 */
export default defineConfig({
  testDir: '.',
  testMatch: /.*\.spec\.ts/,
  // One worker: there is exactly one live WebView2 instance to attach to.
  workers: 1,
  fullyParallel: false,
  // Surface flakes immediately on first run — no retry masking.
  retries: 0,
  // Identity PoW on first boot can take several seconds; give tests headroom.
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [['list'], ['html', { outputFolder: 'playwright-report', open: 'never' }]],
  outputDir: 'test-results',
  use: {
    // No baseURL — specs attach to the live webview and derive page from CDP.
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
});
