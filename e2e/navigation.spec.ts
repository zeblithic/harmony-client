import type { Browser, Page } from '@playwright/test';
import {
  test,
  expect,
  invoke,
  waitForTauriBridge,
  waitForNodeReady,
  findPageByPath,
  countPagesByPath,
  NETWORK_VIZ_PATH,
} from './fixtures/tauri-bridge';

/**
 * Open the network-viz window (or re-discover it if already open) and return
 * its Playwright page. Factored out so every nav/zenoh test agrees on the
 * polling timeout and the bridge-ready contract.
 */
async function getOrOpenNetworkViz(mainPage: Page, browser: Browser): Promise<Page> {
  const alreadyOpen = countPagesByPath(browser, NETWORK_VIZ_PATH) > 0;
  if (!alreadyOpen) {
    await mainPage.getByRole('button', { name: 'Open network visualization' }).click();
  }
  const viz = await findPageByPath(browser, NETWORK_VIZ_PATH, 10_000);
  await waitForTauriBridge(viz);
  return viz;
}

test.describe('navigation', () => {
  test('Network button opens the visualization window [ZEB-144]', async ({
    mainPage,
    cdpBrowser,
  }) => {
    await waitForNodeReady(mainPage);

    // Regression guard for ZEB-144: before the capability fix the click
    // silently failed at the capability layer. We don't care whether this
    // spawns a fresh window or focuses an existing one (NavPanel handles
    // both) — only that after the click the viz page exists and its bridge
    // resolves.
    await mainPage.getByRole('button', { name: 'Open network visualization' }).click();
    await expect
      .poll(() => countPagesByPath(cdpBrowser, NETWORK_VIZ_PATH), { timeout: 10_000 })
      .toBeGreaterThan(0);

    const vizPage = await findPageByPath(cdpBrowser, NETWORK_VIZ_PATH, 10_000);
    await waitForTauriBridge(vizPage);
    expect(await vizPage.title()).toContain('Network');
  });

  test('network-viz page has working Tauri bridge', async ({ mainPage, cdpBrowser }) => {
    await waitForNodeReady(mainPage);
    const vizPage = await getOrOpenNetworkViz(mainPage, cdpBrowser);

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
    const vizPage = await getOrOpenNetworkViz(mainPage, cdpBrowser);

    // network-viz capability deliberately omits core:webview:allow-create-webview-window.
    // The regression guard is the security *property* ("child cannot spawn
    // further children"), not the exact error wording — wording could change
    // across Tauri/plugin versions while enforcement stays correct. Assert
    // both (a) the call was rejected and (b) no extra page materialized.
    const bogusLabel = `must-not-open-${Date.now().toString(36)}`;
    const pagesBefore = countPagesByPath(cdpBrowser, NETWORK_VIZ_PATH);

    let rejected = false;
    try {
      await invoke(vizPage, 'plugin:webview|create_webview_window', {
        options: { label: bogusLabel, url: NETWORK_VIZ_PATH },
      });
    } catch {
      rejected = true;
    }
    expect(
      rejected,
      'create_webview_window should have been rejected from network-viz',
    ).toBe(true);

    // If the capability layer ever regressed and accepted the call, a new
    // target would appear. Poll briefly to avoid a race where CDP publishes
    // the new page slightly after the invoke resolves/rejects.
    await vizPage.waitForTimeout(500);
    expect(countPagesByPath(cdpBrowser, NETWORK_VIZ_PATH)).toBe(pagesBefore);
  });
});
