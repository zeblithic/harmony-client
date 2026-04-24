import { test, expect, invoke, waitForNodeReady } from './fixtures/tauri-bridge';

/**
 * Boot-time smoke tests. Assert the invariants a fresh `tauri dev` should
 * satisfy before a user interacts with anything.
 */
test.describe('boot', () => {
  test('Tauri bridge is available on the main page', async ({ mainPage }) => {
    const bridgeInfo = await mainPage.evaluate(() => {
      const inv = (window as unknown as {
        __TAURI_INTERNALS__?: { invoke?: unknown };
      }).__TAURI_INTERNALS__?.invoke;
      return { hasInvoke: typeof inv === 'function' };
    });
    expect(bridgeInfo.hasInvoke).toBe(true);
  });

  test('node auto-starts and get_node_addr returns 32-char hex [ZEB-143]', async ({
    mainPage,
  }) => {
    // App.svelte auto-invokes start_node on boot (PR #43). Before that fix the
    // UI silently fell back to mock data because start_node was never called.
    const addr = await waitForNodeReady(mainPage);
    expect(addr).toMatch(/^[0-9a-f]{32}$/i);
  });

  test('mail manager initializes and get_mail_counts returns folder counts', async ({
    mainPage,
  }) => {
    // Wait for node before asking for mail — the mail manager is initialized
    // during the start_node flow, so this command errors with "mail not
    // initialized" if you race it.
    await waitForNodeReady(mainPage);
    const counts = await invoke<Record<string, { total: number; unread: number }>>(
      mainPage,
      'get_mail_counts',
    );
    for (const folder of ['inbox', 'sent', 'drafts', 'trash']) {
      expect(counts, `missing folder ${folder}`).toHaveProperty(folder);
      expect(typeof counts[folder].total).toBe('number');
      expect(typeof counts[folder].unread).toBe('number');
    }
  });
});
