import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import ContributionMeter from '../ContributionMeter.svelte';
import type { ContributionSummaryDto } from '../../storage-buddy-service';

function summary(over: Partial<ContributionSummaryDto> = {}): ContributionSummaryDto {
  return {
    hostedBytes: 2_500_000_000,
    budgetBytes: 10_000_000_000,
    buddyCount: 3,
    health: 'healthy',
    ...over,
  };
}

describe('ContributionMeter', () => {
  it('renders nothing until the summary IPC has succeeded (honesty gate)', () => {
    const { container } = render(ContributionMeter, {
      props: { summary: null, onManage: vi.fn() },
    });
    expect(container.querySelector('.contribution-meter')).toBeNull();
  });

  it('renders hosted-of-budget text and a proportional fill', () => {
    const { container } = render(ContributionMeter, {
      props: { summary: summary(), onManage: vi.fn() },
    });
    expect(screen.getByText(/2\.5 GB of 10\.0 GB shared/)).toBeTruthy();
    const fill = container.querySelector('.meter-fill') as HTMLElement;
    expect(fill.style.width).toBe('25%');
    expect(fill.classList.contains('warning')).toBe(false);
  });

  it('renders the spec health copy verbatim', () => {
    render(ContributionMeter, {
      props: { summary: summary({ health: 'catchingUp' }), onManage: vi.fn() },
    });
    expect(screen.getByText(/You host pieces for 3 storage buddies\. Catching up\./)).toBeTruthy();
  });

  it('warns the fill for non-healthy states', () => {
    const { container } = render(ContributionMeter, {
      props: {
        summary: summary({ health: 'overBudget', hostedBytes: 12_000_000_000 }),
        onManage: vi.fn(),
      },
    });
    const fill = container.querySelector('.meter-fill') as HTMLElement;
    expect(fill.classList.contains('warning')).toBe(true);
    expect(fill.style.width).toBe('100%');
  });

  it('singularizes one buddy', () => {
    render(ContributionMeter, {
      props: { summary: summary({ buddyCount: 1 }), onManage: vi.fn() },
    });
    expect(screen.getByText(/You host pieces for 1 storage buddy\. Healthy\./)).toBeTruthy();
  });

  it('zero buddies: honest empty copy + invite affordance, no fabricated count', () => {
    render(ContributionMeter, {
      props: {
        summary: summary({ buddyCount: 0, hostedBytes: 0 }),
        onManage: vi.fn(),
      },
    });
    expect(screen.getByText('No storage buddies yet.')).toBeTruthy();
    expect(screen.getByTestId('buddy-manage-btn').textContent?.trim()).toBe('Invite a friend');
  });

  it('zero budget with usage clamps to 100% instead of dividing by zero', () => {
    const { container } = render(ContributionMeter, {
      props: {
        summary: summary({ budgetBytes: 0, hostedBytes: 5, health: 'overBudget' }),
        onManage: vi.fn(),
      },
    });
    const fill = container.querySelector('.meter-fill') as HTMLElement;
    expect(fill.style.width).toBe('100%');
  });

  it('fires onManage from the button', async () => {
    const onManage = vi.fn();
    render(ContributionMeter, { props: { summary: summary(), onManage } });
    await fireEvent.click(screen.getByTestId('buddy-manage-btn'));
    expect(onManage).toHaveBeenCalledOnce();
  });
});
