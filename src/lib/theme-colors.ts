// ZEB-605: resolve CSS custom-property colors for canvas/TS drawing code that
// cannot consume var(--…) directly (canvas fillStyle, color lerp math).
//
// COMMONS_FALLBACK carries the Commons LIGHT values — the single sanctioned
// raw-hex site for these tokens. jsdom returns '' for stylesheet-defined
// custom properties, so tests resolve deterministically to these constants.
import { THEME_APPLIED_EVENT } from './theme-service';

export const COMMONS_FALLBACK: Record<string, string> = {
  '--accent': '#466b4c',
  '--info': '#4a6fa5',
  '--warning': '#b9742c',
  '--warning-bright': '#b9742c',
  '--danger': '#b1402f',
  '--success-gov': '#466b4c',
  '--success-deep': '#2f4a35',
  '--presence-online': '#466b4c',
  '--text-muted': '#767a6c',
  '--text-faint': '#a39e8e',
  '--text-secondary': '#4b4f44',
  '--text-primary': '#20241c',
  '--bg-primary': '#fbf9f4',
  '--paper': '#f4f1ea',
  '--flashcard-hint': '#b9742c',
  '--gov-purple': '#7d6ba0',
  '--cat-orange': '#c56a46',
  '--cat-yellow': '#b9742c',
  '--cat-blue': '#4a6fa5',
  '--cat-purple': '#7d6ba0',
};

const cache = new Map<string, string>();
let listenerInstalled = false;

function ensureInvalidationListener(): void {
  if (listenerInstalled || typeof document === 'undefined') {
    return;
  }
  document.addEventListener(THEME_APPLIED_EVENT, () => cache.clear());
  listenerInstalled = true;
}

export function tokenColor(name: string): string {
  ensureInvalidationListener();
  const cached = cache.get(name);
  if (cached !== undefined) {
    return cached;
  }
  let value = '';
  try {
    value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  } catch {
    value = '';
  }
  if (value === '') {
    value = COMMONS_FALLBACK[name] ?? '#000000';
  }
  cache.set(name, value);
  return value;
}

/** Test-only. */
export function _clearTokenColorCacheForTest(): void {
  cache.clear();
}
