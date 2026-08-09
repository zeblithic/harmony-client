// ZEB-885: the redeem-invite UI routes its copy off a STRUCTURED error CODE
// returned by the backend (`RedeemInviteError { code, message }`), not by
// regex-matching the raw message. The union below mirrors the Rust
// `RedeemInviteErrorCode` enum (snake_case serde) in
// `src-tauri/src/community_invite.rs` — keep the two in lockstep. The Rust
// test `code_as_str_matches_serde_repr` pins the wire strings.

export type RedeemInviteErrorCode =
  | 'bootstrap_missing'
  | 'bootstrap_actor_mismatch'
  | 'bootstrap_community_mismatch'
  | 'bootstrap_signature_invalid'
  | 'bootstrap_kind_invalid'
  | 'invite_url_malformed'
  | 'inviter_enrollment_invalid'
  | 'invite_expired'
  | 'invite_verify_failed'
  | 'invite_token_missing'
  | 'missing_admin_identity_pub'
  | 'inviter_unreachable'
  | 'relays_warming_up'
  | 'node_not_ready'
  | 'generation_changed'
  | 'engine_insert_failed'
  | 'join_failed'
  | 'internal'
  | 'unknown';

/**
 * Structured error emitted by the redeem IPC commands. Tauri serializes the
 * Rust `RedeemInviteError` to this exact shape in the command rejection.
 *
 * `code` is typed as the known union *plus* an open `string` so a code the
 * frontend hasn't learned yet (newer backend) is carried through unchanged for
 * display/bug-reports without an unsound cast — `redeemInviteCopy` falls back
 * to `unknown` for anything it doesn't recognize.
 */
export interface RedeemInviteError {
  code: RedeemInviteErrorCode | (string & {});
  message: string;
}

export interface RedeemInviteUserError {
  summary: string;
  hint: string;
  code: RedeemInviteErrorCode | (string & {});
  raw: string;
}

interface RedeemInviteCopy {
  summary: string;
  hint: string;
}

const COPY: Record<RedeemInviteErrorCode, RedeemInviteCopy> = {
  bootstrap_missing: {
    summary: 'Invite link is incomplete.',
    hint: 'Ask the inviter to regenerate the link from a recent client build.',
  },
  bootstrap_actor_mismatch: {
    summary: 'Invite link is malformed.',
    hint: 'Its bootstrap event was signed by someone other than the admin. Ask the inviter to regenerate.',
  },
  bootstrap_community_mismatch: {
    summary: 'Invite link points to a different community than it advertises.',
    hint: "The inviter's client may be corrupted. Ask them to reinstall and regenerate.",
  },
  bootstrap_signature_invalid: {
    summary: 'Invite link signature is invalid.',
    hint: "It may have been tampered with in transit, or the inviter's client is buggy. Ask them to regenerate via a different channel.",
  },
  bootstrap_kind_invalid: {
    summary: 'Invite link contains the wrong event type.',
    hint: 'Likely a malformed client. Ask the inviter to regenerate.',
  },
  invite_url_malformed: {
    summary: "That doesn't look like a Harmony invite link.",
    hint: 'Make sure you copied the full URL starting with harmony://invite/.',
  },
  inviter_enrollment_invalid: {
    summary: "Couldn't verify the inviter.",
    hint: "Their enrollment cert didn't check out. Ask them to regenerate the link.",
  },
  invite_expired: {
    summary: 'This invite has expired.',
    hint: 'Ask the inviter for a fresh link.',
  },
  invite_verify_failed: {
    summary: "Couldn't verify this invite.",
    hint: 'The invite failed verification — ask the inviter to regenerate it.',
  },
  invite_token_missing: {
    summary: 'Invite link is incomplete.',
    hint: "It's missing its single-use access token. Ask the inviter to regenerate it.",
  },
  missing_admin_identity_pub: {
    summary: "Couldn't verify the inviter over the network.",
    hint: 'Try the local-network fallback below to join.',
  },
  inviter_unreachable: {
    summary: 'Inviter is offline — try again later.',
    hint: "The community admin needs to be reachable when you redeem. Retry once they're back online.",
  },
  relays_warming_up: {
    // ZEB-879: the pkarr resolver returns this while its relay pool is still
    // cold (~1 min after launch, or after sleep/wake). The relays ARE
    // reachable — it is not a hard network outage.
    summary: 'The network is still warming up.',
    hint: 'Discovery relays warm up for about a minute after launch — try again shortly. If it keeps failing, the inviter may not be discoverable yet.',
  },
  node_not_ready: {
    summary: 'Your identity is still loading.',
    hint: 'Give it a moment after launch, then try redeeming again.',
  },
  generation_changed: {
    summary: 'The app reconfigured while redeeming.',
    hint: 'This can happen right after a restart. Just try redeeming again.',
  },
  engine_insert_failed: {
    summary: "Couldn't apply the invite on this device.",
    hint: 'Most likely transient — retry. See the diagnostic below.',
  },
  join_failed: {
    summary: "Reached the inviter, but couldn't finish joining locally.",
    hint: 'Try restarting the app and redeeming again; if it keeps failing, please report it as a bug.',
  },
  internal: {
    summary: 'Something went wrong redeeming the invite.',
    hint: 'This looks like a bug on our side. Please retry, and report it if it persists.',
  },
  unknown: {
    summary: "Couldn't complete the invite redemption.",
    hint: 'Check your connection and retry.',
  },
};

/** Every known code — derived from `COPY` so it can never drift from it. */
export const REDEEM_INVITE_ERROR_CODES = Object.keys(COPY) as RedeemInviteErrorCode[];

/**
 * User-facing copy for a code, falling back to `unknown` for any code the
 * frontend hasn't learned yet (forward-compat with a newer backend).
 *
 * Uses `Object.hasOwn` rather than a bare index/`in` so an inherited
 * `Object.prototype` key (`'constructor'`, `'toString'`, …) arriving as a code
 * can't return a truthy non-copy value and blank the banner.
 */
export function redeemInviteCopy(code: string): RedeemInviteCopy {
  return Object.hasOwn(COPY, code)
    ? COPY[code as RedeemInviteErrorCode]
    : COPY.unknown;
}

/**
 * Normalize an arbitrary caught IPC rejection into a structured error. The
 * redeem commands reject with the serialized `{ code, message }` object; tests
 * and any un-migrated throw site surface an `Error`/string, which degrade to
 * the `unknown` code. The original `code` string is preserved even when it is
 * not a known variant (so the disclosure/bug-report still shows it).
 */
export function toRedeemInviteError(e: unknown): RedeemInviteError {
  if (e && typeof e === 'object' && 'code' in e && typeof (e as { code: unknown }).code === 'string') {
    const obj = e as { code: string; message?: unknown };
    return {
      code: obj.code,
      message: typeof obj.message === 'string' ? obj.message : String(e),
    };
  }
  const message = e instanceof Error ? e.message : String(e);
  return { code: 'unknown', message };
}

export function mapRedeemInviteError(err: RedeemInviteError): RedeemInviteUserError {
  const copy = redeemInviteCopy(err.code);
  return { summary: copy.summary, hint: copy.hint, code: err.code, raw: err.message };
}
