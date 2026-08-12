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
  freshlyCreated?: boolean;
  /**
   * ZEB-338: true iff an owner identity loaded during this start_node call.
   * The frontend hard-gates the WelcomeModal on this:
   * `showWelcomeModal = !hasOwnerIdentity`.
   * Forward-compat: treat missing/undefined as `false` (older backend mid-
   * deploy → show onboarding, the safe default).
   */
  hasOwnerIdentity?: boolean;
  /**
   * ZEB-668 S2: true when this device's own enrollment is revoked in the
   * owner trust state. `hasOwnerIdentity` is false in that case (boot
   * refuses fleet wiring), so this flag MUST be checked first — otherwise
   * the revoked device misclassifies as first-run and hits the mint gate,
   * which refuses while owner_state.cbor exists (unrecoverable dead-end).
   * Forward-compat: treat missing/undefined as `false`.
   */
  selfRevoked?: boolean;
  /**
   * ZEB-836: true when the device key loaded from the vault is not enrolled in
   * the persisted `owner_state.cbor` (a keychain/on-disk desync). Like
   * `selfRevoked`, `hasOwnerIdentity` is false, so this MUST be checked before
   * `missing` or the user hits the mint gate. Renders a recovery screen ("your
   * other devices are safe") with restore/reset actions rather than the generic
   * startup-error dead-end. Forward-compat: treat missing/undefined as `false`.
   */
  selfEnrollmentMissing?: boolean;
  /**
   * ZEB-904/905: true when an owner identity loaded from disk but this device
   * holds no master seed and no fleet-KeyTree material — the node booted
   * LOCAL-ONLY (communities/channels/profile work; device-to-device sync,
   * friend features, and encrypted file shares are paused). Unlike the two
   * flags above this is NOT an identity-classification input: the owner is
   * present and operational, so it drives an informational banner, never the
   * mint gate or a recovery screen. Forward-compat: treat missing/undefined
   * as `false`.
   */
  fleetCryptoMissing?: boolean;
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
