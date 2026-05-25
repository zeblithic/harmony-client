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
