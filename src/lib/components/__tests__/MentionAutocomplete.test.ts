import { render, fireEvent, screen } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import MentionAutocomplete from '../MentionAutocomplete.svelte';

const A = { ownerId: 'a'.repeat(32), label: 'Jake (Koya)' };
const B = { ownerId: 'b'.repeat(32), label: 'Jasmine' };

describe('MentionAutocomplete', () => {
  beforeEach(() => vi.clearAllMocks());

  it('renders one row per candidate', () => {
    render(MentionAutocomplete, { props: { candidates: [A, B], activeIndex: 0, onPick: vi.fn() } });
    expect(screen.getAllByTestId('mention-option')).toHaveLength(2);
    expect(screen.getByText('Jake (Koya)')).toBeTruthy();
  });

  it('marks the active row', () => {
    render(MentionAutocomplete, { props: { candidates: [A, B], activeIndex: 1, onPick: vi.fn() } });
    const rows = screen.getAllByTestId('mention-option');
    expect(rows[1].getAttribute('data-active')).toBe('true');
    expect(rows[0].getAttribute('data-active')).not.toBe('true');
  });

  it('renders nothing when there are no candidates', () => {
    const { container } = render(MentionAutocomplete, {
      props: { candidates: [], activeIndex: 0, onPick: vi.fn() },
    });
    expect(container.querySelector('[data-testid="mention-option"]')).toBeNull();
  });

  it('calls onPick with the clicked candidate', async () => {
    const onPick = vi.fn();
    render(MentionAutocomplete, { props: { candidates: [A, B], activeIndex: 0, onPick } });
    await fireEvent.mouseDown(screen.getByText('Jasmine'));
    expect(onPick).toHaveBeenCalledWith(B);
  });
});
