import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import Sparkline from '../Sparkline.svelte';
import { RingBuffer } from '../../ring-buffer';

function makeBuffer(values: number[]): RingBuffer<number> {
  const buf = new RingBuffer<number>(values.length + 10);
  for (const v of values) buf.push(v);
  return buf;
}

describe('Sparkline', () => {
  it('renders an SVG with role="img"', () => {
    const data = makeBuffer([10, 20, 30]);
    render(Sparkline, { props: { data, label: 'CPU usage' } });
    const svg = screen.getByRole('img');
    expect(svg).toBeTruthy();
    expect(svg.tagName.toLowerCase()).toBe('svg');
  });

  it('sets aria-label from props', () => {
    const data = makeBuffer([10, 20, 30]);
    render(Sparkline, { props: { data, label: 'CPU at 30%' } });
    const svg = screen.getByRole('img');
    expect(svg.getAttribute('aria-label')).toBe('CPU at 30%');
  });

  it('renders a polyline element', () => {
    const data = makeBuffer([10, 20, 30, 40]);
    render(Sparkline, { props: { data, label: 'test' } });
    const svg = screen.getByRole('img');
    const polyline = svg.querySelector('polyline');
    expect(polyline).toBeTruthy();
  });

  it('renders nothing for empty buffer', () => {
    const data = new RingBuffer<number>(10);
    render(Sparkline, { props: { data, label: 'empty' } });
    const svg = screen.getByRole('img');
    const polyline = svg.querySelector('polyline');
    expect(polyline).toBeNull();
  });

  it('applies the provided color', () => {
    const data = makeBuffer([10, 20]);
    render(Sparkline, { props: { data, label: 'test', color: '#ed4245' } });
    const svg = screen.getByRole('img');
    const polyline = svg.querySelector('polyline');
    expect(polyline?.getAttribute('stroke')).toBe('#ed4245');
  });
});
