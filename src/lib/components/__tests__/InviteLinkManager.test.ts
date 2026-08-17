import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import InviteLinkManager from '../InviteLinkManager.svelte';

describe('InviteLinkManager', () => {
  it('initial state shows Generate button + explanation', () => {
    const { getByText } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate: vi.fn().mockResolvedValue('harmony://invite/...') },
    });
    expect(getByText(/Generate invite link/i)).toBeTruthy();
  });

  it('clicking Generate calls onGenerate and renders the URL', async () => {
    const onGenerate = vi.fn().mockResolvedValue('harmony://invite/v1?ci=foo');
    const { getByText, container } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate },
    });
    await fireEvent.click(getByText(/Generate invite link/i));
    await new Promise((r) => setTimeout(r, 0));
    expect(onGenerate).toHaveBeenCalled();
    expect(container.textContent).toContain('harmony://invite/v1?ci=foo');
  });

  it('Copy button uses clipboard API', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    const onGenerate = vi.fn().mockResolvedValue('harmony://invite/v1?ci=foo');
    const { getByText } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate },
    });
    await fireEvent.click(getByText(/Generate invite link/i));
    await new Promise((r) => setTimeout(r, 0));
    await fireEvent.click(getByText(/Copy/i));
    expect(writeText).toHaveBeenCalledWith('harmony://invite/v1?ci=foo');
  });

  it('Regenerate replaces the visible URL', async () => {
    const onGenerate = vi
      .fn()
      .mockResolvedValueOnce('harmony://invite/v1?ci=first')
      .mockResolvedValueOnce('harmony://invite/v1?ci=second');
    const { getByText, container } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate },
    });
    await fireEvent.click(getByText(/Generate invite link/i));
    await new Promise((r) => setTimeout(r, 0));
    expect(container.textContent).toContain('first');
    await fireEvent.click(getByText(/Regenerate/i));
    await new Promise((r) => setTimeout(r, 0));
    expect(container.textContent).toContain('second');
    expect(container.textContent).not.toContain('first');
  });

  it('shows kind-specific warning after generation (invite-only)', async () => {
    const onGenerate = vi.fn().mockResolvedValue('harmony://invite/v1?ci=foo');
    const { getByText, container } = render(InviteLinkManager, {
      props: { kind: 'invite-only', onGenerate },
    });
    await fireEvent.click(getByText(/Generate invite link/i));
    await new Promise((r) => setTimeout(r, 0));
    // ZEB-953: the warning is crypto-accurate for any inviter (admin OR a
    // moderator, post-ZEB-950) — it embeds the generator's own invite
    // signature, not an "admin bootstrap signature".
    expect(getByText(/invite signature/i)).toBeTruthy();
    expect(container.textContent).not.toMatch(/admin/i);
  });

  it('shows kind-specific warning after generation (open)', async () => {
    const onGenerate = vi.fn().mockResolvedValue('harmony://invite/v1?ci=foo');
    const { getByText } = render(InviteLinkManager, {
      props: { kind: 'open', onGenerate },
    });
    await fireEvent.click(getByText(/Generate invite link/i));
    await new Promise((r) => setTimeout(r, 0));
    expect(getByText(/Anyone with this URL/i)).toBeTruthy();
  });
});
