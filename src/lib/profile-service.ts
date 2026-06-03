import type { Profile } from './types';

const STORAGE_KEY = 'harmony-profile';

/** Generate a random 16-byte hex address (placeholder until real key management). */
function generateRandomAddress(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** Load the local user's profile from localStorage, or create a new one with
 *  a unique random address.
 *
 *  `displayName` is the OWNER's canonical name — it is broadcast owner-keyed by
 *  profile_card_broadcast and resolved by peers per ownerIdHex. It is NOT a
 *  per-device label (see device-label-service for that, ZEB-336).
 *
 *  `address` is a local placeholder id (random 16 bytes), NOT the owner
 *  identity (`owner_id`) and NOT a device key. It predates real key management
 *  and carries no identity-bearing role; treat it as a legacy local handle.
 *  The address is generated once on first launch and persisted — all
 *  subsequent loads return the same address. */
export function loadProfile(): Profile {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored) {
      const parsed = JSON.parse(stored);
      const safe: Record<string, unknown> = {};
      for (const [key, value] of Object.entries(parsed)) {
        if (value != null) {
          safe[key] = value;
        }
      }
      const profile = { displayName: 'Anonymous', ...safe } as Profile;
      // Migrate legacy 'local' address to a unique one
      if (!profile.address || profile.address === 'local') {
        profile.address = generateRandomAddress();
        saveProfile(profile);
      }
      return profile;
    }
  } catch {
    // Corrupt or missing — create fresh
  }
  // First launch: generate unique address and persist
  const profile: Profile = {
    address: generateRandomAddress(),
    displayName: 'Anonymous',
  };
  saveProfile(profile);
  return profile;
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
