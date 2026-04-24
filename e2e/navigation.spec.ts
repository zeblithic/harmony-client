import type { Page } from '@playwright/test';
import { test, expect, invoke, waitForTauriBridge, waitForNodeReady } from './fixtures/tauri-bridge';

const NETWORK_VIZ_PATH = '/src/network.html';

/**
 * Poll the browser for a page whose main frame URL ends with the given
 * pathname. Handles the race where Tauri has opened the window but CDP
 * hasn't published the new target yet.
 */
async function findPageByPath(
  browser: import('@playwright/test').Browser,
  pathname: string,
  timeoutMs: number,
): Promise<Page> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const ctx of browser.contexts()) {
      for (const page of ctx.pages()) {
        const url = page.mainFrame().url();
        if (url.includes(pathname)) return page;
      }
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`Timed out after ${timeoutMs}ms waiting for a page with path ${pathname}`);
}

test.describe('navigation', () => {
  test('Network button opens the visualization window [ZEB-144]', async ({
    mainPage,
    cdpBrowser,
  }) => {
    await waitForNodeReady(mainPage);

    const vizExists = () =>
      cdpBrowser
        .contexts()
        .some((c) => c.pages().some((p) => p.mainFrame().url().includes(NETWORK_VIZ_PATH)));

    // Regression guard for ZEB-144: before the capability fix the click
    // silently failed at the capability layer — the button looked wired but
    // nothing happened. We don't care whether this call spawns a fresh window
    // or focuses an existing one (NavPanel handles both), only that after
    // the click the viz page exists.
    await mainPage.getByRole('button', { name: 'Open network visualization' }).click();
    await expect.poll(vizExists, { timeout: 10_000 }).toBe(true);

    const vizPage = await findPageByPath(cdpBrowser, NETWORK_VIZ_PATH, 10_000);
    await waitForTauriBridge(vizPage);
    expect(await vizPage.title()).toContain('Network');
  });

  test('network-viz page has working Tauri bridge', async ({ mainPage, cdpBrowser }) => {
    await waitForNodeReady(mainPage);
    const alreadyOpen = cdpBrowser
      .contexts()
      .some((c) => c.pages().some((p) => p.mainFrame().url().includes(NETWORK_VIZ_PATH)));
    if (!alreadyOpen) {
      await mainPage.getByRole('button', { name: 'Open network visualization' }).click();
    }

    const vizPage = await findPageByPath(cdpBrowser, NETWORK_VIZ_PATH, 10_000);
    await waitForTauriBridge(vizPage);

    // Reuse waitForNodeReady against the viz page — confirms that invoke()
    // round-trips from the child window, not just the main window.
    const addr = await waitForNodeReady(vizPage);
    expect(addr).toMatch(/^[0-9a-f]{32}$/i);
  });

  test('network-viz cannot spawn further windows (least-privilege)', async ({
    mainPage,
    cdpBrowser,
  }) => {
    await waitForNodeReady(mainPage);
    const alreadyOpen = cdpBrowser
      .contexts()
      .some((c) => c.pages().some((p) => p.mainFrame().url().includes(NETWORK_VIZ_PATH)));
    if (!alreadyOpen) {
      await mainPage.getByRole('button', { name: 'Open network visualization' }).click();
    }

    const vizPage = await findPageByPath(cdpBrowser, NETWORK_VIZ_PATH, 10_000);
    await waitForTauriBridge(vizPage);

    // network-viz capability deliberately omits core:webview:allow-create-webview-window.
    // Attempting to spawn another window from it must error, not succeed silently.
    let rejected = false;
    let errMsg = '';
    try {
      await invoke(vizPage, 'plugin:webview|create_webview_window', {
        options: { label: 'must-not-open', url: '/src/network.html' },
      });
    } catch (err) {
      rejected = true;
      errMsg = String(err);
    }
    expect(rejected, `create_webview_window should have been rejected from network-viz`).toBe(true);
    expect(errMsg.toLowerCase()).toContain('not allowed');
  });
});
