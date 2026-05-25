/**
 * ZEB-331 — Onboarding env collection + GitHub-issue URL builder.
 *
 * Pure functions (no Svelte bindings) for testability. Read by FeedbackModal
 * on submit; never throws.
 *
 * No TOCTOU concern: `collectEnvironment()` + `buildGitHubIssueUrl()` are
 * read-only synthesis + pure URL building; no commit-token / write pattern.
 * Feedback submission is reversible — user reviews on GitHub before clicking
 * Submit there.
 */

import { platform, version } from '@tauri-apps/plugin-os';
import { getVersion } from '@tauri-apps/api/app';
import type { EnvironmentInfo, FeedbackPayload } from './types/onboarding';

/** GitHub new-issue base URL. */
export const GITHUB_ISSUES_URL = 'https://github.com/zeblithic/harmony-client/issues/new';

/**
 * Conservative URL-length budget. GitHub's actual server limit is ~8KB on
 * the query string; staying under 8000 leaves headroom for the
 * `?title=...&body=...` framing.
 */
export const URL_BUDGET = 8000;

const TITLE_PREFIX = '[alpha-feedback] ';
const TITLE_DESCRIPTION_MAX = 50;
const TRUNCATION_MARKER = '\n…[truncated for URL length]';

/**
 * Read platform / OS version / app version via Tauri plugins.
 *
 * Each field is independently best-effort. A rejection from any source
 * collapses to `'unknown'` for that field; submission still proceeds.
 * Never throws — degraded environment beats blocking a feedback report.
 */
export async function collectEnvironment(): Promise<EnvironmentInfo> {
  const timestamp = new Date().toISOString();

  // Each await wrapped individually so one failure doesn't drop the others.
  const platformResult = await safeCall(() => Promise.resolve(platform()));
  const versionResult = await safeCall(() => Promise.resolve(version()));
  const appVersionResult = await safeCall(() => getVersion());

  return {
    appVersion: appVersionResult ?? 'unknown',
    platform: platformResult ?? 'unknown',
    osVersion: versionResult ?? 'unknown',
    timestamp,
  };
}

async function safeCall(fn: () => Promise<string>): Promise<string | null> {
  try {
    return await fn();
  } catch {
    return null;
  }
}

/**
 * Build a fully-encoded GitHub new-issue URL from a feedback payload.
 *
 * Title: `[alpha-feedback] ` + first 50 chars of description (newlines stripped).
 * Body: `## Description` + `## Environment` + optional `## Network diagnostics`.
 * Diagnostics section omitted entirely when payload.diagnostics is undefined.
 *
 * URL-length budget: 8000 chars. When exceeded, diagnostics body is
 * truncated with `…[truncated for URL length]` marker; description + env
 * are preserved intact.
 */
export function buildGitHubIssueUrl(payload: FeedbackPayload): string {
  const title = buildTitle(payload.description);
  const body = buildBody(payload);
  const url = composeUrl(title, body);

  if (url.length <= URL_BUDGET) {
    return url;
  }

  // Over budget: try truncating diagnostics. Description + env are
  // load-bearing for the report; they stay intact.
  if (payload.diagnostics !== undefined) {
    const truncated = truncateToFit(title, payload, URL_BUDGET);
    return truncated;
  }

  // No diagnostics to trim and still over budget: return as-is. GitHub
  // will accept a long URL or render an error; either is preferable to
  // silently dropping the description.
  return url;
}

function buildTitle(description: string): string {
  const singleLine = description.replace(/[\n\r]+/g, ' ').trim();
  const head = singleLine.slice(0, TITLE_DESCRIPTION_MAX);
  return TITLE_PREFIX + head;
}

function buildBody(payload: FeedbackPayload): string {
  const sections: string[] = [];

  sections.push('## Description', '', payload.description, '');

  sections.push(
    '## Environment',
    '',
    `- App version: ${payload.env.appVersion}`,
    `- Platform: ${payload.env.platform}`,
    `- OS version: ${payload.env.osVersion}`,
    `- Submitted: ${payload.env.timestamp}`,
    '',
  );

  if (payload.diagnostics !== undefined) {
    sections.push('## Network diagnostics', '', payload.diagnostics, '');
  }

  return sections.join('\n');
}

function composeUrl(title: string, body: string): string {
  return `${GITHUB_ISSUES_URL}?title=${encodeURIComponent(title)}&body=${encodeURIComponent(body)}`;
}

function truncateToFit(
  title: string,
  payload: FeedbackPayload,
  budget: number,
): string {
  // Build the body with a placeholder for diagnostics, then size what
  // remains to fit.
  const bodyHead = [
    '## Description',
    '',
    payload.description,
    '',
    '## Environment',
    '',
    `- App version: ${payload.env.appVersion}`,
    `- Platform: ${payload.env.platform}`,
    `- OS version: ${payload.env.osVersion}`,
    `- Submitted: ${payload.env.timestamp}`,
    '',
    '## Network diagnostics',
    '',
  ].join('\n');

  // Reserve budget for the fixed framing: GITHUB_ISSUES_URL + `?title=` +
  // encoded title + `&body=` + encoded bodyHead + encoded marker.
  const frameUrl = composeUrl(title, bodyHead + (payload.diagnostics ?? '') + TRUNCATION_MARKER);
  if (frameUrl.length <= budget) {
    return frameUrl;
  }

  // Binary-search the diagnostics chars that fit. Encoding inflation
  // varies per char, so we measure end-to-end URL length.
  let lo = 0;
  let hi = (payload.diagnostics ?? '').length;
  while (lo < hi) {
    const mid = Math.floor((lo + hi + 1) / 2);
    const candidate = composeUrl(
      title,
      bodyHead + (payload.diagnostics ?? '').slice(0, mid) + TRUNCATION_MARKER,
    );
    if (candidate.length <= budget) {
      lo = mid;
    } else {
      hi = mid - 1;
    }
  }

  return composeUrl(
    title,
    bodyHead + (payload.diagnostics ?? '').slice(0, lo) + TRUNCATION_MARKER,
  );
}
