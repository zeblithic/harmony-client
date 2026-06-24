import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import VinePublishDialog from '../VinePublishDialog.svelte';

function props(over: Record<string, unknown> = {}) {
  return {
    onPublish: vi.fn(),
    onClose: vi.fn(),
    onPickVideo: vi.fn(),
    ...over,
  };
}

describe('VinePublishDialog (ZEB-559)', () => {
  it('shows the file picker as the primary affordance, with the raw CID demoted under Advanced', () => {
    render(VinePublishDialog, props());
    expect(screen.getByTestId('choose-video')).toBeTruthy();
    expect(screen.getByText('Advanced: paste a Video CID')).toBeTruthy();
  });

  it('Publish is disabled until a video CID is present', () => {
    render(VinePublishDialog, props());
    expect(screen.getByRole('button', { name: 'Publish' })).toBeDisabled();
  });

  it('choosing a video ingests it, fills the CID, shows the filename, and enables Publish', async () => {
    const onPickVideo = vi.fn().mockResolvedValue({ cid: 'abc123cid', fileName: 'clip.mp4' });
    render(VinePublishDialog, props({ onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByTestId('picked-file')).toBeTruthy());
    expect(screen.getByTestId('picked-file')).toHaveTextContent('clip.mp4');
    expect(onPickVideo).toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Publish' })).not.toBeDisabled();
  });

  it('publishing forwards the minted CID + title to onPublish and closes', async () => {
    const onPickVideo = vi.fn().mockResolvedValue({ cid: 'abc123cid', fileName: 'clip.mp4' });
    const onPublish = vi.fn().mockResolvedValue(undefined);
    const onClose = vi.fn();
    render(VinePublishDialog, props({ onPickVideo, onPublish, onClose }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByTestId('picked-file')).toBeTruthy());
    await fireEvent.input(screen.getByPlaceholderText('Short description'), {
      target: { value: 'My clip' },
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Publish' }));
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('abc123cid', 'My clip'));
    expect(onClose).toHaveBeenCalled();
  });

  it('cancelling the picker (null) leaves the composer unchanged', async () => {
    const onPickVideo = vi.fn().mockResolvedValue(null);
    render(VinePublishDialog, props({ onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(onPickVideo).toHaveBeenCalled());
    expect(screen.queryByTestId('picked-file')).toBeNull();
    expect(screen.getByRole('button', { name: 'Publish' })).toBeDisabled();
  });

  it('surfaces an ingest error from onPickVideo without filling the CID', async () => {
    const onPickVideo = vi.fn().mockRejectedValue(new Error('video too large: exceeds the 100 MB limit'));
    render(VinePublishDialog, props({ onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(/too large/i));
    expect(screen.getByRole('button', { name: 'Publish' })).toBeDisabled();
  });

  it('manually editing the Advanced CID clears the picked filename (no desync)', async () => {
    const onPickVideo = vi.fn().mockResolvedValue({ cid: 'pickedcid', fileName: 'clip.mp4' });
    render(VinePublishDialog, props({ onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByTestId('picked-file')).toBeTruthy());
    // Override the CID via the Advanced field — the filename chip must clear so
    // the UI can't display clip.mp4 while publishing a different CID.
    await fireEvent.input(screen.getByLabelText('Video CID'), { target: { value: 'manualcid' } });
    await waitFor(() => expect(screen.queryByTestId('picked-file')).toBeNull());
    expect(screen.getByTestId('choose-video')).toBeTruthy();
  });

  it('without a native picker (onPickVideo undefined) shows manual CID as the primary control, no dead CTA', async () => {
    const onPublish = vi.fn().mockResolvedValue(undefined);
    render(VinePublishDialog, props({ onPickVideo: undefined, onPublish }));
    // No picker CTA in the fallback mode — it would be a no-op.
    expect(screen.queryByTestId('choose-video')).toBeNull();
    // Manual CID is the primary, usable control.
    await fireEvent.input(screen.getByLabelText('Video CID'), { target: { value: 'fallbackcid' } });
    const publish = screen.getByRole('button', { name: 'Publish' });
    await waitFor(() => expect(publish).not.toBeDisabled());
    await fireEvent.click(publish);
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('fallbackcid', undefined));
  });

  it('Advanced: pasting a raw CID also enables Publish (headless-parity path)', async () => {
    const onPublish = vi.fn().mockResolvedValue(undefined);
    render(VinePublishDialog, props({ onPublish }));
    await fireEvent.input(screen.getByLabelText('Video CID'), {
      target: { value: 'manualcidhex' },
    });
    const publish = screen.getByRole('button', { name: 'Publish' });
    await waitFor(() => expect(publish).not.toBeDisabled());
    await fireEvent.click(publish);
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('manualcidhex', undefined));
  });
});
