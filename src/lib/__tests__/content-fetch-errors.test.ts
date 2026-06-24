import { describe, it, expect } from 'vitest';
import { mapContentFetchError } from '../content-fetch-errors';

describe('mapContentFetchError', () => {
  it('maps the raw zenoh fetch-timeout string to friendly copy without leaking the key-expr', () => {
    const raw = `fetch 'harmony/content/3/${'a'.repeat(64)}' timed out after 30s`;
    const out = mapContentFetchError(raw);
    // No transport internals reach the user.
    expect(out).not.toContain('harmony/content');
    expect(out).not.toContain('timed out');
    expect(out.toLowerCase()).toContain('offline');
  });

  it('maps a not-found transport error to friendly copy', () => {
    expect(mapContentFetchError('content not found for cid abc')).toMatch(/couldn.t be found|removed/i);
  });

  it('maps a leaked key-expression with no specific shape to a generic friendly message', () => {
    const out = mapContentFetchError("fetch 'harmony/content/3/deadbeef' failed: link closed");
    expect(out).not.toContain('harmony/content');
    expect(out).toMatch(/couldn.t load/i);
  });

  it('passes through an already user-facing message unchanged', () => {
    const friendly = 'Not a previewable image — download it to view.';
    expect(mapContentFetchError(friendly)).toBe(friendly);
  });

  it('passes through an unknown non-transport message unchanged', () => {
    const msg = 'Image dimensions exceed the safe limit.';
    expect(mapContentFetchError(msg)).toBe(msg);
  });
});
