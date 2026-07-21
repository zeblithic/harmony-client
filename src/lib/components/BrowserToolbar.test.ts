import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import BrowserToolbar from './BrowserToolbar.svelte';
import type { ContentSection } from '../types';

/** Minimal required prop set — the toolbar has several required callbacks;
 *  the section/badge behaviour under test only cares about `section`,
 *  `onSectionChange`, and `sharedUnreadCount`. */
function baseProps(overrides: Record<string, unknown> = {}) {
  return {
    viewMode: 'list' as const,
    onViewModeChange: vi.fn(),
    searchQuery: '',
    onSearchChange: vi.fn(),
    onUploadClick: vi.fn(),
    onCleanupClick: vi.fn(),
    section: 'private' as ContentSection,
    onSectionChange: vi.fn(),
    ...overrides,
  };
}

describe('BrowserToolbar', () => {
  it('renders a Shared-with-me tab with an unread badge and fires onSectionChange', async () => {
    const onSectionChange = vi.fn();
    render(BrowserToolbar, baseProps({ onSectionChange, sharedUnreadCount: 3 }));

    const tab = screen.getByRole('button', { name: /shared with me/i });
    expect(tab).toBeTruthy();
    expect(screen.getByText('3')).toBeTruthy(); // unread badge

    await fireEvent.click(tab);
    expect(onSectionChange).toHaveBeenCalledWith('sharedWithMe');
  });

  it('hides the badge when sharedUnreadCount is 0', () => {
    render(BrowserToolbar, baseProps({ sharedUnreadCount: 0 }));
    // The tab still renders; there is just no numeric badge.
    expect(screen.getByRole('button', { name: /shared with me/i })).toBeTruthy();
    expect(screen.queryByText('0')).toBeNull();
  });
});
