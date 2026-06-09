/** One-time, idempotent import of legacy localStorage notes into the Rust
 *  Notes dataset (ZEB-417). Keyed by owner id; guarded by a per-owner
 *  "migrated" flag so it runs at most once. The original localStorage key is
 *  left in place as a safety copy. `invokeFn` is injected for testability. */
export async function migrateLocalNotes(
  ownerId: string,
  invokeFn: (cmd: string, args: Record<string, unknown>) => Promise<unknown>,
): Promise<void> {
  if (!ownerId) return;
  const doneKey = `harmony-notes-migrated:${ownerId}`;
  if (localStorage.getItem(doneKey) === '1') return;
  let legacy: Array<{ id?: unknown; text?: unknown }> = [];
  try {
    const raw = localStorage.getItem(`harmony-notes:${ownerId}`);
    const parsed = raw ? JSON.parse(raw) : [];
    legacy = Array.isArray(parsed) ? parsed : [];
  } catch {
    legacy = [];
  }
  for (const e of legacy) {
    if (e && typeof e.text === 'string' && e.text.trim()) {
      // Pass the legacy entry's stable id so a crash mid-import re-imports the
      // SAME ids (LWW no-op) instead of minting fresh ULIDs and duplicating.
      const id = typeof e.id === 'string' && e.id ? e.id : undefined;
      await invokeFn('notes_upsert', { id, text: e.text });
    }
  }
  localStorage.setItem(doneKey, '1');
}
