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
      return { ...DEFAULT_PROFILE, ...parsed };
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

/** Update specific fields on the profile and save. */
export function updateProfile(updates: Partial<Profile>): Profile {
  const current = loadProfile();
  const updated = { ...current, ...updates };
  saveProfile(updated);
  return updated;
}
