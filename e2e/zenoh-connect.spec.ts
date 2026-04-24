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

async function ensureNetworkVizOpen(mainPage: Page, browser: Browser): Promise<Page> {
  await waitForNodeReady(mainPage);
  const already = countPagesByPath(browser, NETWORK_VIZ_PATH) > 0;
  if (!already) {
    await mainPage.getByRole('button', { name: 'Open network visualization' }).click();
  }
  const viz = await findPageByPath(browser, NETWORK_VIZ_PATH, 10_000);
  await waitForTauriBridge(viz);
  return viz;
}

/**
 * Force the ConnectionBar to the disconnected state. Required before any test
 * that wants to exercise the Connect click path — reruns within a single
 * `tauri dev` session would otherwise start with a stale connection from a
 * prior test.
 */
async function resetToDisconnected(viz: Page): Promise<void> {
  const disconnectBtn = viz.getByRole('button', { name: 'Disconnect', exact: true });
  const cancelBtn = viz.getByRole('button', { name: 'Cancel', exact: true });
  if (await disconnectBtn.isVisible().catch(() => false)) {
    await disconnectBtn.click();
  } else if (await cancelBtn.isVisible().catch(() => false)) {
    await cancelBtn.click();
  }
  // Endpoint input is only enabled when the session is disconnected or errored.
  await expect(viz.getByLabel('Zenoh endpoint')).toBeEnabled({ timeout: 5_000 });
}

/**
 * Regression guard for ZEB-149: clicking Connect used to stack-overflow the
 * Tauri process (start_node ran a current-thread Tokio, Zenoh's .wait()
 * block_in_place panicked, and the panic payload overflowed the 2 MiB default
 * thread stack). Fix was in PR #43: multi-thread(1) scheduler + 8 MiB stacks.
 *
 * We detect "did not crash" by doing `invoke('get_node_addr')` after the
 * click — if the host process died, the CDP WebSocket is gone and the invoke
 * would reject with a transport error.
 */
test.describe('zenoh connect', () => {
  test('clicking Connect on tcp/127.0.0.1:7447 does not crash [ZEB-149]', async ({
    mainPage,
    cdpBrowser,
  }) => {
    const viz = await ensureNetworkVizOpen(mainPage, cdpBrowser);
    await resetToDisconnected(viz);

    await viz.getByLabel('Zenoh endpoint').fill('tcp/127.0.0.1:7447');
    await viz.getByRole('button', { name: 'Connect', exact: true }).click();

    // The pre-fix crash surfaced within ~2s. Sit on 8s to match the issue's
    // manual-repro window, then confirm the process is still responsive.
    await viz.waitForTimeout(8_000);

    const addr = await invoke<string>(viz, 'get_node_addr');
    expect(addr).toMatch(/^[0-9a-f]{32}$/i);
  });

  test('status flips to Connected within 10s', async ({ mainPage, cdpBrowser }) => {
    const viz = await ensureNetworkVizOpen(mainPage, cdpBrowser);

    // `exact: true` is critical — default substring match would also pick up
    // the "Disconnect" button, which isn't visible when we're not connected
    // but has the same "Connect" substring.
    const connectBtn = viz.getByRole('button', { name: 'Connect', exact: true });
    if (await connectBtn.isVisible().catch(() => false)) {
      await viz.getByLabel('Zenoh endpoint').fill('tcp/127.0.0.1:7447');
      await connectBtn.click();
    }

    // The connection status indicator is a `role="status"` with its aria-label
    // mirroring the `statusLabel` derivation (Disconnected | Connecting... |
    // Connected, N nodes discovered | Error: ...). Filter by text so we don't
    // collide with NetworkStatusBar or announcement live-regions.
    await expect(
      viz.locator('[role="status"]', { hasText: /Connected/ }),
    ).toBeVisible({ timeout: 10_000 });
  });

  test('invokes still respond after Connect', async ({ mainPage, cdpBrowser }) => {
    const viz = await ensureNetworkVizOpen(mainPage, cdpBrowser);

    const connectBtn = viz.getByRole('button', { name: 'Connect', exact: true });
    if (await connectBtn.isVisible().catch(() => false)) {
      await viz.getByLabel('Zenoh endpoint').fill('tcp/127.0.0.1:7447');
      await connectBtn.click();
      await viz.waitForTimeout(3_000);
    }

    // Probe IPC on the *viz* page — that's the context where Connect ran, so
    // that's the bridge that could have been damaged. Custom harmony commands
    // are registered via `generate_handler!` and are globally callable from
    // any webview; the network-viz capability only restricts `core:*` /
    // webview-spawn, not our own commands.
    const addr = await invoke<string>(viz, 'get_node_addr');
    expect(addr).toMatch(/^[0-9a-f]{32}$/i);

    const followed = await invoke<unknown[]>(viz, 'list_followed');
    expect(Array.isArray(followed)).toBe(true);

    const counts = await invoke<Record<string, { total: number; unread: number }>>(
      viz,
      'get_mail_counts',
    );
    expect(counts).toHaveProperty('inbox');
  });
});
