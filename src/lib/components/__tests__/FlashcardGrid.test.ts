import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import FlashcardGrid from '../FlashcardGrid.svelte';
import type { RowState } from '../../flashcard-types';

describe('FlashcardGrid', () => {
  it('renders BOX characters for a single byte', () => {
    render(FlashcardGrid, {
      props: {
        rows: [[0x00]],
        activeRowIndex: 0,
        rowStates: [],
      },
    });
    const cells = screen.getAllByTestId('byte-cell');
    expect(cells).toHaveLength(1);
    expect(cells[0].textContent).toContain('A');
    expect(cells[0].textContent).toContain('O');
  });

  it('renders correct number of rows and bytes', () => {
    render(FlashcardGrid, {
      props: {
        rows: [[0x00, 0xFF], [0x42, 0x59]],
        activeRowIndex: 0,
        rowStates: [],
      },
    });
    const rows = screen.getAllByTestId('grid-row');
    expect(rows).toHaveLength(2);
    const cells = screen.getAllByTestId('byte-cell');
    expect(cells).toHaveLength(4);
  });

  it('marks active row with active class', () => {
    render(FlashcardGrid, {
      props: {
        rows: [[0x00], [0xFF]],
        activeRowIndex: 1,
        rowStates: [],
      },
    });
    const rows = screen.getAllByTestId('grid-row');
    expect(rows[0].classList.contains('active')).toBe(false);
    expect(rows[1].classList.contains('active')).toBe(true);
  });

  it('applies green class to completed perfect bytes', () => {
    const rowStates: RowState[] = [
      { rowIndex: 0, byteResults: ['green', 'green'], completed: true },
    ];
    render(FlashcardGrid, {
      props: {
        rows: [[0x00, 0xFF]],
        activeRowIndex: 1,
        rowStates,
      },
    });
    const cells = screen.getAllByTestId('byte-cell');
    expect(cells[0].classList.contains('green')).toBe(true);
    expect(cells[1].classList.contains('green')).toBe(true);
  });

  it('applies yellow class to express-matched bytes', () => {
    const rowStates: RowState[] = [
      { rowIndex: 0, byteResults: ['green', 'yellow'], completed: true },
    ];
    render(FlashcardGrid, {
      props: {
        rows: [[0x00, 0xFF]],
        activeRowIndex: 1,
        rowStates,
      },
    });
    const cells = screen.getAllByTestId('byte-cell');
    expect(cells[0].classList.contains('green')).toBe(true);
    expect(cells[1].classList.contains('yellow')).toBe(true);
  });

  it('applies completed-perfect class to all-green row', () => {
    const rowStates: RowState[] = [
      { rowIndex: 0, byteResults: ['green', 'green'], completed: true },
    ];
    render(FlashcardGrid, {
      props: {
        rows: [[0x00, 0xFF]],
        activeRowIndex: 1,
        rowStates,
      },
    });
    const rows = screen.getAllByTestId('grid-row');
    expect(rows[0].classList.contains('completed-perfect')).toBe(true);
    expect(rows[0].classList.contains('completed-express')).toBe(false);
  });

  it('applies completed-express class to row with yellow bytes', () => {
    const rowStates: RowState[] = [
      { rowIndex: 0, byteResults: ['green', 'yellow'], completed: true },
    ];
    render(FlashcardGrid, {
      props: {
        rows: [[0x00, 0xFF]],
        activeRowIndex: 1,
        rowStates,
      },
    });
    const rows = screen.getAllByTestId('grid-row');
    expect(rows[0].classList.contains('completed-express')).toBe(true);
    expect(rows[0].classList.contains('completed-perfect')).toBe(false);
  });

  it('renders with accessible role', () => {
    render(FlashcardGrid, {
      props: {
        rows: [[0x00]],
        activeRowIndex: 0,
        rowStates: [],
      },
    });
    expect(screen.getByRole('grid')).toBeTruthy();
  });
});
