export interface RedeemInviteUserError {
  summary: string;
  hint: string;
  tag: string;
  raw: string;
}

const VARIANT_PATTERNS: Array<{
  match: RegExp;
  summary: string;
  hint: string;
  tag: string;
}> = [
  {
    match: /BootstrapMissing/i,
    summary: 'Invite link is incomplete.',
    hint: 'Ask the inviter to regenerate the link from a recent client build.',
    tag: 'bootstrap_missing',
  },
  {
    match: /BootstrapInvalidPubkey/i,
    summary: 'Invite link is malformed.',
    hint: "The embedded admin key isn't valid. Ask the inviter to regenerate.",
    tag: 'bootstrap_invalid_pubkey',
  },
  {
    match: /BootstrapAddressMismatch/i,
    summary: 'Invite link is malformed.',
    hint: "Embedded admin keys don't agree with each other. Ask the inviter to regenerate.",
    tag: 'bootstrap_address_mismatch',
  },
  {
    match: /BootstrapActorMismatch/i,
    summary: 'Invite link is malformed.',
    hint: 'Bootstrap event was signed by someone other than the admin. Ask the inviter to regenerate.',
    tag: 'bootstrap_actor_mismatch',
  },
  {
    match: /BootstrapCommunityMismatch/i,
    summary: 'Invite link points to a different community than the one it advertises.',
    hint: 'The inviter may have a corrupted client. Ask them to reinstall and regenerate.',
    tag: 'bootstrap_community_mismatch',
  },
  {
    match: /BootstrapSignatureInvalid/i,
    summary: 'Invite link signature is invalid.',
    hint: "Either the link was tampered with in transit, or the inviter's client is buggy. Ask the inviter to regenerate via a different channel.",
    tag: 'bootstrap_signature_invalid',
  },
  {
    match: /BootstrapKindInvalid/i,
    summary: 'Invite link contains the wrong event type.',
    hint: 'Likely a malformed client. Ask the inviter to regenerate.',
    tag: 'bootstrap_kind_invalid',
  },
  {
    match: /BootstrapInsertFailed/i,
    summary: "Couldn't bootstrap the community on this device.",
    hint: 'Most likely transient — retry. See diagnostic below.',
    tag: 'bootstrap_insert_failed',
  },
  {
    match: /timed out|timeout/i,
    summary: 'Inviter is offline — try again later.',
    hint: "The community admin needs to be reachable when you redeem. Retry once they're back online.",
    tag: 'inviter_timeout',
  },
  {
    match: /already a member/i,
    summary: "You're already in this community.",
    hint: '',
    tag: 'already_member',
  },
  {
    match: /invalid invite URL|missing scheme|parse fail/i,
    summary: "That URL doesn't look like a Harmony invite.",
    hint: 'Make sure you copied the full URL starting with harmony://invite/.',
    tag: 'malformed_url',
  },
  {
    // ZEB-879: the pkarr resolver returns this while its relay pool is still
    // cold (~1 min after launch, or after sleep/wake). The relays ARE reachable
    // — surfacing the raw string reads as a hard network outage, which it isn't.
    match: /no relays available/i,
    summary: 'The network is still warming up.',
    hint: 'Discovery relays warm up for about a minute after launch — try again shortly. If it keeps failing, the inviter may not be discoverable yet.',
    tag: 'relays_warming_up',
  },
];

const FALLBACK = {
  summary: "Couldn't reach the network.",
  hint: 'Check your connection and retry.',
  tag: 'network_failure',
};

export function mapRedeemInviteError(raw: string): RedeemInviteUserError {
  for (const p of VARIANT_PATTERNS) {
    if (p.match.test(raw)) {
      return { summary: p.summary, hint: p.hint, tag: p.tag, raw };
    }
  }
  return { ...FALLBACK, raw };
}
