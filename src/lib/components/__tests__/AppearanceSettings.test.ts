import '@testing-library/jest-dom/vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, describe, expect, it } from 'vitest';
import { _resetThemeServiceForTest } from '../../theme-service';
import AppearanceSettings from '../AppearanceSettings.svelte';

beforeEach(() => {
  localStorage.clear();
  _resetThemeServiceForTest();
  delete document.documentElement.dataset.theme;
});

describe('AppearanceSettings', () => {
  it('renders a three-option radiogroup defaulting to System', () => {
    render(AppearanceSettings);
    const group = screen.getByRole('radiogroup', { name: /theme/i });
    const options = screen.getAllByRole('radio');
    expect(group).toBeInTheDocument();
    expect(options).toHaveLength(3);
    expect(screen.getByRole('radio', { name: /system/i })).toHaveAttribute(
      'aria-checked',
      'true'
    );
  });

  it('selecting Dark applies the dark theme and reflects selection', async () => {
    render(AppearanceSettings);
    await fireEvent.click(screen.getByRole('radio', { name: /dark/i }));
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(screen.getByRole('radio', { name: /dark/i })).toHaveAttribute(
      'aria-checked',
      'true'
    );
    expect(screen.getByRole('radio', { name: /system/i })).toHaveAttribute(
      'aria-checked',
      'false'
    );
  });

  it('selecting Light applies the light theme', async () => {
    render(AppearanceSettings);
    await fireEvent.click(screen.getByRole('radio', { name: /dark/i }));
    await fireEvent.click(screen.getByRole('radio', { name: /light/i }));
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  // ZEB-645: the CodecToggle-style keyboard model shipped with no keyboard test
  // (a ZEB-605 plan gap). Arrows/Home/End move+select, Space/Enter select,
  // roving tabindex tracks selection.
  const radio = (name: RegExp) => screen.getByRole('radio', { name });

  it('ArrowRight / ArrowDown moves selection to the next option and applies it', async () => {
    render(AppearanceSettings);
    await fireEvent.keyDown(radio(/system/i), { key: 'ArrowRight' });
    expect(radio(/light/i)).toHaveAttribute('aria-checked', 'true');
    expect(document.documentElement.dataset.theme).toBe('light');
    await fireEvent.keyDown(radio(/light/i), { key: 'ArrowDown' });
    expect(radio(/dark/i)).toHaveAttribute('aria-checked', 'true');
    expect(document.documentElement.dataset.theme).toBe('dark');
  });

  it('ArrowLeft / ArrowUp moves to the previous option, wrapping at the ends', async () => {
    render(AppearanceSettings);
    // System (index 0) → ArrowLeft wraps to Dark (last).
    await fireEvent.keyDown(radio(/system/i), { key: 'ArrowLeft' });
    expect(radio(/dark/i)).toHaveAttribute('aria-checked', 'true');
    expect(document.documentElement.dataset.theme).toBe('dark');
    await fireEvent.keyDown(radio(/dark/i), { key: 'ArrowUp' });
    expect(radio(/light/i)).toHaveAttribute('aria-checked', 'true');
  });

  it('Home selects the first option, End the last', async () => {
    render(AppearanceSettings);
    await fireEvent.keyDown(radio(/system/i), { key: 'End' });
    expect(radio(/dark/i)).toHaveAttribute('aria-checked', 'true');
    await fireEvent.keyDown(radio(/dark/i), { key: 'Home' });
    expect(radio(/system/i)).toHaveAttribute('aria-checked', 'true');
  });

  it('Space and Enter select the option they fire on', async () => {
    render(AppearanceSettings);
    await fireEvent.keyDown(radio(/dark/i), { key: 'Enter' });
    expect(radio(/dark/i)).toHaveAttribute('aria-checked', 'true');
    expect(document.documentElement.dataset.theme).toBe('dark');
    await fireEvent.keyDown(radio(/light/i), { key: ' ' });
    expect(radio(/light/i)).toHaveAttribute('aria-checked', 'true');
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  it('roving tabindex: only the selected option is tabbable', async () => {
    render(AppearanceSettings);
    expect(radio(/system/i)).toHaveAttribute('tabindex', '0');
    expect(radio(/light/i)).toHaveAttribute('tabindex', '-1');
    await fireEvent.keyDown(radio(/system/i), { key: 'ArrowRight' });
    expect(radio(/light/i)).toHaveAttribute('tabindex', '0');
    expect(radio(/system/i)).toHaveAttribute('tabindex', '-1');
  });
});
