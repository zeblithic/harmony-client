const ALLOWED_SCHEMES = ['https:', 'http:', 'mailto:'];

/**
 * Sanitize a URL for use in href attributes.
 * Only allows http:, https:, and mailto: schemes.
 * Returns empty string for dangerous or empty URLs.
 */
export function sanitizeHref(url: string | undefined): string {
  if (!url) return '';
  const trimmed = url.trim();
  if (!trimmed) return '';
  try {
    const parsed = new URL(trimmed);
    return ALLOWED_SCHEMES.includes(parsed.protocol) ? trimmed : '';
  } catch {
    // Relative URLs or malformed — reject to be safe
    return '';
  }
}
