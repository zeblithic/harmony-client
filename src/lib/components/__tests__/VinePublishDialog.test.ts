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
    expect(screen.getByRole('button', { name: 'Publish vine' })).toBeDisabled();
  });

  it('choosing a video ingests it, fills the CID, shows the filename, and enables Publish', async () => {
    const onPickVideo = vi.fn().mockResolvedValue({ cid: 'abc123cid', fileName: 'clip.mp4' });
    render(VinePublishDialog, props({ onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByTestId('picked-file')).toBeTruthy());
    expect(screen.getByTestId('picked-file')).toHaveTextContent('clip.mp4');
    expect(onPickVideo).toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Publish vine' })).not.toBeDisabled();
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
    await fireEvent.click(screen.getByRole('button', { name: 'Publish vine' }));
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('abc123cid', 'My clip'));
    expect(onClose).toHaveBeenCalled();
  });

  it('cancelling the picker (null) leaves the composer unchanged', async () => {
    const onPickVideo = vi.fn().mockResolvedValue(null);
    render(VinePublishDialog, props({ onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(onPickVideo).toHaveBeenCalled());
    expect(screen.queryByTestId('picked-file')).toBeNull();
    expect(screen.getByRole('button', { name: 'Publish vine' })).toBeDisabled();
  });

  it('surfaces an ingest error from onPickVideo without filling the CID', async () => {
    const onPickVideo = vi.fn().mockRejectedValue(new Error('video too large: exceeds the 100 MB limit'));
    render(VinePublishDialog, props({ onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(/too large/i));
    expect(screen.getByRole('button', { name: 'Publish vine' })).toBeDisabled();
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
    const publish = screen.getByRole('button', { name: 'Publish vine' });
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
    const publish = screen.getByRole('button', { name: 'Publish vine' });
    await waitFor(() => expect(publish).not.toBeDisabled());
    await fireEvent.click(publish);
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('manualcidhex', undefined));
  });
});

describe('honest ≤6s gate (ZEB-612 S2)', () => {
  const gateProps = (durations: Record<string, number | Error>, over: Record<string, unknown> = {}) =>
    props({
      onPickVideo: vi.fn().mockResolvedValue({ cid: 'cid-x', fileName: 'clip.mp4' }),
      resolveVideo: vi.fn(async (cid: string) => `blob:for-${cid}`),
      probeDuration: vi.fn(async (url: string) => {
        const d = durations[url];
        if (d instanceof Error) throw d;
        return d as number;
      }),
      ...over,
    });

  it('a ≤6s pick shows the honest picked line and enables Publish', async () => {
    render(VinePublishDialog, gateProps({ 'blob:for-cid-x': 5.8 }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() =>
      expect(screen.getByTestId('picked-file')).toHaveTextContent('clip.mp4 · 5.8s ✓ · ingested to content store'));
    expect(screen.getByRole('button', { name: 'Publish vine' })).not.toBeDisabled();
  });

  it('a >6s pick blocks with the exact trim copy and disables Publish', async () => {
    render(VinePublishDialog, gateProps({ 'blob:for-cid-x': 9.3 }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(
      'This clip is 9.3s — vines are 6 seconds or less. Trim it and re-ingest.'));
    expect(screen.getByRole('button', { name: 'Publish vine' })).toBeDisabled();
  });

  it('replacing an over-long clip with a short one clears the block', async () => {
    const onPickVideo = vi.fn()
      .mockResolvedValueOnce({ cid: 'cid-long', fileName: 'long.mp4' })
      .mockResolvedValueOnce({ cid: 'cid-short', fileName: 'short.mp4' });
    render(VinePublishDialog, gateProps(
      { 'blob:for-cid-long': 9.3, 'blob:for-cid-short': 4.0 }, { onPickVideo }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy());
    await fireEvent.click(screen.getByRole('button', { name: 'Replace clip' }));
    await waitFor(() =>
      expect(screen.getByTestId('picked-file')).toHaveTextContent('short.mp4 · 4.0s ✓'));
    expect(screen.getByRole('button', { name: 'Publish vine' })).not.toBeDisabled();
  });

  it('gates a pasted Advanced CID at submit (>6s → onPublish NOT called)', async () => {
    const onPublish = vi.fn();
    render(VinePublishDialog, gateProps({ 'blob:for-pastedcid': 7.7 }, { onPublish, onPickVideo: undefined }));
    await fireEvent.input(screen.getByLabelText('Video CID'), { target: { value: 'pastedcid' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Publish vine' }));
    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent(/7\.7s/));
    expect(onPublish).not.toHaveBeenCalled();
  });

  it('fails OPEN when the probe errors (honesty courtesy, not security)', async () => {
    const onPublish = vi.fn().mockResolvedValue(undefined);
    render(VinePublishDialog, gateProps(
      { 'blob:for-cid-x': new Error('undecodable') }, { onPublish }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByTestId('picked-file')).toHaveTextContent('clip.mp4'));
    expect(screen.getByTestId('picked-file')).not.toHaveTextContent('✓');
    await fireEvent.click(screen.getByRole('button', { name: 'Publish vine' }));
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('cid-x', undefined));
  });

  it('editing the Advanced CID clears a stale measured duration', async () => {
    const onPublish = vi.fn().mockResolvedValue(undefined);
    render(VinePublishDialog, gateProps(
      { 'blob:for-cid-x': 9.3, 'blob:for-othercid': 3.0 }, { onPublish }));
    await fireEvent.click(screen.getByTestId('choose-video'));
    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy());
    await fireEvent.input(screen.getByLabelText('Video CID'), { target: { value: 'othercid' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Publish vine' }));
    await waitFor(() => expect(onPublish).toHaveBeenCalledWith('othercid', undefined));
  });
});

describe('Commons copy (ZEB-612 S2)', () => {
  it('renders the header, subtitle, and the true-claims-only sovereign note', () => {
    render(VinePublishDialog, props());
    expect(screen.getByRole('dialog', { name: 'Share a vine' })).toBeTruthy();
    expect(screen.getByText('≤ 6 seconds · loops forever')).toBeTruthy();
    expect(screen.getByText(
      "Publishes to your sovereign identity and replicates peer-to-peer. There's no central server to take it down.")).toBeTruthy();
    // The delete claim stays out until ZEB-670 ships the verb.
    expect(screen.queryByText(/only you can delete it/)).toBeNull();
  });

  it('labels the text field Caption', () => {
    render(VinePublishDialog, props());
    expect(screen.getByText('Caption')).toBeTruthy();
  });
});
