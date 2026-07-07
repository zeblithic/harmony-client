import { render } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import WizardProgress from '../WizardProgress.svelte';

const steps = [
  { label: 'Welcome', accent: 'sage' as const },
  { label: 'Create', accent: 'sage' as const },
  { label: 'Back up', accent: 'clay' as const },
];

describe('WizardProgress', () => {
  it('renders one pip per step', () => {
    const { container } = render(WizardProgress, { props: { steps, activeIndex: 0 } });
    expect(container.querySelectorAll('.wizard-progress-pip')).toHaveLength(3);
  });

  it('marks only the active step pip active', () => {
    const { container } = render(WizardProgress, { props: { steps, activeIndex: 1 } });
    const pips = container.querySelectorAll('.wizard-progress-pip');
    expect(pips[1].classList.contains('is-active')).toBe(true);
    expect(pips[0].classList.contains('is-active')).toBe(false);
    expect(pips[2].classList.contains('is-active')).toBe(false);
  });

  it('applies the clay accent class on a clay step', () => {
    const { container } = render(WizardProgress, { props: { steps, activeIndex: 2 } });
    const pips = container.querySelectorAll('.wizard-progress-pip');
    expect(pips[2].classList.contains('accent-clay')).toBe(true);
  });

  it('shows the step counter by default', () => {
    const { queryByText } = render(WizardProgress, { props: { steps, activeIndex: 1 } });
    expect(queryByText('Step 2 of 3')).toBeTruthy();
  });

  it('hides the counter when showCounter is false', () => {
    const { queryByText } = render(WizardProgress, {
      props: { steps, activeIndex: 0, showCounter: false },
    });
    expect(queryByText(/Step \d of \d/)).toBeNull();
  });
});
