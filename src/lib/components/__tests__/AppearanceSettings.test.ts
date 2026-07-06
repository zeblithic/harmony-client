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
});
