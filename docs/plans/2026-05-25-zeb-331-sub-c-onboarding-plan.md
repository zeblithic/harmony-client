# ZEB-331 Sub-C Onboarding + First-Run UX Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship hybrid welcome-modal + ambient-guidance onboarding flow + (?)-icon feedback affordance + troubleshooting docs for v0.1.0-alpha testers, wrapping the existing identity / `harmony://` invite / Network Health surfaces.

**Architecture:** Pure-frontend UX scaffolding wrapping existing IPCs. One small backend addition: `start_node` returns a `freshly_created: bool` flag so the welcome modal can fire only on fresh keychain identity. Reuses ZEB-329's `network_health_export_payload` redactor for optional diagnostic-attached feedback. No new wire-format, no CRDT events, no protocol changes.

**Tech Stack:** Svelte 5 runes, vitest + testing-library/svelte, Tauri 2.x (`tauri-plugin-os` + `tauri-plugin-shell` newly added; existing `tauri-plugin-deep-link`), keyring 3.x.

**Spec:** `docs/specs/2026-05-25-zeb-331-sub-c-onboarding-design.md` (commit `6f60200`).

**Branch:** `zeb-331-sub-c-onboarding-spec` off `origin/main 5091f57` (ZEB-329 PR #161 merge).

---

## File structure

### New files

| Path | Responsibility |
|---|---|
| `src/lib/types/onboarding.ts` | TS mirrors of backend `StartNodeResponse`; `EnvironmentInfo`, `FeedbackPayload` types |
| `src/lib/onboarding-env.ts` | Pure helpers: `collectEnvironment()` (Tauri OS plugin reads with `'unknown'` fallback), `buildGitHubIssueUrl()` (URL-encoded, 8000-char budget, diagnostic truncation marker) |
| `src/lib/components/WelcomeModal.svelte` | First-run welcome: intro + alpha orientation + invite-paste; uses existing `extractHarmonyInviteUrl` validator |
| `src/lib/components/FeedbackModal.svelte` | Description + diagnostic-attach toggle → `shell.open` GitHub issue URL; latestRequest stale-response guard pattern from ZEB-329 R3 |
| `src/lib/components/HelpMenuButton.svelte` | (?) icon + dropdown (Submit Feedback / Network Health / About / Documentation), keyboard nav, ARIA |
| `src/lib/components/AboutModal.svelte` | Simple modal: app version, license, GitHub link |
| `src/lib/__tests__/onboarding-env.test.ts` | Pure-helper unit tests (~15 cases) |
| `src/lib/components/__tests__/WelcomeModal.test.ts` | Component tests (~9 cases) |
| `src/lib/components/__tests__/HelpMenuButton.test.ts` | Component tests (~9 cases incl. keyboard nav) |
| `src/lib/components/__tests__/FeedbackModal.test.ts` | Component tests (~12 cases incl. privacy-invariant + stale-response regression) |
| `docs/troubleshooting.md` | Network/identity/Gatekeeper cookbook (~150-200 lines) |
| `docs/feedback.md` | GitHub-issue submission flow explanation (~80 lines) |

### Modified files

| Path | Change |
|---|---|
| `src-tauri/Cargo.toml` | Add `tauri-plugin-os = "2"` + `tauri-plugin-shell = "2"` |
| `src-tauri/src/lib.rs` | Plugin registration (`builder.plugin(tauri_plugin_os::init())` + `plugin(tauri_plugin_shell::init())`); `start_node` return type → `Result<StartNodeResponse, String>`; new `StartNodeResponse` struct |
| `src-tauri/src/iroh_endpoint.rs` | `load_or_create_secret_key()` returns `(SecretKey, bool /* freshly_created */)` instead of bare `SecretKey` |
| `src-tauri/capabilities/default.json` | Add `os:default` + `shell:allow-open` permissions |
| `package.json` | Add `@tauri-apps/plugin-os` + `@tauri-apps/plugin-shell` deps |
| `src/App.svelte` | Boot sequence destructures `start_node` response; mounts `WelcomeModal` + `FeedbackModal` + `AboutModal`; floating `HelpMenuButton` at top-right; deep-link handler suppresses welcome modal |

---

## Task 0: Pre-flight green-baseline confirm

**Files:** none (read-only verification)

- [ ] **Step 1: Verify branch + base commit**

Run:
```bash
git rev-parse HEAD              # expect 6f60200 (spec commit)
git log --oneline -3 origin/main # expect 5091f57 as latest origin/main
git rev-list HEAD --not origin/main | head -5  # expect just 6f60200
```

If branch state differs, abort and resync.

- [ ] **Step 2: Capture orphan failure baseline (frontend)**

Run from repo root:
```bash
set -o pipefail
npx vitest run 2>&1 | tee /tmp/zeb-331-task-0-vitest.log
echo "VITEST EXIT=${PIPESTATUS[0]}"
```

Expected: all pass (per ZEB-329 PR description, 2063 tests pass). If any fail, capture exact failing-test names to `/tmp/zeb-331-orphan-vitest.txt` as the baseline that Tasks 1-10 must not exceed.

- [ ] **Step 3: Capture orphan failure baseline (backend, time-bounded)**

Run from `src-tauri/`:
```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tee /tmp/zeb-331-task-0-nextest.log
echo "NEXTEST EXIT=${PIPESTATUS[0]}"
```

Expected per ZEB-329 PR description orphan failures: `folder_ingest::tests`, `mint::tests`, `mint_sync::tests`, `rename_content_integration` (port-4242 flake), occasional `zenoh_iroh_*` timeouts under concurrency. Capture the exact failing list to `/tmp/zeb-331-orphan-nextest.txt`.

If wall-clock exceeds 10 minutes without completion → report DONE_WITH_CONCERNS (baseline ship as best-effort + flag for Task 5/10 re-verify with smaller scope).

- [ ] **Step 4: No commit for Task 0**

Pre-flight is read-only verification. No commit, no push. Proceed to Task 1.

---

## Task 1: TypeScript types + pure helpers + Tauri plugins

**Files:**
- Create: `src/lib/types/onboarding.ts`
- Create: `src/lib/onboarding-env.ts`
- Create: `src/lib/__tests__/onboarding-env.test.ts`
- Modify: `src-tauri/Cargo.toml` (add `tauri-plugin-os` + `tauri-plugin-shell`)
- Modify: `src-tauri/src/lib.rs` (register plugins in Tauri builder; line near other `.plugin()` calls)
- Modify: `src-tauri/capabilities/default.json` (add `os:default` + `shell:allow-open`)
- Modify: `package.json` (add `@tauri-apps/plugin-os` + `@tauri-apps/plugin-shell`)

- [ ] **Step 1: Verify current plugin state**

```bash
grep -n "tauri-plugin-os\|tauri-plugin-shell" src-tauri/Cargo.toml || echo "(plugins not yet added)"
grep -n "@tauri-apps/plugin-os\|@tauri-apps/plugin-shell" package.json || echo "(JS deps not yet added)"
grep -n "os:default\|shell:allow-open" src-tauri/capabilities/default.json || echo "(capabilities not yet added)"
grep -n "tauri_plugin_os::init\|tauri_plugin_shell::init" src-tauri/src/lib.rs || echo "(plugin registration not yet added)"
```

Expected (pre-task state): all four "(...not yet added)" messages. If any are present, the plugin is partially installed — read state before modifying.

- [ ] **Step 2: Add Tauri plugins to `src-tauri/Cargo.toml`**

Locate the existing `[dependencies]` section's tauri-plugin-* entries (search for `tauri-plugin-`) and add adjacent:

```toml
tauri-plugin-os = "2"
tauri-plugin-shell = "2"
```

- [ ] **Step 3: Register plugins in `src-tauri/src/lib.rs`**

Locate the `tauri::Builder::default()` chain (grep `.plugin(tauri_plugin_`). Add adjacent to other `.plugin()` calls:

```rust
        .plugin(tauri_plugin_os::init())
        .plugin(tauri_plugin_shell::init())
```

- [ ] **Step 4: Add capabilities to `src-tauri/capabilities/default.json`**

Edit `permissions` array to add:

```json
    "os:default",
    "shell:allow-open"
```

After this step the array should look like:

```json
  "permissions": [
    "core:default",
    "core:webview:allow-create-webview-window",
    "dialog:default",
    "fs:allow-write-text-file",
    "updater:default",
    "deep-link:default",
    "os:default",
    "shell:allow-open"
  ]
```

- [ ] **Step 5: Add JS plugin deps to `package.json`**

Add to `dependencies` (alphabetical-adjacent to existing `@tauri-apps/plugin-*`):

```json
    "@tauri-apps/plugin-os": "~2",
    "@tauri-apps/plugin-shell": "~2",
```

Then run:
```bash
npm install
```

Expected: lockfile updates; no warnings about missing peer deps.

- [ ] **Step 6: Write TS types in `src/lib/types/onboarding.ts`**

```typescript
/**
 * ZEB-331 — Onboarding + first-run UX type definitions.
 *
 * Backend mirrors: `StartNodeResponse` matches the Rust struct of the
 * same name in `src-tauri/src/lib.rs` with `#[serde(rename_all = "camelCase")]`.
 *
 * Frontend-only: `EnvironmentInfo` is collected via `@tauri-apps/plugin-os`;
 * `FeedbackPayload` is consumed by `buildGitHubIssueUrl()` in onboarding-env.ts.
 */

/** Returned by `invoke('start_node', { endpoint })`. */
export interface StartNodeResponse {
  /** Self iroh node address (e.g. "iroh:..."). */
  nodeAddr: string;
  /**
   * True when the keychain identity was minted during this `start_node`
   * call (no prior entry existed); false when an existing entry was loaded.
   *
   * Forward-compat: callers MUST treat missing/undefined `freshlyCreated`
   * as `false` so older backends never accidentally re-show the welcome
   * modal.
   */
  freshlyCreated: boolean;
}

/** Non-identifying environment info attached to feedback submissions. */
export interface EnvironmentInfo {
  /** App version string from Tauri's `app.getVersion()`. */
  appVersion: string;
  /** Platform name from `@tauri-apps/plugin-os` `platform()` (e.g. "macos"). */
  platform: string;
  /** OS version string from `@tauri-apps/plugin-os` `version()`. */
  osVersion: string;
  /** ISO-8601 timestamp captured when the payload was built. */
  timestamp: string;
}

/** Input to `buildGitHubIssueUrl`. */
export interface FeedbackPayload {
  /** Verbatim description from the textarea (≥10 chars at submit time). */
  description: string;
  /** Environment info; degraded to `'unknown'` fields on plugin failure. */
  env: EnvironmentInfo;
  /**
   * Optional redacted markdown from `network_health_export_payload(false)`.
   * When undefined, the `## Network diagnostics` section is omitted entirely.
   */
  diagnostics?: string;
}
```

- [ ] **Step 7: Write failing tests in `src/lib/__tests__/onboarding-env.test.ts`**

```typescript
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
import type { EnvironmentInfo, FeedbackPayload } from '../types/onboarding';

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
```

- [ ] **Step 8: Run tests to confirm they fail**

Run from repo root:
```bash
npx vitest run src/lib/__tests__/onboarding-env.test.ts
```

Expected: ALL tests fail with `Cannot find module '../onboarding-env'` or similar — `onboarding-env.ts` doesn't exist yet.

- [ ] **Step 9: Implement `src/lib/onboarding-env.ts`**

```typescript
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
```

- [ ] **Step 10: Run tests to confirm they pass**

Run from repo root:
```bash
npx vitest run src/lib/__tests__/onboarding-env.test.ts
```

Expected: ALL pass (14 cases).

- [ ] **Step 11: Run frontend type-check**

```bash
npx tsc --noEmit
```

Expected: no new errors. (Pre-existing errors in unrelated files, if any, are not blocking.)

- [ ] **Step 12: Backend gates (plugin add only, no logic change)**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo fmt --all -- --check && echo "FMT EXIT=$?"
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -20
echo "CLIPPY EXIT=${PIPESTATUS[0]}"
```

Expected: fmt=0, clippy=0. The plugin add is dependency-only — no new code to lint. If clippy fails, the plugin version or registration is wrong.

- [ ] **Step 13: Commit**

```bash
cd "$(git rev-parse --show-toplevel)"
git add src/lib/types/onboarding.ts src/lib/onboarding-env.ts \
  src/lib/__tests__/onboarding-env.test.ts \
  src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/lib.rs \
  src-tauri/capabilities/default.json package.json package-lock.json
git commit -m "$(cat <<'EOF'
feat(zeb-331): onboarding types + env helpers + Tauri os/shell plugins

- src/lib/types/onboarding.ts: StartNodeResponse, EnvironmentInfo, FeedbackPayload
- src/lib/onboarding-env.ts: collectEnvironment() (best-effort, never throws)
  + buildGitHubIssueUrl() (8KB budget, diagnostics-truncation marker)
- 14 unit tests covering URL encoding, title truncation/newline-strip,
  diagnostics section omission, URL budget, env field degradation
- tauri-plugin-os + tauri-plugin-shell added; capabilities permit
  os:default + shell:allow-open
- @tauri-apps/plugin-os + @tauri-apps/plugin-shell JS deps

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: WelcomeModal component + tests

**Files:**
- Create: `src/lib/components/WelcomeModal.svelte`
- Create: `src/lib/components/__tests__/WelcomeModal.test.ts`

- [ ] **Step 1: Write failing tests in `WelcomeModal.test.ts`**

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import WelcomeModal from '../WelcomeModal.svelte';

describe('WelcomeModal', () => {
  it('renders when open=true', () => {
    render(WelcomeModal, {
      open: true,
      onDismiss: () => {},
      onJoinWithInvite: () => {},
    });
    expect(screen.getByTestId('welcome-modal')).toBeInTheDocument();
    expect(screen.getByText(/Welcome to Harmony alpha/i)).toBeInTheDocument();
  });

  it('does not render when open=false', () => {
    render(WelcomeModal, {
      open: false,
      onDismiss: () => {},
      onJoinWithInvite: () => {},
    });
    expect(screen.queryByTestId('welcome-modal')).toBeNull();
  });

  it('empty paste + "Join now" → inline error, modal stays', async () => {
    const onJoinWithInvite = vi.fn();
    const onDismiss = vi.fn();
    render(WelcomeModal, { open: true, onDismiss, onJoinWithInvite });
    await fireEvent.click(screen.getByTestId('welcome-join'));
    expect(screen.getByTestId('welcome-invite-error')).toHaveTextContent(
      /paste an invite url or click skip/i,
    );
    expect(onJoinWithInvite).not.toHaveBeenCalled();
    expect(onDismiss).not.toHaveBeenCalled();
  });

  it('malformed URL + "Join now" → inline error', async () => {
    const onJoinWithInvite = vi.fn();
    render(WelcomeModal, {
      open: true,
      onDismiss: () => {},
      onJoinWithInvite,
    });
    const input = screen.getByTestId('welcome-invite-input');
    await fireEvent.input(input, { target: { value: 'https://example.com' } });
    await fireEvent.click(screen.getByTestId('welcome-join'));
    expect(screen.getByTestId('welcome-invite-error')).toHaveTextContent(
      /doesn't look like a harmony:\/\/ invite/i,
    );
    expect(onJoinWithInvite).not.toHaveBeenCalled();
  });

  it('valid harmony:// URL + "Join now" → onJoinWithInvite + dismiss', async () => {
    const onJoinWithInvite = vi.fn();
    const onDismiss = vi.fn();
    render(WelcomeModal, { open: true, onDismiss, onJoinWithInvite });
    const input = screen.getByTestId('welcome-invite-input');
    const validUrl = 'harmony://invite/v1?p=test';
    await fireEvent.input(input, { target: { value: validUrl } });
    await fireEvent.click(screen.getByTestId('welcome-join'));
    await waitFor(() => expect(onJoinWithInvite).toHaveBeenCalledWith(validUrl));
  });

  it('"Skip for now" → onDismiss', async () => {
    const onDismiss = vi.fn();
    render(WelcomeModal, {
      open: true,
      onDismiss,
      onJoinWithInvite: () => {},
    });
    await fireEvent.click(screen.getByTestId('welcome-skip'));
    expect(onDismiss).toHaveBeenCalled();
  });

  it('Escape key → onDismiss', async () => {
    const onDismiss = vi.fn();
    render(WelcomeModal, {
      open: true,
      onDismiss,
      onJoinWithInvite: () => {},
    });
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(onDismiss).toHaveBeenCalled();
  });

  it('backdrop click → onDismiss', async () => {
    const onDismiss = vi.fn();
    render(WelcomeModal, {
      open: true,
      onDismiss,
      onJoinWithInvite: () => {},
    });
    await fireEvent.click(screen.getByTestId('welcome-modal-backdrop'));
    expect(onDismiss).toHaveBeenCalled();
  });

  it('renders feedback-docs footer link', () => {
    render(WelcomeModal, {
      open: true,
      onDismiss: () => {},
      onJoinWithInvite: () => {},
    });
    const link = screen.getByTestId('welcome-feedback-link');
    expect(link.tagName).toBe('A');
    expect(link.getAttribute('href')).toContain('feedback.md');
  });
});
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts
```

Expected: ALL fail with `Cannot find module '../WelcomeModal.svelte'`.

- [ ] **Step 3: Implement `src/lib/components/WelcomeModal.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-331 — First-run welcome modal (spec §4.1).
   *
   * Fires when `start_node` returns freshlyCreated=true (Flow 1).
   * Suppressed when a harmony:// deep-link is delivered during boot
   * (Flow 5 — handled by parent setting open=false in the deep-link
   * receiver).
   *
   * Uses the existing extractHarmonyInviteUrl validator from
   * deep-link-router so we don't drift from the canonical URL shape.
   */
  import { extractHarmonyInviteUrl } from '../deep-link-router';

  interface Props {
    open: boolean;
    onDismiss: () => void;
    onJoinWithInvite: (url: string) => void;
  }
  const { open, onDismiss, onJoinWithInvite }: Props = $props();

  let inviteUrl = $state('');
  let inviteError = $state<string | null>(null);

  function handleJoin() {
    const trimmed = inviteUrl.trim();
    if (trimmed.length === 0) {
      inviteError = 'Paste an invite URL or click Skip for now.';
      return;
    }
    const validated = extractHarmonyInviteUrl([trimmed]);
    if (validated === null) {
      inviteError = "That doesn't look like a harmony:// invite.";
      return;
    }
    inviteError = null;
    onJoinWithInvite(validated);
  }

  function handleBackdropClick(e: MouseEvent) {
    // Only fire when click landed on the backdrop, not on the modal body.
    if (e.target === e.currentTarget) {
      onDismiss();
    }
  }

  // Esc key listener — attached/removed based on `open`.
  $effect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') onDismiss();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
</script>

{#if open}
  <div
    class="modal-backdrop"
    data-testid="welcome-modal-backdrop"
    onclick={handleBackdropClick}
    role="presentation"
  >
    <div
      class="modal-content"
      data-testid="welcome-modal"
      role="dialog"
      aria-labelledby="welcome-title"
    >
      <h2 id="welcome-title">Welcome to Harmony alpha</h2>

      <p>
        Harmony is a federated chat where communities are self-governing.
        You're testing v0.1.0-alpha, so expect rough edges — please report
        issues via the <strong>(?)</strong> icon in the top-right.
      </p>

      <p>
        An identity has been created on this device. You can name yourself
        and customize your avatar in <strong>Settings → Profile</strong>
        whenever you like.
      </p>

      <div class="invite-section">
        <label for="welcome-invite-input">
          Have a <code>harmony://</code> invite?
        </label>
        <input
          id="welcome-invite-input"
          data-testid="welcome-invite-input"
          type="text"
          placeholder="harmony://invite/v1?..."
          bind:value={inviteUrl}
        />
        {#if inviteError}
          <p class="error" data-testid="welcome-invite-error">{inviteError}</p>
        {/if}
        <div class="actions">
          <button
            data-testid="welcome-join"
            class="primary"
            onclick={handleJoin}
          >
            Join now
          </button>
          <button data-testid="welcome-skip" onclick={onDismiss}>
            Skip for now
          </button>
        </div>
      </div>

      <footer>
        <a
          data-testid="welcome-feedback-link"
          href="https://github.com/zeblithic/harmony-client/blob/main/docs/feedback.md"
          target="_blank"
          rel="noopener noreferrer"
        >
          How to submit feedback →
        </a>
      </footer>
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 520px;
    width: 90%;
  }
  .modal-content h2 {
    margin: 0 0 1rem;
    font-size: 1.25rem;
  }
  .modal-content p {
    margin: 0 0 1rem;
    line-height: 1.5;
  }
  .invite-section {
    margin: 1.5rem 0 1rem;
    padding: 1rem;
    background: var(--bg-tertiary, #1f1f1f);
    border-radius: 4px;
  }
  .invite-section label {
    display: block;
    margin-bottom: 0.5rem;
    font-size: 0.9rem;
  }
  .invite-section input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem;
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    font-family: monospace;
    font-size: 0.85rem;
    margin-bottom: 0.5rem;
  }
  .actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
  .actions button.primary {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
  }
  .error {
    color: crimson;
    font-size: 0.85rem;
    margin: 0 0 0.5rem;
  }
  footer {
    margin-top: 1rem;
    font-size: 0.85rem;
  }
  footer a {
    color: var(--accent, #5865f2);
    text-decoration: none;
  }
  footer a:hover {
    text-decoration: underline;
  }
</style>
```

- [ ] **Step 4: Run tests to confirm they pass**

```bash
npx vitest run src/lib/components/__tests__/WelcomeModal.test.ts
```

Expected: ALL 9 pass.

- [ ] **Step 5: Type-check**

```bash
npx tsc --noEmit
```

Expected: no new errors.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/WelcomeModal.svelte src/lib/components/__tests__/WelcomeModal.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-331): WelcomeModal — first-run intro + invite paste

Hybrid welcome (spec §4.1). Title + 2-paragraph intro + optional
harmony:// invite paste field with Join now / Skip for now. Reuses
extractHarmonyInviteUrl validator from deep-link-router. Esc/backdrop/
Skip all dismiss; valid URL routes via onJoinWithInvite callback.

9 component tests cover render gate, validation paths, dismissal modes,
ARIA, footer feedback-docs link.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: HelpMenuButton component + tests

**Files:**
- Create: `src/lib/components/HelpMenuButton.svelte`
- Create: `src/lib/components/__tests__/HelpMenuButton.test.ts`

- [ ] **Step 1: Write failing tests in `HelpMenuButton.test.ts`**

```typescript
import { describe, it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import HelpMenuButton from '../HelpMenuButton.svelte';

function defaultProps() {
  return {
    onSubmitFeedback: vi.fn(),
    onShowAbout: vi.fn(),
    onOpenNetworkHealth: vi.fn(),
    onOpenDocs: vi.fn(),
  };
}

describe('HelpMenuButton', () => {
  it('renders the (?) button with aria-label', () => {
    render(HelpMenuButton, defaultProps());
    const button = screen.getByTestId('help-menu-button');
    expect(button.getAttribute('aria-label')).toBe('Help and feedback');
  });

  it('dropdown is hidden initially', () => {
    render(HelpMenuButton, defaultProps());
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('click → dropdown opens with 4 items in spec order', async () => {
    render(HelpMenuButton, defaultProps());
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    const items = screen.getAllByRole('menuitem');
    expect(items).toHaveLength(4);
    expect(items[0]).toHaveTextContent(/Submit Feedback/i);
    expect(items[1]).toHaveTextContent(/Network Health/i);
    expect(items[2]).toHaveTextContent(/About/i);
    expect(items[3]).toHaveTextContent(/Documentation/i);
  });

  it('click outside closes dropdown', async () => {
    render(HelpMenuButton, defaultProps());
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    expect(screen.getByTestId('help-menu-dropdown')).toBeInTheDocument();
    // Click on the document body
    await fireEvent.mouseDown(document.body);
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('Escape closes dropdown', async () => {
    render(HelpMenuButton, defaultProps());
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    expect(screen.getByTestId('help-menu-dropdown')).toBeInTheDocument();
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('Submit Feedback item → onSubmitFeedback + close', async () => {
    const props = defaultProps();
    render(HelpMenuButton, props);
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    await fireEvent.click(screen.getByTestId('help-menu-feedback'));
    expect(props.onSubmitFeedback).toHaveBeenCalled();
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('Network Health item → onOpenNetworkHealth + close', async () => {
    const props = defaultProps();
    render(HelpMenuButton, props);
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    await fireEvent.click(screen.getByTestId('help-menu-network'));
    expect(props.onOpenNetworkHealth).toHaveBeenCalled();
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('About item → onShowAbout + close', async () => {
    const props = defaultProps();
    render(HelpMenuButton, props);
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    await fireEvent.click(screen.getByTestId('help-menu-about'));
    expect(props.onShowAbout).toHaveBeenCalled();
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });

  it('Documentation item → onOpenDocs + close', async () => {
    const props = defaultProps();
    render(HelpMenuButton, props);
    await fireEvent.click(screen.getByTestId('help-menu-button'));
    await fireEvent.click(screen.getByTestId('help-menu-docs'));
    expect(props.onOpenDocs).toHaveBeenCalled();
    expect(screen.queryByTestId('help-menu-dropdown')).toBeNull();
  });
});
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
npx vitest run src/lib/components/__tests__/HelpMenuButton.test.ts
```

Expected: ALL fail with `Cannot find module '../HelpMenuButton.svelte'`.

- [ ] **Step 3: Implement `src/lib/components/HelpMenuButton.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-331 — Top-right (?) help/feedback button + dropdown (spec §4.3).
   *
   * Mounted in App.svelte at fixed position top-right.
   * Dropdown items in spec order: Submit Feedback / Network Health /
   * About / Documentation. Click-outside, Escape, and item-click all
   * close the dropdown.
   */

  interface Props {
    onSubmitFeedback: () => void;
    onShowAbout: () => void;
    onOpenNetworkHealth: () => void;
    onOpenDocs: () => void;
  }
  const { onSubmitFeedback, onShowAbout, onOpenNetworkHealth, onOpenDocs }: Props =
    $props();

  let dropdownOpen = $state(false);
  let containerEl: HTMLDivElement | undefined;

  function toggleDropdown() {
    dropdownOpen = !dropdownOpen;
  }

  function close() {
    dropdownOpen = false;
  }

  function handleFeedback() {
    close();
    onSubmitFeedback();
  }
  function handleNetwork() {
    close();
    onOpenNetworkHealth();
  }
  function handleAbout() {
    close();
    onShowAbout();
  }
  function handleDocs() {
    close();
    onOpenDocs();
  }

  // Click-outside / Escape listeners — attached only while dropdown open
  // to avoid pollution and to allow other (?)-like buttons to coexist.
  $effect(() => {
    if (!dropdownOpen) return;
    function onMouseDown(e: MouseEvent) {
      if (containerEl && !containerEl.contains(e.target as Node)) {
        close();
      }
    }
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') close();
    }
    document.addEventListener('mousedown', onMouseDown);
    window.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onMouseDown);
      window.removeEventListener('keydown', onKey);
    };
  });
</script>

<div class="help-container" bind:this={containerEl}>
  <button
    type="button"
    class="help-button"
    data-testid="help-menu-button"
    aria-label="Help and feedback"
    aria-haspopup="menu"
    aria-expanded={dropdownOpen}
    onclick={toggleDropdown}
  >
    ?
  </button>
  {#if dropdownOpen}
    <div
      class="help-dropdown"
      data-testid="help-menu-dropdown"
      role="menu"
    >
      <button
        type="button"
        role="menuitem"
        data-testid="help-menu-feedback"
        onclick={handleFeedback}
      >
        Submit Feedback
      </button>
      <button
        type="button"
        role="menuitem"
        data-testid="help-menu-network"
        onclick={handleNetwork}
      >
        Network Health
      </button>
      <button
        type="button"
        role="menuitem"
        data-testid="help-menu-about"
        onclick={handleAbout}
      >
        About
      </button>
      <button
        type="button"
        role="menuitem"
        data-testid="help-menu-docs"
        onclick={handleDocs}
      >
        Documentation
      </button>
    </div>
  {/if}
</div>

<style>
  .help-container {
    position: relative;
    display: inline-block;
  }
  .help-button {
    width: 32px;
    height: 32px;
    border-radius: 50%;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    font-size: 1rem;
    font-weight: bold;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .help-button:hover {
    background: var(--bg-secondary, #2a2a2a);
  }
  .help-button:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 2px;
  }
  .help-dropdown {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    background: var(--bg-secondary, #2a2a2a);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    min-width: 180px;
    z-index: 100;
    display: flex;
    flex-direction: column;
    padding: 4px 0;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
  }
  .help-dropdown button {
    padding: 8px 12px;
    border: none;
    background: transparent;
    color: var(--text-primary, #fff);
    text-align: left;
    cursor: pointer;
    font-size: 0.875rem;
  }
  .help-dropdown button:hover {
    background: var(--bg-tertiary, #1f1f1f);
  }
  .help-dropdown button:focus-visible {
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: -2px;
  }
</style>
```

- [ ] **Step 4: Run tests to confirm they pass**

```bash
npx vitest run src/lib/components/__tests__/HelpMenuButton.test.ts
```

Expected: ALL 9 pass.

- [ ] **Step 5: Type-check**

```bash
npx tsc --noEmit
```

Expected: no new errors.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/HelpMenuButton.svelte src/lib/components/__tests__/HelpMenuButton.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-331): HelpMenuButton — (?) icon + dropdown menu

Spec §4.3 — 4 items in canonical order: Submit Feedback / Network
Health / About / Documentation. Click-outside, Escape, item-click
all close. ARIA: aria-label="Help and feedback", aria-haspopup=menu,
aria-expanded reflects state, role=menu + role=menuitem on items.

9 tests cover render, dropdown open/close paths, callback invocation,
keyboard dismissal.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: FeedbackModal component + tests (privacy invariant FIRST)

**Files:**
- Create: `src/lib/components/FeedbackModal.svelte`
- Create: `src/lib/components/__tests__/FeedbackModal.test.ts`

> **Critical:** Per `feedback_second_order_correctness_review` + ZEB-329 R3 pattern: the privacy-invariant test (URL with toggle ON does NOT match `/[0-9a-f]{32,}/`) MUST be written FIRST, before the diagnostics-attach code is wired up. The redaction invariant is security-adjacent — if it breaks, full Ed25519 hex leaks into GitHub URLs.

- [ ] **Step 1: Write failing tests in `FeedbackModal.test.ts`**

```typescript
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(),
}));
vi.mock('@tauri-apps/plugin-shell', () => ({
  open: vi.fn(),
}));
vi.mock('@tauri-apps/plugin-os', () => ({
  platform: vi.fn().mockResolvedValue('macos'),
  version: vi.fn().mockResolvedValue('15.0'),
}));
vi.mock('@tauri-apps/api/app', () => ({
  getVersion: vi.fn().mockResolvedValue('0.1.0-alpha.1'),
}));

import { invoke } from '@tauri-apps/api/core';
import { open as shellOpen } from '@tauri-apps/plugin-shell';
import FeedbackModal from '../FeedbackModal.svelte';

const REDACTED_FIXTURE = `## Harmony v0.1.0-alpha.1 (darwin/aarch64)
## Network: reachable
a3f9e1c2… direct 18ms`;

describe('FeedbackModal', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('renders textarea + toggle (off default) + Submit/Cancel', () => {
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    expect(screen.getByTestId('feedback-description')).toBeInTheDocument();
    const toggle = screen.getByTestId('feedback-attach-toggle') as HTMLInputElement;
    expect(toggle.checked).toBe(false);
    expect(screen.getByTestId('feedback-submit')).toBeInTheDocument();
    expect(screen.getByTestId('feedback-cancel')).toBeInTheDocument();
  });

  it('Submit disabled when description < 10 chars', async () => {
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    const textarea = screen.getByTestId('feedback-description');
    const submit = screen.getByTestId('feedback-submit') as HTMLButtonElement;
    expect(submit.disabled).toBe(true);
    await fireEvent.input(textarea, { target: { value: 'too short' } });
    expect(submit.disabled).toBe(true);
    await fireEvent.input(textarea, { target: { value: 'this is long enough' } });
    expect(submit.disabled).toBe(false);
  });

  it('toggle ON fetches network_health_export_payload(includeFullIds:false) + shows preview', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    const toggle = screen.getByTestId('feedback-attach-toggle');
    await fireEvent.click(toggle);
    await waitFor(() => screen.getByTestId('feedback-diagnostics-preview'));
    expect(invoke).toHaveBeenCalledWith('network_health_export_payload', {
      includeFullIds: false,
    });
    expect(screen.getByTestId('feedback-diagnostics-preview')).toHaveTextContent(
      /Harmony v0.1.0-alpha.1/,
    );
  });

  it('toggle OFF hides preview', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    const toggle = screen.getByTestId('feedback-attach-toggle');
    await fireEvent.click(toggle);
    await waitFor(() => screen.getByTestId('feedback-diagnostics-preview'));
    await fireEvent.click(toggle);
    expect(screen.queryByTestId('feedback-diagnostics-preview')).toBeNull();
  });

  it('Submit without diagnostics → shell.open URL omits ## Network diagnostics', async () => {
    const onDismiss = vi.fn();
    render(FeedbackModal, { open: true, onDismiss });
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'test feedback message' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    const url = (shellOpen as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    const body = decodeURIComponent(url.split('body=')[1]);
    expect(body).toContain('## Description');
    expect(body).toContain('test feedback message');
    expect(body).not.toContain('## Network diagnostics');
    await waitFor(() => expect(onDismiss).toHaveBeenCalled());
  });

  it('Submit with diagnostics → URL contains redacted markdown', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.click(screen.getByTestId('feedback-attach-toggle'));
    await waitFor(() => screen.getByTestId('feedback-diagnostics-preview'));
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'with diagnostics attached' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    const url = (shellOpen as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    const body = decodeURIComponent(url.split('body=')[1]);
    expect(body).toContain('## Network diagnostics');
    expect(body).toContain('Harmony v0.1.0-alpha.1');
  });

  it('PRIVACY INVARIANT: URL with toggle ON contains NO full Ed25519 hex', async () => {
    // Backend returns redacted markdown (ellipsized addresses).
    (invoke as ReturnType<typeof vi.fn>).mockResolvedValue(REDACTED_FIXTURE);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.click(screen.getByTestId('feedback-attach-toggle'));
    await waitFor(() => screen.getByTestId('feedback-diagnostics-preview'));
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'privacy regression test' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    const url = (shellOpen as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    const decoded = decodeURIComponent(url);
    // No 32+ char lowercase hex run anywhere in the URL.
    expect(decoded).not.toMatch(/[0-9a-f]{32,}/);
  });

  it('shell.open rejects → URL copied to clipboard + toast shown', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    (shellOpen as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('shell unavailable'));
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'test for clipboard fallback' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(writeText).toHaveBeenCalled());
    expect(screen.getByTestId('feedback-toast')).toHaveTextContent(/clipboard/i);
  });

  it('network_health_export_payload rejects → "Diagnostics unavailable" + Submit still works', async () => {
    (invoke as ReturnType<typeof vi.fn>).mockRejectedValue(new Error('not ready'));
    (shellOpen as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.click(screen.getByTestId('feedback-attach-toggle'));
    await waitFor(() =>
      expect(screen.getByTestId('feedback-diagnostics-error')).toHaveTextContent(
        /diagnostics unavailable/i,
      ),
    );
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'submit despite diagnostic fetch failure' },
    });
    await fireEvent.click(screen.getByTestId('feedback-submit'));
    await waitFor(() => expect(shellOpen).toHaveBeenCalled());
    // URL must NOT include the diagnostics section because the fetch failed.
    const url = (shellOpen as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    const body = decodeURIComponent(url.split('body=')[1]);
    expect(body).not.toContain('## Network diagnostics');
  });

  it('stale-response guard: rapid toggle → only latest response reflected', async () => {
    let resolvers: Array<(v: string) => void> = [];
    (invoke as ReturnType<typeof vi.fn>).mockImplementation(
      () => new Promise<string>((resolve) => resolvers.push(resolve)),
    );
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    const toggle = screen.getByTestId('feedback-attach-toggle');
    // First click ON → invoke #1 pending
    await fireEvent.click(toggle);
    // Second click OFF → no invoke (toggle just hides preview)
    await fireEvent.click(toggle);
    // Third click ON → invoke #2 pending
    await fireEvent.click(toggle);
    // Resolve OLDEST first (stale), then NEWEST (latest)
    resolvers[0]('STALE_CONTENT');
    resolvers[1]('FRESH_CONTENT');
    await waitFor(() =>
      expect(screen.getByTestId('feedback-diagnostics-preview')).toHaveTextContent('FRESH_CONTENT'),
    );
    expect(screen.getByTestId('feedback-diagnostics-preview')).not.toHaveTextContent('STALE_CONTENT');
  });

  it('submitting flag disables Submit during shell.open', async () => {
    let resolveShellOpen!: () => void;
    (shellOpen as ReturnType<typeof vi.fn>).mockImplementation(
      () => new Promise<void>((resolve) => { resolveShellOpen = resolve; }),
    );
    render(FeedbackModal, { open: true, onDismiss: () => {} });
    await fireEvent.input(screen.getByTestId('feedback-description'), {
      target: { value: 'submitting flag test' },
    });
    const submit = screen.getByTestId('feedback-submit') as HTMLButtonElement;
    await fireEvent.click(submit);
    expect(submit.disabled).toBe(true);
    resolveShellOpen();
  });

  it('Cancel → onDismiss', async () => {
    const onDismiss = vi.fn();
    render(FeedbackModal, { open: true, onDismiss });
    await fireEvent.click(screen.getByTestId('feedback-cancel'));
    expect(onDismiss).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
npx vitest run src/lib/components/__tests__/FeedbackModal.test.ts
```

Expected: ALL fail with `Cannot find module '../FeedbackModal.svelte'`.

- [ ] **Step 3: Implement `src/lib/components/FeedbackModal.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-331 — Submit Feedback modal (spec §4.2).
   *
   * Description textarea + optional "Attach network diagnostics" toggle
   * → opens browser to pre-filled GitHub new-issue URL via shell.open.
   *
   * Privacy invariant: diagnostics path goes through
   * network_health_export_payload(includeFullIds=false) — server-side
   * redactor from ZEB-329 R3. No new code path can leak full Ed25519 hex.
   *
   * Stale-response guard: `latestRequest` is a plain `let` (NOT $state)
   * to avoid effect_update_depth_exceeded. Matches the
   * DiagnosticExportModal pattern from ZEB-329 R1.
   */
  import { invoke } from '@tauri-apps/api/core';
  import { open as shellOpen } from '@tauri-apps/plugin-shell';
  import { collectEnvironment, buildGitHubIssueUrl } from '../onboarding-env';
  import type { FeedbackPayload } from '../types/onboarding';

  interface Props {
    open: boolean;
    onDismiss: () => void;
  }
  const { open, onDismiss }: Props = $props();

  const MIN_DESCRIPTION_LEN = 10;

  let description = $state('');
  let attachDiagnostics = $state(false);
  let submitting = $state(false);
  let diagnosticsPreview = $state<string | null>(null);
  let diagnosticsError = $state<string | null>(null);
  let toastMsg = $state<string | null>(null);

  // Plain `let` — NOT $state. This is internal control-flow bookkeeping
  // for the stale-response guard, not UI state. Wrapping in $state would
  // cause the $effect below to re-fire on every load() and produce
  // effect_update_depth_exceeded. See DiagnosticExportModal.svelte for
  // the same pattern (ZEB-329 R1).
  let latestRequest = 0;

  let submitDisabled = $derived(description.length < MIN_DESCRIPTION_LEN || submitting);

  async function loadDiagnostics() {
    const requestId = ++latestRequest;
    diagnosticsPreview = null;
    diagnosticsError = null;
    try {
      const result = (await invoke('network_health_export_payload', {
        includeFullIds: false,
      })) as string;
      if (requestId !== latestRequest) return;
      diagnosticsPreview = result;
    } catch (e) {
      if (requestId !== latestRequest) return;
      diagnosticsError =
        'Diagnostics unavailable — submit without?';
      // Underlying error captured for console only:
      console.warn(
        '[zeb-331] network_health_export_payload failed:',
        e instanceof Error ? e.message : String(e),
      );
    }
  }

  // Fire load when the toggle flips ON. Bumping latestRequest in the
  // OFF branch ensures any in-flight ON request can't sneak its
  // result into the now-hidden preview pane.
  $effect(() => {
    if (attachDiagnostics) {
      void loadDiagnostics();
    } else {
      latestRequest++;
      diagnosticsPreview = null;
      diagnosticsError = null;
    }
  });

  async function handleSubmit() {
    if (submitDisabled) return;
    submitting = true;
    toastMsg = null;
    try {
      const env = await collectEnvironment();
      const payload: FeedbackPayload = {
        description,
        env,
        ...(attachDiagnostics && diagnosticsPreview
          ? { diagnostics: diagnosticsPreview }
          : {}),
      };
      const url = buildGitHubIssueUrl(payload);
      try {
        await shellOpen(url);
        onDismiss();
      } catch (e) {
        // shell.open failure → clipboard fallback
        console.warn(
          '[zeb-331] shell.open failed:',
          e instanceof Error ? e.message : String(e),
        );
        try {
          await navigator.clipboard.writeText(url);
          toastMsg =
            "Couldn't open browser. URL copied to clipboard — paste it in your browser.";
        } catch (clipErr) {
          toastMsg = `Couldn't open browser or copy: ${
            clipErr instanceof Error ? clipErr.message : String(clipErr)
          }`;
        }
      }
    } finally {
      submitting = false;
    }
  }

  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) {
      onDismiss();
    }
  }

  // Esc dismiss while open.
  $effect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') onDismiss();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
</script>

{#if open}
  <div
    class="modal-backdrop"
    data-testid="feedback-modal-backdrop"
    onclick={handleBackdropClick}
    role="presentation"
  >
    <div
      class="modal-content"
      data-testid="feedback-modal"
      role="dialog"
      aria-labelledby="feedback-title"
    >
      <h2 id="feedback-title">Submit feedback</h2>

      <label class="field-label" for="feedback-description">
        Describe what happened, what you expected, and what you saw.
      </label>
      <textarea
        id="feedback-description"
        data-testid="feedback-description"
        rows="6"
        bind:value={description}
        placeholder="Steps to reproduce, expected behavior, actual behavior…"
      ></textarea>

      <label class="toggle-row">
        <input
          type="checkbox"
          data-testid="feedback-attach-toggle"
          bind:checked={attachDiagnostics}
        />
        Attach network diagnostics (redacted — no full identifiers)
      </label>

      {#if attachDiagnostics}
        {#if diagnosticsPreview !== null}
          <pre
            class="diagnostics-preview"
            data-testid="feedback-diagnostics-preview"
          >{diagnosticsPreview}</pre>
        {:else if diagnosticsError !== null}
          <p class="error" data-testid="feedback-diagnostics-error">
            {diagnosticsError}
          </p>
        {:else}
          <p class="loading">Loading diagnostics…</p>
        {/if}
      {/if}

      {#if toastMsg !== null}
        <p class="toast" data-testid="feedback-toast">{toastMsg}</p>
      {/if}

      <div class="actions">
        <button
          type="button"
          data-testid="feedback-cancel"
          onclick={onDismiss}
          disabled={submitting}
        >
          Cancel
        </button>
        <button
          type="button"
          class="primary"
          data-testid="feedback-submit"
          onclick={handleSubmit}
          disabled={submitDisabled}
        >
          {submitting ? 'Submitting…' : 'Submit'}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 640px;
    width: 90%;
    max-height: 80vh;
    overflow-y: auto;
  }
  .modal-content h2 {
    margin: 0 0 1rem;
    font-size: 1.25rem;
  }
  .field-label {
    display: block;
    margin: 0 0 0.5rem;
    font-size: 0.9rem;
  }
  textarea {
    width: 100%;
    box-sizing: border-box;
    padding: 0.5rem;
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    font-family: monospace;
    font-size: 0.85rem;
    resize: vertical;
  }
  .toggle-row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 1rem 0;
    font-size: 0.9rem;
  }
  .diagnostics-preview {
    background: var(--bg-primary, #111);
    color: var(--text-primary, #fff);
    padding: 0.75rem;
    border-radius: 4px;
    max-height: 240px;
    overflow-y: auto;
    white-space: pre-wrap;
    font-size: 0.75rem;
  }
  .error {
    color: crimson;
    margin: 0.5rem 0;
    font-size: 0.85rem;
  }
  .loading {
    font-size: 0.85rem;
    color: var(--text-secondary, #aaa);
    margin: 0.5rem 0;
  }
  .toast {
    background: var(--bg-tertiary, #1f1f1f);
    padding: 0.5rem 0.75rem;
    border-radius: 4px;
    margin-top: 0.5rem;
    font-size: 0.85rem;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    margin-top: 1rem;
  }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
  .actions button.primary {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
  }
  .actions button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
```

- [ ] **Step 4: Run tests to confirm they pass**

```bash
npx vitest run src/lib/components/__tests__/FeedbackModal.test.ts
```

Expected: ALL 12 pass — including the privacy invariant `/[0-9a-f]{32,}/` regression test.

- [ ] **Step 5: Type-check**

```bash
npx tsc --noEmit
```

Expected: no new errors.

- [ ] **Step 6: Commit**

```bash
git add src/lib/components/FeedbackModal.svelte src/lib/components/__tests__/FeedbackModal.test.ts
git commit -m "$(cat <<'EOF'
feat(zeb-331): FeedbackModal — GitHub-issue prefill + optional diagnostics

Spec §4.2 — description textarea (≥10 chars) + "Attach network
diagnostics" toggle (default off) → builds GitHub URL via
buildGitHubIssueUrl + tauri-plugin-shell.open(url). Fallback to
navigator.clipboard.writeText + toast on shell.open failure.

Diagnostics path uses network_health_export_payload(includeFullIds=
false) — ZEB-329 R3 server-side redactor invariant unchanged.
Stale-response guard via plain `let latestRequest = 0` (NOT $state)
matches DiagnosticExportModal pattern from ZEB-329 R1.

12 tests cover: render, Submit-disabled gating, toggle on/off
preview show/hide, with/without-diagnostics submit, shell.open
failure → clipboard fallback, diagnostics-fetch failure path,
**privacy-invariant regression** (URL contains no /[0-9a-f]{32,}/),
stale-response guard regression, submitting flag, Cancel.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Backend `start_node` returns `StartNodeResponse` + tests

**Files:**
- Modify: `src-tauri/src/iroh_endpoint.rs` (`load_or_create_secret_key` returns `(SecretKey, bool)`)
- Modify: `src-tauri/src/lib.rs` (`start_node` return type + `StartNodeResponse` struct)

- [ ] **Step 1: Read the existing `load_or_create_secret_key` site**

Already gathered during plan-writing: `src-tauri/src/iroh_endpoint.rs:197-234`. The `Err(keyring::Error::NoEntry)` branch at line 216 is the "freshly minted" path. The `Ok(bytes)` branch at line 205 is the "loaded existing" path.

- [ ] **Step 2: Modify `load_or_create_secret_key` to return `(SecretKey, bool /* freshly_created */)`**

In `src-tauri/src/iroh_endpoint.rs`, change the function signature + return values:

```rust
/// Load a persisted iroh `SecretKey` from the OS keychain, or generate
/// and persist a fresh one on first run.
///
/// Returns `(secret_key, freshly_created)` so callers can know whether a
/// new identity was just minted (true) or an existing entry was loaded
/// (false). The `freshly_created` flag drives the first-run welcome
/// modal in `start_node` (ZEB-331).
///
/// On keychain read failure we surface [`IrohEndpointError::Keychain`]
/// rather than silently re-generating — losing the secret key changes
/// our [`EndpointId`], breaking any peer that knew us by the old id.
pub fn load_or_create_secret_key() -> Result<(SecretKey, bool), IrohEndpointError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).map_err(|e| {
        IrohEndpointError::Keychain {
            context: format!("entry creation {KEYCHAIN_SERVICE}/{KEYCHAIN_USER}"),
            source: e,
        }
    })?;
    match entry.get_secret() {
        Ok(bytes) => {
            // Wrap the keychain payload in Zeroizing so the heap copy is
            // wiped on drop — see identity.rs for the canonical pattern.
            let bytes = Zeroizing::new(bytes);
            if bytes.len() != 32 {
                return Err(IrohEndpointError::KeychainBadLength { len: bytes.len() });
            }
            let mut arr: Zeroizing<[u8; 32]> = Zeroizing::new([0u8; 32]);
            arr.copy_from_slice(&bytes);
            Ok((SecretKey::from_bytes(&arr), false))
        }
        Err(keyring::Error::NoEntry) => {
            let key = SecretKey::generate();
            // Snapshot the secret bytes in a Zeroizing buffer so any
            // intermediate stack copy is wiped after the keychain write.
            let key_bytes: Zeroizing<[u8; 32]> = Zeroizing::new(key.to_bytes());
            entry
                .set_secret(key_bytes.as_ref())
                .map_err(|e| IrohEndpointError::Keychain {
                    context: format!("keychain write {KEYCHAIN_SERVICE}/{KEYCHAIN_USER}"),
                    source: e,
                })?;
            Ok((key, true))
        }
        Err(e) => Err(IrohEndpointError::Keychain {
            context: format!("keychain read {KEYCHAIN_SERVICE}/{KEYCHAIN_USER}"),
            source: e,
        }),
    }
}
```

- [ ] **Step 3: Enumerate + update callers of `load_or_create_secret_key`**

```bash
grep -rn "load_or_create_secret_key" src-tauri/src/
```

Each caller must be updated to destructure the tuple. The expected call sites are in `lib.rs` (the `start_node` body) and possibly tests in `iroh_endpoint.rs`'s `#[cfg(test)]` block.

For each caller in `src-tauri/src/`, replace patterns like:
```rust
let secret = load_or_create_secret_key()?;
```
With:
```rust
let (secret, _freshly_created) = load_or_create_secret_key()?;
```

Except the `start_node` site (next step) which captures the boolean.

- [ ] **Step 4: Add `StartNodeResponse` + update `start_node` return type in `src-tauri/src/lib.rs`**

Locate `start_node` (line ~1665) and any existing `Result<(), String>` return. Add the response struct near other DTOs (search for `#[derive(Serialize)]` near the top of the file for stylistic placement):

```rust
/// Response from `start_node` IPC. Frontend reads `freshly_created` to
/// decide whether to show the first-run welcome modal (ZEB-331).
///
/// Forward-compat: callers MUST treat a missing `freshlyCreated` field as
/// `false`, so an older backend mid-deploy never spuriously re-fires the
/// welcome modal for returning users.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartNodeResponse {
    pub node_addr: String,
    pub freshly_created: bool,
}
```

Change `start_node`'s return type:

```rust
#[tauri::command]
async fn start_node(
    endpoint: Option<String>,
    app: AppHandle,
    state: tauri::State<'_, Mutex<NodeState>>,
) -> Result<StartNodeResponse, String> {
```

At the call site of `load_or_create_secret_key()` inside `start_node`, capture the `freshly_created` bool:

```rust
    let (secret_key, freshly_created) = iroh_endpoint::load_or_create_secret_key()
        .map_err(|e| format!("load_or_create_secret_key: {e}"))?;
```

(The exact line context — where `load_or_create_secret_key()?` is currently called inside `start_node` — needs to be located with grep; the destructure replaces the existing single binding.)

At the `Ok(...)` return at the end of `start_node`, build the response:

```rust
    let node_addr = format!("{}", endpoint_id);
    // ^ or whatever the existing node-addr string-building logic looks like.
    // If there isn't already a node_addr string, build one from the iroh
    // endpoint's id at this point.
    Ok(StartNodeResponse {
        node_addr,
        freshly_created,
    })
```

> **Implementer note**: `start_node` is a long function. The previous return value was likely `Ok(())` at multiple early-return points. Each `Ok(())` must be replaced with the structured response. If multiple early returns made sense as `()`, evaluate whether they should ALL emit a real `StartNodeResponse` or whether they can be collapsed to a single tail return. If splitting is complex, ship a `_` placeholder node_addr only for the genuinely-error-path early returns and document.

- [ ] **Step 5: Add serialization test**

Append to existing tests in `src-tauri/src/lib.rs` (or wherever similar IPC-response DTOs are tested):

```rust
#[cfg(test)]
mod start_node_response_tests {
    use super::*;

    #[test]
    fn start_node_response_serializes_to_camel_case() {
        let r = StartNodeResponse {
            node_addr: "iroh:abc123".to_string(),
            freshly_created: true,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            json.contains("\"freshlyCreated\":true"),
            "expected camelCase freshlyCreated in {json}",
        );
        assert!(
            json.contains("\"nodeAddr\":\"iroh:abc123\""),
            "expected camelCase nodeAddr in {json}",
        );
        // Reject snake_case in the JSON — that would mean serde rename
        // didn't apply.
        assert!(!json.contains("freshly_created"));
        assert!(!json.contains("node_addr"));
    }

    #[test]
    fn start_node_response_freshly_created_false_serializes() {
        let r = StartNodeResponse {
            node_addr: "iroh:xyz".to_string(),
            freshly_created: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"freshlyCreated\":false"));
    }
}
```

- [ ] **Step 6: Add `load_or_create_secret_key` freshness test (if mock keychain available, otherwise document)**

```bash
grep -rn "set_default_credential_builder\|MockCredentialBuilder" src-tauri/src/
```

If results found: write a `#[cfg(test)]` test in `src-tauri/src/iroh_endpoint.rs` that mocks the keyring and asserts `(SecretKey, true)` on NoEntry + `(SecretKey, false)` on Ok.

If NOT found (current state per Task 0 grep): document the limitation in `start_node`'s doc comment, add a serial_test-based real-keychain test only if running on a CI runner where the keychain is wipable (skip on macOS local), and rely on the manual smoke checklist (Task 10) for end-to-end validation.

Documenting limitation (add to `iroh_endpoint.rs` near `load_or_create_secret_key`):

```rust
// The freshness behavior is unit-tested only at the serialization
// boundary (StartNodeResponse). The keychain branch — that the
// `Err(keyring::Error::NoEntry)` arm produces `freshly_created=true` and
// the `Ok(bytes)` arm produces `freshly_created=false` — is verified by
// the Task 10 manual smoke checklist (deleting the keychain entry and
// confirming the welcome modal fires) until a mock-keyring abstraction
// is introduced.
```

- [ ] **Step 7: Run backend tests**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --features test-fixtures -E 'test(start_node_response)' 2>&1 | tee /tmp/zeb-331-task-5-tests.log
echo "EXIT=${PIPESTATUS[0]}"
```

Expected: both serialization tests pass.

- [ ] **Step 8: Run full backend gate**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo fmt --all -- --check && echo "FMT OK"
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tail -30
echo "CLIPPY EXIT=${PIPESTATUS[0]}"
```

Expected: fmt=0, clippy=0. If clippy flags unused `freshly_created` variable in any non-start_node caller, ensure `_freshly_created` prefix.

If clippy or fmt take longer than 10 minutes → report DONE_WITH_CONCERNS; otherwise proceed.

- [ ] **Step 9: Commit**

```bash
cd "$(git rev-parse --show-toplevel)"
git add src-tauri/src/iroh_endpoint.rs src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(zeb-331): start_node returns StartNodeResponse {nodeAddr, freshlyCreated}

Backend change behind ZEB-331's first-run welcome trigger.

- iroh_endpoint::load_or_create_secret_key returns (SecretKey, bool)
  where the bool is true on the keyring::Error::NoEntry branch (fresh
  mint) and false on the Ok branch (existing keychain entry).
- start_node returns Result<StartNodeResponse, String> with
  #[serde(rename_all = "camelCase")] so the JS side reads
  { nodeAddr, freshlyCreated }.
- Serialization tests pin the camelCase wire shape.

Forward-compat: a missing freshlyCreated field in older-backend
responses must default-to-false on the JS side (handled in Task 6).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: App.svelte boot-sequence integration + deep-link suppression

**Files:**
- Modify: `src/App.svelte` (line ~668 `start_node` call + line ~809 deep-link handler + new $state + WelcomeModal mount)

- [ ] **Step 1: Locate the existing `invoke('start_node')` call**

```bash
grep -n "invoke.*'start_node'" src/App.svelte
```

Expected: one match at line 668 (per Task 0 enumeration).

- [ ] **Step 2: Locate the deep-link receiver**

```bash
grep -n "deep-link-received\|extractHarmonyInviteUrl" src/App.svelte
```

Expected: handler around line 809 (per the context gathered at plan-write).

- [ ] **Step 3: Add `showWelcomeModal` + `feedbackModalOpen` + `aboutModalOpen` $state declarations**

Find an appropriate location near other $state declarations in `src/App.svelte` (search for `let availableUpdate = $state` around line 231 — that block is the ZEB-328 update-toast state). Add adjacent:

```svelte
  // ── ZEB-331: first-run welcome + feedback + about ─────────────────
  let showWelcomeModal = $state(false);
  let feedbackModalOpen = $state(false);
  let aboutModalOpen = $state(false);
```

- [ ] **Step 4: Import the new components + StartNodeResponse type**

Near the existing component imports at the top of `<script>`:

```svelte
  import WelcomeModal from './lib/components/WelcomeModal.svelte';
  import FeedbackModal from './lib/components/FeedbackModal.svelte';
  import HelpMenuButton from './lib/components/HelpMenuButton.svelte';
  import AboutModal from './lib/components/AboutModal.svelte';
  import type { StartNodeResponse } from './lib/types/onboarding';
```

- [ ] **Step 5: Update the `start_node` call to destructure the response**

Replace the existing line 668 area:

```svelte
      try {
        await invoke('start_node', { endpoint: null });
      } catch (err) {
        console.warn('[harmony-client] auto-start_node failed:', err);
      }
```

With:

```svelte
      try {
        const response = (await invoke('start_node', { endpoint: null })) as
          | StartNodeResponse
          | undefined
          | null;
        // Forward-compat: an older backend that returned bare void / null
        // produces undefined here; treat as freshly_created=false (privacy-
        // safe default — never re-show the welcome to a returning user).
        if (response?.freshlyCreated === true) {
          showWelcomeModal = true;
        }
      } catch (err) {
        console.warn('[harmony-client] auto-start_node failed:', err);
      }
```

- [ ] **Step 6: Suppress welcome modal when deep-link arrives**

Find the existing deep-link handler around line 809-816:

```svelte
      unlistenDeepLink = await listen<string[]>('deep-link-received', (event) => {
        const url = extractHarmonyInviteUrl(event.payload);
        if (url) {
          redeemUrl = url;
          redeemError = null;
          showRedeemInvite = true;
        }
      });
```

Modify to:

```svelte
      unlistenDeepLink = await listen<string[]>('deep-link-received', (event) => {
        const url = extractHarmonyInviteUrl(event.payload);
        if (url) {
          redeemUrl = url;
          redeemError = null;
          showRedeemInvite = true;
          // ZEB-331 Flow 5: deep-link wins over welcome modal. The user
          // has already taken an explicit action (clicking the harmony://
          // link); stacking the welcome in front of that would be
          // jarring.
          showWelcomeModal = false;
        }
      });
```

Apply the same `showWelcomeModal = false` line in the cold-launch `getCurrentDeepLink()` branch (around line 822-836):

```svelte
        if (queued) {
          const url = extractHarmonyInviteUrl(queued);
          if (url) {
            redeemUrl = url;
            redeemError = null;
            showRedeemInvite = true;
            // ZEB-331 Flow 5: deep-link wins (see also the listen()
            // handler above).
            showWelcomeModal = false;
          }
        }
```

- [ ] **Step 7: Mount `WelcomeModal` in the App template**

Find where other modals are rendered in App.svelte template (search for `RedeemInviteDialog` or `DiagnosticExportModal` in the template — they're likely conditional). Add adjacent (the exact position depends on z-index requirements; placing it near the existing modal stack is fine):

```svelte
<WelcomeModal
  open={showWelcomeModal}
  onDismiss={() => (showWelcomeModal = false)}
  onJoinWithInvite={(url) => {
    redeemUrl = url;
    redeemError = null;
    showRedeemInvite = true;
    showWelcomeModal = false;
  }}
/>
```

- [ ] **Step 8: Verify (no test yet — App.svelte has no direct tests; rely on type-check + manual smoke later)**

```bash
npx tsc --noEmit
```

Expected: no new errors. If TS complains about `StartNodeResponse` shape mismatch, verify the import path matches `src/lib/types/onboarding.ts`.

- [ ] **Step 9: Run vitest to confirm no regression in component tests**

```bash
npx vitest run
```

Expected: all tests (existing + ZEB-331's so far) pass. No `App.svelte`-shaped test regressions because there are no App.svelte tests.

- [ ] **Step 10: Commit**

```bash
git add src/App.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-331): App.svelte first-run welcome wiring + deep-link suppression

- start_node response destructured into StartNodeResponse; sets
  showWelcomeModal=true only when freshlyCreated=true (forward-compat
  default false on missing field).
- WelcomeModal mounted; "Join now" with valid harmony:// URL routes
  through the existing RedeemInviteDialog path (reuses Phase 2c
  handshake).
- Deep-link receiver (both cold-launch getCurrent and warm listen())
  sets showWelcomeModal=false to enforce Flow 5 race resolution: the
  user has already taken an explicit action by clicking the link.

Stubs feedbackModalOpen + aboutModalOpen $state for Task 7's
HelpMenuButton callbacks.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: HelpMenuButton mount + AboutModal + callback wiring

**Files:**
- Create: `src/lib/components/AboutModal.svelte`
- Modify: `src/App.svelte` (mount HelpMenuButton at fixed position + wire callbacks + mount FeedbackModal + AboutModal)

- [ ] **Step 1: Implement `src/lib/components/AboutModal.svelte`**

```svelte
<script lang="ts">
  /**
   * ZEB-331 — Simple About modal (spec §4.3 / Task 7).
   *
   * Shows the app version (read from Tauri's app.getVersion), license
   * line, and a link to the GitHub repo. Reached via HelpMenuButton's
   * "About" item.
   */
  import { onMount } from 'svelte';
  import { getVersion } from '@tauri-apps/api/app';

  interface Props {
    open: boolean;
    onDismiss: () => void;
  }
  const { open, onDismiss }: Props = $props();

  let appVersion = $state<string>('unknown');

  onMount(async () => {
    try {
      appVersion = await getVersion();
    } catch {
      // Outside Tauri (dev/browser) — leave as 'unknown'.
    }
  });

  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) {
      onDismiss();
    }
  }

  $effect(() => {
    if (!open) return;
    function onKey(e: KeyboardEvent) {
      if (e.key === 'Escape') onDismiss();
    }
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });
</script>

{#if open}
  <div
    class="modal-backdrop"
    data-testid="about-modal-backdrop"
    onclick={handleBackdropClick}
    role="presentation"
  >
    <div
      class="modal-content"
      data-testid="about-modal"
      role="dialog"
      aria-labelledby="about-title"
    >
      <h2 id="about-title">Harmony</h2>
      <p class="version" data-testid="about-version">
        Version <code>{appVersion}</code>
      </p>
      <p>
        A federated chat with self-governing communities.
      </p>
      <p class="license">
        Licensed under MIT.
      </p>
      <p>
        <a
          href="https://github.com/zeblithic/harmony-client"
          target="_blank"
          rel="noopener noreferrer"
        >
          github.com/zeblithic/harmony-client
        </a>
      </p>
      <div class="actions">
        <button type="button" onclick={onDismiss} data-testid="about-close">
          Close
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .modal-content {
    background: var(--bg-secondary, #2a2a2a);
    color: var(--text-primary, #fff);
    padding: 1.5rem;
    border-radius: 8px;
    max-width: 420px;
    width: 90%;
  }
  .modal-content h2 {
    margin: 0 0 1rem;
    font-size: 1.25rem;
  }
  .modal-content p {
    margin: 0 0 0.75rem;
    line-height: 1.5;
  }
  .version code {
    background: var(--bg-tertiary, #1f1f1f);
    padding: 0.1rem 0.3rem;
    border-radius: 3px;
  }
  .license {
    font-size: 0.85rem;
    color: var(--text-secondary, #aaa);
  }
  .modal-content a {
    color: var(--accent, #5865f2);
    text-decoration: none;
  }
  .modal-content a:hover {
    text-decoration: underline;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    margin-top: 1rem;
  }
  .actions button {
    padding: 0.5rem 1rem;
    border: 1px solid var(--border, #444);
    background: var(--bg-tertiary, #1f1f1f);
    color: var(--text-primary, #fff);
    border-radius: 4px;
    cursor: pointer;
  }
</style>
```

- [ ] **Step 2: Mount HelpMenuButton + FeedbackModal + AboutModal in App.svelte**

In `src/App.svelte`, find where Layout (or root) is rendered (likely a `<Layout>` snippet block ending the template). At the root of the template (outside Layout) add:

```svelte
<!-- ZEB-331: fixed-position help button overlay. Position top-right. -->
<div class="help-overlay">
  <HelpMenuButton
    onSubmitFeedback={() => (feedbackModalOpen = true)}
    onShowAbout={() => (aboutModalOpen = true)}
    onOpenNetworkHealth={() => switchMode('network')}
    onOpenDocs={async () => {
      try {
        const { open: shellOpen } = await import('@tauri-apps/plugin-shell');
        await shellOpen('https://github.com/zeblithic/harmony-client/blob/main/README.md');
      } catch (e) {
        console.warn(
          '[zeb-331] failed to open docs:',
          e instanceof Error ? e.message : String(e),
        );
      }
    }}
  />
</div>

<FeedbackModal
  open={feedbackModalOpen}
  onDismiss={() => (feedbackModalOpen = false)}
/>

<AboutModal
  open={aboutModalOpen}
  onDismiss={() => (aboutModalOpen = false)}
/>
```

Add corresponding CSS at the bottom of the `<style>` block in App.svelte:

```svelte
<style>
  /* ZEB-331: HelpMenuButton fixed-position overlay. Below modal z-index
     (1000) so modals always layer above the (?) icon; above general
     content so it's always reachable. */
  .help-overlay {
    position: fixed;
    top: 12px;
    right: 12px;
    z-index: 50;
  }
</style>
```

If App.svelte already has a `<style>` block at the end, append to it; otherwise add a new `<style>` block.

- [ ] **Step 3: Type-check**

```bash
npx tsc --noEmit
```

Expected: no new errors.

- [ ] **Step 4: Run full vitest**

```bash
npx vitest run
```

Expected: all tests pass (HelpMenuButton + WelcomeModal + FeedbackModal + onboarding-env tests all green).

- [ ] **Step 5: Smoke-render in dev (optional, time-boxed)**

```bash
# In a separate terminal:
npm run tauri dev &
# Wait ~30s for build.
# Manually verify: (?) icon visible top-right.
# Click → dropdown shows 4 items in order.
# Click each item: Submit Feedback opens FeedbackModal; About opens
# AboutModal; Network Health switches to /network route; Documentation
# opens GitHub README in browser.
```

If `npm run tauri dev` boots fail, document in commit message + flag for Task 10 smoke checklist.

- [ ] **Step 6: Commit**

```bash
git add src/App.svelte src/lib/components/AboutModal.svelte
git commit -m "$(cat <<'EOF'
feat(zeb-331): mount HelpMenuButton + FeedbackModal + AboutModal in App.svelte

- HelpMenuButton positioned fixed top-right (z-index 50, below modal
  z-index 1000 so modals layer above it).
- onSubmitFeedback → feedbackModalOpen; onShowAbout → aboutModalOpen;
  onOpenNetworkHealth → switchMode('network'); onOpenDocs →
  shell.open(GitHub README URL).
- AboutModal shows app version (from Tauri app.getVersion()), license,
  and GitHub repo link.
- FeedbackModal + AboutModal both follow the inline-backdrop pattern
  established by DiagnosticExportModal + WelcomeModal.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Troubleshooting documentation

**Files:**
- Create: `docs/troubleshooting.md`

- [ ] **Step 1: Write `docs/troubleshooting.md`**

```markdown
# Troubleshooting

Common issues hand-picked alpha testers may hit on `harmony-client` v0.1.0-alpha. If your problem isn't covered here, submit feedback via the **(?) → Submit Feedback** menu in the app — the form pre-fills environment info and (optionally) a redacted network diagnostic snapshot for you.

## Install + first-launch

### Gatekeeper warns "Harmony cannot be opened" (macOS)

The binary is unsigned for the alpha. macOS Gatekeeper blocks unsigned `.app` bundles on first launch:

1. Open **System Settings → Privacy & Security**.
2. Scroll to the **Security** section.
3. Find the "Harmony was blocked..." message.
4. Click **Open Anyway** and confirm with your password.

Subsequent launches don't re-prompt. See [`install-macos.md`](install-macos.md) for the full Gatekeeper walkthrough.

### SmartScreen warns "Windows protected your PC" (Windows)

Same root cause — unsigned binary:

1. Click **More info** in the SmartScreen dialog.
2. Click **Run anyway**.

See [`install-windows.md`](install-windows.md) for the SmartScreen walkthrough.

### AppImage won't launch (Linux)

If `./Harmony.AppImage` fails with "Permission denied":

```bash
chmod +x ./Harmony.AppImage
./Harmony.AppImage
```

If FUSE isn't installed, extract + run:

```bash
./Harmony.AppImage --appimage-extract
./squashfs-root/AppRun
```

See [`install-linux.md`](install-linux.md) for the AppImage walkthrough.

## First-run welcome behaviour

### "I don't see the welcome modal on launch"

The welcome modal fires only on **fresh identity** — when no `harmony.client` keychain entry exists. If you've launched Harmony before on this machine, the modal is suppressed by design (we don't pester returning users).

To force the modal again (e.g., for testing the fresh-install path):

**macOS:**
```bash
security delete-generic-password -s "harmony.client"
```

**Windows / Linux:** delete the equivalent secret-service entry via your OS keychain manager.

Then relaunch Harmony.

### "Welcome modal didn't appear after I pasted my invite"

A welcome modal is automatically suppressed when a `harmony://` URL is delivered to the app at launch — clicking an invite is an explicit action, and stacking a welcome screen in front of it would be jarring. You'll see the invite-redeem dialog directly. The information from welcome (alpha-tester orientation, where the (?) icon lives) is reachable any time via that icon.

## Network connectivity

### Network Health says "Unreachable" or shows red

Open **Sidebar → Network**. The panel diagnoses the four common breakage modes:

1. **No relay home_relay_url** — your iroh endpoint hasn't picked up a default relay. Wait 10-15 seconds after first launch; if still missing, file a feedback report.
2. **Pkarr publish failing** — your reachability record isn't being published to Mainline DHT. Often means the network is blocking outbound UDP. Try a different network (e.g., tethering off your phone) to confirm.
3. **No peers in shared communities** — expected if you haven't joined any community yet. Paste a `harmony://invite/...` URL via Sidebar → Communities → Redeem invite.
4. **Self-test fails on `relay_rtt`** — your local network blocks connections to n0's relay infrastructure. Check firewall settings; corporate networks often need allowlisting `*.n0.network`.

The **Run Self-Test** button at the bottom of the Network panel reports which of the four steps (endpoint init / relay RTT / pkarr publish / pkarr resolve) fails first. Each Fail line includes a short reason.

### "I can't see anyone in my community"

Network Health green but the community looks empty? Check:

1. **Are you actually joined?** Sidebar → Communities → click the community name → Members panel. If you see only yourself, the join handshake didn't complete. Re-paste the invite.
2. **Are your peers online?** Communities are peer-to-peer; if no other member is reachable right now, the roster appears with last-seen timestamps but no live presence.
3. **Have you been offline for a long time?** When you come back online, give it a few minutes for pkarr re-resolution + reachability re-publish. Watch the Network Health "Peers" section for last-seen freshness.

### Cross-WAN doesn't work

See [`cross-wan-validation.md`](cross-wan-validation.md) for the two-host playbook (Step 1: single-machine baseline → Step 2: first contact → Step 3: bidirectional exchange → Step 4: diagnostic export). If both hosts pass Step 1 but fail Step 2, the issue is almost always either NAT topology (double-NAT on one side) or pkarr DHT reachability (some ISPs block).

## Identity + backup

### "How do I back up my identity?"

Identity backup is shipped separately ([ZEB-202](https://linear.app/zeblith/issue/ZEB-202)) and not yet available in v0.1.0-alpha. For now:

- **macOS:** your iroh secret key lives in Keychain Access → "harmony.client". You can export the keychain item via Keychain Access → File → Export. Treat the export as you would a password — it grants full control of your identity.
- **Windows:** Credential Manager → Generic Credentials → "harmony.client". Currently no export UI from Credential Manager; expect ZEB-202 to add this.
- **Linux:** secret-service entry under the "harmony.client" name. Use `secret-tool` or `seahorse` for inspection.

Until ZEB-202 ships, treat the alpha as **non-recoverable** — if you lose the keychain entry, your identity is gone forever. Use only on devices you intend to keep.

### "I see a 'Identity not backed up' warning"

The `BackupStalenessWarning` reminds you to back up the identity. For v0.1.0-alpha there's no in-app backup flow yet (ZEB-202); the warning is informational. You can dismiss it for now.

## Help + feedback

### "I want to send feedback"

Click the **(?)** icon in the top-right of the app. Choose **Submit Feedback**:

- Type a description (≥10 characters).
- Optional: toggle "Attach network diagnostics" — this includes a redacted snapshot (no full identifiers) of your Network Health panel in the GitHub issue body. Review the preview before submitting.
- Click **Submit**. Your default browser opens a pre-filled GitHub new-issue page. Review the body and click **Submit new issue** on GitHub.

See [`feedback.md`](feedback.md) for full details on what gets included.

### "The browser didn't open when I clicked Submit"

The app falls back to copying the GitHub URL to your clipboard with a toast notification ("Couldn't open browser. URL copied to clipboard."). Paste it manually in your browser of choice.

## Where to get more help

- **GitHub issues:** [zeblithic/harmony-client/issues](https://github.com/zeblithic/harmony-client/issues) — file a new issue or search existing ones.
- **In-app diagnostic export:** Sidebar → Network → "Export diagnostics" — produces a redacted markdown report you can attach to bug reports manually.
- **Cross-WAN validation playbook:** [`cross-wan-validation.md`](cross-wan-validation.md) — for testing two-host scenarios end-to-end.
```

- [ ] **Step 2: Commit**

```bash
git add docs/troubleshooting.md
git commit -m "$(cat <<'EOF'
docs(zeb-331): troubleshooting cookbook for alpha testers

Covers install (Gatekeeper/SmartScreen/AppImage), first-run welcome
behavior (when it fires + how to force-reset), Network Health red
diagnoses, "I can't see anyone" investigation, identity backup
limitations until ZEB-202 ships, and feedback channel pointers.

Cross-links install-{macos,windows,linux}.md from Sub-A and
cross-wan-validation.md from Sub-B.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Feedback documentation

**Files:**
- Create: `docs/feedback.md`

- [ ] **Step 1: Write `docs/feedback.md`**

```markdown
# Submitting feedback

This page explains what happens when you click **(?) → Submit Feedback** in Harmony alpha, what's included in your report, what isn't, and what to expect after submitting.

## The flow

1. Click the **(?)** icon in the top-right corner of Harmony.
2. Click **Submit Feedback** in the dropdown menu.
3. A modal opens with a description field. Type what happened, what you expected, and what you saw — at least 10 characters.
4. Optional: toggle **"Attach network diagnostics"** to include a redacted snapshot of your Network Health panel.
5. Click **Submit**. Your default browser opens a pre-filled GitHub new-issue page.
6. Review the body, edit if you'd like, and click **Submit new issue** on GitHub.

The Harmony app never sends your feedback anywhere on its own. It only opens a pre-filled GitHub URL in your browser; you submit (or don't) from there.

## What's auto-included

Every feedback submission includes:

- **`## Description`** — exactly what you typed, verbatim.
- **`## Environment`** — four lines:
  - App version (e.g., `0.1.0-alpha.1`)
  - Platform (`macos` / `windows` / `linux`)
  - OS version (e.g., `15.0`)
  - Timestamp (ISO-8601 UTC of when you submitted)

If you toggled **"Attach network diagnostics"** ON:

- **`## Network diagnostics`** — the same redacted markdown produced by the **Export diagnostics** button in your Network Health panel. Identifiers are server-side redacted (no full Ed25519 hex). You can preview the exact text in the modal before submitting.

If the diagnostic toggle is OFF, the `## Network diagnostics` section is omitted entirely from the report.

## What's NOT included

- **No automatic telemetry.** Harmony never sends usage data, logs, or crash reports anywhere by itself. Feedback is opt-in, manual, and routed through your browser.
- **No identity material.** Your Ed25519 secret keys, pkarr secrets, and ALPN tokens never flow through the feedback path. Diagnostic snapshots use the same redactor as the in-app Export diagnostics button.
- **No content.** Messages you've sent, files you've stored, communities you're in — none of this is included unless you paste it into the description yourself.
- **No persistent draft.** If you dismiss the modal, your typed description is discarded. Submit when you're ready.

## URL-length budget

GitHub URLs have a practical limit around 8000 characters. If you attach a large diagnostic snapshot, the snapshot section may be truncated with a `…[truncated for URL length]` marker. Your description, environment info, and the beginning of the diagnostics are always preserved intact.

If you need the full diagnostic, use the **Export diagnostics** button in the Network Health panel and attach the resulting `.txt` file to the GitHub issue manually after submitting.

## What if my browser doesn't open?

If the Tauri shell plugin can't launch your default browser (e.g., on a Linux desktop without `xdg-open`), the app falls back to copying the GitHub URL to your clipboard with a toast notification. Paste it into your browser of choice manually.

## Where reports go

All feedback flows to the public [`zeblithic/harmony-client` GitHub issue tracker](https://github.com/zeblithic/harmony-client/issues). Other alpha testers can see and comment on your issues, which helps build a shared knowledge base.

Issues are reviewed by the development team on a rolling basis. There's no formal SLA during alpha — bugs blocking the validation flow get priority. Feel free to comment, edit, or close issues yourself.

## Privacy expectations

- You review the GitHub URL body **before** clicking Submit on GitHub. Edit out anything you'd rather not share.
- Diagnostic snapshots are redacted by Harmony before you see them — you can verify by inspecting the modal preview before clicking Submit.
- GitHub itself is a public forum. Don't include anything sensitive in your description if you wouldn't put it in a public forum.

## When in doubt

Submit a feedback report anyway. We'd rather know.
```

- [ ] **Step 2: Commit**

```bash
git add docs/feedback.md
git commit -m "$(cat <<'EOF'
docs(zeb-331): feedback submission flow guide

Explains the (?) → Submit Feedback flow end-to-end: what's auto-
included (description, environment, optional redacted diagnostics),
what isn't (no telemetry, no identity, no content), URL-length
budget + truncation behavior, browser-failure clipboard fallback,
where reports go (public GitHub issue tracker), privacy expectations.

Referenced from the WelcomeModal footer link + the troubleshooting
doc's "I want to send feedback" section.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Final 5-gate sweep + push + PR creation

**Files:** none (verification + git push + PR)

- [ ] **Step 1: Verify branch history**

```bash
git log --oneline zeb-331-sub-c-onboarding-spec | head -15
```

Expected: ~10 commits from Tasks 1-9 + the spec commit (`6f60200`). Verify ZEB-331 references in commit messages.

- [ ] **Step 2: Frontend gates (foreground, time-bounded)**

```bash
set -o pipefail
npx tsc --noEmit 2>&1 | tee /tmp/zeb-331-tsc.log
echo "TSC EXIT=${PIPESTATUS[0]}"
```

Expected: TSC EXIT=0.

```bash
set -o pipefail
npx vitest run 2>&1 | tee /tmp/zeb-331-vitest.log
echo "VITEST EXIT=${PIPESTATUS[0]}"
```

Expected: VITEST EXIT=0. New tests added (44 ZEB-331 cases) + all pre-existing tests pass.

- [ ] **Step 3: Backend gates**

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo fmt --all -- --check 2>&1 | tee /tmp/zeb-331-fmt.log
echo "FMT EXIT=${PIPESTATUS[0]}"
```

Expected: FMT EXIT=0.

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings 2>&1 | tee /tmp/zeb-331-clippy.log
echo "CLIPPY EXIT=${PIPESTATUS[0]}"
```

Expected: CLIPPY EXIT=0.

```bash
cd src-tauri
set -o pipefail
timeout 600 cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast 2>&1 | tee /tmp/zeb-331-nextest.log
echo "NEXTEST EXIT=${PIPESTATUS[0]}"
```

Expected: NEXTEST EXIT=0 OR matches the Task 0 orphan baseline (folder_ingest, mint, mint_sync, rename_content_integration, occasional zenoh_iroh_*). NEW failures from Tasks 1-9 are blocking — diff against `/tmp/zeb-331-orphan-nextest.txt`.

If wall-clock exceeds 10 min on any gate → report DONE_WITH_CONCERNS + document timing in PR body.

- [ ] **Step 4: Manual smoke checklist (per spec §8)**

Execute each scenario on the local machine. Record results in `/tmp/zeb-331-smoke.md` for inclusion in the PR body.

1. **Fresh install path** — delete keychain entry:
   ```bash
   security delete-generic-password -s "harmony.client" 2>/dev/null || echo "(no entry to delete)"
   ```
   Launch `npm run tauri dev`. Verify WelcomeModal renders. Paste a valid `harmony://invite/v1?...` URL. Click Join now. Verify RedeemInviteDialog opens. (Joining may fail without a live peer — record outcome.)

2. **Returning user path** — relaunch the app with the keychain entry that was just created. Verify NO welcome modal. App boots into normal UI.

3. **Feedback submit (no diagnostics)** — Click (?) → Submit Feedback → type "test feedback message" → Submit. Verify browser opens to `https://github.com/zeblithic/harmony-client/issues/new?title=[alpha-feedback]+test+feedback+message&body=...`. Verify body contains `## Description`, `## Environment`, but NOT `## Network diagnostics`.

4. **Feedback submit (with diagnostics)** — (?) → Submit Feedback → type "test with diagnostics" → toggle "Attach network diagnostics" → verify preview pane shows redacted markdown → Submit. Verify browser body contains `## Network diagnostics`.

5. **Deep-link suppresses welcome** — delete keychain again. Launch app. Before WelcomeModal appears, paste a `harmony://invite/...` URL into the terminal: `open "harmony://invite/v1?..."` (macOS) or platform equivalent. Verify RedeemInviteDialog appears, NOT WelcomeModal.

6. **Help menu dropdown** — (?) → verify 4 items in order. Click each: Submit Feedback opens FeedbackModal; Network Health switches to /network; About shows AboutModal with version; Documentation opens GitHub README.

7. **shell.open failure** — SKIP on macOS (where Tauri's shell plugin reliably opens URLs); document as deferred. If Linux/Windows tester available, verify clipboard-fallback fires when default browser is unset.

Record any failures or anomalies in `/tmp/zeb-331-smoke.md`.

- [ ] **Step 5: Push to remote**

```bash
git push -u origin zeb-331-sub-c-onboarding-spec
```

Expected: new remote branch + tracking set up.

- [ ] **Step 6: Create PR**

```bash
gh pr create --title "ZEB-331: Sub-C onboarding + first-run UX (ZEB-327 Sub-C)" --body "$(cat <<'EOF'
## Summary

Implements [ZEB-331](https://linear.app/zeblith/issue/ZEB-331) — Sub-project C of the [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) alpha-validation umbrella (Sub-D community minting + invite distribution still parked, so this PR does NOT close ZEB-327).

Ships the first-run welcome modal + ambient (?) feedback affordance so fresh alpha testers can self-serve through first launch without zeblith holding their hand. Pure-frontend UX scaffolding wrapping existing identity / `harmony://` invite / Network Health surfaces, plus one small backend addition (`start_node` returns `freshly_created: bool`) to gate the welcome trigger.

## What changed

**Frontend** (`src/lib/components/*.svelte` NEW + targeted App.svelte + Layout/types additions, ~1200 LOC + tests):
- `WelcomeModal` — first-run intro + alpha orientation + optional `harmony://` invite paste (reuses existing `extractHarmonyInviteUrl` validator)
- `FeedbackModal` — description textarea + optional "Attach network diagnostics" toggle → opens browser to pre-filled GitHub new-issue URL via `tauri-plugin-shell`; clipboard fallback on `shell.open` failure
- `HelpMenuButton` — top-right (?) icon + dropdown (Submit Feedback / Network Health / About / Documentation); keyboard nav + ARIA
- `AboutModal` — app version + license + GitHub link
- `onboarding-env.ts` — pure helpers (`collectEnvironment` never throws; `buildGitHubIssueUrl` URL-encodes + truncates diagnostics body to 8KB budget with marker)
- App.svelte boot-sequence destructures `start_node` response; mounts WelcomeModal + FeedbackModal + AboutModal + HelpMenuButton
- Deep-link receiver suppresses welcome modal (Flow 5 race resolution — deep-link wins)

**Backend** (`src-tauri/src/lib.rs` + `iroh_endpoint.rs`):
- `load_or_create_secret_key` returns `(SecretKey, bool /* freshly_created */)` — true on `Err(keyring::Error::NoEntry)`, false on existing entry
- `start_node` returns `Result<StartNodeResponse, String>` with `#[serde(rename_all = "camelCase")]` → JS reads `{ nodeAddr, freshlyCreated }`
- `tauri-plugin-os` + `tauri-plugin-shell` added; capabilities permit `os:default` + `shell:allow-open`
- Forward-compat: missing `freshlyCreated` field on JS side defaults to `false` — older backend mid-deploy never spuriously re-fires welcome

**Tests** (~44 new):
- 14 pure-helper tests (`onboarding-env.test.ts`)
- 9 WelcomeModal tests
- 9 HelpMenuButton tests (including dropdown order + ARIA)
- 12 FeedbackModal tests including:
  - Privacy-invariant regression (`/[0-9a-f]{32,}/` redaction-leak check)
  - Stale-response guard regression (rapid toggle → only latest response reflected)
  - shell.open failure → clipboard fallback
  - URL truncation marker visible when diagnostics body exceeds 8KB budget
- 2 backend serialization tests (`StartNodeResponse` camelCase wire shape)

**Documentation:**
- `docs/troubleshooting.md` (~200 lines) — install (Gatekeeper/SmartScreen/AppImage), first-run welcome behavior, Network Health red diagnoses, "I can't see anyone" investigation, identity backup limitations, feedback channel pointers
- `docs/feedback.md` (~80 lines) — what's auto-included, what isn't, URL budget + truncation, browser-failure fallback, privacy expectations

## Phase 1 caveats (intentionally shipped — documented for follow-up)

- **Keychain-existence freshness tests deferred.** `load_or_create_secret_key`'s `(SecretKey, true)`-on-`NoEntry` vs `(SecretKey, false)`-on-existing-entry behavior is verified end-to-end via the manual smoke checklist (#1 + #2). The serialization invariant (camelCase wire shape) is unit-tested. Mock-keyring abstraction → future follow-up.
- **Smoke test #7 (Linux shell.open fallback) deferred** to a Linux-equipped tester. macOS reliably opens URLs; the fallback path is verified by unit test only.

## Spec + plan

- Spec: `docs/specs/2026-05-25-zeb-331-sub-c-onboarding-design.md` (commit `6f60200`)
- Plan: `docs/plans/2026-05-25-zeb-331-sub-c-onboarding-plan.md`

## Test plan

- [x] `npx tsc --noEmit`
- [x] `npx vitest run` (44 new tests pass + all pre-existing pass)
- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [x] `cargo nextest run --locked --workspace --all-targets --features test-fixtures --no-fail-fast` (only pre-existing orphan failures remain: `folder_ingest`, `mint`, `mint_sync`, `rename_content_integration`)
- [x] Manual smoke: fresh-install welcome / returning-user silent / feedback no-diag / feedback with-diag / deep-link suppression / help menu dropdown
- [ ] Manual: Linux `shell.open` clipboard fallback (deferred to Linux tester)

## Notes for review

- **Privacy invariant**: FeedbackModal's diagnostic-attach toggle flows through the existing `network_health_export_payload(includeFullIds: false)` IPC — same server-side redactor as ZEB-329's DiagnosticExportModal. The regex-leak regression test (`/[0-9a-f]{32,}/` rejected in submitted URL) was written FIRST before the diagnostics-attach code per `feedback_second_order_correctness_review`.
- **Stale-response guard**: FeedbackModal mirrors DiagnosticExportModal's `latestRequest` plain-`let`-NOT-`$state` pattern from PR #161 R1 to avoid `effect_update_depth_exceeded`.
- **Identity portability invariant unaffected**: no keychain entry shape changes; `KEYCHAIN_SERVICE` + `KEYCHAIN_USER` constants frozen in v0.1.0-alpha remain canonical. The new `freshly_created` flag is computed from the existing `keyring::Error::NoEntry` branch.
- **No new wire-format CRDT events introduced**. Trigger semantics live entirely in the `StartNodeResponse` IPC return shape.

## Cross-references

- Parent umbrella: [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) (Sub-D still parked at [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) — no close trigger)
- Sub-A: [ZEB-328](https://linear.app/zeblith/issue/ZEB-328) (PR #160 merged) — install docs extended here
- Sub-B: [ZEB-329](https://linear.app/zeblith/issue/ZEB-329) (PR #161 merged) — `network_health_export_payload` reused for diagnostic-attach

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR opens; URL printed; bot reviewers fire automatically.

- [ ] **Step 7: Verify PR creation**

```bash
gh pr view --json number,url,state | head -10
```

Expected: state=OPEN, URL printed.

- [ ] **Step 8: Bot-review loop**

The PR autonomous-loop convention applies (per memory `feedback_autonomous_pr_monitoring_loop`):

- Wait for Cursor Bugbot + CodeRabbit + Qodo + CodeAnt initial findings
- Address actionable findings via local commits + force-push (or amend + push, depending on review-round count)
- Re-poll bots for re-review pass on the latest commit
- Pushover when ready to merge

ScheduleWakeup at 1200-1800s between polls; never Bash-sleep poll.

---

## Plan self-review

### Spec coverage check

| Spec section | Plan task(s) |
|---|---|
| §3.1 Components added | Task 1 (types + helpers), Task 2 (WelcomeModal), Task 3 (HelpMenuButton), Task 4 (FeedbackModal), Task 7 (AboutModal) |
| §3.2 Existing files modified | Task 1 (Cargo.toml + capabilities + package.json), Task 5 (lib.rs + iroh_endpoint.rs), Task 6 + 7 (App.svelte) |
| §3.3 Docs added | Task 8 (troubleshooting.md), Task 9 (feedback.md) |
| §3.5 Boot sequence change | Task 6 Step 5 |
| §3.6 Trigger semantics | Task 5 + Task 6 Step 5 |
| §4.1 WelcomeModal | Task 2 |
| §4.2 FeedbackModal (incl. plain `let` latestRequest) | Task 4 Step 3 |
| §4.3 HelpMenuButton | Task 3 |
| §4.4 onboarding-env.ts | Task 1 Step 9 |
| §4.5 Backend StartNodeResponse | Task 5 Step 4 |
| §5.1 Flow 1 first-run boot | Task 6 |
| §5.2 Flow 2 returning user | Task 6 (`freshlyCreated === false` → no modal) |
| §5.3 Flow 3 feedback no-diag | Task 4 Step 1 (test) + Step 3 (impl) |
| §5.4 Flow 4 feedback with diag | Task 4 same |
| §5.5 Flow 5 deep-link race | Task 6 Step 6 |
| §5.6 Flow 6 dropdown | Task 3 |
| §5.7 Privacy invariants | Task 4 Step 1 (privacy regression test) |
| §6.1 Welcome errors | Task 2 Step 3 |
| §6.2 FeedbackModal errors | Task 4 Step 3 |
| §6.3 start_node errors | Task 6 Step 5 (forward-compat default to false) |
| §6.4 Dropdown UX | Task 3 Step 3 |
| §6.5 Race conditions | Task 6 Step 6 + Task 4 Step 3 (stale-response guard) |
| §6.6 Non-handlers | spec §; out of scope by construction |
| §7.1 Pure-function tests | Task 1 Step 7 |
| §7.2 Component tests | Tasks 2, 3, 4 |
| §7.3 Backend tests | Task 5 Step 5 |
| §7.5 Test discipline | Throughout (privacy regression FIRST in Task 4, stale-response regression in Task 4) |
| §8 Manual smoke checklist | Task 10 Step 4 |
| §9 Out of scope | Not implemented (correct) |

No gaps.

### Placeholder scan

- No "TBD" / "TODO" / "implement later".
- Step 4 in Task 5 contains the phrase "the exact line context needs to be located with grep" — that's an instruction to the implementer to locate the call site, not a placeholder. The required code shape is fully specified.
- Step 6 in Task 5 includes an `if/else` fork on mock-keychain availability with concrete code for both paths.
- Task 4 Step 1 (the privacy invariant test) has full code, no placeholder.
- All commit messages are complete.

### Type consistency

- `StartNodeResponse { nodeAddr, freshlyCreated }` consistent across:
  - `src/lib/types/onboarding.ts` (Task 1 Step 6)
  - `src-tauri/src/lib.rs` (Task 5 Step 4)
  - `src/App.svelte` (Task 6 Step 5)
  - Backend tests (Task 5 Step 5)
- `EnvironmentInfo { appVersion, platform, osVersion, timestamp }` consistent across:
  - `src/lib/types/onboarding.ts` (Task 1 Step 6)
  - `src/lib/onboarding-env.ts` `collectEnvironment` (Task 1 Step 9)
  - `src/lib/__tests__/onboarding-env.test.ts` `FIXED_ENV` (Task 1 Step 7)
- `FeedbackPayload { description, env, diagnostics? }` consistent across:
  - `src/lib/types/onboarding.ts` (Task 1 Step 6)
  - `src/lib/onboarding-env.ts` `buildGitHubIssueUrl` (Task 1 Step 9)
  - `src/lib/components/FeedbackModal.svelte` (Task 4 Step 3)
- `data-testid` values match between test (Step 1) and component (Step 3) in every component task.
- IPC arg shape `{ includeFullIds: false }` matches the existing ZEB-329 IPC contract.

No type inconsistencies.

---

**End of implementation plan.** Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to execute.
