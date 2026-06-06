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

/// Default for [`PkarrSettings::relays`]: a vetted, liveness-probed >=2 set
/// (2026-06-05). `relay.pkarr.org` (n0-operated) + `pkarr.pubky.app` (Pubky).
/// Redundancy means one host-level relay hiccup is no longer terminal for
/// first-contact (ZEB-330).
pub fn default_relays() -> Vec<String> {
    vec![
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
/// loopback / private hosts — pkarr's local-relay-on-:6881 guidance), and more
/// than [`MAX_RELAYS`]. Dedups on the trailing-slash-normalized URL, preserving
/// first-seen order. Returns the normalized list on success.
pub fn validate_relay_urls(input: Vec<String>) -> Result<Vec<String>, String> {
    if input.is_empty() {
        return Err("at least one relay is required".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in input {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("relay URL must not be empty".to_string());
        }
        let parsed =
            url::Url::parse(trimmed).map_err(|_| format!("invalid relay URL: {trimmed}"))?;
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
        let normalized = trimmed.trim_end_matches('/').to_string();
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    if out.len() > MAX_RELAYS {
        return Err(format!("too many relays (max {MAX_RELAYS})"));
    }
    Ok(out)
}

/// True for loopback / private / link-local hosts where a plaintext `http://`
/// relay is acceptable (a local pkarr relay on :6881).
fn is_local_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}

impl PkarrSettings {
    pub fn load_or_default(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
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
    fn load_corrupted_file_returns_default() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("bad.json");
        std::fs::write(&path, "not json {{").expect("write");
        let settings = PkarrSettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
    }
}
