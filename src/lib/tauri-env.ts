/**
 * True when running inside a Tauri webview.
 *
 * Re-exported from `@tauri-apps/api/core` so the rest of the app doesn't
 * have to reach into the Tauri package directly (and so we can swap the
 * check in one place if Tauri's detection ever changes). Absence means
 * we're in a plain browser — Vite `npm run dev` without `tauri dev`,
 * jsdom under vitest, a hosted preview, etc. — where services should
 * fall back to mock data rather than error.
 *
 * Use this instead of wrapping the whole init flow in `try/catch`: that
 * pattern conflates "Tauri not present" (expected) with "Tauri is
 * present but a command/import failed" (a real bug that should surface).
 */
export { isTauri } from '@tauri-apps/api/core';
