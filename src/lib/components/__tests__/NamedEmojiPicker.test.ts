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
});
