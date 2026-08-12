import { describe, it, expect } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import FleetSyncDisabledBanner from '../FleetSyncDisabledBanner.svelte';

// ZEB-904/905: local-only-mode banner — visibility rides entirely on the
// `fleetCryptoMissing` start_node flag plus a session-only dismiss.
describe('FleetSyncDisabledBanner', () => {
  it('mounts when fleetCryptoMissing is true', () => {
    const { queryByTestId } = render(FleetSyncDisabledBanner, {
      props: { fleetCryptoMissing: true },
    });
    expect(queryByTestId('fleet-sync-disabled-banner')).toBeTruthy();
  });

  it('does not mount when fleetCryptoMissing is false', () => {
    const { queryByTestId } = render(FleetSyncDisabledBanner, {
      props: { fleetCryptoMissing: false },
    });
    expect(queryByTestId('fleet-sync-disabled-banner')).toBeNull();
  });

  it('names the restore path (Account → Devices) in the hint', () => {
    const { getByTestId } = render(FleetSyncDisabledBanner, {
      props: { fleetCryptoMissing: true },
    });
    expect(getByTestId('fleet-sync-disabled-hint').textContent).toContain('recovery phrase');
    expect(getByTestId('fleet-sync-disabled-hint').textContent).toContain('Devices');
  });

  it('dismiss hides the banner for the session', async () => {
    const { queryByTestId, getByTestId } = render(FleetSyncDisabledBanner, {
      props: { fleetCryptoMissing: true },
    });
    await fireEvent.click(getByTestId('fleet-sync-disabled-dismiss'));
    expect(queryByTestId('fleet-sync-disabled-banner')).toBeNull();
  });
});
