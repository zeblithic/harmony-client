import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, fireEvent, waitFor } from '@testing-library/svelte';
import MessageAttachments from '../MessageAttachments.svelte';
import type { ChannelAttachmentDto } from '../../channel-message-service';

// vi.mock is hoisted to the top of the file; vi.hoisted makes the spy
// available at factory-call time (repo pattern — see WelcomeModal.test.ts).
const { saveMock } = vi.hoisted(() => ({ saveMock: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => ({
  save: saveMock,
  open: vi.fn(),
}));

function att(over: Partial<ChannelAttachmentDto> = {}): ChannelAttachmentDto {
  return { cid: 'cid1', mime: 'text/plain', name: 'log.txt', size: 2048, encrypted: true, ...over };
}
function makeService(downloadArtifact = vi.fn().mockResolvedValue(2048)) {
  return { downloadArtifact } as any;
}
function props(over: Record<string, unknown> = {}) {
  return { communityId: 'c', channelId: 'ch', attachments: [att()], channelMessageService: makeService(), ...over };
}

describe('MessageAttachments', () => {
  beforeEach(() => { saveMock.mockReset(); });

  it('renders a chip per attachment with name, size, icon, lock', () => {
    const { container } = render(MessageAttachments, { props: props() });
    expect(container.textContent).toContain('log.txt');
    expect(container.textContent).toContain('2.0 KB');
    expect(container.querySelector('.att-lock')).not.toBeNull();
    expect(container.querySelectorAll('.attachment-chip').length).toBe(1);
  });

  it('omits the lock badge when not encrypted', () => {
    const { container } = render(MessageAttachments, { props: props({ attachments: [att({ encrypted: false })] }) });
    expect(container.querySelector('.att-lock')).toBeNull();
  });

  it('renders one chip and does not throw for duplicate CIDs', () => {
    const dup = att({ cid: 'dup' });
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [dup, { ...dup }], channelMessageService: makeService() }),
    });
    expect(container.querySelectorAll('.attachment-chip').length).toBe(1);
  });

  it('download: save → downloadArtifact called with chosen path', async () => {
    saveMock.mockResolvedValue('/tmp/out.txt');
    const service = makeService();
    const a = att();
    const { container } = render(MessageAttachments, { props: props({ attachments: [a], channelMessageService: service }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => {
      expect(service.downloadArtifact).toHaveBeenCalledWith('c', 'ch', a, '/tmp/out.txt');
    });
  });

  it('cancel (save → null) does not call downloadArtifact', async () => {
    saveMock.mockResolvedValue(null);
    const service = makeService();
    const { container } = render(MessageAttachments, { props: props({ channelMessageService: service }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await Promise.resolve();
    expect(service.downloadArtifact).not.toHaveBeenCalled();
  });

  it('sanitizes a path-traversal name to a basename for the save dialog', async () => {
    saveMock.mockResolvedValue(null); // cancel — we only assert the dialog args
    const { container } = render(MessageAttachments, {
      props: props({ attachments: [att({ name: '../../etc/passwd' })], channelMessageService: makeService() }),
    });
    await fireEvent.click(container.querySelector('.att-download')!);
    expect(saveMock).toHaveBeenCalledWith(expect.objectContaining({ defaultPath: 'passwd' }));
  });

  it('download error → error message + retry re-invokes', async () => {
    saveMock.mockResolvedValue('/tmp/out.txt');
    const downloadArtifact = vi.fn()
      .mockRejectedValueOnce(new Error('peer offline'))
      .mockResolvedValueOnce(2048);
    const { container } = render(MessageAttachments, { props: props({ channelMessageService: makeService(downloadArtifact) }) });
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => expect(container.querySelector('.att-error')?.textContent).toContain('peer offline'));
    await fireEvent.click(container.querySelector('.att-download')!);
    await waitFor(() => expect(downloadArtifact).toHaveBeenCalledTimes(2));
  });
});
