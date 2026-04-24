import type { Page } from '@playwright/test';
import { test, expect, waitForNodeReady } from './fixtures/tauri-bridge';

/**
 * Regression guard for the reactivity half of ZEB-148.
 *
 * The bug: saving in ProfileEditor updated localStorage but the in-memory
 * `myProfile` $state in App.svelte did not re-render downstream consumers
 * (NavPanel, MailCompose, etc.), so send paths used a stale displayName.
 * PR #43's fix moved display-name setters into `$effect` hooks.
 *
 * At the E2E level we prove the fix by two signals:
 *   1. **Persistence** — localStorage reflects the new displayName after Save.
 *   2. **Reactivity** — closing and reopening the settings panel re-mounts
 *      ProfileEditor, which initializes its input from the `profile` prop
 *      (see `ProfileEditor.svelte` line 17: `untrack(() => profile.displayName)`).
 *      If the parent's `myProfile` state hadn't updated, the input would
 *      repopulate with the pre-save value. A match confirms the prop
 *      propagated.
 */

const SETTINGS_BUTTON = 'Notification settings';
const NAME_INPUT = 'Display name';
const SAVE_BUTTON = 'Save';
const PROFILE_KEY = 'harmony-profile';
const TEST_NAME = `E2E Test ${Date.now().toString(36)}`;

async function openSettings(page: Page): Promise<void> {
  const nameInput = page.getByLabel(NAME_INPUT);
  if (await nameInput.isVisible().catch(() => false)) return;
  await page.getByRole('button', { name: SETTINGS_BUTTON }).click();
  await expect(nameInput).toBeVisible({ timeout: 5_000 });
}

async function closeSettings(page: Page): Promise<void> {
  const nameInput = page.getByLabel(NAME_INPUT);
  if (!(await nameInput.isVisible().catch(() => false))) return;
  await page.getByRole('button', { name: SETTINGS_BUTTON }).click();
  await expect(nameInput).not.toBeVisible({ timeout: 5_000 });
}

/**
 * Read the persisted displayName, defensively: returns `null` if storage is
 * empty, if the entry is not valid JSON, or if the parsed value's displayName
 * isn't a string. Callers must not `.fill()` a non-string into the input.
 */
async function readPersistedDisplayName(page: Page): Promise<string | null> {
  return page.evaluate((key: string): string | null => {
    const raw = localStorage.getItem(key);
    if (!raw) return null;
    try {
      const parsed = JSON.parse(raw) as { displayName?: unknown };
      return typeof parsed.displayName === 'string' ? parsed.displayName : null;
    } catch {
      return null;
    }
  }, PROFILE_KEY);
}

/**
 * Snapshot the raw `harmony-profile` localStorage value (or `null` if absent).
 * We deliberately preserve the *raw* string — not just the parsed displayName
 * — so cleanup can put back the exact pre-test state, including the original
 * absence (vs. a default-shaped profile), original malformation if any, and
 * any sibling fields beyond `displayName`. Restoring a synthesized
 * `{displayName: "Anonymous"}` would mask first-run / malformed-profile
 * behavior for later specs in the same `tauri dev` session.
 */
async function snapshotProfileEntry(page: Page): Promise<string | null> {
  return page.evaluate((key: string): string | null => localStorage.getItem(key), PROFILE_KEY);
}

/**
 * Restore the `harmony-profile` entry to whatever was captured by
 * `snapshotProfileEntry`. Bypasses the UI (which only knows how to write
 * displayName) so we can faithfully restore absence and other fields.
 */
async function restoreProfileEntry(page: Page, raw: string | null): Promise<void> {
  await page.evaluate(
    ({ key, raw }) => {
      if (raw === null) localStorage.removeItem(key);
      else localStorage.setItem(key, raw);
    },
    { key: PROFILE_KEY, raw },
  );
}

test.describe('profile sync', () => {
  test('displayName persists and re-hydrates on settings reopen [ZEB-148]', async ({
    mainPage,
  }) => {
    await waitForNodeReady(mainPage);

    const originalRaw = await snapshotProfileEntry(mainPage);

    try {
      await openSettings(mainPage);

      const nameInput = mainPage.getByLabel(NAME_INPUT);
      await nameInput.fill(TEST_NAME);
      await mainPage.getByRole('button', { name: SAVE_BUTTON }).click();
      await expect(mainPage.getByText('Profile saved')).toBeVisible({ timeout: 3_000 });

      // Persistence signal.
      expect(await readPersistedDisplayName(mainPage)).toBe(TEST_NAME);

      // Reactivity signal: close+reopen. ProfileEditor untracks its initial
      // displayName from the `profile` prop, so a stale prop would show the
      // old name after remount.
      await closeSettings(mainPage);
      await openSettings(mainPage);
      await expect(mainPage.getByLabel(NAME_INPUT)).toHaveValue(TEST_NAME);
    } finally {
      // Restore the exact raw entry. Cleanup failure is signal, not noise: a
      // dirty `harmony-profile` key pollutes every subsequent run and every
      // later spec in this session. Errors are not swallowed.
      await restoreProfileEntry(mainPage, originalRaw);
      await expect
        .poll(() => snapshotProfileEntry(mainPage), { timeout: 5_000 })
        .toBe(originalRaw);
    }
  });
});
