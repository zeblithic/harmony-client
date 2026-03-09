import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import Breadcrumbs from '../Breadcrumbs.svelte';

describe('Breadcrumbs', () => {
  it('renders path segments with separator', () => {
    const path = [
      { cid: null, name: 'My Content' },
      { cid: 'cid-folder-projects', name: 'Projects' },
    ];
    const { container } = render(Breadcrumbs, { props: { path, onNavigate: vi.fn() } });
    expect(screen.getByText('My Content')).toBeTruthy();
    expect(screen.getByText('Projects')).toBeTruthy();
    // Separator between segments
    const separators = container.querySelectorAll('.separator');
    expect(separators.length).toBe(1);
    expect(separators[0].textContent).toBe('/');
  });

  it('calls onNavigate with cid when segment clicked', async () => {
    const onNavigate = vi.fn();
    const path = [
      { cid: null, name: 'My Content' },
      { cid: 'cid-folder-projects', name: 'Projects' },
    ];
    render(Breadcrumbs, { props: { path, onNavigate } });

    await fireEvent.click(screen.getByText('My Content'));
    expect(onNavigate).toHaveBeenCalledWith(null);

    await fireEvent.click(screen.getByText('Projects'));
    expect(onNavigate).toHaveBeenCalledWith('cid-folder-projects');
  });

  it('renders single root segment without separator', () => {
    const path = [{ cid: null, name: 'My Content' }];
    const { container } = render(Breadcrumbs, { props: { path, onNavigate: vi.fn() } });
    expect(screen.getByText('My Content')).toBeTruthy();
    expect(container.querySelectorAll('.separator').length).toBe(0);
  });

  it('has navigation aria-label', () => {
    const path = [{ cid: null, name: 'My Content' }];
    render(Breadcrumbs, { props: { path, onNavigate: vi.fn() } });
    expect(screen.getByLabelText('File navigation')).toBeTruthy();
  });
});
