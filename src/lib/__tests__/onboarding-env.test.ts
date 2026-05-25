import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('@tauri-apps/plugin-os', () => ({
  platform: vi.fn(),
  version: vi.fn(),
}));
vi.mock('@tauri-apps/api/app', () => ({
  getVersion: vi.fn(),
}));

import { platform, version } from '@tauri-apps/plugin-os';
import { getVersion } from '@tauri-apps/api/app';
import {
  collectEnvironment,
  buildGitHubIssueUrl,
  URL_BUDGET,
  GITHUB_ISSUES_URL,
} from '../onboarding-env';
import type { EnvironmentInfo } from '../types/onboarding';

const FIXED_ENV: EnvironmentInfo = {
  appVersion: '0.1.0-alpha.1',
  platform: 'macos',
  osVersion: '15.0',
  timestamp: '2026-05-25T08:00:00.000Z',
};

describe('buildGitHubIssueUrl', () => {
  it('produces a GitHub new-issue URL', () => {
    const url = buildGitHubIssueUrl({
      description: 'something broke when I clicked join',
      env: FIXED_ENV,
    });
    expect(url.startsWith(`${GITHUB_ISSUES_URL}?`)).toBe(true);
    expect(url).toMatch(/title=/);
    expect(url).toMatch(/body=/);
  });

  it('includes ## Description section with full description verbatim', () => {
    const url = buildGitHubIssueUrl({
      description: 'multi\nline\ndescription',
      env: FIXED_ENV,
    });
    const decoded = decodeURIComponent(url.split('body=')[1]);
    expect(decoded).toContain('## Description');
    expect(decoded).toContain('multi\nline\ndescription');
  });

  it('includes ## Environment section with all four fields', () => {
    const url = buildGitHubIssueUrl({
      description: 'short report',
      env: FIXED_ENV,
    });
    const decoded = decodeURIComponent(url.split('body=')[1]);
    expect(decoded).toContain('## Environment');
    expect(decoded).toContain('App version: 0.1.0-alpha.1');
    expect(decoded).toContain('Platform: macos');
    expect(decoded).toContain('OS version: 15.0');
    expect(decoded).toContain('Submitted: 2026-05-25T08:00:00.000Z');
  });

  it('includes ## Network diagnostics section when diagnostics provided', () => {
    const url = buildGitHubIssueUrl({
      description: 'short report',
      env: FIXED_ENV,
      diagnostics: '## Snapshot\nrelay: ok',
    });
    const decoded = decodeURIComponent(url.split('body=')[1]);
    expect(decoded).toContain('## Network diagnostics');
    expect(decoded).toContain('## Snapshot');
    expect(decoded).toContain('relay: ok');
  });

  it('OMITS ## Network diagnostics entirely when diagnostics undefined', () => {
    const url = buildGitHubIssueUrl({
      description: 'short report',
      env: FIXED_ENV,
    });
    const decoded = decodeURIComponent(url.split('body=')[1]);
    expect(decoded).not.toContain('## Network diagnostics');
  });

  it('URL-encodes special chars (spaces, &, =, #, newlines)', () => {
    const url = buildGitHubIssueUrl({
      description: 'has & symbols = and # and \n newlines',
      env: FIXED_ENV,
    });
    // Raw URL must not contain unencoded & or = inside the body param;
    // it should appear as %26 / %3D etc.
    const bodyParam = url.split('body=')[1];
    expect(bodyParam).toMatch(/%26/); // encoded &
    expect(bodyParam).toMatch(/%23/); // encoded #
    expect(bodyParam).toMatch(/%0A/); // encoded \n
  });

  it('truncates title at 50 chars', () => {
    const long = 'x'.repeat(200);
    const url = buildGitHubIssueUrl({
      description: long,
      env: FIXED_ENV,
    });
    const decodedTitle = decodeURIComponent(url.match(/title=([^&]+)/)![1]);
    // Prefix '[alpha-feedback] ' (17 chars) + first 50 of description
    expect(decodedTitle).toBe('[alpha-feedback] ' + 'x'.repeat(50));
  });

  it('strips newlines from title (single-line invariant)', () => {
    const url = buildGitHubIssueUrl({
      description: 'first\nsecond\nthird',
      env: FIXED_ENV,
    });
    const decodedTitle = decodeURIComponent(url.match(/title=([^&]+)/)![1]);
    expect(decodedTitle).not.toContain('\n');
  });

  it('truncates diagnostics body when total URL exceeds 8000 chars, with marker', () => {
    const longDiagnostics = '## Snapshot\n' + 'd'.repeat(20000);
    const url = buildGitHubIssueUrl({
      description: 'normal description',
      env: FIXED_ENV,
      diagnostics: longDiagnostics,
    });
    expect(url.length).toBeLessThanOrEqual(URL_BUDGET);
    const decoded = decodeURIComponent(url.split('body=')[1]);
    expect(decoded).toContain('…[truncated for URL length]');
  });

  it('preserves description + env intact even when diagnostics truncated', () => {
    const url = buildGitHubIssueUrl({
      description: 'load-bearing description text',
      env: FIXED_ENV,
      diagnostics: 'd'.repeat(20000),
    });
    const decoded = decodeURIComponent(url.split('body=')[1]);
    expect(decoded).toContain('load-bearing description text');
    expect(decoded).toContain('App version: 0.1.0-alpha.1');
    expect(decoded).toContain('Platform: macos');
  });
});

describe('collectEnvironment', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('returns full info when all plugin calls succeed', async () => {
    (platform as ReturnType<typeof vi.fn>).mockResolvedValue('macos');
    (version as ReturnType<typeof vi.fn>).mockResolvedValue('15.0');
    (getVersion as ReturnType<typeof vi.fn>).mockResolvedValue('0.1.0-alpha.1');
    const env = await collectEnvironment();
    expect(env.platform).toBe('macos');
    expect(env.osVersion).toBe('15.0');
    expect(env.appVersion).toBe('0.1.0-alpha.1');
    expect(env.timestamp).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/);
  });

  it('returns "unknown" for fields whose plugin call rejects', async () => {
    (platform as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('plugin gone'));
    (version as ReturnType<typeof vi.fn>).mockResolvedValue('15.0');
    (getVersion as ReturnType<typeof vi.fn>).mockResolvedValue('0.1.0-alpha.1');
    const env = await collectEnvironment();
    expect(env.platform).toBe('unknown');
    expect(env.osVersion).toBe('15.0');
    expect(env.appVersion).toBe('0.1.0-alpha.1');
  });

  it('returns all "unknown" when every plugin rejects', async () => {
    (platform as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('x'));
    (version as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('x'));
    (getVersion as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('x'));
    const env = await collectEnvironment();
    expect(env.platform).toBe('unknown');
    expect(env.osVersion).toBe('unknown');
    expect(env.appVersion).toBe('unknown');
  });

  it('never throws to caller', async () => {
    (platform as ReturnType<typeof vi.fn>).mockImplementation(() => {
      throw new Error('synchronous throw');
    });
    (version as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('x'));
    (getVersion as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('x'));
    await expect(collectEnvironment()).resolves.toBeDefined();
  });
});
