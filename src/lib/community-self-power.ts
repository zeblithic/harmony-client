import type { CommunityMember } from './types';

/**
 * ZEB-396: resolve the viewer's own power level within a community.
 *
 * The community roster (`listCommunityMembers` → MemberInfoDto.addr) is keyed by
 * **owner_id** (32-char lowercase hex of the OwnerAddr). The viewer's self-identity
 * in the community world is therefore `selfOwnerId` (from get_owner_state), NOT the
 * node/transport address (`get_node_addr`). Matching against the node address — the
 * prior bug — never resolved, so the owner's power fell through to 0 and every
 * moderation affordance (create/rename/delete channel, kick, set-power) was hidden.
 *
 * Returns 0 when owner identity hasn't loaded yet (`selfOwnerId === null`) or when
 * the viewer has no materialized roster entry.
 */
export function selfCommunityPower(
  members: Pick<CommunityMember, 'address' | 'power'>[],
  selfOwnerId: string | null,
): number {
  if (!selfOwnerId) return 0;
  return members.find((m) => m.address === selfOwnerId)?.power ?? 0;
}
