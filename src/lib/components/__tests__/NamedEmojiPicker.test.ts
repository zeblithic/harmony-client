import { render, waitFor, fireEvent, cleanup } from '@testing-library/svelte';
import { describe, it, expect, vi, afterEach } from 'vitest';
import NamedEmojiPicker from '../NamedEmojiPicker.svelte';

function svcWith(names: Array<{ cid: string; name: string; mime: string; size: number }>) {
  return {
    listEmojiNames: vi.fn().mockResolvedValue(names),
    previewNamedEmoji: vi.fn().mockResolvedValue(new Uint8Array([0x89, 0x50, 0x4e, 0x47])),
    setEmojiName: vi.fn().mockResolvedValue(undefined),
    onEmojiNamesChanged: vi.fn().mockReturnValue(() => {}),
  } as any;
}

describe('NamedEmojiPicker', () => {
  afterEach(() => cleanup());

  it('lists named emoji and filters by search', async () => {
    const svc = svcWith([
      { cid: 'aa', name: 'catjam', mime: 'image/png', size: 1 },
      { cid: 'bb', name: 'shipit', mime: 'image/png', size: 1 },
    ]);
    const { getByLabelText, queryByTitle } = render(NamedEmojiPicker, {
      props: { channelMessageService: svc, onpick: vi.fn() },
    });
    await waitFor(() => expect(queryByTitle('catjam')).toBeTruthy());
    expect(queryByTitle('shipit')).toBeTruthy();
    await fireEvent.input(getByLabelText('Search named emoji'), { target: { value: 'cat' } });
    expect(queryByTitle('catjam')).toBeTruthy();
    expect(queryByTitle('shipit')).toBeNull();
  });

  it('calls onpick with the descriptor when a tile is clicked', async () => {
    const onpick = vi.fn();
    const svc = svcWith([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 7 }]);
    const { getByTitle } = render(NamedEmojiPicker, { props: { channelMessageService: svc, onpick } });
    await waitFor(() => expect(getByTitle('catjam')).toBeTruthy());
    await fireEvent.click(getByTitle('catjam'));
    expect(onpick).toHaveBeenCalledWith({ cid: 'aa', mime: 'image/png', size: 7 });
  });

  it('remove calls setEmojiName(cid, null, ...)', async () => {
    const svc = svcWith([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 7 }]);
    const { getByLabelText, getByTitle } = render(NamedEmojiPicker, { props: { channelMessageService: svc, onpick: vi.fn() } });
    await waitFor(() => expect(getByTitle('catjam')).toBeTruthy());
    await fireEvent.click(getByLabelText('Remove name catjam'));
    expect(svc.setEmojiName).toHaveBeenCalledWith('aa', null, 'image/png', 7);
  });

  it('keeps the rename editor open with a friendly error on an invalid name (finding 10)', async () => {
    // A name with a space must not bounce off the backend with a raw error and
    // close the editor (losing the text). The client validates first: editor
    // stays open, a friendly message shows, and setEmojiName is never called.
    const svc = svcWith([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 7 }]);
    const { getByLabelText, queryByLabelText, queryByRole } = render(NamedEmojiPicker, {
      props: { channelMessageService: svc, onpick: vi.fn() },
    });
    await waitFor(() => expect(getByLabelText('Rename catjam')).toBeTruthy());
    await fireEvent.click(getByLabelText('Rename catjam')); // opens the inline editor
    const input = getByLabelText('Rename catjam') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'cat jam' } });
    await fireEvent.keyDown(input, { key: 'Enter' });
    // Invalid → no IPC, editor still open, friendly error shown (no raw bounce).
    expect(svc.setEmojiName).not.toHaveBeenCalled();
    expect(queryByRole('alert')?.textContent).toMatch(/letters, numbers/i);
    expect(queryByLabelText('Rename catjam')).toBeTruthy(); // input still present
    // Fixing the name and pressing Enter commits it.
    await fireEvent.input(input, { target: { value: 'cat_jam2' } });
    await fireEvent.keyDown(input, { key: 'Enter' });
    expect(svc.setEmojiName).toHaveBeenCalledWith('aa', 'cat_jam2', 'image/png', 7);
  });

  it('Escape cancels a rename without calling setEmojiName (finding 10)', async () => {
    const svc = svcWith([{ cid: 'aa', name: 'catjam', mime: 'image/png', size: 7 }]);
    const { getByLabelText, queryByLabelText } = render(NamedEmojiPicker, {
      props: { channelMessageService: svc, onpick: vi.fn() },
    });
    await waitFor(() => expect(getByLabelText('Rename catjam')).toBeTruthy());
    await fireEvent.click(getByLabelText('Rename catjam'));
    const input = getByLabelText('Rename catjam') as HTMLInputElement;
    await fireEvent.input(input, { target: { value: 'whatever' } });
    await fireEvent.keyDown(input, { key: 'Escape' });
    expect(svc.setEmojiName).not.toHaveBeenCalled();
    // Editor closed — the rename button (not the input) is back.
    await waitFor(() => expect(queryByLabelText('Rename catjam')).toBeTruthy());
  });

  it('clears a stale error when a later refresh succeeds (CodeRabbit ZEB-541)', async () => {
    // First refresh rejects → an error alert shows. A subsequent refresh that
    // resolves must clear the error so a transient failure doesn't stick.
    let changeCb: () => void = () => {};
    const svc = svcWith([]);
    svc.listEmojiNames = vi
      .fn()
      .mockRejectedValueOnce(new Error('boom'))
      .mockResolvedValue([]);
    svc.onEmojiNamesChanged = vi.fn().mockImplementation((cb: () => void) => {
      changeCb = cb;
      return () => {};
    });
    const { queryByRole } = render(NamedEmojiPicker, {
      props: { channelMessageService: svc, onpick: vi.fn() },
    });
    // The first (mount) refresh rejected → error alert present.
    await waitFor(() => expect(queryByRole('alert')).toBeTruthy());
    expect(queryByRole('alert')?.textContent).toContain('boom');
    // Trigger a second refresh that resolves; the error must clear.
    changeCb();
    await waitFor(() => expect(queryByRole('alert')).toBeNull());
  });
});
