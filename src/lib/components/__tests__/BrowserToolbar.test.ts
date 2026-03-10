import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import BrowserToolbar from '../BrowserToolbar.svelte';

const baseProps = {
  viewMode: 'list' as const,
  onViewModeChange: vi.fn(),
  searchQuery: '',
  onSearchChange: vi.fn(),
  onUploadClick: vi.fn(),
  onCleanupClick: vi.fn(),
  section: 'private' as const,
  onSectionChange: vi.fn(),
};

describe('BrowserToolbar', () => {
  it('renders search input with correct aria-label', () => {
    render(BrowserToolbar, { props: baseProps });
    const input = screen.getByLabelText('Search files');
    expect(input).toBeTruthy();
    expect(input.getAttribute('placeholder')).toBe('Search files...');
  });

  it('toggles view mode on button click', async () => {
    const onViewModeChange = vi.fn();
    render(BrowserToolbar, { props: { ...baseProps, onViewModeChange } });
    const gridBtn = screen.getByLabelText('Grid view');
    await fireEvent.click(gridBtn);
    expect(onViewModeChange).toHaveBeenCalledWith('grid');
  });

  it('hides upload button when section is published', () => {
    render(BrowserToolbar, { props: { ...baseProps, section: 'published' as const } });
    expect(screen.queryByLabelText('Upload')).toBeNull();
    expect(screen.queryByLabelText('Cleanup')).toBeNull();
  });

  it('shows upload and cleanup buttons for private section', () => {
    render(BrowserToolbar, { props: baseProps });
    expect(screen.getByLabelText('Upload')).toBeTruthy();
    expect(screen.getByLabelText('Cleanup')).toBeTruthy();
  });

  it('marks current view mode as pressed', () => {
    render(BrowserToolbar, { props: baseProps });
    const listBtn = screen.getByLabelText('List view');
    const gridBtn = screen.getByLabelText('Grid view');
    expect(listBtn.getAttribute('aria-pressed')).toBe('true');
    expect(gridBtn.getAttribute('aria-pressed')).toBe('false');
  });

  it('calls onSearchChange when typing', async () => {
    const onSearchChange = vi.fn();
    render(BrowserToolbar, { props: { ...baseProps, onSearchChange } });
    const input = screen.getByLabelText('Search files');
    await fireEvent.input(input, { target: { value: 'test' } });
    expect(onSearchChange).toHaveBeenCalledWith('test');
  });

  it('calls onSectionChange when clicking section toggle', async () => {
    const onSectionChange = vi.fn();
    render(BrowserToolbar, { props: { ...baseProps, onSectionChange } });
    const pubBtn = screen.getByText('Published');
    await fireEvent.click(pubBtn);
    expect(onSectionChange).toHaveBeenCalledWith('published');
  });
});
