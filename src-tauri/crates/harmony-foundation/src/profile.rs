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
    // Already activated → no-op WITHOUT re-reading the env: main() may
    // have activated from a valid --profile while HARMONY_PROFILE holds
    // garbage, and run()'s eager re-call must not abort a launch the flag
    // already validated (PR #245 round 1, Qodo + Cursor).
    if ACTIVE_PROFILE.get().is_some() {
        return Ok(());
    }
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
            desired_profile(None, std::env::var("HARMONY_PROFILE").ok().as_deref()).unwrap_or_else(
                |e| panic!("HARMONY_PROFILE invalid and no entrypoint validated it: {e}"),
            )
        })
        .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_sane_names_rejects_garbage() {
        for ok in [
            "coord",
            "a",
            "x2",
            "test-prof_1",
            "abcdefghijklmnopqrstuvwxyz012345",
        ] {
            assert!(validate_profile_name(ok).is_ok(), "{ok:?} should be valid");
        }
        for bad in [
            "",
            "Coord", // uppercase
            "-lead", // bad start char
            "_lead", // bad start char
            "has space",
            "has/slash",
            "has.dot",
            "..",
            "abcdefghijklmnopqrstuvwxyz0123456", // 33 chars
        ] {
            assert!(
                validate_profile_name(bad).is_err(),
                "{bad:?} should be invalid"
            );
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

    /// Regression for PR #245 round 1 (Qodo + Cursor): once a flag
    /// activated the profile, a later no-op call must NOT re-read (and
    /// fail on) a garbage HARMONY_PROFILE. nextest (the supported runner,
    /// CLAUDE.md) is process-per-test, so mutating the process-global
    /// OnceLock and env here cannot leak into other tests.
    #[test]
    fn activation_is_first_wins_and_never_revalidates_env() {
        std::env::set_var("HARMONY_PROFILE", "BAD NAME");
        set_active_profile(Some("flagprof")).expect("flag activation must win");
        assert_eq!(active_profile(), Some("flagprof"));
        set_active_profile(None).expect("post-activation call must be a no-op, not an env re-read");
        assert_eq!(active_profile(), Some("flagprof"));
        std::env::remove_var("HARMONY_PROFILE");
    }

    // NOTE (ZEB-548 Stage 1): two tests once lived here —
    // `app_data_dir_in_maps_default_and_named` and
    // `resolve_app_data_dir_override_wins_else_platform_else_errors`. They
    // exercised `crate::app_data_dir_in` / `crate::resolve_app_data_dir_from`,
    // which live in `harmony-app` (lib.rs), not in this leaf crate. When
    // `profile` was extracted into harmony-foundation they moved to
    // harmony-app's `mod tests`, still pinning the profile-scoped path layout
    // from the crate that actually owns those functions.
}
