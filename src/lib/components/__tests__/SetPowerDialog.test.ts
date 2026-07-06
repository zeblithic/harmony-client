import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import SetPowerDialog from '../SetPowerDialog.svelte';

describe('SetPowerDialog', () => {
  const baseProps = {
    targetName: 'Bob',
    targetAddress: 'b1c4...88af',
    currentPower: 0,
    onSubmit: vi.fn(),
    onCancel: vi.fn(),
  };

  it('renders slider and number input synced to currentPower', () => {
    const { container } = render(SetPowerDialog, { props: { ...baseProps, currentPower: 25 } });
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    expect(slider.value).toBe('25');
    expect(numberInput.value).toBe('25');
  });

  it('typing in number input updates the slider', async () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    await fireEvent.input(numberInput, { target: { value: '75' } });
    expect(slider.value).toBe('75');
  });

  it('moving slider updates number input', async () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    await fireEvent.input(slider, { target: { value: '60' } });
    expect(numberInput.value).toBe('60');
  });

  it('role badge shows MEMBER for power < 50', () => {
    const { getByText } = render(SetPowerDialog, { props: { ...baseProps, currentPower: 30 } });
    expect(getByText('MEMBER')).toBeTruthy();
  });

  it('role badge shows MOD for power 50-99', () => {
    const { getByText } = render(SetPowerDialog, { props: { ...baseProps, currentPower: 75 } });
    expect(getByText('MOD')).toBeTruthy();
  });

  it('role badge shows ADMIN for power 100', () => {
    const { getByText } = render(SetPowerDialog, { props: { ...baseProps, currentPower: 100 } });
    expect(getByText('ADMIN')).toBeTruthy();
  });

  it('clamps out-of-range typed value on blur', async () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    const numberInput = container.querySelector('input[type="number"]') as HTMLInputElement;
    await fireEvent.input(numberInput, { target: { value: '500' } });
    await fireEvent.blur(numberInput);
    expect(numberInput.value).toBe('100');
  });

  it('submit calls onSubmit with current power value', async () => {
    const onSubmit = vi.fn();
    const { container, getByText } = render(SetPowerDialog, { props: { ...baseProps, onSubmit } });
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    await fireEvent.input(slider, { target: { value: '50' } });
    await fireEvent.click(getByText('Set role'));
    expect(onSubmit).toHaveBeenCalledWith(50);
  });

  it('renders the power band track with threshold-derived flex widths (ZEB-608 D6)', () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    const bands = container.querySelectorAll('.band');
    expect(bands.length).toBe(3);
    expect(bands[0].classList.contains('band-member')).toBe(true);
    expect(bands[1].classList.contains('band-mod')).toBe(true);
    expect(bands[2].classList.contains('band-admin')).toBe(true);
    // Widths derive from POWER_THRESHOLDS (member: 0→50, mod: 50→100).
    expect((bands[0] as HTMLElement).style.flexGrow).toBe('50');
    expect((bands[1] as HTMLElement).style.flexGrow).toBe('50');
  });

  it('helper line tracks the previewed role (ZEB-608 D6)', async () => {
    const { container } = render(SetPowerDialog, { props: baseProps });
    expect(container.querySelector('.role-help')?.textContent).toContain('Member can');
    const slider = container.querySelector('input[type="range"]') as HTMLInputElement;
    await fireEvent.input(slider, { target: { value: '60' } });
    expect(container.querySelector('.role-help')?.textContent).toContain(
      'Moderator can manage channels & join requests.',
    );
    await fireEvent.input(slider, { target: { value: '100' } });
    expect(container.querySelector('.role-help')?.textContent).toContain('Admin can');
  });
});
