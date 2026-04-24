import { test as base, chromium, type Browser, type Page } from '@playwright/test';

const DEFAULT_CDP_ENDPOINT = 'http://localhost:9222';
const VITE_ORIGIN = 'http://localhost:5173';
// Child windows (currently just network-viz) are also served from the Vite
// origin, so matching by origin alone isn't enough to identify the root app
// page — we must filter child window paths out.
const CHILD_WINDOW_PATHS = ['/src/network.html'];

/**
 * Poll the CDP browser for the main app page: the root Vite target whose
 * path is NOT one of the known child-window routes. Child windows (e.g.
 * network-viz) are on the same origin, so a naive origin match picks the
 * wrong page when a child window is already open.
 *
 * The webview briefly loads `about:blank` before Svelte mounts (see
 * `project_playwright_tauri_bridge.md`), so callers must never assume
 * `browser.contexts()[0].pages()[0]` is the target.
 */
async function findViteMainPage(
  browser: Browser,
  timeoutMs: number,
): Promise<Page> {
  const deadline = Date.now() + timeoutMs;
  const isMain = (url: string) =>
    url.startsWith(VITE_ORIGIN) && !CHILD_WINDOW_PATHS.some((p) => url.includes(p));
  while (Date.now() < deadline) {
    for (const ctx of browser.contexts()) {
      for (const page of ctx.pages()) {
        if (isMain(page.mainFrame().url())) return page;
      }
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(
    `Timed out after ${timeoutMs}ms waiting for the main page on ${VITE_ORIGIN}. ` +
      `Is \`tauri dev\` running with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222?`,
  );
}

/**
 * Invoke a Tauri command through `window.__TAURI_INTERNALS__.invoke`. Returns
 * the deserialized result; throws if the bridge is missing or the command
 * rejects. Args shape follows Tauri v2 conventions (a single object of named
 * params per command, or omitted for no-arg commands).
 */
export async function invoke<T = unknown>(
  page: Page,
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return page.evaluate(
    async ({ command, args }) => {
      const inv = (window as unknown as {
        __TAURI_INTERNALS__?: {
          invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
        };
      }).__TAURI_INTERNALS__?.invoke;
      if (typeof inv !== 'function') {
        throw new Error('window.__TAURI_INTERNALS__.invoke is not available');
      }
      return inv(command, args);
    },
    { command, args },
  ) as Promise<T>;
}

/**
 * Wait for the Tauri bridge to be attached to the page. This is cheap; the
 * webview exposes `__TAURI_INTERNALS__` before Svelte mounts, so this usually
 * resolves on the first poll.
 */
export async function waitForTauriBridge(
  page: Page,
  timeoutMs = 15_000,
): Promise<void> {
  await page.waitForFunction(
    () =>
      typeof (window as unknown as { __TAURI_INTERNALS__?: { invoke?: unknown } })
        .__TAURI_INTERNALS__?.invoke === 'function',
    null,
    { timeout: timeoutMs },
  );
}

const NODE_ADDR_HEX_RE = /^[0-9a-f]{32}$/i;

/**
 * Wait for the Zenoh node to be started and identified. Polls `get_node_addr`
 * until it returns a 32-char hex identity (Tauri's string repr of the 16-byte
 * ZenohId). Early in boot the command returns "not connected".
 *
 * Returns the hex identity for downstream assertions.
 */
export async function waitForNodeReady(
  page: Page,
  timeoutMs = 20_000,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown = '<never polled>';
  while (Date.now() < deadline) {
    try {
      last = await invoke<string>(page, 'get_node_addr');
      if (typeof last === 'string' && NODE_ADDR_HEX_RE.test(last)) return last;
    } catch (err) {
      last = `threw: ${String(err)}`;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(
    `Node did not report a hex identity within ${timeoutMs}ms. Last get_node_addr: ${String(last)}`,
  );
}

type WorkerFixtures = {
  /** CDP-attached browser. Worker-scoped: one connection per worker. */
  cdpBrowser: Browser;
};

type TestFixtures = {
  /** The main page (not network-viz) whose origin is the Vite dev server. */
  mainPage: Page;
};

/**
 * Test fixture augmented with a live CDP connection. Tests receive a `mainPage`
 * already confirmed to be on the Vite origin.
 *
 * Teardown deliberately does NOT call `browser.close()`: on a CDP-attached
 * WebView2, Playwright forwards `Browser.close`, which terminates the Tauri
 * host process (observed exit `0xcfffffff`). We let the node worker exit and
 * rely on the WebSocket dying with it. See project_playwright_tauri_bridge.md.
 */
export const test = base.extend<TestFixtures, WorkerFixtures>({
  cdpBrowser: [
    async ({}, use) => {
      const endpoint = process.env.CDP_ENDPOINT ?? DEFAULT_CDP_ENDPOINT;
      const browser = await chromium.connectOverCDP(endpoint);
      await use(browser);
      // Intentional no-op on teardown; see doc comment above.
    },
    { scope: 'worker' },
  ],
  mainPage: async ({ cdpBrowser }, use) => {
    const page = await findViteMainPage(cdpBrowser, 15_000);
    await waitForTauriBridge(page);
    await use(page);
  },
});

export { expect } from '@playwright/test';
