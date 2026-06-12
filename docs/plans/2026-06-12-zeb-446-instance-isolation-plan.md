# ZEB-446: Side-by-side instance isolation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Named storage profiles + degradable Reticulum bind + pairing-over-API so a pinned headless `harmony-app serve` coordination instance runs beside the default-profile dev GUI on one machine, enrolled as a separate device in the owner's fleet.

**Architecture:** A process-global, first-wins profile value (new `profile.rs`, `OnceLock`) consulted by the three existing path chokepoints (`resolve_app_data_dir`, `identity::resolve_path`, `app_tracing` log dir). Named profiles are file-vault-only (the ZEB-428 keychain constructor gate gains a third refusal condition) with a fail-fast passphrase guard at both entrypoints. The fixed UDP 4242 Reticulum bind becomes non-fatal with a `HARMONY_RETICULUM_PORT` override (`0` = disabled) — the socket stays non-`Option` at its ~20 downstream call sites via a loopback-bound "dead socket". The 6 pairing IPCs get `*_inner` seams and RPC registry entries (29 → 35 commands).

**Tech Stack:** Rust (tauri app lib), clap, tokio, existing `api/rpc.rs` `rpc!` macro, cargo-nextest.

**Spec:** `docs/specs/2026-06-12-zeb-446-instance-isolation-design.md` (commit `30943e18`). Branch `zeb-446-instance-isolation` off main `95e54850`.

---

## House rules (every task)

- Work directly in `/Users/zeblith/work/zeblithic/harmony-client` on branch `zeb-446-instance-isolation`. **No worktrees.** Never push.
- **Commit BEFORE running gates** (so a gate timeout never strands uncommitted work), then amend if the gate forces fixes.
- Per-task gates (run from `src-tauri/`), with `set -o pipefail` on any piped command:
  ```bash
  cargo fmt --all
  cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
  cargo nextest run --locked -p harmony-app --lib --features test-fixtures
  ```
  Tasks touching `src-tauri/src/main.rs` add `--bins` to the clippy line. Task 6 adds `--test profile_isolation` to the nextest line. **`--all-targets` is reserved for Task 7's final sweep only** (lib changes relink ~97 integration-test binaries; ~25 min).
- 10-minute wall-clock kill switch per gate command (Bash tool timeout param — macOS has no `timeout`). If a gate exceeds it: report `DONE_WITH_CONCERNS` with the partial output; do not silently wait.
- Report status as `DONE` / `DONE_WITH_CONCERNS` / `NEEDS_CONTEXT` / `BLOCKED`.
- Never weaken `tests/keychain_isolation.rs`. Never construct `KeychainStore::new()` in test-reachable code — inject through `*_inner` seams.
- Rust 2021; match surrounding comment density and style. Error strings are user-facing — keep them actionable.

---

### Task 1: `profile.rs` — named-profile selection module

**Files:**
- Create: `src-tauri/src/profile.rs`
- Modify: `src-tauri/src/lib.rs` (one `pub mod profile;` line, next to the other mod declarations near the top — find the block with `pub mod api;` / `mod app_tracing;`)

- [ ] **Step 1: Write `src-tauri/src/profile.rs`** (complete file, including its unit tests):

```rust
// src-tauri/src/profile.rs — ZEB-446: named-profile selection.
//
// A "profile" scopes BOTH storage roots — the `~/.harmony` identity tree
// and the `net.zeblith.harmony` app-data tree — so a second harmony-app
// instance (the pinned headless "coordination" node) can run beside the
// default GUI without clobbering it. No profile = the historical layout,
// byte-for-byte.
//
// Activation is FIRST-WINS and process-global: binary entrypoints
// (main.rs after argv parse, lib.rs run()) activate eagerly before any
// tracing init or path resolution, so an invalid name is a loud startup
// error — never a silent fall-through into the default profile's data.

use std::sync::OnceLock;

static ACTIVE_PROFILE: OnceLock<Option<String>> = OnceLock::new();

/// Profile names are conservative path components: `[a-z0-9][a-z0-9_-]{0,31}`.
/// `default` is reserved — the default profile is selected by OMITTING the
/// flag/env, and a literal `profiles/default` dir would masquerade as it.
pub fn validate_profile_name(name: &str) -> Result<(), String> {
    let ok_len = (1..=32).contains(&name.len());
    let ok_start = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let ok_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !(ok_len && ok_start && ok_chars) {
        return Err(format!(
            "invalid profile name {name:?}: must match [a-z0-9][a-z0-9_-]{{0,31}}"
        ));
    }
    if name == "default" {
        return Err(
            "profile name \"default\" is reserved — omit --profile / HARMONY_PROFILE \
             to use the default profile"
                .to_string(),
        );
    }
    Ok(())
}

/// Resolve the desired profile from an explicit flag (wins) or the
/// HARMONY_PROFILE env value. Pure for unit-testability; whitespace-only
/// env is treated as unset.
fn desired_profile(flag: Option<&str>, env: Option<&str>) -> Result<Option<String>, String> {
    let raw = match (flag, env) {
        (Some(f), _) => Some(f),
        (None, Some(e)) if !e.trim().is_empty() => Some(e),
        _ => None,
    };
    match raw {
        None => Ok(None),
        Some(name) => {
            let name = name.trim();
            validate_profile_name(name)?;
            Ok(Some(name.to_string()))
        }
    }
}

/// Activate the profile from a CLI flag (or, when `None`, from
/// HARMONY_PROFILE). First call wins; later calls are no-ops — by then
/// paths may already be resolved under the live value, so re-activation
/// would lie. Hard error on an invalid name (flag OR env): silently
/// landing in the default profile's data is the worst outcome.
pub fn set_active_profile(flag: Option<&str>) -> Result<(), String> {
    let desired = desired_profile(flag, std::env::var("HARMONY_PROFILE").ok().as_deref())?;
    let _ = ACTIVE_PROFILE.set(desired);
    Ok(())
}

/// The live profile. Lazily activates from HARMONY_PROFILE if no
/// entrypoint called [`set_active_profile`] (library/test consumers;
/// nextest is process-per-test, so the OnceLock never leaks across
/// tests). Panics on an invalid env value in this lazy path — every
/// binary entrypoint validates eagerly, so that is unreachable in the
/// shipped app and a programming error in a test.
pub fn active_profile() -> Option<&'static str> {
    ACTIVE_PROFILE
        .get_or_init(|| {
            desired_profile(None, std::env::var("HARMONY_PROFILE").ok().as_deref())
                .unwrap_or_else(|e| {
                    panic!("HARMONY_PROFILE invalid and no entrypoint validated it: {e}")
                })
        })
        .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_sane_names_rejects_garbage() {
        for ok in ["coord", "a", "x2", "test-prof_1", "abcdefghijklmnopqrstuvwxyz012345"] {
            assert!(validate_profile_name(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in [
            "",
            "Coord",            // uppercase
            "-lead",            // bad start char
            "_lead",            // bad start char
            "has space",
            "has/slash",
            "has.dot",
            "..",
            "abcdefghijklmnopqrstuvwxyz0123456", // 33 chars
        ] {
            assert!(validate_profile_name(bad).is_err(), "{bad:?} should be invalid");
        }
        let err = validate_profile_name("default").unwrap_err();
        assert!(err.contains("reserved"), "default must be reserved: {err}");
    }

    #[test]
    fn desired_profile_flag_wins_env_fallback_empty_env_ignored() {
        assert_eq!(desired_profile(None, None).unwrap(), None);
        assert_eq!(desired_profile(None, Some("  ")).unwrap(), None);
        assert_eq!(
            desired_profile(None, Some("envprof")).unwrap(),
            Some("envprof".to_string())
        );
        assert_eq!(
            desired_profile(Some("flagprof"), Some("envprof")).unwrap(),
            Some("flagprof".to_string())
        );
        assert!(desired_profile(Some("BAD NAME"), None).is_err());
        assert!(desired_profile(None, Some("BAD NAME")).is_err());
    }

    /// The pure join used by lib.rs's resolve_app_data_dir (defined there;
    /// exercised here so the profile module's test suite pins the layout).
    #[test]
    fn app_data_dir_in_maps_default_and_named() {
        use std::path::Path;
        let base = Path::new("base");
        assert_eq!(
            crate::app_data_dir_in(base, None),
            base.join("net.zeblith.harmony")
        );
        assert_eq!(
            crate::app_data_dir_in(base, Some("coord")),
            base.join("net.zeblith.harmony")
                .join("profiles")
                .join("coord")
        );
    }
}
```

NOTE: the `app_data_dir_in` test references a function Task 2 creates. **Tasks 1 and 2 are one compile/gate unit** — implement both before running the Task 2 gate. (Task 1's commit may be made with the test commented in only if you insist on per-task commits; preferred: a single commit covering Tasks 1+2.)

- [ ] **Step 2: Add the module declaration in `src-tauri/src/lib.rs`** — locate the mod-declaration block (search `pub mod api;`) and add:

```rust
pub mod profile;
```

- [ ] **Step 3: Proceed directly to Task 2** (shared gate).

---

### Task 2: Profile-aware path chokepoints (+ straggler audit)

**Files:**
- Modify: `src-tauri/src/lib.rs:268-278` (`resolve_app_data_dir`)
- Modify: `src-tauri/src/identity.rs:2050-2061` (`resolve_path`)
- Modify: `src-tauri/src/app_tracing.rs:16-35,111-124` (log dir + its test)

- [ ] **Step 1: Replace `resolve_app_data_dir` in `lib.rs`** (current body at lines 274-278) with:

```rust
pub fn resolve_app_data_dir() -> Result<std::path::PathBuf, String> {
    let base = dirs::data_dir().ok_or_else(|| "cannot resolve platform data dir".to_string())?;
    Ok(app_data_dir_in(&base, crate::profile::active_profile()))
}

/// Pure join for [`resolve_app_data_dir`] (and app_tracing's log dir):
/// `<base>/net.zeblith.harmony[/profiles/<p>]`. ZEB-446: a named profile
/// nests under `profiles/` so the default layout is untouched.
pub(crate) fn app_data_dir_in(
    base: &std::path::Path,
    profile: Option<&str>,
) -> std::path::PathBuf {
    let root = base.join("net.zeblith.harmony");
    match profile {
        Some(p) => root.join("profiles").join(p),
        None => root,
    }
}
```

Keep the existing doc comment on `resolve_app_data_dir` (the ZEB-445 split-brain note) and append one line: `/// ZEB-446: profile-aware — named profiles nest under profiles/<name>.`

- [ ] **Step 2: Replace `identity::resolve_path` in `identity.rs`** (lines 2050-2061) with:

```rust
/// Resolve the identity file path. `~/.harmony/identity.key` on the
/// default profile; `~/.harmony/profiles/<p>/identity.key` on a named
/// profile (ZEB-446 — named profiles get their own identity tree, which
/// also scopes the ZEB-449 encrypted-file vault and `iroh_sk.enc`).
pub fn resolve_path(override_path: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            "Cannot determine identity file path: neither $HOME nor $USERPROFILE is set".to_string()
        })?;
    Ok(identity_path_in(
        Path::new(&home),
        crate::profile::active_profile(),
    ))
}

/// Pure path join for [`resolve_path`] — unit-testable without env state.
fn identity_path_in(home: &Path, profile: Option<&str>) -> PathBuf {
    let root = home.join(".harmony");
    let root = match profile {
        Some(p) => root.join("profiles").join(p),
        None => root,
    };
    root.join("identity.key")
}
```

- [ ] **Step 3: Add a unit test for `identity_path_in`** in identity.rs's existing `#[cfg(test)] mod tests` (search `mod tests` in identity.rs):

```rust
#[test]
fn identity_path_in_maps_default_and_named_profiles() {
    use std::path::Path;
    let home = Path::new("/home/u");
    assert_eq!(
        identity_path_in(home, None),
        Path::new("/home/u/.harmony/identity.key")
    );
    assert_eq!(
        identity_path_in(home, Some("coord")),
        Path::new("/home/u/.harmony/profiles/coord/identity.key")
    );
}
```

- [ ] **Step 4: Make `app_tracing.rs` profile-aware.** Delete the `APP_IDENTIFIER` const (lines 16-20) and replace `log_dir_in`/`log_dir` (lines 22-35) with:

```rust
/// Pure path join — the profile-aware app-data dir + `/logs`. Split out
/// from `log_dir` so it can be unit-tested deterministically without
/// depending on the host's data dir. ZEB-446: delegates to the same
/// `app_data_dir_in` join `resolve_app_data_dir` uses, so logs always
/// live inside the active profile's app-data tree.
fn log_dir_in(base: &Path, profile: Option<&str>) -> PathBuf {
    crate::app_data_dir_in(base, profile).join("logs")
}

/// Directory the rolling log files live in:
/// `dirs::data_dir()/net.zeblith.harmony[/profiles/<p>]/logs`, byte-identical
/// to Tauri v2's `app_data_dir()/logs` on the default profile. `None` when
/// the platform data dir can't be resolved.
fn log_dir() -> Option<PathBuf> {
    Some(log_dir_in(
        &dirs::data_dir()?,
        crate::profile::active_profile(),
    ))
}
```

- [ ] **Step 5: Update app_tracing's existing unit test** (`log_dir_in_is_base_then_identifier_then_logs`, lines 115-124) to:

```rust
#[test]
fn log_dir_in_is_base_then_identifier_then_logs() {
    // Deterministic: no dependency on the host data dir. Pins the structure
    // `<base>/net.zeblith.harmony[/profiles/<p>]/logs`.
    let base = Path::new("base");
    assert_eq!(
        log_dir_in(base, None),
        base.join("net.zeblith.harmony").join("logs")
    );
    assert_eq!(
        log_dir_in(base, Some("coord")),
        base.join("net.zeblith.harmony")
            .join("profiles")
            .join("coord")
            .join("logs")
    );
}
```

- [ ] **Step 6: Straggler audit.** Run:

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
grep -rn "net\.zeblith\.harmony" src/ | grep -v "app_data_dir_in\|tauri.conf\|//"
grep -rn "\.harmony\b" src/ | grep -v "identity_path_in\|net\.zeblith\|//\|\.harmony_"
grep -rn "dirs::data_dir" src/
```

Expected survivors: the two pure joins (`app_data_dir_in`, `identity_path_in`), `resolve_app_data_dir`/`log_dir` call sites, and doc comments. Any OTHER code path that builds a storage path from `dirs::data_dir()` or a literal `.harmony` is an isolation leak: route it through `resolve_app_data_dir()` / `identity::resolve_path()` and note it in your report. (Known-clean per the 2026-06-12 survey: follows, vine_feed, mail, dm_inbox, dm_outhold, fleet_net, notes, mint, connectivity-settings, content-index, api — all flow through `resolve_app_data_dir`.)

- [ ] **Step 7: Commit Tasks 1+2, then gate.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/profile.rs src-tauri/src/lib.rs src-tauri/src/identity.rs src-tauri/src/app_tracing.rs
git commit -m "feat(zeb-446): named-profile module + profile-aware path chokepoints"
cd src-tauri
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

Expected: clippy clean; new tests `validate_accepts_sane_names_rejects_garbage`, `desired_profile_flag_wins_env_fallback_empty_env_ignored`, `app_data_dir_in_maps_default_and_named`, `identity_path_in_maps_default_and_named_profiles`, updated `log_dir_in_is_base_then_identifier_then_logs` all pass; zero regressions. Amend the commit if fmt/fixes changed anything.

---

### Task 3: Vault gate, passphrase fail-fast, and the `--profile` CLI flag

**Files:**
- Modify: `src-tauri/src/identity.rs` (KeychainStore::new at ~1791; new `passphrase_env_configured` helper near `resolve_path`)
- Modify: `src-tauri/src/lib.rs` (`run()` at 41690; `serve_cli` at 15413)
- Modify: `src-tauri/src/main.rs` (Cli struct + activation)

- [ ] **Step 1: Add the named-profile refusal to `KeychainStore::new()`.** Insert AFTER the `HARMONY_DISABLE_KEYCHAIN` check (identity.rs:1792-1794) and BEFORE the `#[cfg(any(test, feature = "test-fixtures"))]` block — this ordering lets test builds observe the ZEB-446 error:

```rust
        // ZEB-446: named profiles never touch the OS keychain — the
        // service/account names below are machine-global, so two profiles
        // on one machine would read/clobber EACH OTHER'S vault (the
        // ZEB-428 class, in production). Named profiles use the
        // encrypted-file vault under their own identity dir instead.
        if let Some(p) = crate::profile::active_profile() {
            return Err(format!(
                "OS keychain refused for named profile {p:?} (ZEB-446): keychain names are \
                 machine-global; this profile uses the encrypted-file vault — set \
                 HARMONY_PASSPHRASE or HARMONY_PASSPHRASE_FILE"
            ));
        }
```

Also extend the constructor's doc comment ("Two gates close that class" → "Three gates"), adding: `/// 3. A named profile (ZEB-446) → Err in every build: keychain names are machine-global, so named profiles are file-vault-only.`

- [ ] **Step 2: Add `passphrase_env_configured` to identity.rs**, placed right after `resolve_path`/`identity_path_in`. First check whether an equivalent helper already exists near the passphrase reader at identity.rs:1984-1990 (`HARMONY_PASSPHRASE` precedence logic) — if one does, make it `pub` and reuse it instead of duplicating:

```rust
/// ZEB-446: true when the encrypted-file vault has a passphrase source.
/// Named profiles are file-vault-only, so entrypoints fail fast on this
/// instead of letting the first vault access fail later (the ZEB-450
/// silent-degradation class).
pub fn passphrase_env_configured() -> bool {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.trim().is_empty());
    set("HARMONY_PASSPHRASE") || set("HARMONY_PASSPHRASE_FILE")
}
```

- [ ] **Step 3: Unit test the gate** in identity.rs's tests mod (nextest is process-per-test, so setting the OnceLock here cannot leak):

```rust
#[test]
fn keychain_constructor_refuses_on_named_profile() {
    crate::profile::set_active_profile(Some("gatetest")).expect("activate");
    let err = KeychainStore::new().expect_err("named profile must refuse the OS keychain");
    assert!(
        err.contains("ZEB-446"),
        "named-profile refusal must cite ZEB-446 (got the test-build gate instead?): {err}"
    );
}
```

- [ ] **Step 4: `main.rs` — global `--profile` flag + eager activation.** Add the field to the `Cli` struct (after line 8's attribute, before `command`):

```rust
    /// Named storage profile (ZEB-446): scopes ~/.harmony and the app-data
    /// dir to profiles/<NAME> so a second instance (e.g. the pinned headless
    /// coordination node) can run beside the default GUI. Falls back to
    /// HARMONY_PROFILE; omit for the default profile.
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,
```

Then in `main()`, make profile activation the FIRST action of the `Ok(cli)` arm (before the `match cli.command` — every subcommand's tracing init and path resolution depends on it):

```rust
        Ok(cli) => {
            // ZEB-446: activate the storage profile BEFORE tracing init or
            // any path resolution (log dirs and both storage roots depend
            // on it). Invalid names are a hard startup error — silently
            // landing in the default profile's data is the worst outcome.
            if let Err(e) = harmony_app::profile::set_active_profile(cli.profile.as_deref()) {
                eprintln!("harmony-app: {e}");
                std::process::exit(2);
            }
            match cli.command {
                // ... existing arms unchanged ...
            }
        }
```

(The `Err` fall-through arm stays unchanged — `run()` activates from env itself, Step 5.)

- [ ] **Step 5: `lib.rs run()` — eager env activation + passphrase guard.** At the very top of `pub fn run()` (lib.rs:41690), BEFORE `app_tracing::init_app_tracing()` (the log dir is profile-aware):

```rust
    // ZEB-446: GUI launches honor HARMONY_PROFILE (a --profile flag arrives
    // via main.rs, which already activated — this call is then a no-op).
    // Validate eagerly and exit loudly; and refuse a named profile with no
    // vault passphrase, because named profiles are file-vault-only and
    // booting on would hit the ZEB-450 silent-degradation class at the
    // first vault access. Named-profile GUI launches are terminal-driven
    // by definition, so stderr + exit is the honest failure surface.
    if let Err(e) = crate::profile::set_active_profile(None) {
        eprintln!("harmony-app: {e}");
        std::process::exit(2);
    }
    if crate::profile::active_profile().is_some() && !crate::identity::passphrase_env_configured()
    {
        eprintln!(
            "harmony-app: named profile requires HARMONY_PASSPHRASE or \
             HARMONY_PASSPHRASE_FILE (named profiles use the encrypted-file vault, \
             not the OS keychain)"
        );
        std::process::exit(2);
    }
```

- [ ] **Step 6: `serve_cli` passphrase guard.** In lib.rs `serve_cli` (15413), immediately after `crate::app_tracing::init_serve_tracing();`:

```rust
    // ZEB-446: fail fast on a named profile with no vault passphrase —
    // named profiles are file-vault-only (OS keychain refused), so the
    // node would otherwise boot into ZEB-450-style silent degradation at
    // the first vault access.
    if crate::profile::active_profile().is_some() && !crate::identity::passphrase_env_configured()
    {
        eprintln!(
            "serve: named profile requires HARMONY_PASSPHRASE or HARMONY_PASSPHRASE_FILE \
             (named profiles use the encrypted-file vault, not the OS keychain)"
        );
        return 1;
    }
```

- [ ] **Step 7: Commit, then gate (with `--bins` — main.rs changed).**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/identity.rs src-tauri/src/lib.rs src-tauri/src/main.rs
git commit -m "feat(zeb-446): file-vault-only named profiles + --profile flag + fail-fast guards"
cd src-tauri
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --bins --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

Expected: `keychain_constructor_refuses_on_named_profile` passes; `tests/keychain_isolation.rs` is untouched (verify with `git status`); zero regressions.

---

### Task 4: Degradable Reticulum bind + `HARMONY_RETICULUM_PORT`

**Files:**
- Modify: `src-tauri/src/event_loop.rs` (const at :22 stays; parse helper added near it; bind block at :844-863 replaced; unit tests appended to event_loop's tests mod — search `#[cfg(test)]` in the file; if event_loop.rs has no tests mod, add one at the end of the file)

- [ ] **Step 1: Add the parse helper** directly below `const RETICULUM_UDP_PORT: u16 = 4242;` (event_loop.rs:22):

```rust
/// ZEB-446: HARMONY_RETICULUM_PORT parse. Unset/blank → default 4242;
/// `0` → `None` = Reticulum LAN discovery disabled this session; garbage →
/// warn + the default. Reticulum is default-ON, so a bad override must not
/// silently change behavior (contrast `api/gui_host.rs::parse_api_port`,
/// where the feature is opt-in and disabling loudly is the right failure
/// mode).
pub(crate) fn parse_reticulum_port(raw: Option<&str>) -> Option<u16> {
    let raw = raw.map(str::trim).filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return Some(RETICULUM_UDP_PORT);
    };
    match raw.parse::<u16>() {
        Ok(0) => None,
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(
                value = raw,
                error = %e,
                "HARMONY_RETICULUM_PORT invalid; using default 4242"
            );
            Some(RETICULUM_UDP_PORT)
        }
    }
}
```

- [ ] **Step 2: Replace the bind block** (event_loop.rs:844-863 — from `let udp = match cancellable!(` through `tracing::info!(port = RETICULUM_UDP_PORT, "UDP socket bound");`) with:

```rust
    // ZEB-446: the Reticulum LAN-discovery bind is degradable. The port
    // comes from HARMONY_RETICULUM_PORT (unset → 4242; 0 → disabled;
    // garbage → warn + 4242), and a failed bind — typically another local
    // instance holding the port — must NOT kill transport init: zenoh,
    // iroh, and pkarr carry on, and two local instances still interconnect
    // via zenoh scouting. This also retires the ZEB-420/ZEB-165 class of
    // integration-test failures racing on the fixed port (tests set 0).
    //
    // The socket stays non-Option at its ~20 downstream call sites: a
    // disabled/degraded session gets a loopback-bound ephemeral "dead"
    // socket that receives nothing (nobody knows its port) and whose
    // 255.255.255.255 broadcasts fail or route nowhere (loopback-bound) —
    // already swallowed by the `let _ = udp.send_to(...)` announce sites.
    let reticulum_port =
        parse_reticulum_port(std::env::var("HARMONY_RETICULUM_PORT").ok().as_deref());
    let live_udp = match reticulum_port {
        Some(port) => {
            match cancellable!(UdpSocket::bind(format!("0.0.0.0:{port}")), "UDP bind") {
                Ok(s) => match s.set_broadcast(true) {
                    Ok(()) => {
                        tracing::info!(port, "UDP socket bound");
                        Some(s)
                    }
                    Err(e) => {
                        tracing::warn!(
                            port,
                            error = %e,
                            "UDP set_broadcast failed; Reticulum LAN discovery disabled this session"
                        );
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        port,
                        error = %e,
                        "UDP bind failed (another local instance holding the port?); \
                         Reticulum LAN discovery disabled this session"
                    );
                    None
                }
            }
        }
        None => {
            tracing::info!(
                "HARMONY_RETICULUM_PORT=0 — Reticulum LAN discovery disabled this session"
            );
            None
        }
    };
    let udp = match live_udp {
        Some(s) => s,
        None => match cancellable!(UdpSocket::bind("127.0.0.1:0"), "fallback UDP bind") {
            Ok(s) => s,
            Err(e) => {
                // Even a loopback-ephemeral bind failed: that is a genuine
                // environment failure, not a port collision — keep it fatal.
                let e = format!("fallback UDP bind failed: {e}");
                let _ = ready_tx.send(Err(e));
                return;
            }
        },
    };
    let broadcast_addr: SocketAddr = format!(
        "255.255.255.255:{}",
        reticulum_port.unwrap_or(RETICULUM_UDP_PORT)
    )
    .parse()
    .expect("static broadcast addr");
```

Note the original `set_broadcast` failure was fatal; it now degrades like a bind failure (uniform policy). The original `tracing::info!(port = RETICULUM_UDP_PORT, ...)` line is subsumed.

- [ ] **Step 3: Unit-test the parse helper** in event_loop.rs's tests mod:

```rust
#[test]
fn parse_reticulum_port_matrix() {
    assert_eq!(super::parse_reticulum_port(None), Some(4242), "unset → default");
    assert_eq!(super::parse_reticulum_port(Some("")), Some(4242), "blank → default");
    assert_eq!(super::parse_reticulum_port(Some("  ")), Some(4242));
    assert_eq!(super::parse_reticulum_port(Some("0")), None, "0 → disabled");
    assert_eq!(super::parse_reticulum_port(Some("4343")), Some(4343));
    assert_eq!(super::parse_reticulum_port(Some(" 4343 ")), Some(4343));
    assert_eq!(
        super::parse_reticulum_port(Some("notaport")),
        Some(4242),
        "garbage → warn + default (Reticulum is default-on)"
    );
    assert_eq!(super::parse_reticulum_port(Some("70000")), Some(4242), "u16 overflow → default");
}
```

- [ ] **Step 4: Commit, then gate.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/event_loop.rs
git commit -m "feat(zeb-446): degradable Reticulum bind + HARMONY_RETICULUM_PORT (0=off)"
cd src-tauri
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

Expected: `parse_reticulum_port_matrix` passes; zero regressions.

---

### Task 5: Pairing over the headless API (29 → 35 commands)

**Files:**
- Modify: `src-tauri/src/pairing_commands.rs` (full-file refactor to `*_inner` seams; ~170 lines after)
- Modify: `src-tauri/src/api/rpc.rs` (2 arg structs, 6 `rpc!` lines, test updates)

- [ ] **Step 1: Refactor `pairing_commands.rs` to `*_inner` seams.** Each Tauri command body moves into a `pub(crate) async fn *_inner(state: &Mutex<NodeState>, ...)`; the `#[tauri::command]` wrapper delegates (`&state` deref-coerces `State<'_, Mutex<NodeState>>` → `&Mutex<NodeState>`). `require_pairing_handle` changes its parameter to `&Mutex<NodeState>` (its body is unchanged — `state.lock()` works identically). Complete new file body for the changed parts:

```rust
#[tauri::command]
pub async fn start_inviter_pairing(
    display_name: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    start_inviter_pairing_inner(&state, display_name).await
}

/// ZEB-446: seam shared by the Tauri wrapper and the RPC registry, so the
/// headless coordination instance's owner-side GUI (or its API) can drive
/// enrollment. Loads owner_state + master_seed from the persisted ZEB-170
/// artifacts; on a named profile the keychain constructor refuses and the
/// encrypted-file vault serves the load (ZEB-446 vault routing).
pub(crate) async fn start_inviter_pairing_inner(
    state: &Mutex<NodeState>,
    display_name: String,
) -> Result<(), String> {
    let identity_dir = crate::owner_commands::resolve_identity_dir()?;
    let loaded = load_owner_state(&identity_dir, KeychainStore::new().ok())?
        .ok_or_else(|| "no owner identity on this device".to_string())?;
    let master_seed = loaded
        .master_seed
        .ok_or_else(|| "master seed not on this device — cannot enroll".to_string())?;

    let (cmd_tx, _state_rx) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::StartInviter {
            display_name,
            owner_state: loaded.state,
            master_seed,
        })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn start_joiner_pairing(
    display_name: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    start_joiner_pairing_inner(&state, display_name).await
}

pub(crate) async fn start_joiner_pairing_inner(
    state: &Mutex<NodeState>,
    display_name: String,
) -> Result<(), String> {
    let signing_key = SigningKey::generate(&mut OsRng);
    let (cmd_tx, _state_rx) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::StartJoiner {
            display_name,
            signing_key,
        })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn select_pairing_peer(
    peer_session_id: String,
    state: State<'_, Mutex<NodeState>>,
) -> Result<(), String> {
    select_pairing_peer_inner(&state, peer_session_id).await
}

pub(crate) async fn select_pairing_peer_inner(
    state: &Mutex<NodeState>,
    peer_session_id: String,
) -> Result<(), String> {
    let id = Uuid::parse_str(&peer_session_id).map_err(|e| format!("invalid uuid: {e}"))?;
    let (cmd_tx, _) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::SelectPeer {
            peer_session_id: id,
        })
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn confirm_pairing_sas(state: State<'_, Mutex<NodeState>>) -> Result<(), String> {
    confirm_pairing_sas_inner(&state).await
}

pub(crate) async fn confirm_pairing_sas_inner(state: &Mutex<NodeState>) -> Result<(), String> {
    let (cmd_tx, _) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::ConfirmSas)
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn cancel_pairing(state: State<'_, Mutex<NodeState>>) -> Result<(), String> {
    cancel_pairing_inner(&state).await
}

pub(crate) async fn cancel_pairing_inner(state: &Mutex<NodeState>) -> Result<(), String> {
    let (cmd_tx, _) = require_pairing_handle(state)?;
    cmd_tx
        .send(PairingCommand::Cancel)
        .await
        .map_err(|_| "pairing state machine not running".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn get_pairing_state(state: State<'_, Mutex<NodeState>>) -> Result<PairingState, String> {
    get_pairing_state_inner(&state).await
}

/// async for uniformity with the other seams (the rpc! macro awaits every
/// call); the body is synchronous.
pub(crate) async fn get_pairing_state_inner(
    state: &Mutex<NodeState>,
) -> Result<PairingState, String> {
    let (_cmd_tx, state_rx) = require_pairing_handle(state)?;
    Ok(state_rx.borrow().clone())
}

fn require_pairing_handle(
    state: &Mutex<NodeState>,
) -> Result<
    (
        tokio::sync::mpsc::Sender<PairingCommand>,
        tokio::sync::watch::Receiver<PairingState>,
    ),
    String,
> {
    let guard = state.lock().map_err(|e| format!("lock: {e}"))?;
    let h = guard
        .pairing_handle
        .as_ref()
        .ok_or_else(|| "pairing not initialized — start node first".to_string())?;
    Ok((h.cmd_tx.clone(), h.state_rx.clone()))
}
```

(Note: `get_pairing_state`'s original direct-read body is replaced by the handle-clone path — same observable behavior, one error string, one lock.)

- [ ] **Step 2: Register the 6 commands in `api/rpc.rs`.** Add two arg structs after `ReadDmThreadArgs` (line ~193):

```rust
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisplayNameArgs {
    display_name: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerSessionIdArgs {
    peer_session_id: String,
}
```

Add the registrations in `build_registry()` after the "Network health" block (line ~399):

```rust
    // Pairing (ZEB-446): enroll a second local instance — e.g. the pinned
    // headless coordination node — into the owner's fleet without a GUI.
    // SAS verification flows through get_pairing_state polling on both
    // sides; the joiner side runs on the headless instance.
    rpc!(
        m,
        "start_inviter_pairing",
        DisplayNameArgs,
        |state, _sink, a| async move {
            crate::pairing_commands::start_inviter_pairing_inner(state, a.display_name).await
        }
    );
    rpc!(
        m,
        "start_joiner_pairing",
        DisplayNameArgs,
        |state, _sink, a| async move {
            crate::pairing_commands::start_joiner_pairing_inner(state, a.display_name).await
        }
    );
    rpc!(
        m,
        "select_pairing_peer",
        PeerSessionIdArgs,
        |state, _sink, a| async move {
            crate::pairing_commands::select_pairing_peer_inner(state, a.peer_session_id).await
        }
    );
    rpc!(
        m,
        "confirm_pairing_sas",
        EmptyArgs,
        |state, _sink, _a| async move {
            crate::pairing_commands::confirm_pairing_sas_inner(state).await
        }
    );
    rpc!(m, "cancel_pairing", EmptyArgs, |state, _sink, _a| {
        async move { crate::pairing_commands::cancel_pairing_inner(state).await }
    });
    rpc!(
        m,
        "get_pairing_state",
        EmptyArgs,
        |state, _sink, _a| async move {
            crate::pairing_commands::get_pairing_state_inner(state).await
        }
    );
```

Update `build_registry()`'s doc comment: "curated v1 surface (29 commands)" → "(35 commands)".

- [ ] **Step 3: Update the surface-pinning test** in rpc.rs (`registry_has_exactly_the_curated_v1_surface`): change the count assertion to `35`, extend the comment's category list with `+ pairing (start_inviter_pairing, start_joiner_pairing, select_pairing_peer, confirm_pairing_sas, cancel_pairing, get_pairing_state) = 35`, and add `"start_joiner_pairing"` and `"get_pairing_state"` to the must-contain array.

- [ ] **Step 4: Add the pairing dispatch test** to rpc.rs's tests mod:

```rust
#[tokio::test]
async fn pairing_commands_dispatch_with_ipc_parity_pre_node() {
    let reg = build_registry();
    // Pre-node, every pairing command must fail with the SAME error string
    // the Tauri IPC layer produces — proving the seam is shared, not forked.
    let err = reg
        .dispatch(
            "get_pairing_state",
            test_state(),
            test_sink(),
            serde_json::Value::Null,
        )
        .await
        .unwrap_err();
    match err {
        RpcError::Command(msg) => {
            assert_eq!(msg, "pairing not initialized — start node first")
        }
        other => panic!("expected Command, got {other:?}"),
    }
    // Args parse: camelCase displayName reaches the seam (the seam then
    // fails pre-node, which is fine — BadArgs would mean parsing broke).
    let err = reg
        .dispatch(
            "start_joiner_pairing",
            test_state(),
            test_sink(),
            serde_json::json!({ "displayName": "coord-device" }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RpcError::Command(_)),
        "displayName must parse (got {err:?})"
    );
}
```

- [ ] **Step 5: Commit, then gate.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/src/pairing_commands.rs src-tauri/src/api/rpc.rs
git commit -m "feat(zeb-446): pairing commands over the headless RPC surface (29->35)"
cd src-tauri
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --lib --features test-fixtures
```

Expected: updated surface test passes at 35; both new rpc tests pass; zero regressions.

---

### Task 6: Integration test — `tests/profile_isolation.rs`

**Files:**
- Create: `src-tauri/tests/profile_isolation.rs`

Model: `tests/api_server.rs` (temp-HOME harness, `common::set_env` guards, ZEB-347 iroh warm-up, serve-core boot construction). Teardown: mirror api_server.rs's node teardown exactly (check its tail; if it relies on process exit, do the same).

- [ ] **Step 1: Write the test file:**

```rust
//! tests/profile_isolation.rs — ZEB-446: named-profile storage isolation +
//! degradable Reticulum bind, proven on real headless node boots.
//!
//! Temp-HOME, keychain-hermetic (ZEB-428: `KeychainStore::new()` refuses in
//! test-fixtures builds AND on named profiles; HARMONY_PASSPHRASE routes
//! identity persistence to the encrypted-file store inside the tempdir).
//!
//! Two test fns; nextest gives each its own process, so the profile
//! OnceLock and env guards cannot leak between them.

mod common;

use std::sync::{Arc, Mutex};

/// A named profile scopes BOTH storage roots, and the node boots with
/// Reticulum disabled (HARMONY_RETICULUM_PORT=0) — the coordination-
/// instance configuration from the ZEB-446 recipe.
#[tokio::test(flavor = "multi_thread")]
async fn named_profile_scopes_both_roots_and_boots() {
    let home = tempfile::tempdir().expect("tempdir for HOME override");
    let home_str = home
        .path()
        .to_str()
        .expect("tempdir path is valid utf8")
        .to_string();
    let _g1 = common::set_env("HOME", &home_str);
    let _g2 = common::set_env("USERPROFILE", &home_str);
    let _g3 = common::set_env("HARMONY_PASSPHRASE", "profile-isolation-pp");
    let _g4 = common::set_env("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
    let _g5 = common::set_env("APPDATA", &format!("{home_str}/appdata"));
    let _g6 = common::set_env("HARMONY_RETICULUM_PORT", "0");

    harmony_app::profile::set_active_profile(Some("coordtest")).expect("activate profile");
    assert_eq!(harmony_app::profile::active_profile(), Some("coordtest"));

    // Read-only path checks BEFORE any boot writes anything.
    let data_dir = harmony_app::resolve_app_data_dir().expect("resolve app data dir");
    assert!(
        data_dir.starts_with(home.path()),
        "HOME override must scope the app data dir to the tempdir; got {}",
        data_dir.display()
    );
    assert!(
        data_dir.ends_with("net.zeblith.harmony/profiles/coordtest"),
        "named profile must nest under profiles/<name>; got {}",
        data_dir.display()
    );
    let identity_path = harmony_app::identity::resolve_path(None).expect("identity path");
    assert!(
        identity_path.ends_with(".harmony/profiles/coordtest/identity.key"),
        "named profile must scope the identity tree; got {}",
        identity_path.display()
    );

    // ZEB-347: the first iroh bind per process pays a one-time global init
    // (~10s CI / ~30s macOS); warm up before the boot below.
    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    // Boot the serve core exactly as serve_cli does.
    let state = Arc::new(Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = Arc::new(events.clone());
    harmony_app::start_node_inner(None, sink.clone(), None, &state)
        .await
        .expect("node must boot on a named profile with Reticulum disabled");

    // The boot persisted ONLY under the profile trees.
    assert!(
        home.path()
            .join(".harmony")
            .join("profiles")
            .join("coordtest")
            .is_dir(),
        "profile identity tree must exist after boot"
    );
    assert!(
        !home.path().join(".harmony").join("identity.key").exists(),
        "default-profile identity must NOT be touched by a named-profile boot"
    );
}

/// An occupied Reticulum port degrades (warn + boot continues) instead of
/// killing transport init — the ZEB-420/ZEB-165 class, and the exact
/// collision a second local instance produces.
#[tokio::test(flavor = "multi_thread")]
async fn occupied_reticulum_port_degrades_instead_of_failing_boot() {
    let home = tempfile::tempdir().expect("tempdir for HOME override");
    let home_str = home
        .path()
        .to_str()
        .expect("tempdir path is valid utf8")
        .to_string();
    let _g1 = common::set_env("HOME", &home_str);
    let _g2 = common::set_env("USERPROFILE", &home_str);
    let _g3 = common::set_env("HARMONY_PASSPHRASE", "profile-isolation-pp");
    let _g4 = common::set_env("XDG_DATA_HOME", &format!("{home_str}/xdg-data"));
    let _g5 = common::set_env("APPDATA", &format!("{home_str}/appdata"));

    // Deterministic collision without touching the real 4242: pre-bind an
    // ephemeral UDP port and point the node at it.
    let blocker = std::net::UdpSocket::bind("0.0.0.0:0").expect("blocker socket");
    let port = blocker.local_addr().expect("local addr").port();
    let _g6 = common::set_env("HARMONY_RETICULUM_PORT", &port.to_string());

    harmony_app::iroh_endpoint::warm_up_iroh_global_init().await;

    let state = Arc::new(Mutex::new(harmony_app::NodeState::default()));
    let events = harmony_app::api::events::ApiEventSink::new();
    let sink: Arc<dyn harmony_app::node_event_sink::NodeEventSink> = Arc::new(events.clone());
    harmony_app::start_node_inner(None, sink.clone(), None, &state)
        .await
        .expect("occupied Reticulum port must degrade, not fail the boot");
    drop(blocker);
}
```

If `harmony_app::identity::resolve_path` is not visible from integration tests (check: `pub fn` in a `pub mod identity`?), assert via the on-disk effect only (the `.harmony/profiles/coordtest` dir + absent default `identity.key` already pin the same property) and drop the pre-boot `identity_path` block.

- [ ] **Step 2: Commit, then gate (integration test included).**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add src-tauri/tests/profile_isolation.rs
git commit -m "test(zeb-446): profile-isolation + degradable-reticulum integration proof"
cd src-tauri
cargo fmt --all
cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked -p harmony-app --features test-fixtures --test profile_isolation
```

Expected: both tests pass (each boots a real node — allow a few minutes; the iroh warm-up note in the file explains the first-bind cost). If a boot fails, the error string tells you which subsystem — do NOT loosen assertions to pass.

---

### Task 7: Docs, spec amendment, final sweep

**Files:**
- Modify: `docs/headless-install.md` (new section + command-surface list)
- Modify: `docs/troubleshooting.md` (pointers)
- Modify: `docs/specs/2026-06-12-zeb-446-instance-isolation-design.md` (one amendment)

- [ ] **Step 1: `docs/headless-install.md`** — in the "API control surface" command list, add the six pairing commands with one-line descriptions (match the list's existing format). Then add a new section after the existing serve/CLI content:

```markdown
## Side-by-side coordination instance (named profiles)

Run a pinned release build as an always-on headless "coordination" device
beside your dev instance — same machine, separate enrolled device, zero
shared state.

1. **Pin a binary** (copy it OUT of the build tree so dev rebuilds can't
   touch it):

   ```bash
   cp target/release/harmony-app ~/harmony-pinned/harmony-app
   ```

2. **Create a vault passphrase file** (named profiles never touch the OS
   keychain — they use the encrypted-file vault):

   ```bash
   umask 077
   echo 'a-long-random-passphrase' > ~/.harmony-coord-pass
   ```

3. **Start the coordination instance** on its own profile and API port:

   ```bash
   HARMONY_PASSPHRASE_FILE=~/.harmony-coord-pass \
     ~/harmony-pinned/harmony-app serve --profile coord --api-port 7421
   ```

   Storage lands under `~/.harmony/profiles/coord/` and
   `<data-dir>/net.zeblith.harmony/profiles/coord/`. If the dev instance
   holds UDP 4242, the coordination instance logs a warning and runs
   without Reticulum LAN discovery (zenoh/iroh/pkarr are unaffected); set
   `HARMONY_RETICULUM_PORT=0` to silence the bind attempt entirely.

4. **Verify it's up** (pre-enrollment, the owner state is `null`):

   ```bash
   ~/harmony-pinned/harmony-app api --profile coord get_owner_state
   ```

5. **Enroll it as a device in your fleet** (the inviter side runs on your
   default-profile GUI — or its API when launched with `HARMONY_API_PORT`):

   ```bash
   # coordination instance (joiner):
   ~/harmony-pinned/harmony-app api --profile coord start_joiner_pairing \
     '{"displayName":"coord"}'
   # dev GUI: start inviter pairing from the device-pairing UI (or
   # `api start_inviter_pairing '{"displayName":"main"}'`), then on both
   # sides select the peer and compare the short auth string:
   ~/harmony-pinned/harmony-app api --profile coord get_pairing_state
   ~/harmony-pinned/harmony-app api --profile coord confirm_pairing_sas
   ```

6. **Verify** both devices appear in your fleet, then kill / rebuild /
   relaunch the dev instance freely — the coordination instance is
   unaffected.

A single GUI can also run on a named profile (onboarding tests against a
scratch profile): launch with `HARMONY_PROFILE=<name>` plus a passphrase
env. Two *simultaneous* GUIs remain unsupported (the single-instance
plugin is identifier-global), and `harmony://` deep links always reach the
default-profile GUI.
```

- [ ] **Step 2: `docs/troubleshooting.md`** — add bullets (match the file's existing format):
  - "**named profile requires HARMONY_PASSPHRASE**" at startup → named profiles are file-vault-only; set `HARMONY_PASSPHRASE` or `HARMONY_PASSPHRASE_FILE`.
  - "**invalid profile name**" → profile names match `[a-z0-9][a-z0-9_-]{0,31}`; `default` is reserved (omit the flag).
  - "**UDP bind failed … Reticulum LAN discovery disabled**" is a warning, not an error — another local instance holds the port; the node still networks via zenoh/iroh. Set `HARMONY_RETICULUM_PORT` to rebind or `0` to disable.

- [ ] **Step 3: Spec amendment** — in `docs/specs/2026-06-12-zeb-446-instance-isolation-design.md`, the §2 "Vault" fail-fast sentence and the error-handling table row currently say a GUI launch "surfaces the error and refuses to start the node". Implementation settles on exit-at-startup (named-profile GUI launches are terminal-driven by definition). Update both spots to: "serve and GUI launches alike exit non-zero at startup with the error on stderr".

- [ ] **Step 4: Commit docs.**

```bash
cd /Users/zeblith/work/zeblithic/harmony-client
git add docs/headless-install.md docs/troubleshooting.md docs/specs/2026-06-12-zeb-446-instance-isolation-design.md
git commit -m "docs(zeb-446): side-by-side coordination-instance recipe + troubleshooting"
```

- [ ] **Step 5: FINAL SWEEP** (the only `--all-targets` run; budget ~25-50 min wall-clock — use a 10-min timeout per command and report partial progress rather than stalling silently; if a command exceeds it, re-run with a longer timeout up to 60 min for nextest):

```bash
cd /Users/zeblith/work/zeblithic/harmony-client/src-tauri
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings
cargo nextest run --locked --workspace --all-targets --features test-fixtures
```

Expected: fmt clean; clippy clean; full suite green (the api_server zid flake is FIXED on this base — a nextest failure is real until proven otherwise). Report any failure verbatim; unrelated breakage gets reported, never fixed in this branch.

---

## Plan self-review notes (already applied)

- Spec §1-§5 each map to Tasks 1-7 (profile→1-3, vault→3, reticulum→4, pairing→5, recipe/docs→7, tests→2-6).
- Tasks 1+2 are one compile unit (cross-referenced test); flagged inline.
- Type consistency: `set_active_profile(Option<&str>) -> Result<(), String>`, `active_profile() -> Option<&'static str>`, `app_data_dir_in(&Path, Option<&str>) -> PathBuf`, `identity_path_in(&Path, Option<&str>) -> PathBuf`, `parse_reticulum_port(Option<&str>) -> Option<u16>`, `*_inner(state: &Mutex<NodeState>, ...)` — used identically across tasks.
- The `rpc!` macro rebinds `$state` to `__access.node_state()` (`&Mutex<NodeState>`), matching the `*_inner` signatures; call sites pass `state` bare (clippy `needless_borrow` rejects `&state` here — learned in the PR #242 cycle).
