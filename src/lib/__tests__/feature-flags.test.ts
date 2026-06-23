import { describe, it, expect, beforeEach } from 'vitest';
import { isNavModeEnabled, setNavModeOverride } from '../feature-flags';

// The storage contract under test (the key is module-internal by design).
const OVERRIDE_KEY = 'harmony-feature-flags';

describe('feature-flags nav gating', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  describe('alpha defaults', () => {
    it('shows the focused Communities-first surface (messages, vines, files)', () => {
      expect(isNavModeEnabled('messages')).toBe(true);
      expect(isNavModeEnabled('vines')).toBe(true);
      expect(isNavModeEnabled('files')).toBe(true);
    });

    it('hides the deferred/experimental modes (spellbook, network, mint, mail)', () => {
      expect(isNavModeEnabled('spellbook')).toBe(false);
      expect(isNavModeEnabled('network')).toBe(false);
      expect(isNavModeEnabled('mint')).toBe(false);
      expect(isNavModeEnabled('mail')).toBe(false);
    });
  });

  describe('device-local overrides', () => {
    it('re-enables a hidden mode without a rebuild', () => {
      expect(isNavModeEnabled('mail')).toBe(false);
      setNavModeOverride('mail', true);
      expect(isNavModeEnabled('mail')).toBe(true);
    });

    it('can force-hide a default-shown mode', () => {
      expect(isNavModeEnabled('vines')).toBe(true);
      setNavModeOverride('vines', false);
      expect(isNavModeEnabled('vines')).toBe(false);
    });

    it('merges successive overrides rather than replacing them', () => {
      setNavModeOverride('mail', true);
      setNavModeOverride('mint', true);
      expect(isNavModeEnabled('mail')).toBe(true);
      expect(isNavModeEnabled('mint')).toBe(true);
    });

    it('never gates off the home mode, even if an override tries to', () => {
      setNavModeOverride('messages', false);
      expect(isNavModeEnabled('messages')).toBe(true);
    });
  });

  describe('robustness — a malformed override never hides the rail', () => {
    const cases: Array<[string, string]> = [
      ['invalid JSON', '{not json'],
      ['a JSON array', '[1,2,3]'],
      ['a JSON primitive', '"mail"'],
      ['null', 'null'],
    ];
    for (const [label, raw] of cases) {
      it(`falls back to defaults for ${label}`, () => {
        localStorage.setItem(OVERRIDE_KEY, raw);
        // Defaults intact: deferred hidden, core shown.
        expect(isNavModeEnabled('mail')).toBe(false);
        expect(isNavModeEnabled('messages')).toBe(true);
        expect(isNavModeEnabled('vines')).toBe(true);
      });
    }

    it('ignores a non-boolean value for a known mode but honors valid siblings', () => {
      localStorage.setItem(OVERRIDE_KEY, JSON.stringify({ mail: 'yes', mint: true }));
      expect(isNavModeEnabled('mail')).toBe(false); // non-boolean ignored → default
      expect(isNavModeEnabled('mint')).toBe(true); // valid sibling honored
    });

    it('ignores unknown keys in the override map', () => {
      localStorage.setItem(OVERRIDE_KEY, JSON.stringify({ bogus: true, mail: true }));
      expect(isNavModeEnabled('mail')).toBe(true); // known key still honored
    });
  });
});
