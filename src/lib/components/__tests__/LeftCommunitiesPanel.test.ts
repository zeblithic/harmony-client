import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import LeftCommunitiesPanel from '../LeftCommunitiesPanel.svelte';
import type { LeftCommunityNavDto } from '../../community-service';
// ZEB-946: the "Left …" date honors the owner's date-order preference.
import {
  setTimeFormatSettings,
  _resetTimeFormatServiceForTest,
} from '../../time-format-service';
import { formatDateOnly } from '../../time-format';

const ROW: LeftCommunityNavDto = {
  spaceId: 'aa'.repeat(16),
  name: 'Old Crew',
  leftAtMs: 1_700_000_000_000,
};

/** Sequenced mock: each listLeftCommunities call returns the next snapshot
 *  (last one repeats), so a post-delete re-fetch can observe the row gone. */
function makeService(rowsSeq: LeftCommunityNavDto[][], removeImpl?: () => Promise<void>) {
  let call = 0;
  return {
    listLeftCommunities: vi.fn(async () => rowsSeq[Math.min(call++, rowsSeq.length - 1)]),
    removeSpace: vi.fn(removeImpl ?? (async () => {})),
  };
}

describe('LeftCommunitiesPanel (ZEB-435)', () => {
  it('shows the empty state when no communities are left', async () => {
    const service = makeService([[]]);
    const { findByText } = render(LeftCommunitiesPanel, { props: { service } });
    await findByText(/No left communities/i);
    expect(service.listLeftCommunities).toHaveBeenCalledTimes(1);
  });

  it('lists left communities with a Delete forever action', async () => {
    const service = makeService([[ROW]]);
    const { findByText, getByText } = render(LeftCommunitiesPanel, { props: { service } });
    await findByText('Old Crew');
    expect(getByText('Delete forever…')).toBeTruthy();
  });

  it('delete forever is typed-confirm: exact name required, then removes and re-fetches', async () => {
    const service = makeService([[ROW], []]);
    const { findByText, getByText, getByPlaceholderText, queryByText } = render(
      LeftCommunitiesPanel,
      { props: { service } },
    );
    await findByText('Old Crew');
    await fireEvent.click(getByText('Delete forever…'));

    // Tier-3 typed confirmation (TypedConfirmationModal): confirm starts
    // disabled, enables only on the exact community name.
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    const confirm = getByText('Delete forever').closest('button') as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);
    await fireEvent.input(input, { target: { value: 'old crew' } });
    expect(confirm.disabled).toBe(true);
    await fireEvent.input(input, { target: { value: 'Old Crew' } });
    expect(confirm.disabled).toBe(false);

    await fireEvent.click(confirm);
    await waitFor(() => {
      expect(service.removeSpace).toHaveBeenCalledWith(ROW.spaceId);
      expect(queryByText('Old Crew')).toBeNull();
    });
    await findByText(/No left communities/i);
    expect(service.listLeftCommunities).toHaveBeenCalledTimes(2);
  });

  it('surfaces a backend refusal and keeps the row', async () => {
    const service = makeService([[ROW], [ROW]], async () => {
      throw new Error('community aa… has not been left — call leave_community before remove_space');
    });
    const { findByText, getByText, getByPlaceholderText } = render(LeftCommunitiesPanel, {
      props: { service },
    });
    await findByText('Old Crew');
    await fireEvent.click(getByText('Delete forever…'));
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'Old Crew' } });
    await fireEvent.click(getByText('Delete forever').closest('button') as HTMLButtonElement);

    await findByText(/has not been left/i);
    await findByText('Old Crew');
  });

  it('defers the first load until the tab is activated', async () => {
    const service = makeService([[]]);
    const { findByText, rerender } = render(LeftCommunitiesPanel, {
      props: { service, active: false },
    });
    // Mounted hidden (SettingsPanel keeps all tabs mounted): no IPC yet.
    await Promise.resolve();
    expect(service.listLeftCommunities).not.toHaveBeenCalled();

    await rerender({ active: true });
    await findByText(/No left communities/i);
    expect(service.listLeftCommunities).toHaveBeenCalledTimes(1);
  });

  it('drops the deleted row locally even when the refresh fails', async () => {
    let call = 0;
    const service = {
      listLeftCommunities: vi.fn(async () => {
        call += 1;
        if (call === 1) return [ROW];
        throw new Error('adapter went away');
      }),
      removeSpace: vi.fn(async () => {}),
    };
    const { findByText, getByText, getByPlaceholderText, queryByText } = render(
      LeftCommunitiesPanel,
      { props: { service } },
    );
    await findByText('Old Crew');
    await fireEvent.click(getByText('Delete forever…'));
    const input = getByPlaceholderText('Type community name exactly...') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'Old Crew' } });
    await fireEvent.click(getByText('Delete forever').closest('button') as HTMLButtonElement);

    // The tombstoned community must not linger looking undeleted just
    // because the reconciling re-fetch failed.
    await waitFor(() => {
      expect(service.removeSpace).toHaveBeenCalledWith(ROW.spaceId);
      expect(queryByText('Old Crew')).toBeNull();
    });
    await findByText(/adapter went away/i);
  });

  it('retries a failed load when the tab is re-activated', async () => {
    let call = 0;
    const service = {
      listLeftCommunities: vi.fn(async () => {
        call += 1;
        if (call === 1) throw new Error('adapter not connected');
        return [];
      }),
      removeSpace: vi.fn(async () => {}),
    };
    const { findByText, rerender } = render(LeftCommunitiesPanel, {
      props: { service, active: true },
    });
    await findByText(/adapter not connected/i);

    // Leaving and re-entering the tab is the retry edge.
    await rerender({ active: false });
    await rerender({ active: true });
    await findByText(/No left communities/i);
    expect(service.listLeftCommunities).toHaveBeenCalledTimes(2);
  });
});

describe('LeftCommunitiesPanel left-at date honors the preference (ZEB-946)', () => {
  afterEach(() => {
    _resetTimeFormatServiceForTest();
  });

  it('renders the left-at date in the chosen order', async () => {
    setTimeFormatSettings({ clock: 'system', dateOrder: 'ymd' });
    const service = makeService([[ROW]]);
    const { container, findByText } = render(LeftCommunitiesPanel, { props: { service } });
    await findByText('Old Crew');
    expect(container.querySelector('.left-at')?.textContent).toBe(
      `Left ${formatDateOnly(ROW.leftAtMs, { dateOrder: 'ymd' })}`,
    );
  });
});
