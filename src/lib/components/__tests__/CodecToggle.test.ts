import { render, screen, fireEvent } from '@testing-library/svelte';
import { describe, it, expect, vi } from 'vitest';
import CodecToggle from '../CodecToggle.svelte';

describe('CodecToggle', () => {
  it('renders two radio options', () => {
    render(CodecToggle, { props: { selected: 'opus' } });
    const group = screen.getByRole('radiogroup', { name: /voice codec/i });
    expect(group).toBeTruthy();
    const radios = screen.getAllByRole('radio');
    expect(radios.length).toBe(2);
  });

  it('marks opus as checked when selected', () => {
    render(CodecToggle, { props: { selected: 'opus' } });
    const opus = screen.getByRole('radio', { name: /opus/i });
    expect(opus.getAttribute('aria-checked')).toBe('true');
  });

  it('marks codec2 as checked when selected', () => {
    render(CodecToggle, { props: { selected: 'codec2' } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    expect(codec2.getAttribute('aria-checked')).toBe('true');
  });

  it('fires onCodecChange on click', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.click(codec2);
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('fires onCodecChange on Enter key', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.keyDown(codec2, { key: 'Enter' });
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('fires onCodecChange on Space key', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.keyDown(codec2, { key: ' ' });
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('navigates with arrow keys', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange } });
    const opus = screen.getByRole('radio', { name: /opus/i });
    await fireEvent.keyDown(opus, { key: 'ArrowRight' });
    expect(onCodecChange).toHaveBeenCalledWith('codec2');
  });

  it('is disabled when disabled prop is true', () => {
    render(CodecToggle, { props: { selected: 'opus', disabled: true } });
    const radios = screen.getAllByRole('radio');
    for (const radio of radios) {
      expect(radio.getAttribute('aria-disabled')).toBe('true');
    }
  });

  it('does not fire onCodecChange when disabled', async () => {
    const onCodecChange = vi.fn();
    render(CodecToggle, { props: { selected: 'opus', onCodecChange, disabled: true } });
    const codec2 = screen.getByRole('radio', { name: /codec2/i });
    await fireEvent.click(codec2);
    expect(onCodecChange).not.toHaveBeenCalled();
  });
});
