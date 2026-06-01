import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import VoiceChannelView from '../VoiceChannelView.svelte';

describe('VoiceChannelView (V1 scaffold)', () => {
  it('renders the channel name in a speaker header', () => {
    render(VoiceChannelView, { props: { channelName: 'hangout' } });
    expect(screen.getByText(/hangout/)).toBeTruthy();
    // The speaker glyph anchors the header.
    expect(screen.getByText('🔊')).toBeTruthy();
  });

  it('renders a DISABLED Join button', () => {
    render(VoiceChannelView, { props: { channelName: 'hangout' } });
    const join = screen.getByRole('button', { name: /join/i }) as HTMLButtonElement;
    expect(join.disabled).toBe(true);
  });

  it('renders a roster placeholder and a coming-soon note', () => {
    render(VoiceChannelView, { props: { channelName: 'hangout' } });
    expect(screen.getByText(/No one is here yet/i)).toBeTruthy();
    expect(screen.getByText(/coming soon/i)).toBeTruthy();
  });
});
