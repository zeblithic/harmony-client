import type { Profile } from './types';

const STORAGE_KEY = 'harmony-profile';

const DEFAULT_PROFILE: Profile = {
  address: 'local',
  displayName: 'Anonymous',
};

/** Load the local user's profile from localStorage, or return defaults. */
export function loadProfile(): Profile {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored) {
      const parsed = JSON.parse(stored);
      // Filter out null/undefined values so they don't override defaults.
      // e.g. { "address": null } in stored JSON shouldn't bypass the
      // required string fields from DEFAULT_PROFILE.
      const safe: Record<string, unknown> = {};
      for (const [key, value] of Object.entries(parsed)) {
        if (value != null) {
          safe[key] = value;
        }
      }
      return { ...DEFAULT_PROFILE, ...safe };
    }
  } catch {
    // Corrupt or missing — return defaults
  }
  return { ...DEFAULT_PROFILE };
}

/** Save the local user's profile to localStorage. */
export function saveProfile(profile: Profile): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(profile));
  } catch {
    // localStorage may be unavailable (SSR, private browsing quota)
  }
}

// updateProfile was removed — unused in production code.
// Use loadProfile() + spread + saveProfile() at call sites instead.
}
