import type { StartNodeResponse } from './types/onboarding';

/**
 * ZEB-338 / PR #169 — backend-authoritative owner-identity gate state.
 *
 * Distinguishing these four is load-bearing. The original implementation
 * collapsed them into a boolean (`startResp?.hasOwnerIdentity === true`), which
 * made `start_node` *failure* indistinguishable from "no owner identity". That
 * dumped returning users into the non-dismissible WelcomeModal — and
 * `mint_owner_identity` refuses to re-mint when `owner_state.cbor` already
 * exists, so they were permanently trapped (the exact deadlock class this
 * feature exists to remove).
 *
 * - `unknown` — boot in flight; `start_node` has not resolved yet.
 * - `present` — `start_node` succeeded AND reports an owner identity.
 * - `missing` — `start_node` succeeded AND explicitly reports no owner identity
 *               (the genuine first-run case → show WelcomeModal hard gate).
 * - `error`   — `start_node` threw; we must NOT assume "no identity" and must
 *               NOT show the mint gate. Surface a retry instead.
 * - `revoked` — `start_node` succeeded but this device's own enrollment is
 *               revoked (ZEB-668 S2). `hasOwnerIdentity` is false here, so
 *               this MUST be classified before `missing` — the mint gate
 *               refuses while owner_state.cbor exists, which would trap the
 *               user. Renders the terminal "removed from your account" screen.
 * - `enrollment-missing` — `start_node` succeeded but the loaded device key is
 *               not enrolled in the persisted owner_state (ZEB-836 desync).
 *               `hasOwnerIdentity` is false; like `revoked` it MUST be
 *               classified before `missing` to avoid the mint-gate trap.
 *               Renders a recovery screen ("your other devices are safe") with
 *               restore/reset actions.
 */
export type OwnerIdentityState =
  | 'unknown'
  | 'present'
  | 'missing'
  | 'error'
  | 'revoked'
  | 'enrollment-missing';

/**
 * Classify the owner-identity gate from a `start_node` outcome.
 *
 * @param resp   the resolved {@link StartNodeResponse}, or null if it threw
 * @param failed true iff the `start_node` invoke rejected
 *
 * Forward-compat: an older backend that omits `hasOwnerIdentity` classifies as
 * `missing` (undefined is not `=== true`), so onboarding is shown rather than
 * silently skipped.
 */
export function classifyOwnerIdentity(
  resp: StartNodeResponse | null,
  failed: boolean,
): OwnerIdentityState {
  if (failed || resp == null) return 'error';
  if (resp.selfRevoked === true) return 'revoked';
  if (resp.selfEnrollmentMissing === true) return 'enrollment-missing';
  return resp.hasOwnerIdentity === true ? 'present' : 'missing';
}
