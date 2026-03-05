import { describe, it, expect } from 'vitest';
import { sanitizeHref } from './url-sanitize';

describe('sanitizeHref', () => {
  it('allows https URLs', () => {
    expect(sanitizeHref('https://example.com/path')).toBe('https://example.com/path');
  });

  it('allows http URLs', () => {
    expect(sanitizeHref('http://example.com')).toBe('http://example.com');
  });

  it('allows mailto URLs', () => {
    expect(sanitizeHref('mailto:user@example.com')).toBe('mailto:user@example.com');
  });

  it('rejects javascript: URLs', () => {
    expect(sanitizeHref('javascript:alert(1)')).toBe('');
  });

  it('rejects javascript: with mixed case', () => {
    expect(sanitizeHref('JaVaScRiPt:alert(1)')).toBe('');
  });

  it('rejects data: URLs', () => {
    expect(sanitizeHref('data:text/html,<script>alert(1)</script>')).toBe('');
  });

  it('rejects vbscript: URLs', () => {
    expect(sanitizeHref('vbscript:MsgBox("hi")')).toBe('');
  });

  it('rejects javascript: with leading whitespace', () => {
    expect(sanitizeHref('  javascript:alert(1)')).toBe('');
  });

  it('returns empty string for empty input', () => {
    expect(sanitizeHref('')).toBe('');
  });

  it('returns empty string for undefined input', () => {
    expect(sanitizeHref(undefined)).toBe('');
  });
});
