import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import BrowserToolbar from '../BrowserToolbar.svelte';

const baseProps = {
  viewMode: 'list' as const,
  onViewModeChange: vi.fn(),
  searchQuery: '',
  onSearchChange: vi.fn(),
  onUploadClick: vi.fn(),
  section: 'private' as const,
  onSectionChange: vi.fn(),
};

describe('BrowserToolbar', () => {
  it('renders search input with correct aria-label', () => {
    render(BrowserToolbar, { props: baseProps });
    const input = screen.getByLabelText('Search files');
    expect(input).toBeTruthy();
    expect(input.getAttribute('placeholder')).toBe('Search files or paste a CID…');
  });

  it('toggles view mode on button click', async () => {
    const onViewModeChange = vi.fn();
    render(BrowserToolbar, { props: { ...baseProps, onViewModeChange } });
    const gridBtn = screen.getByLabelText('Grid view');
    await fireEvent.click(gridBtn);
    expect(onViewModeChange).toHaveBeenCalledWith('grid');
  });

  it('hides upload button when section is sharedWithMe', () => {
    render(BrowserToolbar, { props: { ...baseProps, section: 'sharedWithMe' as const } });
    expect(screen.queryByLabelText('Add files')).toBeNull();
  });

  it('shows the upload button for private section', () => {
    render(BrowserToolbar, { props: baseProps });
    expect(screen.getByLabelText('Add files')).toBeTruthy();
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
    const sharedBtn = screen.getByText('Shared with me');
    await fireEvent.click(sharedBtn);
    expect(onSectionChange).toHaveBeenCalledWith('sharedWithMe');
  });

  // ── ZEB-674 Gap A: encrypt-on-upload toggle ─────────────────────────

  it('shows the encrypt toggle, unchecked by default, for the private section', () => {
    render(BrowserToolbar, { props: baseProps });
    const toggle = screen.getByLabelText('Encrypt (private)') as HTMLInputElement;
    expect(toggle).toBeTruthy();
    expect(toggle.checked).toBe(false);
  });

  it('hides the encrypt toggle when section is sharedWithMe', () => {
    render(BrowserToolbar, { props: { ...baseProps, section: 'sharedWithMe' as const } });
    expect(screen.queryByLabelText('Encrypt (private)')).toBeNull();
  });

  it('calls onUploadClick with encrypted=false when the toggle is left off', async () => {
    const onUploadClick = vi.fn();
    render(BrowserToolbar, { props: { ...baseProps, onUploadClick } });
    const uploadBtn = screen.getByLabelText('Add files');
    await fireEvent.click(uploadBtn);
    expect(onUploadClick).toHaveBeenCalledWith(false);
  });

  it('calls onUploadClick with encrypted=true when the toggle is checked before uploading', async () => {
    const onUploadClick = vi.fn();
    render(BrowserToolbar, { props: { ...baseProps, onUploadClick } });
    const toggle = screen.getByLabelText('Encrypt (private)');
    await fireEvent.click(toggle);
    const uploadBtn = screen.getByLabelText('Add files');
    await fireEvent.click(uploadBtn);
    expect(onUploadClick).toHaveBeenCalledWith(true);
  });
});
