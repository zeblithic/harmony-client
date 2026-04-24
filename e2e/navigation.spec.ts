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
 * ZEB-144 regression guard vacuous: the count-after assertion would pass
 * even if the button did nothing at all.
 *
 * Invokes `e2e_close_window` — a `#[cfg(debug_assertions)]`-gated Tauri
 * command in `src-tauri/src/lib.rs` that closes a child window by label and
 * is hard-coded to only accept `network-viz`. We deliberately do NOT use
 * `plugin:window|close` from the main webview, because that would require
 * adding `core:window:allow-close` to the production capability, expanding
 * the main window's attack surface for a test-only need (any JS in main
 * could then close any window, including itself). Routing through a
 * debug-only Rust command keeps production caps untouched and is stripped
 * from release binaries entirely.
 *
 * Raw CDP `page.close()` would also NOT work: it destroys the webview
 * target but leaves Tauri's Rust-side window registry pointing at a ghost,
 * so the next button click calls `setFocus()` on a dead window and silently
 * no-ops. The Rust command uses `WebviewWindow::close()` which routes
 * through `WindowEvent::Destroyed` and drops the label cleanly.
 *
 * Failures are NOT swallowed. If `e2e_close_window` is missing (release
 * build, registration regressed, label rule changed) we want the test to
 * fail loudly — silent fallthrough would re-introduce the very vacuous-pass
 * we're guarding against. Short-circuit when there's nothing to close so
 * "no viz exists" doesn't get conflated with "command failed".
 */
async function closeAllNetworkVizPages(browser: Browser, mainPage: Page): Promise<void> {
  if (countPagesByPath(browser, NETWORK_VIZ_PATH) === 0) return;
  await invoke(mainPage, 'e2e_close_window', { label: 'network-viz' });
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
