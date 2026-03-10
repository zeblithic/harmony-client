import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import LayoutTestWrapper from './LayoutTestWrapper.svelte';

describe('Layout', () => {
  it('renders files-mode class when mode is files', () => {
    const { container } = render(LayoutTestWrapper, {
      props: { mode: 'files' },
    });
    const layout = container.querySelector('.layout');
    expect(layout).toBeTruthy();
    expect(layout!.classList.contains('files-mode')).toBe(true);
    expect(screen.getByTestId('file-browser')).toBeTruthy();
    expect(screen.queryByTestId('text-feed')).toBeNull();
  });

  it('does not render files-area in messages mode', () => {
    const { container } = render(LayoutTestWrapper, {
      props: { mode: 'messages' },
    });
    const layout = container.querySelector('.layout');
    expect(layout).toBeTruthy();
    expect(layout!.classList.contains('files-mode')).toBe(false);
    expect(screen.getByTestId('text-feed')).toBeTruthy();
    expect(screen.queryByTestId('file-browser')).toBeNull();
  });

  it('renders detail panel when not collapsed in files mode', () => {
    render(LayoutTestWrapper, {
      props: { mode: 'files', collapsed: false },
    });
    expect(screen.getByTestId('file-detail')).toBeTruthy();
  });

  it('hides detail panel when collapsed in files mode', () => {
    render(LayoutTestWrapper, {
      props: { mode: 'files', collapsed: true },
    });
    expect(screen.queryByTestId('file-detail')).toBeNull();
  });
});
