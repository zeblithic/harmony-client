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

  it('disables graph-only buttons when in table mode', () => {
    render(NetworkToolbar, { props: { showTable: true } });
    const recenter = screen.getByRole('button', { name: /re-center/i });
    const fit = screen.getByRole('button', { name: /zoom to fit/i });
    expect(recenter).toHaveProperty('disabled', true);
    expect(fit).toHaveProperty('disabled', true);
  });

  it('enables graph-only buttons when in graph mode', () => {
    render(NetworkToolbar, { props: { showTable: false } });
    const recenter = screen.getByRole('button', { name: /re-center/i });
    const fit = screen.getByRole('button', { name: /zoom to fit/i });
    expect(recenter).toHaveProperty('disabled', false);
    expect(fit).toHaveProperty('disabled', false);
  });
});
