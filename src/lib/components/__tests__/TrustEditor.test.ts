import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import TrustEditor from '../TrustEditor.svelte';
import { buildScore, getIdentity, getCompliance, getAssociation, getEndorsement } from '../../trust-score';

describe('TrustEditor', () => {
  it('renders four radiogroups', () => {
    render(TrustEditor, { props: { score: null, onScoreChange: vi.fn() } });
    const groups = screen.getAllByRole('radiogroup');
    expect(groups.length).toBe(4);
  });

  it('labels radiogroups by dimension', () => {
    render(TrustEditor, { props: { score: null, onScoreChange: vi.fn() } });
    expect(screen.getByRole('radiogroup', { name: /identity/i })).toBeTruthy();
    expect(screen.getByRole('radiogroup', { name: /compliance/i })).toBeTruthy();
    expect(screen.getByRole('radiogroup', { name: /association/i })).toBeTruthy();
    expect(screen.getByRole('radiogroup', { name: /endorsement/i })).toBeTruthy();
  });

  it('renders 4 radio buttons per dimension', () => {
    render(TrustEditor, { props: { score: null, onScoreChange: vi.fn() } });
    const radios = screen.getAllByRole('radio');
    expect(radios.length).toBe(16);
  });

  it('reflects current score in selected radios', () => {
    const score = buildScore(1, 2, 3, 0);
    render(TrustEditor, { props: { score, onScoreChange: vi.fn() } });
    const radios = screen.getAllByRole('radio') as HTMLInputElement[];
    const checked = radios.filter((r) => r.checked);
    expect(checked.length).toBe(4);
  });

  it('emits onScoreChange when a radio is clicked', async () => {
    const onScoreChange = vi.fn();
    render(TrustEditor, { props: { score: buildScore(0, 0, 0, 0), onScoreChange } });
    // Click the "3" option for Identity
    const identityGroup = screen.getByRole('radiogroup', { name: /identity/i });
    const radios = identityGroup.querySelectorAll('input[type="radio"]');
    await fireEvent.click(radios[3]); // level 3
    expect(onScoreChange).toHaveBeenCalled();
    const newScore = onScoreChange.mock.calls[0][0];
    expect(getIdentity(newScore)).toBe(3);
  });

  it('displays overall score as hex', () => {
    render(TrustEditor, { props: { score: 0xFF, onScoreChange: vi.fn() } });
    expect(screen.getByText(/0xff/i)).toBeTruthy();
  });

  it('renders a clear button', () => {
    render(TrustEditor, { props: { score: 0xFF, onScoreChange: vi.fn(), onClear: vi.fn() } });
    expect(screen.getByRole('button', { name: /clear/i })).toBeTruthy();
  });

  it('emits onClear when clear button is clicked', async () => {
    const onClear = vi.fn();
    render(TrustEditor, { props: { score: 0xFF, onScoreChange: vi.fn(), onClear } });
    await fireEvent.click(screen.getByRole('button', { name: /clear/i }));
    expect(onClear).toHaveBeenCalled();
  });

  it('shows all radios unchecked when score is null', () => {
    render(TrustEditor, { props: { score: null, onScoreChange: vi.fn() } });
    const radios = screen.getAllByRole('radio') as HTMLInputElement[];
    const checked = radios.filter((r) => r.checked);
    expect(checked.length).toBe(0);
  });
});
