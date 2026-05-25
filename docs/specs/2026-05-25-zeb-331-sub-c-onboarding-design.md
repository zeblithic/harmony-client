# ZEB-331 Sub-C: Onboarding docs + first-run UX — design spec

**Ticket:** [ZEB-331](https://linear.app/zeblith/issue/ZEB-331) — Sub-project C of the [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) v0.1.0-alpha distribution + onboarding + validation umbrella.

**Status:** Approved 2026-05-25. Ready for implementation plan.

**Brainstorm record:** Hybrid welcome-modal + ambient-guidance shape; fresh-identity trigger; GitHub-issue prefill feedback channel; minimal welcome (intro + alpha orientation + optional invite paste).

---

## 1. Overview

Sub-C wraps the existing identity / `harmony://` invite / Network Health / profile surfaces with an onboarding flow + tester-feedback affordance, so a fresh alpha tester can self-serve through first launch without zeblith holding their hand.

Today, a tester who downloads v0.1.0-alpha launches into an app with no welcome, no guided identity creation, no "now what?" affordance. The keychain identity is created lazily on first key-needed; community-join requires manually navigating to a paste UI; Network Health is a sidebar item with no contextual hint about *when* to look at it. Sub-C removes that bottleneck.

Sub-C is **pure-frontend UX scaffolding** wrapping existing IPCs, plus one tiny backend addition: `start_node` returns a `freshly_created: bool` flag so the welcome modal knows when to fire. No new wire-format, no CRDT events, no protocol changes.

## 2. Goals + non-goals

### Goals

1. Fresh-install testers see a brief welcome modal explaining what Harmony is, that they're testing v0.1.0-alpha, and where to find help.
2. Welcome modal surfaces the highest-value first action (paste a `harmony://invite/...` URL from zeblith) without forcing it.
3. Returning testers (post-reinstall, keychain identity persisted) boot silently — no welcome re-prompt.
4. Testers can submit feedback at any time via a discoverable (?) icon → GitHub issue prefill, with optional redacted network-diagnostic attachment.
5. Troubleshooting + feedback-process docs cover the common failure modes testers will hit.

### Non-goals

- **No mandatory wizard.** Welcome is dismissable; no required input.
- **No display-name capture in welcome.** Default to anonymous; the existing Profile editor handles it whenever the user cares.
- **No in-app form posting to a backend.** Self-sovereign + no-telemetry invariant: feedback opens GitHub in the user's default browser; we have no visibility past `shell.open()`.
- **No mailto: feedback channel.** Private channels don't grow the tester community.
- **No automatic identity backup prompt.** `BackupStalenessWarning` already covers this ambiently.
- **No "Show welcome again" affordance.** YAGNI for v0.1.0-alpha. Information is reachable via the (?) icon.
- **No identity recovery / multi-device binding work** — separate tracks ([ZEB-202](https://linear.app/zeblith/issue/ZEB-202), [ZEB-173](https://linear.app/zeblith/issue/ZEB-173)).
- **No telemetry or call-home of any kind.**

## 3. Architecture

### 3.1 Components added

| Path | Type | Purpose |
|---|---|---|
| `src/lib/components/WelcomeModal.svelte` | Svelte 5 component (NEW) | First-run welcome: intro + alpha orientation + optional `harmony://` invite paste |
| `src/lib/components/FeedbackModal.svelte` | Svelte 5 component (NEW) | Description textarea + diagnostics-attach toggle → opens browser to pre-filled GitHub new-issue URL |
| `src/lib/components/HelpMenuButton.svelte` | Svelte 5 component (NEW) | Top-right (?) icon with dropdown (Submit Feedback / Network Health / About / Documentation) |
| `src/lib/onboarding-env.ts` | TypeScript helper module (NEW) | Pure helpers: `collectEnvironment()`, `buildGitHubIssueUrl()` |
| `src/lib/types/onboarding.ts` | TypeScript types (NEW) | `EnvironmentInfo`, `FeedbackPayload` |

### 3.2 Existing files modified

| Path | Change |
|---|---|
| `src/App.svelte` | Boot sequence calls `start_node`; mounts `WelcomeModal` when `localStorage['harmony.onboarding.welcomeAcknowledged']` !== `'true'` (R3 — see §3.6); wires `HelpMenuButton` callbacks |
| `src/lib/components/Layout.svelte` | Mounts `HelpMenuButton` in top-right chrome alongside existing controls |
| `src-tauri/src/lib.rs` | `start_node` IPC returns `{ node_addr: String, freshly_created: bool }` instead of bare success |

### 3.3 Documentation added

| Path | Purpose |
|---|---|
| `docs/troubleshooting.md` (NEW, ~150-200 lines) | Network issues (Network Health red?), identity backup pointers, "I can't see anyone" cookbook, common Gatekeeper/SmartScreen workarounds (cross-link to install-* from Sub-A) |
| `docs/feedback.md` (NEW, ~80 lines) | What the GitHub-issue submission flow does, what's auto-included, what's not, privacy expectations, where issues are tracked, response-time expectations |

### 3.4 What we explicitly do NOT add

- No sidebar nav arm for feedback ((?) icon only — keeps sidebar focused on app surfaces, not chrome)
- No display-name capture in welcome
- No multi-step wizard (hybrid means lightweight)
- No automatic identity backup prompt in welcome (BackupStalenessWarning already ambient)
- No new CRDT events, no wire-format change, no protocol changes
- No new Tauri capabilities beyond what already exists (uses existing `tauri-plugin-shell`)

### 3.5 Boot sequence change

App.svelte Tauri-init IIFE today:
```typescript
await invoke('start_node', { endpoint: null });
```

Becomes:
```typescript
await invoke('start_node', { endpoint: null });
// existing service wiring continues
if (localStorage.getItem('harmony.onboarding.welcomeAcknowledged') !== 'true') {
  showWelcomeModal = true;
}
```

The IPC returns `freshlyCreated` as forward-compat metadata (future flows may use it for analytics or one-time prompts like "back up your identity"), but it is **not** the welcome trigger anymore — see §3.6.

### 3.6 Trigger semantics

**R3 update (Cursor "welcome lost after failed start"):** Welcome modal fires on every launch until the user explicitly acknowledges it via Skip / Join / Esc / backdrop. Acknowledgement persists in `localStorage['harmony.onboarding.welcomeAcknowledged']='true'`. Re-show after acknowledgement requires the user to clear that key (e.g., dev-tools or fresh install).

**Why not gate on `freshly_created`?** `start_node` writes the iroh secret to the OS keychain inside `load_or_create_secret_key` (`Err(NoEntry)` branch), then proceeds through ~2900 lines of additional bring-up (endpoint bind, link manager, publisher spawn, zenoh session, identity hydration, CRDT replay). Any error path along the way returns `Err` from `start_node` *after* the keychain write has committed. On the next launch the keychain entry exists, so `freshly_created` would be `false` — and the in-memory signal that the user is owed a welcome is lost forever. Persisting the inverse signal (acknowledgement) in localStorage makes the welcome trigger resilient to mid-`start_node` failures.

**Edge cases:**

- *Failed first start*: keychain mints, `start_node` returns Err, welcome never shows during this session. Next launch: localStorage flag is unset → welcome shows. Resolved.
- *User force-quits before dismissing*: localStorage flag unset → welcome shows on next launch. Resolved by the same mechanism.
- *Existing user (post-alpha upgrade)*: when there is a path to "have an identity but no welcome-acknowledged flag," that user will see welcome once on upgrade. Acceptable cost.
- *localStorage unavailable* (sandbox / incognito / quota): default to showing welcome; the next acknowledgement attempt will retry the write.

A tester who explicitly nukes their keychain entry to test fresh-install paths will also need to clear the localStorage key to see welcome again. The `freshly_created` signal still flows to the frontend so future analytics paths can distinguish "first-time" from "returning, never acknowledged."

## 4. Components & data types

### 4.1 `WelcomeModal.svelte`

```typescript
interface WelcomeModalProps {
  open: boolean;
  onDismiss: () => void;
  onJoinWithInvite: (url: string) => void;  // parent opens RedeemInviteDialog with this URL
}
```

**Internal `$state`:**
- `inviteUrl: string` — the pasted URL
- `inviteError: string | null` — validation error if URL is malformed

**Layout:**
- Title: "Welcome to Harmony alpha"
- Body: 2-paragraph intro
  - What Harmony is (one sentence: federated chat with self-governing communities)
  - Alpha-tester expectations (you're testing v0.1.0-alpha, expect rough edges, click the (?) top-right to submit feedback)
- "Have a `harmony://` invite?" section: text input + "Join now" / "Skip for now" buttons
- Footer: version label + link to `docs/feedback.md`

**Validation:**
- Uses existing `extractHarmonyInviteUrl()` from `src/lib/deep-link-router.ts` — no new validator
- Empty input + "Join now" → inline error "Paste an invite URL or click Skip for now"
- Invalid URL + "Join now" → inline error "That doesn't look like a harmony:// invite"
- Valid URL + "Join now" → `onJoinWithInvite(url)` called, modal dismisses

**Dismissal:**
- "Skip for now" button → `onDismiss()`
- Escape key → `onDismiss()`
- Backdrop click → `onDismiss()`
- No "are you sure" gating (welcome is dismissable per the hybrid choice)

### 4.2 `FeedbackModal.svelte`

```typescript
interface FeedbackModalProps {
  open: boolean;
  onDismiss: () => void;
}
```

**Internal `$state`:**
- `description: string` (default `''`, min length 10 chars for submit)
- `attachDiagnostics: boolean` (default `false` — privacy default)
- `submitting: boolean` (disables Submit during URL build + shell.open)
- `diagnosticsPreview: string | null` (fetched when toggle on)
- `diagnosticsError: string | null` (set when fetch fails)
- `latestRequest: number` (plain `let`, NOT `$state`, monotonic counter for stale-response guard per ZEB-329 R3 lesson)

**Layout:**
- Title: "Submit feedback"
- Description textarea (full-width, ≥4 rows, placeholder "Describe what happened, what you expected, what you saw…")
- "Attach network diagnostics" toggle (default OFF) with hint text "Includes a redacted snapshot of your Network Health panel — no full identifiers"
- Collapsible diagnostics-preview pane (when toggle ON): shows redacted markdown from `network_health_export_payload({ includeFullIds: false })`
- Footer: Submit (right) / Cancel (left)
- Submit disabled when `description.length < 10` OR `submitting === true`

**Behavior:**

On toggle ON:
1. `requestId = ++latestRequest`
2. `invoke('network_health_export_payload', { includeFullIds: false })`
3. If `requestId === latestRequest`, set `diagnosticsPreview = result`
4. On reject: `if (requestId === latestRequest) diagnosticsError = String(...)`

On Submit:
1. `submitting = true`
2. `env = await collectEnvironment()` (never throws — returns `'unknown'` fields on failure)
3. `payload = { description, env, diagnostics: attachDiagnostics ? diagnosticsPreview : undefined }`
4. `url = buildGitHubIssueUrl(payload)`
5. `await tauri.shell.open(url)` — on success: `onDismiss()`
6. On `shell.open` reject: copy URL to clipboard via `navigator.clipboard.writeText(url)` + show toast "Couldn't open browser. URL copied to clipboard."
7. `submitting = false`

**Stale-response guard pattern** (per `feedback_second_order_correctness_review` + ZEB-329 R3):
- `latestRequest` is plain `let` (not `$state`) to avoid Svelte's `effect_update_depth_exceeded`
- Increments on every toggle change
- Only assign `diagnosticsPreview` / `diagnosticsError` when the captured `requestId === latestRequest`

### 4.3 `HelpMenuButton.svelte`

```typescript
interface HelpMenuButtonProps {
  onSubmitFeedback: () => void;
  onShowAbout: () => void;
  onOpenNetworkHealth: () => void;
  onOpenDocs: () => void;
}
```

**Internal `$state`:**
- `dropdownOpen: boolean` (default `false`)

**Layout:**
- Circular (?) icon button, ARIA label "Help and feedback"
- Click → dropdown opens beneath/adjacent
- Dropdown items (in order): Submit Feedback / Network Health / About / Documentation
- Each item calls its respective callback + closes dropdown
- Click-outside closes dropdown
- Escape key closes dropdown
- Keyboard navigation: arrow keys cycle through items, Enter triggers, Tab closes

App.svelte wires the callbacks:
- `onSubmitFeedback` → `feedbackModalOpen = true`
- `onShowAbout` → `aboutModalOpen = true`
- `onOpenNetworkHealth` → `switchMode('network')` (existing nav pattern)
- `onOpenDocs` → `tauri.shell.open('https://github.com/zeblithic/harmony-client/blob/main/README.md')`

### 4.4 `onboarding-env.ts`

Pure module — no `$state`, no Svelte bindings, fully testable.

```typescript
export interface EnvironmentInfo {
  appVersion: string;
  platform: string;       // 'macos' | 'windows' | 'linux' | 'unknown'
  osVersion: string;
  timestamp: string;      // ISO-8601
}

export interface FeedbackPayload {
  description: string;
  env: EnvironmentInfo;
  diagnostics?: string;   // optional redacted markdown
}

/**
 * Reads platform/version info via @tauri-apps/plugin-os.
 * Returns 'unknown' for any field whose source rejects.
 * Never throws — degraded environment info beats blocking feedback submission.
 */
export async function collectEnvironment(): Promise<EnvironmentInfo>;

/**
 * Pure URL builder. Returns a fully-encoded GitHub issue URL with title + body.
 * - Title: '[alpha-feedback] ' + first 50 chars of description, single-line (newlines stripped).
 * - Body: ## Description / ## Environment / ## Network diagnostics (if attached).
 * - If total URL exceeds 8000 chars (~8KB GitHub query budget), truncate the diagnostics
 *   section with a '…[truncated for URL length]' marker. Description + environment are
 *   preserved intact.
 */
export function buildGitHubIssueUrl(payload: FeedbackPayload): string;
```

**Title format:** `[alpha-feedback] <first 50 chars, newlines stripped>`

**Body template:**
```
## Description

<full description verbatim>

## Environment

- App version: <env.appVersion>
- Platform: <env.platform>
- OS version: <env.osVersion>
- Submitted: <env.timestamp>

## Network diagnostics

<redacted markdown from network_health_export_payload>
```

The `## Network diagnostics` section is **omitted entirely** when `payload.diagnostics === undefined`. Tests in §7.2 assert this (URL does not contain the heading at all when toggle OFF).

**URL constants:**
```typescript
const GITHUB_ISSUES_URL = 'https://github.com/zeblithic/harmony-client/issues/new';
const URL_BUDGET = 8000;  // GitHub's effective query-string limit; conservative
```

### 4.5 Backend `StartNodeResponse`

`src-tauri/src/lib.rs`:

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartNodeResponse {
    pub node_addr: String,
    pub freshly_created: bool,
}

#[tauri::command(rename_all = "snake_case")]
pub async fn start_node(
    state: tauri::State<'_, Mutex<NodeState>>,
    endpoint: Option<String>,
) -> Result<StartNodeResponse, String> {
    // existing identity-load path; capture whether identity was minted-vs-loaded
    let freshly_created = /* set by identity-load branch */;
    let node_addr = /* existing return value */;
    Ok(StartNodeResponse { node_addr, freshly_created })
}
```

**Detection logic:** existing keychain-load path branches on `KeychainError::NotFound`. The "not found" branch mints a new identity → `freshly_created = true`. The "loaded" branch (entry exists in keychain) → `freshly_created = false`. Implementer locates the exact code site in `lib.rs` (it's a large file; grep for `KeychainError::NotFound` and `keyring::Entry::get_password`).

**Forward-compat:** existing JS callers that ignored the return value need to be updated to destructure (or accept the change without destructuring, since TS `unknown` would auto-pass). Implementer enumerates `invoke('start_node'` sites.

## 5. Data flow

### 5.1 Flow 1: First-run boot (welcome owed to user)

R3 update: welcome trigger is gated on `localStorage['harmony.onboarding.welcomeAcknowledged']`, not on `start_node`'s `freshlyCreated` (see §3.6 for rationale — start_node has many post-keychain-write failure paths that would lose the in-memory signal).

```
App.svelte Tauri-init IIFE
  └─ await invoke('start_node', { endpoint: null })  // success or failure both proceed
  └─ existing service wiring continues (messageService, mailService, navService, ...)
  └─ welcomeAcknowledged = localStorage.getItem('harmony.onboarding.welcomeAcknowledged')
  └─ if (welcomeAcknowledged !== 'true' && !showRedeemInvite) showWelcomeModal = true
  └─ WelcomeModal renders over normal UI
       User either:
         (a) pastes harmony:// invite → "Join now" → onJoinWithInvite(url)
              → App.svelte: redeemUrl = url; redeemError = null;
                            showRedeemInvite = true; showWelcomeModal = false;
                            localStorage.welcomeAcknowledged = 'true'
              → existing RedeemInviteDialog flow (Phase 2c handshake)
         (b) "Skip for now" / Esc / backdrop → onDismiss()
              → showWelcomeModal = false;
                localStorage.welcomeAcknowledged = 'true'
              → user lands in empty app; ambient guidance takes over
```

### 5.2 Flow 2: Returning-user boot (welcome previously acknowledged)

```
await invoke('start_node', { endpoint: null })
  └─ welcomeAcknowledged = localStorage.getItem('harmony.onboarding.welcomeAcknowledged')
  └─ welcomeAcknowledged === 'true' → showWelcomeModal stays false → silent boot
```

This is the post-acknowledgement path. The user has previously dismissed or joined a community via the welcome; we don't pester returning users.

### 5.3 Flow 3: Submit Feedback (no diagnostics)

```
HelpMenuButton (?) icon → dropdown → "Submit Feedback" click
  → feedbackModalOpen = true, dropdownOpen = false
  → FeedbackModal renders (description='', attachDiagnostics=false)
  → user types description (≥10 chars) → Submit button enabled
  → onSubmit:
       submitting = true
       env = await collectEnvironment()    // {appVersion, platform, osVersion, timestamp}
       url = buildGitHubIssueUrl({ description, env })
       await tauri.shell.open(url)         // default browser opens GH issue
       onDismiss()
       submitting = false
  → user reviews prefilled body on GH and clicks Submit there
    (we never see whether they actually submitted — by design, no telemetry)
```

### 5.4 Flow 4: Submit Feedback (with diagnostics)

```
... same as Flow 3 through "user types description" ...
User toggles "Attach network diagnostics" ON
  → requestId = ++latestRequest
  → invoke('network_health_export_payload', { includeFullIds: false })
       returns redacted markdown (server-side redaction per ZEB-329 R3 invariant)
  → if (requestId === latestRequest) diagnosticsPreview = markdown
  → collapsible preview pane renders the redacted content
  → user reviews preview (no full Ed25519 hex visible)
  → user clicks Submit
       url = buildGitHubIssueUrl({ description, env, diagnostics: diagnosticsPreview })
       // url-length budget: 8000 chars; truncate diagnostics if needed
       await tauri.shell.open(url)
       onDismiss()
```

### 5.5 Flow 5: Race — deep-link arrives during fresh-identity boot

```
Deep-link plugin delivers harmony:// URL (cold or warm launch)
  → existing handler at App.svelte:849 sets redeemUrl + showRedeemInvite = true
  → ALSO sets showWelcomeModal = false  // deep-link wins; reduce modal stacking
  → ALSO sets localStorage.welcomeAcknowledged = 'true'  (R5)
       deep-link arrival is itself an onboarding-complete action; without
       this the next launch would re-show welcome to a user who has
       already joined a community
  → RedeemInviteDialog handles the join
  → user lands in the joined community; welcome modal skipped intentionally
```

Deep-link wins because the user has already taken an explicit action (clicking a `harmony://` link). A welcome modal in front of that would be jarring. The welcome's information is partially captured by the redeem flow (it confirms the join + names the community). The remaining alpha-tester orientation content is reachable via the (?) icon afterward.

### 5.6 Flow 6: HelpMenuButton dropdown

```
(?) click → dropdownOpen = true
  Items (in order):
    "Submit Feedback"  → onSubmitFeedback() → feedbackModalOpen = true; dropdownOpen = false
    "Network Health"   → onOpenNetworkHealth() → switchMode('network'); dropdownOpen = false
    "About"            → onShowAbout() → aboutModalOpen = true; dropdownOpen = false
    "Documentation"    → onOpenDocs() → tauri.shell.open(GH docs URL); dropdownOpen = false
  Click outside / Esc → dropdownOpen = false
```

### 5.7 Privacy invariants (no new wire-format, no new identifier exposure)

- Diagnostics attached to feedback flow through the existing `network_health_export_payload(includeFullIds: false)` IPC — same server-side redactor as ZEB-329's DiagnosticExportModal. No new code path can leak full Ed25519 hex.
- Environment info (`appVersion`, `platform`, `osVersion`) is non-identifying. `timestamp` is local clock.
- GitHub URL opens in the user's default browser via `shell.open()`; we have no control or visibility past that call. User reviews on GH before clicking Submit there.
- No identity material (Ed25519 keys, pkarr secrets, ALPN tokens) ever flows through the feedback path. The `collectEnvironment()` helper does NOT read keychain or call `get_node_addr`.

## 6. Error handling

### 6.1 Welcome modal

| Scenario | Behavior |
|---|---|
| Empty paste + "Join now" | Inline "Paste an invite URL or click Skip" — modal stays open |
| Malformed `harmony://` URL | `extractHarmonyInviteUrl()` returns null → inline "That doesn't look like a harmony:// invite" — modal stays open |
| Valid URL pasted + "Join now" | `onJoinWithInvite(url)` fires; parent opens existing RedeemInviteDialog; any redeem errors surface there (existing handling, unchanged) |
| Escape / backdrop / Skip | `onDismiss()` fires; no "are you sure" gating |

### 6.2 FeedbackModal

| Scenario | Behavior |
|---|---|
| Description < 10 chars | Submit button disabled (no toast — visual cue is the disabled state) |
| `collectEnvironment()` rejects | Each field defaults to `'unknown'`; submission proceeds (degraded env > blocking submit) |
| `network_health_export_payload` rejects with toggle ON | Preview pane shows "Diagnostics unavailable" + Submit-with-failed-diagnostics path (user can submit without diagnostics or untoggle) |
| `tauri.shell.open()` rejects | Fallback: `navigator.clipboard.writeText(url)` + toast "Couldn't open browser. URL copied to clipboard — paste it in your browser." |
| URL > 8000 chars | Truncate diagnostics body to fit budget (with `…[truncated for URL length]` marker); preserve description + env intact. User sees truncation note in modal before submitting |
| User clicks Submit while submitting=true | No-op (button disabled) |

### 6.3 Boot sequence — `start_node` × welcomeAcknowledged interactions

R3 update: welcome trigger now depends on localStorage, not on `start_node`'s `freshlyCreated`. See §3.6 for rationale.

| Scenario | Behavior |
|---|---|
| Any `start_node` outcome, `welcomeAcknowledged === 'true'` | Welcome modal stays closed (Flow 2) — user has previously acknowledged |
| Any `start_node` outcome, `welcomeAcknowledged !== 'true'`, no in-flight deep-link | Welcome modal fires (Flow 1) — user owed a welcome |
| `start_node` returned Err (post-keychain-write failure), `welcomeAcknowledged !== 'true'` | Welcome modal still fires — localStorage-gated trigger is resilient to mid-`start_node` failures |
| Any `start_node` outcome, `showRedeemInvite === true` (deep-link won) | Welcome modal stays closed (Flow 5) — deep-link wins the race |
| localStorage unavailable (sandbox / quota / incognito) | Default to showing welcome — safer to greet than to silently skip a possibly-new user |
| `start_node` rejects | Existing handler logs `auto-start_node failed`; the welcome modal trigger still consults `welcomeAcknowledged` (covered by row above) — welcome can still fire on a failed start so the user isn't permanently denied the welcome by a mid-bring-up error. Boot-degraded service behavior is pre-existing; Sub-C doesn't change the service-init recovery path |

### 6.4 HelpMenuButton dropdown

| Scenario | Behavior |
|---|---|
| Dropdown opens while another modal is showing | Z-index above modal; click-outside closes dropdown only (does not dismiss other modals). Standard UX |
| `tauri.shell.open()` rejects for "Documentation" or About-link | Same clipboard-copy fallback as feedback URL |
| Keyboard navigation | Arrow keys cycle items; Enter triggers; Esc closes; Tab closes |

### 6.5 Race conditions

| Scenario | Resolution |
|---|---|
| Deep-link delivered during fresh-identity boot | Deep-link wins: setting `redeemUrl` also sets `showWelcomeModal = false` (Flow 5) |
| User opens FeedbackModal while WelcomeModal is open | Allowed — modal stacking. Closing FeedbackModal returns to WelcomeModal |
| User rapid-toggles "Attach diagnostics" on/off/on | Stale-response guard pattern: only the latest request's response is reflected (`requestId === latestRequest`). Prevents stale-response overwrites |
| User clicks Submit then immediately clicks again | `submitting` flag disables button; double-submit prevented |

### 6.6 Explicit non-handlers (out of scope for Sub-C)

- GitHub rate-limiting on unauthenticated issue creation — GitHub's UI handles
- User on corporate network blocking github.com — user-environment issue; GH URL still copies to clipboard via fallback
- Profanity / spam filtering on feedback content — GitHub's job, not ours
- Persisting unfinished feedback drafts across modal dismissal — YAGNI for v0.1.0-alpha
- User intentionally submitting unredacted diagnostics — they can use the existing DiagnosticExportModal directly via Network Health panel + copy-paste (the (?) feedback flow is locked to redacted by design)

### 6.7 Bounded error vocabulary (per `feedback_engineer_for_real_scale`)

User-facing error strings are short, plain-language, and never leak transport internals:
- Welcome inline errors: "Paste an invite URL or click Skip" / "That doesn't look like a harmony:// invite"
- Feedback toast: "Couldn't open browser. URL copied to clipboard — paste it in your browser."
- Diagnostics-unavailable: "Diagnostics unavailable — submit without?"

## 7. Testing

### 7.1 Pure-function tests

`src/lib/__tests__/onboarding-env.test.ts`:

```typescript
describe('buildGitHubIssueUrl', () => {
  it('returns URL with title and Description section when description-only', ...);
  it('includes Environment section with all four fields', ...);
  it('includes Network diagnostics section when diagnostics provided', ...);
  it('URL-encodes special chars (spaces, &, =, #, newlines)', ...);
  it('truncates title at 50 chars', ...);
  it('strips newlines from title', ...);
  it('truncates diagnostics body when total URL > 8000 chars with marker', ...);
  it('preserves description + env intact even when diagnostics truncated', ...);
  it('handles empty diagnostics as no Network diagnostics section', ...);
});

describe('collectEnvironment', () => {
  it('returns full info when Tauri OS plugin works (mocked)', ...);
  it('returns unknown for fields when plugin rejects', ...);
  it('never throws to caller', ...);
});
```

### 7.2 Component tests

**`src/lib/components/__tests__/WelcomeModal.test.ts`** (~10 tests):
- renders when `open=true`; doesn't render when `open=false`
- empty paste + "Join now" → inline error, modal stays open, `onJoinWithInvite` not called
- malformed URL + "Join now" → inline error, modal stays open
- valid `harmony://invite/...` + "Join now" → `onJoinWithInvite(url)` called with the URL
- "Skip for now" → `onDismiss()` called
- Escape key → `onDismiss()` called
- backdrop click → `onDismiss()` called
- version label rendered in footer
- feedback-docs link rendered in footer

**`src/lib/components/__tests__/FeedbackModal.test.ts`** (~12 tests):
- renders textarea + diagnostics toggle (off default) + Submit/Cancel
- Submit disabled when description < 10 chars
- toggle ON → invokes `network_health_export_payload({ includeFullIds: false })` → preview shows redacted markdown
- toggle OFF → preview hides; diagnostics not in submitted URL
- Submit with description + toggle OFF → `tauri.shell.open` called; URL does NOT contain `## Network diagnostics`
- Submit with description + toggle ON → `tauri.shell.open` called; URL contains diagnostics body
- `tauri.shell.open` rejects → clipboard.writeText called; toast shown
- `network_health_export_payload` rejects with toggle ON → "Diagnostics unavailable" shown; Submit still works (without diagnostics)
- **stale-response guard regression test** (per ZEB-329 R3): rapid toggle on/off/on → only latest response reflected; intermediate responses dropped
- URL > 8000 chars → diagnostics truncated; truncation marker visible in preview before submit
- **privacy-invariant test (per `feedback_second_order_correctness_review`)**: when diagnostics are attached, the built URL contains NO match for `/[0-9a-f]{32,}/` (full Ed25519 hex). Mirrors ZEB-329's redaction-leak regex test.
- Submit button shows loading state while `submitting=true`; second click no-op

**`src/lib/components/__tests__/HelpMenuButton.test.ts`** (~8 tests):
- renders (?) button initially; no dropdown visible
- click → dropdown opens with 4 items in expected order: Submit Feedback / Network Health / About / Documentation
- click outside → dropdown closes
- Escape → dropdown closes
- click each item → corresponding callback fires + dropdown closes
- keyboard navigation: ↓/↑ cycles items, Enter triggers, Tab closes (a11y baseline)
- ARIA: button has `aria-label="Help and feedback"`, dropdown has `role="menu"`, items have `role="menuitem"`

### 7.3 Backend tests

`src-tauri/src/lib.rs` (or wherever `start_node` lives):

```rust
#[test]
fn start_node_response_serializes_to_camel_case() {
    let r = StartNodeResponse {
        node_addr: "iroh:abc".to_string(),
        freshly_created: true,
    };
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains("\"freshlyCreated\":true"));
    assert!(json.contains("\"nodeAddr\":"));
}

#[test]
fn start_node_freshly_created_when_no_keychain_entry() { ... }

#[test]
fn start_node_not_freshly_created_when_keychain_has_existing() { ... }
```

The keychain-existence tests use the existing test-fixture pattern (likely `#[cfg(test)]` mock keychain — implementer verifies; otherwise use a tempdir-scoped real keychain in a serial test).

### 7.4 App.svelte integration (implicit, via fixtures)

App.svelte has no direct test today (top-level component convention). Integration is implicit through:
- Updated mock for `start_node` IPC in component-test fixtures (returns the new `{ nodeAddr, freshlyCreated }` shape)
- Manual smoke testing per §8

### 7.5 Test discipline (per memory rules)

- `feedback_test_drift_is_our_fault`: any new test failures from this PR are blocking; pre-existing orphan failures captured at Task 0 baseline are not blocking
- `feedback_tauri_error_extraction`: both modals catch with `e instanceof Error ? e.message : String(e)`; tests mock both Error-object and string rejections
- `feedback_second_order_correctness_review`: write the diagnostic-attach privacy-invariant test FIRST (regex-leak assertion that a URL with toggle ON contains the redacted markdown but does NOT match `/[0-9a-f]{32,}/`)

## 8. Manual smoke checklist

To be executed by implementer at PR-ready and called out in PR description:

1. **Fresh install path**: delete keychain entry (`security delete-generic-password -s "harmony.client"` on macOS) → launch app → WelcomeModal fires → paste valid `harmony://invite/...` → RedeemInviteDialog opens → join succeeds → land in community
2. **Returning user path**: launch app with existing keychain entry → no welcome modal → silent boot
3. **Feedback submit (no diagnostics)**: (?) → Submit Feedback → type "test feedback message" → Submit → default browser opens to `https://github.com/zeblithic/harmony-client/issues/new?title=...&body=...` with description visible in body
4. **Feedback submit (with diagnostics)**: same + toggle ON → preview pane shows redacted markdown → Submit → browser body contains `## Network diagnostics` section
5. **Deep-link during fresh boot**: delete keychain → launch app + open `harmony://invite/...` URL from terminal → RedeemInviteDialog shows over background; welcome modal NOT shown (suppressed by deep-link)
6. **Help menu dropdown**: (?) → all 4 items present → Network Health routes to `/network` → About modal renders → Documentation opens README in browser
7. **shell.open failure path**: (Linux without xdg-open or similar) → Submit Feedback → confirm clipboard-copy fallback fires

## 9. Out of scope

Cross-referenced explicitly so the implementer doesn't drift:

- **Sub-D's invite-distribution playbook + Zeblithic minting** ([ZEB-330](https://linear.app/zeblith/issue/ZEB-330)) — parked
- **[ZEB-321](https://linear.app/zeblith/issue/ZEB-321) Phase 3+** — liveness / rebinding / mobile push, not yet started
- **[ZEB-173](https://linear.app/zeblith/issue/ZEB-173) multi-device identity binding** — single-device alpha
- **Identity recovery flow itself** ([ZEB-202](https://linear.app/zeblith/issue/ZEB-202)) — separate track; C may surface "back up your identity" as a *prompt* in troubleshooting.md but does NOT implement recovery
- **Display-name capture in welcome** — defer to existing Profile editor; lower urgency than invite paste
- **"Show welcome again" affordance** — YAGNI
- **In-app feedback form posting to a backend** — violates no-telemetry invariant
- **Mailto: feedback channel** — private channel breaks tester community visibility
- **Profanity / spam filtering on feedback** — GitHub's job
- **GitHub-issue rate-limit handling** — GitHub's UI
- **Identity backup automatic prompt** — `BackupStalenessWarning` already ambient
- **New CRDT events** — none required; no wire-format change

## 10. References

- Parent umbrella: [ZEB-327](https://linear.app/zeblith/issue/ZEB-327) — v0.1.0-alpha distribution + onboarding + validation
- Sub-A: [ZEB-328](https://linear.app/zeblith/issue/ZEB-328) (PR #160 merged) — build pipeline + auto-updater + per-OS install docs + harmony:// deep-link wiring
- Sub-B: [ZEB-329](https://linear.app/zeblith/issue/ZEB-329) (PR #161 merged) — Network Health panel + DiagnosticExportModal + server-side redaction invariant
- Sub-D: [ZEB-330](https://linear.app/zeblith/issue/ZEB-330) (parked) — Zeblithic minting + invite-distribution playbook + end-to-end alpha-tester run
- Phase 2c invite handshake: PR #159 (`c5c4da9`) — iroh bi-stream completes cross-WAN join; this spec reuses the existing RedeemInviteDialog flow without protocol changes
- Existing components reused: `RedeemInviteDialog`, `BackupStalenessWarning`, `IdentityPanel`, `NetworkHealthView`, `DiagnosticExportModal`
- Existing IPC reused: `network_health_export_payload({ includeFullIds: false })` from ZEB-329
- Existing helper reused: `extractHarmonyInviteUrl` from `src/lib/deep-link-router.ts`
- Existing Tauri plugins reused: `@tauri-apps/plugin-shell` (browser open), `@tauri-apps/plugin-os` (platform/version), `@tauri-apps/plugin-deep-link` (already wired)

---

**End of design spec.** Implementation plan to follow in `docs/plans/2026-05-25-zeb-331-sub-c-onboarding-plan.md`.
