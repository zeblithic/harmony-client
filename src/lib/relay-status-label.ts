import type { RelayHealth } from './types/network-health';

/**
 * ZEB-651 — shared pkarr relay-status label ('Healthy' / 'Cooling down (Ns)').
 * Single source used by both NetworkStatusPill consumers (NetworkHealthView's
 * pkarr list + NetworkDiscoverabilitySettings' relay manager) so the countdown
 * wording can't drift between the two components.
 */
export function relayStatusLabel(relay: RelayHealth, nowMs: number): string {
  if (relay.state.kind === 'healthy') return 'Healthy';
  const secsLeft = Math.max(0, Math.ceil((relay.state.untilMs - nowMs) / 1000));
  return `Cooling down (${secsLeft}s)`;
}
