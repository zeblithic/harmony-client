import '@testing-library/jest-dom/vitest';
import { render } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import HarmonyMark from '../HarmonyMark.svelte';

describe('HarmonyMark', () => {
  it('renders three brand circles, no dot by default, at default size 24', () => {
    const { container } = render(HarmonyMark);
    const svg = container.querySelector('svg');
    expect(svg).toHaveAttribute('width', '24');
    expect(svg).toHaveAttribute('viewBox', '0 0 92 92');
    expect(container.querySelectorAll('circle')).toHaveLength(3);
    expect(container.querySelector('circle[stroke="#466b4c"]')).toHaveAttribute(
      'stroke-width',
      '5'
    );
  });

  it('renders the center dot and thinner stroke at header size', () => {
    const { container } = render(HarmonyMark, { props: { size: 58, withDot: true } });
    expect(container.querySelectorAll('circle')).toHaveLength(4);
    expect(container.querySelector('circle[stroke="#466b4c"]')).toHaveAttribute(
      'stroke-width',
      '4'
    );
  });
});
