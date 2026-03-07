import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import NetworkToolbar from '../NetworkToolbar.svelte';

describe('NetworkToolbar', () => {
  it('renders re-center button', () => {
    render(NetworkToolbar, { props: { showTable: false } });
    expect(screen.getByRole('button', { name: /re-center/i })).toBeTruthy();
  });

  it('renders zoom fit button', () => {
    render(NetworkToolbar, { props: { showTable: false } });
    expect(screen.getByRole('button', { name: /zoom to fit/i })).toBeTruthy();
  });

  it('renders table/graph toggle', () => {
    render(NetworkToolbar, { props: { showTable: false } });
    expect(screen.getByRole('button', { name: /table view/i })).toBeTruthy();
  });

  it('toggle label changes when showTable is true', () => {
    render(NetworkToolbar, { props: { showTable: true } });
    expect(screen.getByRole('button', { name: /graph view/i })).toBeTruthy();
  });

  it('emits onToggleView when toggle clicked', async () => {
    const onToggleView = vi.fn();
    render(NetworkToolbar, { props: { showTable: false, onToggleView } });
    const btn = screen.getByRole('button', { name: /table view/i });
    await fireEvent.click(btn);
    expect(onToggleView).toHaveBeenCalled();
  });

  it('emits onRecenter when re-center clicked', async () => {
    const onRecenter = vi.fn();
    render(NetworkToolbar, { props: { showTable: false, onRecenter } });
    const btn = screen.getByRole('button', { name: /re-center/i });
    await fireEvent.click(btn);
    expect(onRecenter).toHaveBeenCalled();
  });
});
