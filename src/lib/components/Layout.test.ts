import { describe, it, expect, beforeEach } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import { createRawSnippet, tick } from 'svelte';
import Layout from './Layout.svelte';

// Minimal single-root snippets so we can drive Layout's slots from a test.
const slot = (id: string, label: string) =>
  createRawSnippet(() => ({
    render: () => `<div data-testid="${id}">${label}</div>`,
  }));

const baseSnippets = {
  nav: slot('nav', 'nav'),
  textFeed: slot('text', 'text feed'),
  mediaFeed: slot('media', 'No media yet'),
};

beforeEach(() => {
  localStorage.clear();
});

describe('Layout media panel — ZEB-405 (WS-C)', () => {
  it('defaults to collapsed: shows the reveal rail, hides the media feed', () => {
    const { queryByTestId, getByLabelText } = render(Layout, {
      props: { ...baseSnippets, mode: 'messages', collapsed: false, mediaPanelOpen: false },
    });
    expect(getByLabelText('Show media panel')).toBeInTheDocument();
    expect(queryByTestId('media')).not.toBeInTheDocument();
  });

  it('shows the media feed and a hide control when opened', () => {
    const { getByTestId, getByLabelText, queryByLabelText } = render(Layout, {
      props: { ...baseSnippets, mode: 'messages', collapsed: false, mediaPanelOpen: true },
    });
    expect(getByTestId('media')).toBeInTheDocument();
    expect(getByLabelText('Hide media panel')).toBeInTheDocument();
    expect(queryByLabelText('Show media panel')).not.toBeInTheDocument();
  });

  it('reveals the panel when the rail is clicked', async () => {
    const { getByLabelText, findByTestId } = render(Layout, {
      props: { ...baseSnippets, mode: 'messages', collapsed: false, mediaPanelOpen: false },
    });
    await fireEvent.click(getByLabelText('Show media panel'));
    await tick();
    expect(await findByTestId('media')).toBeInTheDocument();
  });

  it('keeps the Settings panel in the column even when the media feed is collapsed', () => {
    const { getByTestId, queryByLabelText } = render(Layout, {
      props: {
        ...baseSnippets,
        settingsPanel: slot('settings', 'settings here'),
        mode: 'messages',
        collapsed: false,
        showSettings: true,
        mediaPanelOpen: false,
      },
    });
    expect(getByTestId('settings')).toBeInTheDocument();
    // While Settings owns the column there is no reveal rail and no media hide control.
    expect(queryByLabelText('Show media panel')).not.toBeInTheDocument();
    expect(queryByLabelText('Hide media panel')).not.toBeInTheDocument();
  });

  it('hides the rail on narrow (responsive-collapsed) screens', () => {
    const { queryByLabelText, queryByTestId } = render(Layout, {
      props: { ...baseSnippets, mode: 'messages', collapsed: true, mediaPanelOpen: false },
    });
    expect(queryByLabelText('Show media panel')).not.toBeInTheDocument();
    expect(queryByTestId('media')).not.toBeInTheDocument();
  });

  it('widens the panel via the keyboard resize handle (ArrowLeft)', async () => {
    const { getByRole } = render(Layout, {
      props: {
        ...baseSnippets,
        mode: 'messages',
        collapsed: false,
        mediaPanelOpen: true,
        mediaPanelWidth: 340,
      },
    });
    const sep = getByRole('separator');
    expect(sep).toHaveAttribute('aria-valuenow', '340');
    await fireEvent.keyDown(sep, { key: 'ArrowLeft' });
    await tick();
    expect(sep).toHaveAttribute('aria-valuenow', '356');
  });

  it('re-syncs width (does not freeze) when a drag is interrupted by hiding the splitter', async () => {
    const settingsProps = (showSettings: boolean) => ({
      ...baseSnippets,
      settingsPanel: slot('settings', 'settings'),
      mode: 'messages' as const,
      collapsed: false,
      mediaPanelOpen: true,
      showSettings,
      mediaPanelWidth: 340,
    });
    const { getByRole, rerender } = render(Layout, { props: settingsProps(false) });
    const sep = getByRole('separator');
    // Begin a drag and move so liveWidth diverges from the committed 340.
    await fireEvent.pointerDown(sep, { pointerId: 1, clientX: 1000 });
    await fireEvent.pointerMove(sep, { pointerId: 1, clientX: 940 });
    await tick();
    expect(Number(sep.getAttribute('aria-valuenow'))).toBeGreaterThan(340);

    // Interrupt: Settings takes the column, unmounting the splitter (no pointerup).
    await rerender(settingsProps(true));
    await tick();
    // Return to the media feed — width must re-sync to the committed 340.
    await rerender(settingsProps(false));
    await tick();
    expect(getByRole('separator').getAttribute('aria-valuenow')).toBe('340');
  });

  it('does not render the media column for non-messages modes', () => {
    const { queryByLabelText, getByTestId, queryByTestId } = render(Layout, {
      props: {
        ...baseSnippets,
        mintLedger: slot('mint', 'mint ledger'),
        mode: 'mint',
        collapsed: false,
        mediaPanelOpen: true,
      },
    });
    expect(getByTestId('mint')).toBeInTheDocument();
    expect(queryByLabelText('Show media panel')).not.toBeInTheDocument();
    expect(queryByLabelText('Hide media panel')).not.toBeInTheDocument();
    expect(queryByTestId('media')).not.toBeInTheDocument();
  });
});

/**
 * ZEB-767. `showSettings: true` is NOT sufficient for Settings to be on screen —
 * Layout also requires messages mode. The left-rail gear is rendered in every
 * mode and used to toggle `showSettings` blindly, so in the 5 non-messages modes
 * it flipped to `aria-pressed="true"` while nothing opened: a dead control that
 * announced success to a screen reader.
 *
 * App.svelte now routes to messages mode before opening (and derives the gear's
 * pressed state from `showSettings && appMode === 'messages'`). That fix is only
 * necessary while the asymmetry below holds, so pin it: if Settings ever renders
 * outside messages mode, these fail and the routing can be reconsidered.
 */
describe('Layout settings gating — ZEB-767', () => {
  const settingsIn = (mode: string) =>
    render(Layout, {
      props: {
        ...baseSnippets,
        settingsPanel: slot('settings', 'settings here'),
        mintLedger: slot('mint', 'mint ledger'),
        vineFeed: slot('vine', 'vines'),
        mode,
        collapsed: false,
        showSettings: true,
      },
    });

  it('renders Settings in messages mode when showSettings is set', () => {
    expect(settingsIn('messages').getByTestId('settings')).toBeInTheDocument();
  });

  it.each(['mint', 'vines'])(
    'ignores showSettings in %s mode — the state is set but nothing renders',
    (mode) => {
      expect(settingsIn(mode).queryByTestId('settings')).not.toBeInTheDocument();
    },
  );
});
