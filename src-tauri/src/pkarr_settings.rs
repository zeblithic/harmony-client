//! Persisted user preferences for Phase 2 pkarr policies.
//!
//! Only case B (opt-in identity-keyed discoverability) needs persistence today.
//! Lives at `<app_data_dir>/connectivity-settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PkarrSettings {
    /// Case B (identity-keyed discoverability) — opt-in, default OFF.
    #[serde(default)]
    pub identity_discoverable: bool,
    /// ZEB-371 Task 12 (spec §7.1): per-user "auto-accept known requesters"
    /// toggle for the Path-A (no-token) friend flow. A KNOWN requester (already
    /// an Active|Pending friend) is accepted inline without prompting when this
    /// is ON; an UNKNOWN requester is NEVER auto-accepted regardless. Jake's
    /// "Both" choice — default ON.
    #[serde(default = "default_friend_auto_accept_known")]
    pub friend_auto_accept_known: bool,
    /// ZEB-380: user-configurable, persisted pkarr relay pool. A serde field
    /// default fills a vetted >=2 set for old settings files (forward-compat),
    /// guaranteeing relay redundancy on upgrade. Applied live via
    /// `set_pkarr_relays` (no restart).
    #[serde(default = "default_relays")]
    pub relays: Vec<String>,
}

/// Default for [`PkarrSettings::friend_auto_accept_known`]: ON (spec §7.1).
fn default_friend_auto_accept_known() -> bool {
    true
}

/// Default for [`PkarrSettings::relays`]: the Zeblithic-operated
/// `pkarr.q8.fyi` (primary) followed by the vetted public fallbacks
/// `relay.pkarr.org` (n0-operated) + `pkarr.pubky.app` (Pubky).
///
/// The self-hosted primary gives the fleet a single deterministic
/// publish/resolve rendezvous, avoiding the cross-relay partition where two
/// peers each publish to a *different* reachable relay and so never resolve
/// each other (ZEB-513). The public relays are retained behind it so one
/// host-level relay hiccup is never terminal for first-contact (ZEB-330,
/// ZEB-380 redundancy).
pub fn default_relays() -> Vec<String> {
    vec![
        "https://pkarr.q8.fyi".to_string(),
        "https://relay.pkarr.org".to_string(),
        "https://pkarr.pubky.app".to_string(),
    ]
}

impl Default for PkarrSettings {
    fn default() -> Self {
        Self {
            identity_discoverable: false,
            friend_auto_accept_known: default_friend_auto_accept_known(),
            relays: default_relays(),
        }
    }
}

/// Maximum number of relays a user may configure.
pub const MAX_RELAYS: usize = 8;

/// Validate + normalize a user-submitted relay list. Rejects an empty list,
/// blank/malformed URLs, non-`https` remote schemes (`http` allowed only for
/// loopback / private hosts — pkarr's local-relay-on-:6881 guidance), URLs
/// carrying a path/query/fragment/userinfo (a relay is a `scheme://host` base —
/// pkarr builds requests as `{base}/{z32_key}`, so a path-bearing base would
/// silently misroute every publish/resolve; userinfo (`user:pass@`) would
/// persist credentials into `connectivity-settings.json`), and more than
/// [`MAX_RELAYS`]. Dedups on the trailing-slash-normalized URL, preserving
/// first-seen order. Returns the normalized list on success.
pub fn validate_relay_urls(input: Vec<String>) -> Result<Vec<String>, String> {
    if input.is_empty() {
        return Err("at least one relay is required".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in input {
        let normalized = validate_single_relay(raw.trim())?;
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    if out.len() > MAX_RELAYS {
        return Err(format!("too many relays (max {MAX_RELAYS})"));
    }
    Ok(out)
}

/// Validate ONE trimmed relay URL, returning its trailing-slash-normalized form
/// or a human-readable error. The shared per-entry rule behind both the strict
/// [`validate_relay_urls`] (rejects the whole list on any failure — correct for
/// user input) and the lenient [`sanitize_relay_urls`] (drops only the bad
/// entries — correct for reading a possibly hand-edited persisted list).
fn validate_single_relay(trimmed: &str) -> Result<String, String> {
    if trimmed.is_empty() {
        return Err("relay URL must not be empty".to_string());
    }
    let parsed = url::Url::parse(trimmed).map_err(|_| format!("invalid relay URL: {trimmed}"))?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let host = parsed.host_str().unwrap_or("");
            if !is_local_host(host) {
                return Err(format!(
                    "http:// is only allowed for localhost relays: {trimmed}"
                ));
            }
        }
        other => return Err(format!("unsupported relay scheme '{other}': {trimmed}")),
    }
    // A relay is a bare `scheme://host[:port]` base; pkarr appends
    // `/{z32_key}`. A path (beyond root `/`), query, fragment, or userinfo
    // (`user:pass@`) on the base would either silently misroute every request
    // or persist credentials into connectivity-settings.json — reject up front.
    let path = parsed.path();
    let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
    if (!path.is_empty() && path != "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || has_userinfo
    {
        return Err(format!(
            "relay URL must be scheme://host only (no path/query/fragment/userinfo): {trimmed}"
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

/// ZEB-380: lenient reader-side sanitizer for a *persisted* relay list. Unlike
/// [`validate_relay_urls`] (which rejects the entire list on any bad entry —
/// correct for surfacing an error to a user typing into Settings), this keeps
/// every valid relay and silently drops only the malformed ones, so a single
/// bad entry in a hand-edited `connectivity-settings.json` can't discard an
/// otherwise-good custom pool. Dedups (trailing-slash-normalized, first wins)
/// and truncates to [`MAX_RELAYS`]. May return an empty vec (input empty or all
/// entries invalid); the caller decides the empty fallback (callers use
/// [`default_relays`]).
pub fn sanitize_relay_urls(input: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in input {
        match validate_single_relay(raw.trim()) {
            Ok(normalized) => {
                if seen.insert(normalized.clone()) {
                    out.push(normalized);
                    if out.len() == MAX_RELAYS {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "ZEB-380: dropping invalid persisted relay URL");
            }
        }
    }
    out
}

/// True for loopback / private / link-local hosts where a plaintext `http://`
/// relay is acceptable (a local pkarr relay on :6881).
///
/// IPv6 coverage: loopback (`::1`), ULA (`fc00::/7`), link-local (`fe80::/10`).
/// The `is_unique_local` / `is_unicast_link_local` methods are unstable, so we
/// use stable bit-mask checks on the first 16-bit segment instead.
///
/// Note: `url::Url::host_str()` returns IPv6 addresses bracketed as `[::1]`
/// per the URL spec; we strip the brackets before parsing.
pub(crate) fn is_local_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // `url::Url::host_str()` wraps IPv6 in `[…]` brackets per URL spec — strip
    // them so `str::parse::<IpAddr>()` can handle the address.
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => {
            let seg0 = v6.segments()[0];
            // loopback ::1, ULA fc00::/7, link-local fe80::/10
            v6.is_loopback() || (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

impl PkarrSettings {
    pub fn load_or_default(path: &PathBuf) -> Self {
        let contents = match std::fs::read_to_string(path) {
            Ok(s) => s,
            // Missing / unreadable file is the normal first-run case — quiet default.
            Err(_) => return Self::default(),
        };
        match serde_json::from_str(&contents) {
            Ok(settings) => settings,
            Err(e) => {
                // Fail CLOSED and LOUD: a corrupt settings file must not silently
                // revert a prior opt-in, but it must NEVER fail open either —
                // silently becoming discoverable would violate a real opt-out, and
                // privacy-fail-open is worse than a freeze. Surface it so the
                // operator can fix the file; fall back to the (not-discoverable)
                // default in the meantime.
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "connectivity-settings.json failed to parse — failing closed to defaults; a prior opt-in (e.g. identity_discoverable) will NOT take effect until the file is fixed"
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &PathBuf) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_to_not_discoverable() {
        let settings = PkarrSettings::default();
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn parse_error_fails_closed_not_open() {
        // A corrupt settings file must fail CLOSED (not discoverable), never
        // open. Privacy-fail-open would silently violate a real opt-out.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("connectivity-settings.json");
        std::fs::write(&path, b"{ this is not valid json").unwrap();
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn defaults_to_auto_accept_known_on() {
        // ZEB-371 spec §7.1: auto-accept KNOWN requesters defaults ON.
        let settings = PkarrSettings::default();
        assert!(settings.friend_auto_accept_known);
    }

    #[test]
    fn missing_auto_accept_field_defaults_on() {
        // An older settings file (pre-ZEB-371) has no `friend_auto_accept_known`
        // key; serde's field default must fill it ON so existing users keep the
        // spec default rather than silently flipping to OFF.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("legacy.json");
        std::fs::write(&path, r#"{"identity_discoverable":true}"#).expect("write");
        let loaded = PkarrSettings::load_or_default(&path);
        assert!(loaded.identity_discoverable);
        assert!(loaded.friend_auto_accept_known);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("nonexistent.json");
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn round_trip_save_then_load() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let settings = PkarrSettings {
            identity_discoverable: true,
            friend_auto_accept_known: false,
            relays: vec!["https://relay.pkarr.org".to_string()],
        };
        settings.save(&path).expect("save");

        let loaded = PkarrSettings::load_or_default(&path);
        assert_eq!(loaded, settings);
    }

    #[test]
    fn defaults_to_recommended_relays() {
        let settings = PkarrSettings::default();
        assert_eq!(settings.relays, default_relays());
        assert!(settings.relays.len() >= 2, "must ship a >=2 relay default");
    }

    #[test]
    fn self_hosted_relay_leads_default_pool() {
        // The Zeblithic-operated relay leads the pool so the fleet shares one
        // deterministic rendezvous (ZEB-513); the public relays stay behind it
        // as redundancy fallbacks (ZEB-330/380).
        let relays = default_relays();
        assert_eq!(
            relays.first().map(String::as_str),
            Some("https://pkarr.q8.fyi"),
            "self-hosted relay must be the primary default"
        );
        assert!(relays.iter().any(|r| r == "https://relay.pkarr.org"));
        assert!(relays.iter().any(|r| r == "https://pkarr.pubky.app"));
    }

    #[test]
    fn missing_relays_field_defaults_on_load() {
        // A pre-ZEB-380 settings file has no `relays` key; serde's field default
        // must fill it with the recommended >=2 set so existing users gain
        // redundancy on upgrade rather than booting with an empty pool.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("legacy.json");
        std::fs::write(&path, r#"{"identity_discoverable":true}"#).expect("write");
        let loaded = PkarrSettings::load_or_default(&path);
        assert_eq!(loaded.relays, default_relays());
    }

    #[test]
    fn round_trips_custom_relays() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let settings = PkarrSettings {
            identity_discoverable: false,
            friend_auto_accept_known: true,
            relays: vec!["https://relay.pkarr.org".to_string()],
        };
        settings.save(&path).expect("save");
        assert_eq!(
            PkarrSettings::load_or_default(&path).relays,
            settings.relays
        );
    }

    #[test]
    fn validate_rejects_empty_list() {
        assert!(validate_relay_urls(vec![]).is_err());
    }

    #[test]
    fn validate_rejects_blank_and_malformed() {
        assert!(validate_relay_urls(vec!["".into()]).is_err());
        assert!(validate_relay_urls(vec!["not a url".into()]).is_err());
        assert!(validate_relay_urls(vec!["ftp://relay.example".into()]).is_err());
    }

    #[test]
    fn validate_rejects_http_for_remote_host() {
        assert!(validate_relay_urls(vec!["http://relay.pkarr.org".into()]).is_err());
    }

    #[test]
    fn validate_allows_http_for_loopback() {
        let ok = validate_relay_urls(vec!["http://127.0.0.1:6881".into()]).expect("loopback ok");
        assert_eq!(ok, vec!["http://127.0.0.1:6881".to_string()]);
        assert!(validate_relay_urls(vec!["http://localhost:6881".into()]).is_ok());
    }

    #[test]
    fn validate_dedups_trailing_slash() {
        let ok = validate_relay_urls(vec![
            "https://relay.pkarr.org".into(),
            "https://relay.pkarr.org/".into(),
        ])
        .expect("dedup");
        assert_eq!(ok, vec!["https://relay.pkarr.org".to_string()]);
    }

    #[test]
    fn validate_caps_at_eight() {
        let many: Vec<String> = (0..9)
            .map(|i| format!("https://r{i}.example.com"))
            .collect();
        assert!(validate_relay_urls(many).is_err());
    }

    #[test]
    fn validate_rejects_path_query_fragment() {
        // A relay base must be scheme://host only — a path/query/fragment would
        // silently misroute pkarr's `{base}/{z32_key}` requests.
        assert!(validate_relay_urls(vec!["https://relay.pkarr.org/foo".into()]).is_err());
        assert!(validate_relay_urls(vec!["https://relay.pkarr.org/?x=1".into()]).is_err());
        assert!(validate_relay_urls(vec!["https://relay.pkarr.org/#frag".into()]).is_err());
        // Userinfo (credentials) must also be rejected — persisting them into
        // connectivity-settings.json would be a credential leak.
        assert!(validate_relay_urls(vec!["https://user:pass@relay.pkarr.org".into()]).is_err());
        assert!(validate_relay_urls(vec!["https://user@relay.pkarr.org".into()]).is_err());
        // A bare host (with or without a single trailing slash) is still accepted
        // and normalized.
        assert_eq!(
            validate_relay_urls(vec!["https://relay.pkarr.org/".into()])
                .expect("trailing slash ok"),
            vec!["https://relay.pkarr.org".to_string()]
        );
    }

    #[test]
    fn validate_allows_http_for_ipv6_local() {
        assert!(validate_relay_urls(vec!["http://[::1]:6881".into()]).is_ok());
        assert!(validate_relay_urls(vec!["http://[fe80::1]:6881".into()]).is_ok());
        // ULA is fc00::/7 — covers BOTH the fc00::/8 and fd00::/8 halves. Real
        // locally-assigned ULAs live in fd00::/8, so assert both; the `0xfe00`
        // mask in is_local_host is a /7 (top 7 bits), not /9.
        assert!(validate_relay_urls(vec!["http://[fc00::1]:6881".into()]).is_ok());
        assert!(validate_relay_urls(vec!["http://[fd12::1]:6881".into()]).is_ok());
        assert!(validate_relay_urls(vec!["http://[fdff:ffff::1]:6881".into()]).is_ok());
        // Just below the ULA range (fbff::/16) must NOT be treated as local.
        assert!(validate_relay_urls(vec!["http://[fbff::1]:6881".into()]).is_err());
        // A global IPv6 over http is still rejected.
        assert!(validate_relay_urls(vec!["http://[2606:4700::1111]:6881".into()]).is_err());
    }

    #[test]
    fn load_corrupted_file_returns_default() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("bad.json");
        std::fs::write(&path, "not json {{").expect("write");
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }

    #[test]
    fn sanitize_keeps_valid_drops_only_invalid() {
        // ZEB-380: one bad entry in a hand-edited list must NOT discard the good
        // relays — unlike strict validate_relay_urls (all-or-nothing).
        let out = sanitize_relay_urls(vec![
            "https://good1.example".into(),
            "not a url".into(), // dropped: unparseable
            "https://good2.example".into(),
            "ftp://nope.example".into(),     // dropped: bad scheme
            "https://good1.example/".into(), // dropped: dedup of good1
            "https://creds:pw@good3.example".into(), // dropped: userinfo
        ]);
        assert_eq!(
            out,
            vec![
                "https://good1.example".to_string(),
                "https://good2.example".to_string(),
            ],
            "keeps every valid relay (deduped), drops only the malformed ones"
        );
    }

    #[test]
    fn sanitize_all_invalid_or_empty_returns_empty() {
        // The caller (effective_pkarr_relays) maps empty → default_relays().
        assert!(sanitize_relay_urls(vec![]).is_empty());
        assert!(
            sanitize_relay_urls(vec!["garbage".into(), "ftp://x".into(), "".into()]).is_empty()
        );
    }

    #[test]
    fn sanitize_truncates_to_max_relays() {
        // More than MAX_RELAYS valid entries: strict validate errors, lenient
        // sanitize keeps the first MAX_RELAYS rather than discarding everything.
        let many: Vec<String> = (0..(MAX_RELAYS + 3))
            .map(|i| format!("https://r{i}.example.com"))
            .collect();
        let out = sanitize_relay_urls(many);
        assert_eq!(out.len(), MAX_RELAYS, "sanitize caps at MAX_RELAYS");
        assert_eq!(out[0], "https://r0.example.com", "keeps the first entries");
    }
}
