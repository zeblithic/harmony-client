import type { Profile } from './types';

/** Base name for the local-profile cache. The persisted key is ALWAYS
 *  owner-scoped — `harmony-profile:owner-<ownerId>` — so that a machine which
 *  hosts more than one identity (recreate-in-place, a second profile, ZEB-173
 *  fresh-identity-on-loss) never bleeds one identity's name into another.
 *
 *  WebView localStorage is keyed by bundle id, NOT by `HARMONY_PROFILE`, so a
 *  fixed owner-agnostic key would be shared across every identity on the box —
 *  that was the ZEB-586 leak. Do NOT reintroduce a fixed/global key here. */
const KEY_PREFIX = 'harmony-profile';

function ownerKey(ownerId: string): string {
  return `${KEY_PREFIX}:owner-${ownerId}`;
}

/** Generate a random 16-byte hex address (placeholder until real key management). */
function generateRandomAddress(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

function freshProfile(): Profile {
  return { address: generateRandomAddress(), displayName: 'Anonymous' };
}

/** Load the local user's profile for `ownerId` from localStorage, or create a
 *  new one with a unique random address.
 *
 *  `displayName` is the OWNER's canonical name — it is broadcast owner-keyed by
 *  profile_card_broadcast and resolved by peers per ownerIdHex. The backend
 *  does NOT persist it (`get_owner_state` returns a `"this device"` placeholder
 *  and `publish_profile` forwards the name straight to the broadcast), so this
 *  owner-scoped localStorage entry is its only canonical home. It is NOT a
 *  per-device label (see device-label-service for that, ZEB-336).
 *
 *  `address` is a local placeholder id (random 16 bytes), NOT the owner
 *  identity (`owner_id`) and NOT a device key. It predates real key management
 *  and carries no identity-bearing role; treat it as a legacy local handle.
 *  The address is generated once per owner on first load and persisted — all
 *  subsequent loads for that owner return the same address.
 *
 *  When `ownerId` is omitted (the brief pre-identity window before
 *  `get_owner_state` resolves), an ephemeral default is returned and NOTHING is
 *  persisted — there is no owner to scope to, and reading a shared key would be
 *  the ZEB-586 leak. Callers re-load with the real `ownerId` once it is known. */
export function loadProfile(ownerId?: string): Profile {
  if (!ownerId) {
    // No identity yet: hand back a throwaway default, persist nothing.
    return freshProfile();
  }
  const key = ownerKey(ownerId);
  try {
    const stored = localStorage.getItem(key);
    if (stored) {
      const parsed = JSON.parse(stored);
      const safe: Record<string, unknown> = {};
      for (const [k, value] of Object.entries(parsed)) {
        if (value != null) {
          safe[k] = value;
        }
      }
      const profile = { displayName: 'Anonymous', ...safe } as Profile;
      // Migrate legacy 'local' / missing address to a unique one
      if (!profile.address || profile.address === 'local') {
        profile.address = generateRandomAddress();
        saveProfile(profile, ownerId);
      }
      return profile;
    }
  } catch {
    // Corrupt or missing — create fresh
  }
  // First load for this owner: generate unique address and persist
  const profile = freshProfile();
  saveProfile(profile, ownerId);
  return profile;
}

/** Save the local user's profile for `ownerId` to localStorage.
 *
 *  When `ownerId` is omitted there is no identity to scope to, so the save is a
 *  no-op (rather than writing a shared key and reintroducing the ZEB-586 leak).
 *  Real profile edits always run with an identity present. */
export function saveProfile(profile: Profile, ownerId?: string): void {
  if (!ownerId) {
    return;
  }
  try {
    localStorage.setItem(ownerKey(ownerId), JSON.stringify(profile));
  } catch {
    // localStorage may be unavailable (SSR, private browsing quota)
  }
}

// updateProfile was removed — unused in production code.
// Use loadProfile(ownerId) + spread + saveProfile(..., ownerId) at call sites instead.
