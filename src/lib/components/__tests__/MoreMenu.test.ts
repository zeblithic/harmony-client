import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import MoreMenu from '../MoreMenu.svelte';
import type { AppMode } from '../../types';

function props(over: Record<string, unknown> = {}) {
  return {
    secondaryModes: [] as { mode: AppMode; label: string }[],
    activeMode: 'messages' as AppMode,
    onSelectMode: vi.fn(),
    onOpenNetworkHealth: vi.fn(),
    onSubmitFeedback: vi.fn(),
    onShowAbout: vi.fn(),
    onOpenDocs: vi.fn(),
    ...over,
  };
}

const open = () => fireEvent.click(screen.getByTestId('more-menu-button'));

describe('MoreMenu (ZEB-555)', () => {
  it('renders the More trigger with menu semantics', () => {
    render(MoreMenu, props());
    const btn = screen.getByTestId('more-menu-button');
    expect(btn.getAttribute('aria-label')).toBe('More');
    expect(btn.getAttribute('aria-haspopup')).toBe('menu');
    expect(btn.getAttribute('aria-expanded')).toBe('false');
  });

  it('menu is hidden initially', () => {
    render(MoreMenu, props());
    expect(screen.queryByTestId('more-menu')).toBeNull();
  });

  it('renders a compact icon trigger (collapsed rail) and still opens the menu', async () => {
    render(MoreMenu, props({ compact: true }));
    const btn = screen.getByTestId('more-menu-button');
    expect(btn.getAttribute('aria-label')).toBe('More');
    expect(btn.className).toContain('more-icon-button');
    await open();
    expect(screen.getByTestId('more-feedback')).toBeTruthy();
  });

  it('opens to a Help-only menu (4 items, no "Go to") when there are no secondary modes', async () => {
    render(MoreMenu, props());
    await open();
    const items = screen.getAllByRole('menuitem');
    expect(items).toHaveLength(4);
    expect(items[0]).toHaveTextContent(/Network Health/i);
    expect(items[1]).toHaveTextContent(/Submit Feedback/i);
    expect(items[2]).toHaveTextContent(/About/i);
    expect(items[3]).toHaveTextContent(/Documentation/i);
    expect(screen.queryByText('Go to')).toBeNull();
  });

  it('lists enabled secondary modes under a "Go to" section before Help', async () => {
    render(
      MoreMenu,
      props({
        secondaryModes: [
          { mode: 'mail', label: 'Mail' },
          { mode: 'network', label: 'Network' },
        ],
      }),
    );
    await open();
    expect(screen.getByText('Go to')).toBeTruthy();
    const items = screen.getAllByRole('menuitem');
    expect(items).toHaveLength(6); // 2 secondary + 4 help
    expect(items[0]).toHaveTextContent('Mail');
    expect(items[1]).toHaveTextContent('Network');
    expect(items[2]).toHaveTextContent(/Network Health/i);
  });

  it('marks the active secondary mode with aria-current', async () => {
    render(
      MoreMenu,
      props({ secondaryModes: [{ mode: 'mail', label: 'Mail' }], activeMode: 'mail' }),
    );
    await open();
    expect(screen.getByRole('menuitem', { name: 'Mail' }).getAttribute('aria-current')).toBe('true');
  });

  it('selecting a secondary mode calls onSelectMode and closes', async () => {
    const p = props({ secondaryModes: [{ mode: 'spellbook', label: 'Spellbook' }] });
    render(MoreMenu, p);
    await open();
    await fireEvent.click(screen.getByRole('menuitem', { name: 'Spellbook' }));
    expect(p.onSelectMode).toHaveBeenCalledWith('spellbook');
    expect(screen.queryByTestId('more-menu')).toBeNull();
  });

  it('Network Health item → onOpenNetworkHealth + close', async () => {
    const p = props();
    render(MoreMenu, p);
    await open();
    await fireEvent.click(screen.getByTestId('more-network-health'));
    expect(p.onOpenNetworkHealth).toHaveBeenCalled();
    expect(screen.queryByTestId('more-menu')).toBeNull();
  });

  it('Submit Feedback item → onSubmitFeedback + close', async () => {
    const p = props();
    render(MoreMenu, p);
    await open();
    await fireEvent.click(screen.getByTestId('more-feedback'));
    expect(p.onSubmitFeedback).toHaveBeenCalled();
    expect(screen.queryByTestId('more-menu')).toBeNull();
  });

  it('About item → onShowAbout + close', async () => {
    const p = props();
    render(MoreMenu, p);
    await open();
    await fireEvent.click(screen.getByTestId('more-about'));
    expect(p.onShowAbout).toHaveBeenCalled();
  });

  it('Documentation item → onOpenDocs + close', async () => {
    const p = props();
    render(MoreMenu, p);
    await open();
    await fireEvent.click(screen.getByTestId('more-docs'));
    expect(p.onOpenDocs).toHaveBeenCalled();
  });

  it('click outside closes the menu', async () => {
    render(MoreMenu, props());
    await open();
    expect(screen.getByTestId('more-menu')).toBeInTheDocument();
    await fireEvent.mouseDown(document.body);
    expect(screen.queryByTestId('more-menu')).toBeNull();
  });

  it('Escape closes the menu and returns focus to the trigger', async () => {
    render(MoreMenu, props());
    await open();
    await new Promise((r) => setTimeout(r, 0));
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(screen.queryByTestId('more-menu')).toBeNull();
    expect(document.activeElement).toBe(screen.getByTestId('more-menu-button'));
  });

  it('Tab closes the menu', async () => {
    render(MoreMenu, props());
    await open();
    await fireEvent.keyDown(window, { key: 'Tab' });
    expect(screen.queryByTestId('more-menu')).toBeNull();
  });

  it('opening focuses the first menu item', async () => {
    render(MoreMenu, props());
    await open();
    await new Promise((r) => setTimeout(r, 0));
    expect(document.activeElement).toBe(screen.getByTestId('more-network-health'));
  });

  it('ArrowDown moves focus to the next item; ArrowUp from first wraps to last', async () => {
    render(MoreMenu, props());
    await open();
    await new Promise((r) => setTimeout(r, 0));
    await fireEvent.keyDown(window, { key: 'ArrowDown' });
    expect(document.activeElement).toBe(screen.getByTestId('more-feedback'));
    await fireEvent.keyDown(window, { key: 'ArrowUp' });
    expect(document.activeElement).toBe(screen.getByTestId('more-network-health'));
    await fireEvent.keyDown(window, { key: 'ArrowUp' });
    expect(document.activeElement).toBe(screen.getByTestId('more-docs'));
  });
});
