/**
 * True when running inside a Tauri webview.
 *
 * Tauri injects `window.__TAURI_INTERNALS__` before any user script runs
 * (it's how `@tauri-apps/api/core`'s `invoke` reaches native code). Its
 * absence means we're in a plain browser — Vite `npm run dev` without
 * `tauri dev`, jsdom under vitest, a hosted preview, etc. — where
 * services should fall back to mock data rather than error.
 *
 * Use this instead of wrapping the whole init flow in `try/catch`: that
 * pattern conflates "Tauri not present" (expected) with "Tauri is
 * present but a command/import failed" (a real bug that should surface).
 */
export function isTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}
