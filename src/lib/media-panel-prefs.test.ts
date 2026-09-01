import { describe, it, expect, beforeEach } from 'vitest';
import {
  loadMediaPanelOpen,
  saveMediaPanelOpen,
  loadMediaPanelWidth,
  saveMediaPanelWidth,
  MEDIA_PANEL_MIN_WIDTH,
  MEDIA_PANEL_MAX_WIDTH,
  MEDIA_PANEL_DEFAULT_WIDTH,
} from './media-panel-prefs';

beforeEach(() => {
  localStorage.clear();
});

describe('media-panel-prefs — ZEB-405 (WS-C)', () => {
  describe('open/closed preference', () => {
    it('defaults to collapsed (false) when nothing is stored', () => {
      // Opt-in reveal: a fresh user sees the panel collapsed.
      expect(loadMediaPanelOpen()).toBe(false);
    });

    it('round-trips an opened panel', () => {
      saveMediaPanelOpen(true);
      expect(loadMediaPanelOpen()).toBe(true);
    });

    it('round-trips a re-collapsed panel', () => {
      saveMediaPanelOpen(true);
      saveMediaPanelOpen(false);
      expect(loadMediaPanelOpen()).toBe(false);
    });

    it('treats a corrupt stored value as collapsed', () => {
      localStorage.setItem('harmony-media-panel-open', 'banana');
      expect(loadMediaPanelOpen()).toBe(false);
    });
  });

  describe('width preference', () => {
    it('defaults to MEDIA_PANEL_DEFAULT_WIDTH when nothing is stored', () => {
      expect(loadMediaPanelWidth()).toBe(MEDIA_PANEL_DEFAULT_WIDTH);
    });

    it('round-trips a valid width', () => {
      saveMediaPanelWidth(420);
      expect(loadMediaPanelWidth()).toBe(420);
    });

    it('clamps a too-small width up to the minimum on save', () => {
      saveMediaPanelWidth(100);
      expect(loadMediaPanelWidth()).toBe(MEDIA_PANEL_MIN_WIDTH);
    });

    it('clamps a too-large width down to the maximum on save', () => {
      saveMediaPanelWidth(9999);
      expect(loadMediaPanelWidth()).toBe(MEDIA_PANEL_MAX_WIDTH);
    });

    it('clamps an out-of-range stored value on load (defends against tampering / older builds)', () => {
      localStorage.setItem('harmony-media-panel-width', '50');
      expect(loadMediaPanelWidth()).toBe(MEDIA_PANEL_MIN_WIDTH);
    });

    it('falls back to the default for a non-numeric stored value', () => {
      localStorage.setItem('harmony-media-panel-width', 'wide-please');
      expect(loadMediaPanelWidth()).toBe(MEDIA_PANEL_DEFAULT_WIDTH);
    });

    it('rounds fractional widths to whole pixels', () => {
      saveMediaPanelWidth(333.7);
      expect(loadMediaPanelWidth()).toBe(334);
    });
  });
});
