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

/**
 * Close any open network-viz window so the next click must actually spawn a
 * fresh one. Without this, a prior spec's leftover window would make the
 * ZEB-144 regression guard vacuous: the count-after assertion would pass even
 * if the button did nothing at all.
 *
 * Invokes Tauri's `plugin:window|close` with the explicit `network-viz`
 * label — the same underlying command `WebviewWindow.close()` uses (see
 * `@tauri-apps/api/window.js`). We can't just dynamic-import that module
 * inside `page.evaluate` because bare-specifier resolution only works
 * through Vite, not in a raw eval context.
 *
 * Raw CDP `page.close()` would NOT work here: it kills the webview target
 * but leaves Tauri's Rust-side window registry pointing at a ghost, so the
 * next button click calls `setFocus()` on a dead window and silently no-ops.
 * Routing through the window plugin goes through `WindowEvent::Destroyed`
 * and drops the label cleanly.
 */
async function closeAllNetworkVizPages(browser: Browser, mainPage: Page): Promise<void> {
  // Invoke `plugin:window|close` with the child's label to remove it from
  // Tauri's window registry. "window not found" on a fresh boot is fine —
  // there's nothing to close. This requires `core:window:allow-close` on the
  // main window's capability (granted in capabilities/default.json).
  await invoke(mainPage, 'plugin:window|close', { label: 'network-viz' }).catch(() => {});
  await expect
    .poll(() => countPagesByPath(browser, NETWORK_VIZ_PATH), { timeout: 5_000 })
    .toBe(0);
}

test.describe('navigation', () => {
  test('Network button opens the visualization window [ZEB-144]', async ({
    mainPage,
    cdpBrowser,
  }) => {
    await waitForNodeReady(mainPage);

    // Regression guard for ZEB-144: before the capability fix the click
    // silently failed at the capability layer. Start from zero viz pages so
    // the assertion below is proof that the *click* spawned the window, not
    // that a prior spec's leftover is still hanging around.
    await closeAllNetworkVizPages(cdpBrowser, mainPage);

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
