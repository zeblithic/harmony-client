# ZEB-721 Clock-Regression Liveness Implementation Plan

> **For agentic workers:** Steps use checkbox (`- [ ]`) syntax for tracking. TDD, DRY, YAGNI, frequent commits.

**Goal:** Make the shared self-liveness refresh path detect a regressed/pre-epoch host clock, never emit a bad cert, and surface the anomaly to the Devices panel — without fabricating time.

**Architecture:** `refresh_self_liveness` returns a `LivenessRefreshOutcome` enum (was `bool`), detecting a future-stamped (regressed) cert in the shared path and warn-logging once. The two panel call sites skip on a pre-epoch clock (fixing the `unwrap_or(0)` footgun). A shared `self_liveness_future_skew_secs` helper feeds a new `OwnerStateView.self_clock_regressed_skew_secs` field, rendered as a DevicesPanel banner.

**Tech Stack:** Rust (Tauri backend), Svelte 5 + TypeScript frontend; `cargo nextest`, `vitest`, `tsc`.

## Global Constraints

- **harmony-client only** — no `harmony-owner` change (it is only read).
- **Iterative gates use `--lib`/scoped** (`refresh_self_liveness` lives in the lib; a lib change relinks ~97 integ binaries ≈ 50 min). Full `--workspace --all-targets` sweep runs ONCE at the end (Task 5). Per-task Rust gate: `cargo nextest run --locked --lib --features test-fixtures` + `cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings`.
- **camelCase over IPC**: Rust `self_clock_regressed_skew_secs` ⇄ TS `selfClockRegressedSkewSecs`.
- **No fabricated time, no new persisted state, no CRDT/merge/threshold changes** (spec §Scope guardrails).
- Final gate set (Task 5): `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`, `npx tsc --noEmit`, `npx vitest run`.

## File Structure

- `src-tauri/src/owner_state.rs` — `LivenessRefreshOutcome` enum; `self_liveness_future_skew_secs` helper; `refresh_self_liveness` refactor; `OwnerStateView.self_clock_regressed_skew_secs`; unit tests.
- `src-tauri/src/liveness_heartbeat.rs` — map the enum via `.wrote()`; delete the now-duplicate future-warn.
- `src-tauri/src/owner_commands.rs` — `now_unix_checked()`; both panel sites skip pre-epoch + use `.wrote()`; `build_owner_state_view` populates the DTO field.
- `src/lib/owner-service.ts` — add `selfClockRegressedSkewSecs?: number`.
- `src/lib/components/DevicesPanel.svelte` — clock-regressed banner.

---

### Task 1: Enum + shared regressed-detection + migrate all call sites

**Files:**
- Modify: `src-tauri/src/owner_state.rs:873-899` (fn), add enum+helper above it; tests `:1739-1800`, `:1836`.
- Modify: `src-tauri/src/liveness_heartbeat.rs:41-66` (drop duplicate warn), tests `:190-219`.
- Modify: `src-tauri/src/owner_commands.rs:729`, `:759` (mechanical `.wrote()`; pre-epoch skip comes in Task 2).

**Interfaces produced:**
- `enum LivenessRefreshOutcome { Refreshed, Fresh, ClockRegressed { skew_secs: u64 }, SignFailed }` with `fn wrote(self) -> bool`.
- `fn self_liveness_future_skew_secs(state: &OwnerState, device_sk: &SigningKey, now: u64) -> Option<u64>`.
- `fn refresh_self_liveness(state: &mut OwnerState, device_sk: &SigningKey, now: u64) -> LivenessRefreshOutcome`.

- [ ] **Step 1: Write failing tests** in `owner_state.rs` `mod tests` (new + migrate). Add:

```rust
#[test]
fn refresh_self_liveness_reports_clock_regressed_and_does_not_resign() {
    let mint_t = 1_700_000_000;
    let (mut state, device_signing_key) = mint_fixture(mint_t); // existing test seam used by the sibling tests
    assert_eq!(
        refresh_self_liveness(&mut state, &device_signing_key, mint_t),
        LivenessRefreshOutcome::Refreshed
    );
    // Clock regresses 100 days behind the cert.
    let regressed = mint_t - 100 * 24 * 60 * 60;
    let out = refresh_self_liveness(&mut state, &device_signing_key, regressed);
    assert_eq!(
        out,
        LivenessRefreshOutcome::ClockRegressed { skew_secs: mint_t - regressed }
    );
    assert!(!out.wrote(), "regressed clock must not write");
    let id = *state.enrollments.keys().next().unwrap();
    assert_eq!(state.liveness.get(&id).unwrap().timestamp, mint_t, "timestamp must not move");
}

#[test]
fn self_liveness_future_skew_secs_some_when_future_none_when_healthy() {
    let t = 1_700_000_000;
    let (mut state, sk) = mint_fixture(t);
    let _ = refresh_self_liveness(&mut state, &sk, t);
    assert_eq!(self_liveness_future_skew_secs(&state, &sk, t), None, "cert at now = healthy");
    assert_eq!(self_liveness_future_skew_secs(&state, &sk, t - 10), Some(10), "cert 10s in future");
    assert_eq!(self_liveness_future_skew_secs(&state, &sk, t + 10), None, "cert in past = healthy");
}
```

(Match the existing tests' fixture idiom — `mint_owner(t)`/`MintResult` destructuring as at `owner_state.rs:1739+`; use whatever seam the sibling tests already use rather than inventing `mint_fixture` if none exists.)

Migrate existing bool assertions:
- `:1749` `assert!(mutated, …)` → `assert_eq!(refresh_self_liveness(…), LivenessRefreshOutcome::Refreshed, …)`.
- `:1774` (second call, fresh) → `assert_eq!(…, LivenessRefreshOutcome::Fresh, …)`.
- `:1792` (stale re-sign) → `assert_eq!(…, LivenessRefreshOutcome::Refreshed, …)`.
- `:1836` `if refresh_self_liveness(…) {` → `if refresh_self_liveness(…).wrote() {`.

- [ ] **Step 2: Run tests — expect compile failure** (enum/helper not defined yet). `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures -E 'test(refresh_self_liveness) + test(self_liveness_future_skew)'`

- [ ] **Step 3: Implement** in `owner_state.rs` (above the fn, ~line 860):

```rust
/// Outcome of a self-liveness refresh attempt (ZEB-721). `Refreshed` is the only
/// variant that mutated `state`; the others explain *why* nothing was written so
/// callers can persist-on-write and surface clock health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessRefreshOutcome {
    /// Re-signed a fresh cert at `now`. Caller MUST persist + notify_dirty.
    Refreshed,
    /// Existing cert is still fresh (< freshness/2 old). Healthy steady state.
    Fresh,
    /// Our own cert is stamped in the FUTURE vs `now` — the host clock regressed
    /// behind it. Not re-signed (a lower timestamp loses the CRDT merge, and
    /// fabricating time is not our posture). Self-heals when the clock recovers.
    ClockRegressed { skew_secs: u64 },
    /// Signing or `add_liveness` failed (already warn-logged). No-op for callers.
    SignFailed,
}

impl LivenessRefreshOutcome {
    /// True iff the call mutated `state` (caller must persist + notify_dirty).
    pub fn wrote(self) -> bool {
        matches!(self, Self::Refreshed)
    }
}

/// Seconds this device's own liveness cert is stamped in the FUTURE relative to
/// `now` — i.e. the host clock regressed behind our own cert. `None` when there
/// is no self-cert or it is at/behind `now` (healthy). Shared by the refresh
/// decision and the `OwnerStateView` surfacing so both agree. ZEB-721.
pub fn self_liveness_future_skew_secs(
    state: &OwnerState,
    device_sk: &SigningKey,
    now: u64,
) -> Option<u64> {
    let device_id = device_id_from_signing_key(device_sk);
    state
        .liveness
        .get(&device_id)
        .and_then(|c| c.timestamp.checked_sub(now))
        .filter(|&skew| skew > 0)
}
```

Replace the fn body (`:873-899`) with:

```rust
pub fn refresh_self_liveness(
    state: &mut OwnerState,
    device_sk: &SigningKey,
    now: u64,
) -> LivenessRefreshOutcome {
    use harmony_owner::certs::LivenessCert;

    let device_id = device_id_from_signing_key(device_sk);
    let threshold = harmony_owner::trust::DEFAULT_FRESHNESS_WINDOW_SECS / 2;
    match state.liveness.get(&device_id) {
        // Regressed clock: cert looks fresh forever → renewal suppressed. Do NOT
        // re-sign (lower timestamp loses the CRDT merge; fabricating time is not
        // our posture, ZEB-721). Surface instead of a silent no-op; self-heals
        // when the clock recovers.
        Some(cert) if cert.timestamp > now => {
            let skew_secs = cert.timestamp - now;
            tracing::warn!(
                target: "harmony_liveness",
                cert_ts = cert.timestamp,
                now,
                skew_secs,
                "self-liveness cert is stamped in the future — host clock regressed; not renewing until the clock recovers"
            );
            LivenessRefreshOutcome::ClockRegressed { skew_secs }
        }
        Some(cert) if cert.timestamp >= now.saturating_sub(threshold) => {
            LivenessRefreshOutcome::Fresh
        }
        // Stale or missing → (re-)sign.
        _ => match LivenessCert::sign(device_sk, state.owner_id, now) {
            Ok(cert) => match state.add_liveness(cert) {
                Ok(()) => LivenessRefreshOutcome::Refreshed,
                Err(e) => {
                    tracing::warn!(error = %e, "refresh_self_liveness: add_liveness failed");
                    LivenessRefreshOutcome::SignFailed
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "refresh_self_liveness: liveness sign failed");
                LivenessRefreshOutcome::SignFailed
            }
        },
    }
}
```

- [ ] **Step 4: Migrate production call sites to `.wrote()`** (still using `now_unix()` — pre-epoch skip is Task 2):
  - `liveness_heartbeat.rs:41-66`: reduce `run_liveness_heartbeat_once` to `refresh_self_liveness(&mut g, device_sk, now_secs).wrote()`; **delete** the manual future-stamp warn block (`:47-64`) and its now-unused `device_id` line + `device_id_from_signing_key` use; update the fn doc comment to note detection now lives in the shared path.
  - `owner_commands.rs:729`: `let refreshed = refresh_self_liveness(&mut g, &loaded.device_signing_key, now_unix()).wrote();`
  - `owner_commands.rs:759`: `if refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now_unix()).wrote() {`

- [ ] **Step 5: Run gate** — `cd src-tauri && cargo nextest run --locked --lib --features test-fixtures` (heartbeat + owner_state tests) — expect PASS (incl. the unchanged `heartbeat_once_noop_on_regressed_clock`, which still sees a no-op + preserved timestamp). Then `cargo clippy --locked --lib --features test-fixtures --no-deps -- -D warnings` and `cargo fmt --all`.

- [ ] **Step 6: Commit** — `git add -A && git commit -m "feat(zeb-721): LivenessRefreshOutcome enum + shared regressed-clock detection"` (+ trailers).

---

### Task 2: Panel-path pre-epoch skip (fix `unwrap_or(0)` footgun)

**Files:** Modify `src-tauri/src/owner_commands.rs` — add `now_unix_checked()` near `now_unix()` (`:61`); guard both refresh sites (`:729`, `:759`). Test: `owner_commands.rs` `mod tests`.

- [ ] **Step 1: Write failing test** — a `get_owner_state`-path refresh under a pre-epoch clock must NOT stamp a `timestamp=0` cert. If the existing tests can't inject a pre-epoch `now` into the command, test the seam directly instead: assert that `refresh_self_liveness` is only reached with a real `now` by unit-testing `now_unix_checked()`'s contract via a small helper that mirrors the guard (skip when `None`). Concretely add:

```rust
#[test]
fn pre_epoch_guard_skips_refresh_leaving_no_zero_cert() {
    // Mirror the call-site guard: when the epoch-checked clock is None, we must
    // not call refresh_self_liveness (which would stamp a 0-timestamp cert).
    let t = 1_700_000_000;
    let (mut state, sk) = /* mint fixture as elsewhere in this module */;
    let now_checked: Option<u64> = None; // simulate pre-epoch
    let wrote = match now_checked {
        Some(now) => crate::owner_state::refresh_self_liveness(&mut state, &sk, now).wrote(),
        None => false,
    };
    assert!(!wrote);
    assert!(state.liveness.get(&crate::owner_state::device_id_from_signing_key(&sk)).is_none()
        || state.liveness.values().all(|c| c.timestamp != 0),
        "no timestamp-0 cert may exist");
    let _ = t;
}
```

(If the module already has a `get_owner_state` integration harness that can inject `now`, prefer asserting the real path; otherwise this guard-mirroring unit test is the pragmatic pin.)

- [ ] **Step 2: Run — expect FAIL/compile-until-implemented.**

- [ ] **Step 3: Implement** `now_unix_checked()` after `now_unix()`:

```rust
/// Epoch-checked sibling of `now_unix()` (ZEB-721): `None` when the host clock is
/// before the Unix epoch, so liveness callers SKIP signing a bogus timestamp-0
/// cert instead of `unwrap_or(0)`-ing.
fn now_unix_checked() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .ok()
}
```

Site A (`:727-736`):

```rust
        let (snapshot, wrote) = {
            let mut g = doc.lock().await;
            let wrote = match now_unix_checked() {
                Some(now) => refresh_self_liveness(&mut g, &loaded.device_signing_key, now).wrote(),
                None => {
                    tracing::warn!(target: "harmony_liveness", "get_owner_state: host clock before Unix epoch; skipping self-liveness refresh");
                    false
                }
            };
            (g.clone(), wrote)
        };
        if wrote {
            engine.notify_dirty();
        }
```

Site B (`:759-770`):

```rust
            let did_write = match now_unix_checked() {
                Some(now) => refresh_self_liveness(&mut loaded.state, &loaded.device_signing_key, now).wrote(),
                None => {
                    tracing::warn!(target: "harmony_liveness", "get_owner_state: host clock before Unix epoch; skipping self-liveness refresh");
                    false
                }
            };
            if did_write {
                if let Err(e) = save_owner_state_cbor_only(&identity_dir, &loaded.state) {
                    tracing::warn!(error = %e, "get_owner_state: failed to persist refreshed liveness; rendering from in-memory state");
                }
            }
```

- [ ] **Step 4: Gate** — `cargo nextest run --locked --lib --features test-fixtures` + clippy `--lib` + fmt. PASS.
- [ ] **Step 5: Commit** — `feat(zeb-721): skip liveness refresh on a pre-epoch clock (panel path)`.

---

### Task 3: Surface skew on `OwnerStateView` + Devices panel builder

**Files:** Modify `src-tauri/src/owner_state.rs` (struct `:16-49`, literal in test `:2056`); `src-tauri/src/owner_commands.rs` (`build_owner_state_view` `:385-575`, literal `:562`).

- [ ] **Step 1: Write failing test** in `owner_commands.rs` `mod tests`: build a view whose self-device cert is future-stamped and assert `view.self_clock_regressed_skew_secs == Some(skew)`, and `None` when healthy. Use the module's existing `build_owner_state_view` fixture path (e.g. the `two_device_fixture` + `LoadedOwnerState` seams already used at `:2937`+); if `now` cannot be injected into `build_owner_state_view` (it calls `now_unix()`), assert the helper directly at the DTO level: `assert_eq!(crate::owner_state::self_liveness_future_skew_secs(&loaded.state, &loaded.device_signing_key, past_now), Some(skew))`.

- [ ] **Step 2: Run — expect FAIL (field missing).**

- [ ] **Step 3: Implement.** Add to `OwnerStateView` (after `quorum_armed_until_ms`, `:48`):

```rust
    /// ZEB-721: seconds THIS device's own liveness cert is stamped in the future
    /// relative to the host clock at snapshot time — the host clock regressed
    /// behind an already-signed cert, pausing liveness renewal until it recovers.
    /// `None` when healthy. Drives the DevicesPanel clock-regressed banner.
    #[serde(default)]
    pub self_clock_regressed_skew_secs: Option<u64>,
```

In `build_owner_state_view` (compute near `:394`, using the existing `now` at `:391`):

```rust
    let self_clock_regressed_skew_secs = crate::owner_state::self_liveness_future_skew_secs(
        &loaded.state,
        &loaded.device_signing_key,
        now,
    );
```

Add `self_clock_regressed_skew_secs,` to the struct literal at `:562`. Add `self_clock_regressed_skew_secs: None,` to the test literal at `owner_state.rs:2056`.

- [ ] **Step 4: Gate** — `cargo nextest run --locked --lib --features test-fixtures` + clippy `--lib` + fmt. PASS.
- [ ] **Step 5: Commit** — `feat(zeb-721): surface self-clock-regressed skew on OwnerStateView`.

---

### Task 4: Frontend — TS type + DevicesPanel banner

**Files:** Modify `src/lib/owner-service.ts` (interface `:3-41`); `src/lib/components/DevicesPanel.svelte` (banner near the `fleet-epoch-banner`, `:915`).

- [ ] **Step 1: Add the optional field** to `OwnerStateView` (before the closing `}` at `:41`):

```ts
  /**
   * ZEB-721: seconds THIS device's own liveness cert is stamped in the future
   * vs the host clock — the clock regressed behind an already-signed cert,
   * pausing liveness renewal until it recovers. Absent/`undefined` when healthy.
   */
  selfClockRegressedSkewSecs?: number;
```

- [ ] **Step 2: Add the banner** in `DevicesPanel.svelte`, immediately before the `{#if state.fleetEpochStale}` block (`:915`), reusing the `epoch-banner` class:

```svelte
      {#if typeof state.selfClockRegressedSkewSecs === 'number' && state.selfClockRegressedSkewSecs > 0}
        <div class="epoch-banner" data-testid="clock-regressed-banner" role="alert">
          <p class="epoch-text">
            This device's clock appears to have moved backwards (~{formatApproxDuration(
              state.selfClockRegressedSkewSecs
            )} behind its last check-in). Liveness renewal is paused until the clock is
            corrected — re-sync system time (NTP) to restore trust freshness on your other devices.
          </p>
        </div>
      {/if}
```

Add a small local `formatApproxDuration(secs: number): string` in the component script (days/hours/minutes; reuse the existing countdown/relative formatter in this file if one is in scope — search for `formatCountdown` at `:424` and prefer it if its unit is seconds).

- [ ] **Step 3: Frontend gate** — from repo root: `npx tsc --noEmit` (PASS) and `npx vitest run` (PASS). Add a focused vitest only if a service-level seam maps the field → banner; the banner itself is presentational (tsc-covered) and listed in the ZEB-224 manual checklist.
- [ ] **Step 4: Commit** — `feat(zeb-721): DevicesPanel clock-regressed banner`.

---

### Task 5: Full CI-parity sweep + final commit

- [ ] **Step 1:** `cd src-tauri && cargo fmt --all -- --check`
- [ ] **Step 2:** `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- [ ] **Step 3:** `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (the ~50-min relink sweep; background with a wall-clock net).
- [ ] **Step 4:** repo root: `npx tsc --noEmit` && `npx vitest run`.
- [ ] **Step 5:** Update spec/plan "as-built" if anything diverged; commit any fmt/fixups. Working tree clean.

## Self-Review

- **Spec coverage:** enum + shared detection (spec §Arch 1) = Task 1; pre-epoch fix (§Arch 2) = Task 2; DTO + builder (§Arch 3) = Task 3; heartbeat/glue (§Arch 4) = Task 1 step 4; frontend banner (§Arch 3) = Task 4. ✓
- **Type consistency:** `self_clock_regressed_skew_secs: Option<u64>` ⇄ `selfClockRegressedSkewSecs?: number`; `self_liveness_future_skew_secs` signature identical in Tasks 1 & 3; `LivenessRefreshOutcome::wrote()` used at all 3 prod sites. ✓
- **No placeholders:** all substantive code inlined; the only deferred detail is matching each test's existing fixture idiom (mint seam), resolved with the file open. ✓
