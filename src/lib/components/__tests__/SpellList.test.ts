import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import SpellList from '../SpellList.svelte';

describe('SpellList', () => {
  it('shows empty state message', () => {
    render(SpellList);
    expect(screen.getByText(/no spells yet/i)).toBeTruthy();
  });

  it('suggests trying practice', () => {
    render(SpellList);
    expect(screen.getByText(/practice/i)).toBeTruthy();
  });
});
