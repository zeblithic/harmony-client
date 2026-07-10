/**
 * ZEB-650 slice 1 — derivable owner/identity meta facts.
 *
 * Kept OUT of DevicesPanel deliberately: its test file stubs the global
 * tauri `invoke` with ordered mockResolvedValueOnce chains, so any extra
 * component-level invoke call would consume stubs meant for later calls.
 * Mock this module as a unit there instead.
 */
import { invoke } from '@tauri-apps/api/core';
import type { CommunityNavDto } from './community-service';

/** Number of communities this owner has persisted rows for, or null when the
 *  IPC fails or returns a non-array — callers omit the fact, never render 0. */
export async function fetchCommunitiesCount(): Promise<number | null> {
  try {
    const rows = await invoke<CommunityNavDto[]>('list_owner_communities', {});
    return Array.isArray(rows) ? rows.length : null;
  } catch (e) {
    console.debug('[zeb-650] communities count fetch failed:', e instanceof Error ? e.message : String(e));
    return null;
  }
}
