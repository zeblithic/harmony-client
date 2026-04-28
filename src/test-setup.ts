// Vitest global setup file.
//
// Node.js 22+ exposes its own `localStorage` global backed by a file, which
// lacks `clear()` and emits a "--localstorage-file" warning.  The jsdom
// environment provides a proper in-memory Storage implementation, but
// vitest's populateGlobal() skips keys that already exist on the Node.js
// global unless they are in its hardcoded KEYS list.
//
// vitest sets `global.jsdom = dom` (the raw JSDOM instance) after environment
// setup.  We use that to retrieve the real jsdom Storage objects and install
// them as the global localStorage/sessionStorage before each test file runs.

// Adds jest-dom matchers (`toBeInTheDocument`, `toBeDisabled`, `toHaveClass`,
// etc.) to vitest's `expect`. The package is in devDependencies but doesn't
// auto-wire — this one-line import unblocks idiomatic component-test
// assertions across the suite.
import '@testing-library/jest-dom/vitest';

declare const jsdom: { window: Window & typeof globalThis };

if (typeof jsdom !== 'undefined' && jsdom.window) {
  const jsdomWindow = jsdom.window;
  Object.defineProperty(globalThis, 'localStorage', {
    get() { return jsdomWindow.localStorage; },
    configurable: true,
  });
  Object.defineProperty(globalThis, 'sessionStorage', {
    get() { return jsdomWindow.sessionStorage; },
    configurable: true,
  });
}
