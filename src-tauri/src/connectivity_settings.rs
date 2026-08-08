//! Persisted user preferences for Phase 2 pkarr policies.
//!
//! Only case B (opt-in identity-keyed discoverability) needs persistence today.
//! Lives at `<app_data_dir>/connectivity-settings.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectivitySettings {
    /// Case B (identity-keyed discoverability) — default ON (ZEB-881): fresh
    /// identities are discoverable so first cross-WAN contact works; users opt
    /// OUT to go private. `#[serde(default)]` fills `false` for a legacy file
    /// that predates the field (no silent migration), and `fail_closed_defaults`
    /// keeps it OFF for a corrupt/unreadable file.
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
    #[serde(default = "default_pkarr_relays")]
    pub relays: Vec<String>,
    /// ZEB-624: custom iroh relay URL list. EMPTY = follow the iroh preset's
    /// default relay map (n0 stable). Applied at endpoint build and live via
    /// insert/remove diff. Distinct from `relays` (the pkarr publish/resolve
    /// pool): these steer iroh's transport home-relay selection. Serde default is
    /// empty so an old settings file (no key) keeps the preset defaults.
    #[serde(default)]
    pub iroh_relays: Vec<String>,
    /// ZEB-600: user "appear offline" toggle. When true, the node suppresses
    /// its community-presence beacons (others see it offline; it still receives
    /// their presence). Default OFF (visible) — presence is a product default.
    #[serde(default)]
    pub presence_invisible: bool,
    /// ZEB-376 (Friends Phase 2b): per-user policy for inbound friend-vouched
    /// introductions. Enforced on X's node when it receives an `Introduction`
    /// (never the voucher's). Fresh-install default `FriendsOfFriends`; a
    /// corrupt file fails closed to `Closed` (see `fail_closed_defaults`).
    #[serde(default = "default_peer_intro_policy")]
    pub peer_intro_policy: crate::friend_graph::PeerIntroPolicy,
}

/// Default for [`ConnectivitySettings::friend_auto_accept_known`]: ON (spec §7.1).
fn default_friend_auto_accept_known() -> bool {
    true
}

/// Default for [`ConnectivitySettings::peer_intro_policy`]: `FriendsOfFriends`
/// (arc §4.2 default — accept an introduction only when the voucher is an Active
/// friend).
fn default_peer_intro_policy() -> crate::friend_graph::PeerIntroPolicy {
    crate::friend_graph::PeerIntroPolicy::FriendsOfFriends
}

/// Default for [`ConnectivitySettings::relays`]: the Zeblithic-operated
/// `pkarr.q8.fyi` (primary) followed by the vetted public fallbacks
/// `relay.pkarr.org` (n0-operated) + `pkarr.pubky.app` (Pubky).
///
/// The self-hosted primary gives the fleet a single deterministic
/// publish/resolve rendezvous, avoiding the cross-relay partition where two
/// peers each publish to a *different* reachable relay and so never resolve
/// each other (ZEB-513). The public relays are retained behind it so one
/// host-level relay hiccup is never terminal for first-contact (ZEB-330,
/// ZEB-380 redundancy).
pub fn default_pkarr_relays() -> Vec<String> {
    vec![
        "https://pkarr.q8.fyi".to_string(),
        "https://relay.pkarr.org".to_string(),
        "https://pkarr.pubky.app".to_string(),
    ]
}

impl Default for ConnectivitySettings {
    fn default() -> Self {
        Self {
            // ZEB-881: discoverable by default. A fresh identity's pkarr case-B
            // routing record must publish so first cross-WAN contact works;
            // default-off was a usability cliff, not a real privacy gain. The
            // fail-closed path (`fail_closed_defaults`) stays OFF, and existing
            // persisted files are never migrated (see `load_or_default`).
            identity_discoverable: true,
            friend_auto_accept_known: default_friend_auto_accept_known(),
            relays: default_pkarr_relays(),
            // Empty = follow the iroh preset's default relay map (n0 stable);
            // there is no first-run custom iroh pool.
            iroh_relays: Vec::new(),
            presence_invisible: false,
            peer_intro_policy: default_peer_intro_policy(),
        }
    }
}

/// Maximum number of pkarr relays a user may configure.
pub const MAX_RELAYS: usize = 8;

/// ZEB-624: maximum number of custom iroh relays a user may configure. Mirrors
/// [`MAX_RELAYS`] — a small ceiling keeps the persisted list bounded and the
/// endpoint's relay map sane.
pub const MAX_IROH_RELAYS: usize = 8;

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
    validate_relay_list(input, MAX_RELAYS, validate_single_relay)
}

/// Shared strict list-walk behind [`validate_relay_urls`] (pkarr) and
/// [`validate_iroh_relay_urls`] (iroh): reject an empty list, run each trimmed
/// entry through `validate` (returns the normalized form, or an error that
/// aborts the whole list), dedup on the normalized value (first-seen wins), and
/// reject more than `max`. The per-entry `validate` closure carries the
/// family-specific rule so this validate/dedup/cap walk lives in exactly one
/// place rather than being copy-pasted per relay family.
fn validate_relay_list(
    input: Vec<String>,
    max: usize,
    validate: impl Fn(&str) -> Result<String, String>,
) -> Result<Vec<String>, String> {
    if input.is_empty() {
        return Err("at least one relay is required".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in input {
        let normalized = validate(raw.trim())?;
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    if out.len() > max {
        return Err(format!("too many relays (max {max})"));
    }
    Ok(out)
}

/// ZEB-624: strict validator for a user-submitted custom iroh relay list. Each
/// entry must satisfy the shared base rule ([`validate_single_relay`]: non-empty,
/// `https` for remote hosts / `http` only for loopback, no path/query/fragment/
/// userinfo) AND parse as an [`iroh::RelayUrl`] — a base `url::Url` accepts but
/// iroh's endpoint builder rejects would otherwise fail silently at connect
/// time. Rejects an empty list (use "reset" to fall back to the preset
/// defaults), dedups, and caps at [`MAX_IROH_RELAYS`]. Returns the normalized
/// list on success.
pub fn validate_iroh_relay_urls(input: Vec<String>) -> Result<Vec<String>, String> {
    validate_relay_list(input, MAX_IROH_RELAYS, validate_iroh_single)
}

/// Per-entry iroh relay rule: the shared pkarr base check
/// ([`validate_single_relay`], which returns the trailing-slash-normalized base)
/// plus an [`iroh::RelayUrl`] parse. An iroh relay URL must satisfy BOTH. The
/// normalized base (not the `RelayUrl`'s re-serialized string, which re-adds a
/// trailing slash) is returned so the persisted form stays canonical.
fn validate_iroh_single(trimmed: &str) -> Result<String, String> {
    let normalized = validate_single_relay(trimmed)?;
    normalized
        .parse::<iroh::RelayUrl>()
        .map_err(|e| format!("invalid iroh relay URL '{normalized}': {e}"))?;
    Ok(normalized)
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
/// [`default_pkarr_relays`]).
pub fn sanitize_relay_urls(input: Vec<String>) -> Vec<String> {
    sanitize_relay_list(input, MAX_RELAYS, validate_single_relay)
}

/// Shared lenient list-walk behind [`sanitize_relay_urls`] (pkarr) and
/// [`sanitize_iroh_relay_urls`] (iroh): keep every entry that `validate` accepts
/// (returning its normalized form), silently drop the rest (a single bad
/// hand-edited URL must not discard an otherwise-good pool), dedup (first-seen
/// wins), and stop at `max`. May return empty; the caller decides the fallback.
fn sanitize_relay_list(
    input: Vec<String>,
    max: usize,
    validate: impl Fn(&str) -> Result<String, String>,
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in input {
        match validate(raw.trim()) {
            Ok(normalized) => {
                if seen.insert(normalized.clone()) {
                    out.push(normalized);
                    if out.len() == max {
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "dropping invalid persisted relay URL");
            }
        }
    }
    out
}

/// ZEB-624: lenient reader-side sanitizer for a *persisted* iroh relay list — the
/// iroh mirror of [`sanitize_relay_urls`]. Keeps every entry that passes
/// [`validate_iroh_single`], drops the malformed ones, dedups, and caps at
/// [`MAX_IROH_RELAYS`]. May return empty (input empty or all-invalid); the caller
/// ([`effective_iroh_relays`]) maps empty → `None` = follow the iroh preset
/// defaults.
pub fn sanitize_iroh_relay_urls(input: Vec<String>) -> Vec<String> {
    sanitize_relay_list(input, MAX_IROH_RELAYS, validate_iroh_single)
}

/// ZEB-624: the EFFECTIVE custom iroh relay pool for endpoint construction.
/// Sanitizes the persisted `iroh_relays` (drops malformed entries, dedups, caps)
/// and maps an empty result to `None` = "follow the iroh preset's built-in relay
/// map" (n0 stable), distinct from `Some(list)` = "use exactly these". Task 5
/// consumes this at endpoint build and for the live relay-map diff — keep the
/// name and signature stable.
pub fn effective_iroh_relays(settings: &ConnectivitySettings) -> Option<Vec<String>> {
    let sanitized = sanitize_iroh_relay_urls(settings.iroh_relays.clone());
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
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

impl ConnectivitySettings {
    /// Most-restrictive settings for when persisted state is present but
    /// untrustworthy (corrupt or unreadable). Distinct from [`Default`], which
    /// is the genuine first-run profile: `Default` leaves `friend_auto_accept_known`
    /// ON (the product default), so reusing it on a corrupt file would silently
    /// re-grant auto-accept to a user who had opted OUT. Fail closed on every
    /// privacy/trust toggle (`identity_discoverable` OFF, `friend_auto_accept_known`
    /// OFF) while keeping the vetted relay pool — relays are operational
    /// infrastructure, not a user opt-out, and emptying them would brick
    /// connectivity for no security gain.
    fn fail_closed_defaults() -> Self {
        Self {
            identity_discoverable: false,
            friend_auto_accept_known: false,
            relays: default_pkarr_relays(),
            // ZEB-624: empty = follow the iroh preset defaults. Like `relays`,
            // iroh relays are operational infrastructure, not a privacy/trust
            // opt-out, so the fail-closed value is simply "no custom override"
            // (defaults) rather than a restrictive flip.
            iroh_relays: Vec::new(),
            // ZEB-600: fail closed = INVISIBLE. A corrupt/unreadable file must
            // never silently re-broadcast a user who had opted to appear offline.
            // This is the INVERSE of identity_discoverable's closed value (false):
            // presence's restrictive value is "don't broadcast" = invisible = true.
            presence_invisible: true,
            // ZEB-376: fail closed = Closed. A corrupt/unreadable file must never
            // silently accept an introduction from a stranger; Closed rejects all
            // inbound introductions until the file is fixed. (Distinct from the
            // fresh-install default `FriendsOfFriends`, which trusts active-friend
            // vouchers — the closed value is strictly the restrictive floor.)
            peer_intro_policy: crate::friend_graph::PeerIntroPolicy::Closed,
        }
    }

    /// Persist the fail-closed posture (invisible: `identity_discoverable` OFF,
    /// `presence_invisible`, auto-accept OFF, intro Closed) while keeping the
    /// vetted relay pool so connectivity is not bricked.
    ///
    /// ZEB-881 mint-recovery use: post-flip a MISSING settings file loads
    /// [`Default`] = discoverable **ON**, so the mint's privacy-posture reset can
    /// no longer "fail safe" by deleting the file. When the reset write fails,
    /// the recovery path writes this explicit non-discoverable state instead, so
    /// a degraded mint never broadcasts the new identity. The happy path still
    /// gets the ON product default; only the error path fails closed.
    pub(crate) fn persist_fail_closed(path: &PathBuf) -> std::io::Result<()> {
        Self::fail_closed_defaults().save(path)
    }

    pub fn load_or_default(path: &PathBuf) -> Self {
        let contents = match std::fs::read_to_string(path) {
            Ok(s) => s,
            // A genuinely-absent file is the normal first-run case — quiet default.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                // An existing-but-unreadable file (permissions, I/O error) is NOT
                // first-run: treating it as one could silently drop a prior
                // opt-out. Fail CLOSED and LOUD until the file is readable again.
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "connectivity-settings.json could not be read — failing closed; any prior privacy/trust opt-out stays in effect until the file is readable"
                );
                return Self::fail_closed_defaults();
            }
        };
        match serde_json::from_str(&contents) {
            Ok(settings) => settings,
            Err(e) => {
                // Fail CLOSED and LOUD: a corrupt settings file must NEVER fail
                // open — silently becoming discoverable or re-enabling auto-accept
                // would violate a real opt-out, and privacy-fail-open is worse
                // than a freeze. Surface it so the operator can fix the file; use
                // the most-restrictive defaults in the meantime.
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "connectivity-settings.json failed to parse — failing closed; any prior privacy/trust opt-in will NOT take effect until the file is fixed"
                );
                Self::fail_closed_defaults()
            }
        }
    }

    /// Atomically persist the settings to `path`. Serializes to a *uniquely*
    /// named sibling tempfile (`tempfile::NamedTempFile`), fsyncs it, then
    /// renames it into place; on Unix the parent directory is fsynced afterwards
    /// so the rename itself is durable. A same-directory rename is atomic on
    /// macOS/Linux (POSIX `rename(2)`), so a concurrent reader never observes a
    /// half-written settings file and a crash mid-write leaves the prior file
    /// intact.
    ///
    /// This mirrors `owner_state_persist::save_atomically`. Two reasons it beats
    /// the previous fixed `<name>.json.tmp` + `std::fs::rename`: (1) the random
    /// temp name can't collide with a concurrent writer's temp (the fixed name
    /// was racy); (2) on Windows the fixed-name `std::fs::rename` is best-effort
    /// and fails when the destination already exists, whereas `NamedTempFile::
    /// persist` uses `MoveFileEx`/`ReplaceFile`, which atomically *replaces* an
    /// existing destination.
    pub fn save(&self, path: &PathBuf) -> std::io::Result<()> {
        use std::io::Write as _;
        // `NamedTempFile` must be created in the destination directory so the
        // final `persist` is a same-volume atomic rename. Fall back to `.` for a
        // bare filename with no directory component (no real caller does this).
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(json.as_bytes())?;
        tmp.as_file().sync_all()?;
        tmp.persist(path).map_err(std::io::Error::other)?;
        // Unix-only dir fsync (matches owner_state_persist): `File::open(dir)`
        // fails on Windows, whose journaled `MoveFileEx`/`ReplaceFile` already
        // durably records the rename.
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    /// ZEB-796: reset the identity-scoped privacy/trust posture to product
    /// first-run defaults for a freshly-minted identity, PRESERVING the
    /// machine-level relay infrastructure. `relays` / `iroh_relays` are
    /// operational infra, not a user opt-out (see [`Self::fail_closed_defaults`]),
    /// so they carry across an identity rotation; the four privacy/trust toggles
    /// do not.
    ///
    /// `connectivity-settings.json` is keyed to the app-data dir, not the
    /// identity, so without this a new identity silently inherits the previous
    /// one's discoverability (it produced a false conclusion during ZEB-770).
    /// Every new identity funnels through mint, so this single call normalizes
    /// the three start-fresh paths that today disagree on whether the file
    /// survives: the boot-failure reset (`reset_local_identity`) and profile
    /// reuse preserve it, while the ZEB-842 clean-slate wipe deletes it.
    ///
    /// Effect: `identity_discoverable` → OFF, `friend_auto_accept_known` → ON,
    /// `presence_invisible` → visible, `peer_intro_policy` → FriendsOfFriends;
    /// `relays` / `iroh_relays` carried over from any existing file, else the
    /// default pool. Uses product [`Default`], not [`Self::fail_closed_defaults`]:
    /// a deliberate mint is a fresh *install*, not untrusted state — and the one
    /// safety-critical toggle (`identity_discoverable`) is OFF in both, so the
    /// fail-safe direction is covered regardless.
    pub fn reset_privacy_posture_for_new_identity(path: &PathBuf) -> std::io::Result<()> {
        // Carry the machine's relay infra across the reset. `load_or_default`
        // already fails closed on a corrupt/unreadable file (relays fall back to
        // the vetted default pool), which is the right behavior here too — a new
        // identity never inherits a *privacy* toggle, but it should keep whatever
        // relay pool this machine can actually reach.
        let existing = Self::load_or_default(path);
        let reset = Self {
            relays: existing.relays,
            iroh_relays: existing.iroh_relays,
            ..Self::default()
        };
        reset.save(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---- ZEB-624: iroh relay list ----

    #[test]
    fn iroh_relays_default_empty_and_roundtrip() {
        let s = ConnectivitySettings::default();
        assert!(s.iroh_relays.is_empty()); // empty = follow iroh preset defaults
                                           // old file without the key parses with empty vec (serde default)
        let old = r#"{"identity_discoverable":false,"friend_auto_accept_known":true,"relays":["https://pkarr.q8.fyi"],"presence_invisible":false}"#;
        let parsed: ConnectivitySettings = serde_json::from_str(old).unwrap();
        assert!(parsed.iroh_relays.is_empty());
    }

    #[test]
    fn validate_iroh_relay_urls_rules() {
        // https accepted + normalized (trailing slash stripped)
        assert_eq!(
            validate_iroh_relay_urls(vec!["https://use1-1.relay.n0.iroh.link/".into()]).unwrap(),
            vec!["https://use1-1.relay.n0.iroh.link".to_string()]
        );
        // must also parse as an iroh RelayUrl
        assert!(validate_iroh_relay_urls(vec!["https://relay example".into()]).is_err());
        // empty list rejected (use reset for defaults)
        assert!(validate_iroh_relay_urls(vec![]).is_err());
        // http only for local hosts; dedup; cap MAX_IROH_RELAYS=8 — mirror the pkarr test matrix
        assert!(validate_iroh_relay_urls(vec!["http://127.0.0.1:3340".into()]).is_ok());
        assert!(validate_iroh_relay_urls(vec!["http://relay.evil.example".into()]).is_err());
    }

    #[test]
    fn validate_iroh_relay_urls_dedups_and_caps() {
        // Dedup on the trailing-slash-normalized value (first wins), mirroring the
        // pkarr matrix; and reject more than MAX_IROH_RELAYS distinct entries.
        let deduped = validate_iroh_relay_urls(vec![
            "https://use1-1.relay.n0.iroh.link".into(),
            "https://use1-1.relay.n0.iroh.link/".into(),
        ])
        .expect("dedup");
        assert_eq!(
            deduped,
            vec!["https://use1-1.relay.n0.iroh.link".to_string()]
        );
        let many: Vec<String> = (0..(MAX_IROH_RELAYS + 1))
            .map(|i| format!("https://r{i}.relay.example"))
            .collect();
        assert!(validate_iroh_relay_urls(many).is_err());
    }

    #[test]
    fn effective_iroh_relays_empty_is_none() {
        // Empty persisted list → None = "follow the iroh preset's default relay
        // map" (n0 stable), the sentinel Task 5 consumes at endpoint build.
        let s = ConnectivitySettings::default();
        assert!(effective_iroh_relays(&s).is_none());
    }

    #[test]
    fn effective_iroh_relays_custom_is_some_sanitized() {
        // Lenient sanitize: drop the malformed entry, dedup the trailing-slash
        // duplicate, keep the valid relay — then wrap in Some.
        let s = ConnectivitySettings {
            iroh_relays: vec![
                "https://use1-1.relay.n0.iroh.link".to_string(),
                "not a url".to_string(),                          // dropped
                "https://use1-1.relay.n0.iroh.link/".to_string(), // dedup
            ],
            ..Default::default()
        };
        assert_eq!(
            effective_iroh_relays(&s),
            Some(vec!["https://use1-1.relay.n0.iroh.link".to_string()])
        );
    }

    #[test]
    fn iroh_relays_round_trips() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let s = ConnectivitySettings {
            iroh_relays: vec!["https://use1-1.relay.n0.iroh.link".to_string()],
            ..Default::default()
        };
        s.save(&path).expect("save");
        assert_eq!(
            ConnectivitySettings::load_or_default(&path).iroh_relays,
            s.iroh_relays
        );
    }

    #[test]
    fn save_is_atomic_no_stray_temp_files() {
        // save() writes to a uniquely-named NamedTempFile sibling then renames it
        // into place. After a successful save: (1) the persisted file round-trips
        // to an equal value, and (2) the parent dir holds ONLY the settings file
        // — the temp file was renamed away, not left behind (glob the dir).
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let s = ConnectivitySettings::default();
        s.save(&path).expect("save");
        // (1) Round-trips.
        assert_eq!(ConnectivitySettings::load_or_default(&path), s);
        // (2) No stray temp files: the fresh tempdir contains exactly one entry,
        // the settings file itself.
        let entries: Vec<_> = std::fs::read_dir(td.path())
            .expect("read tempdir")
            .map(|e| e.expect("dir entry").path())
            .collect();
        assert_eq!(
            entries,
            vec![path.clone()],
            "only the settings file should remain in the parent dir (no temp leftovers)"
        );
    }

    #[test]
    fn defaults_to_discoverable() {
        // ZEB-881: fresh identities are discoverable by default so first
        // cross-WAN contact works; users opt into privacy, not out of usability.
        let settings = ConnectivitySettings::default();
        assert!(settings.identity_discoverable);
    }

    #[test]
    fn persisted_opt_out_is_preserved_not_migrated() {
        // ZEB-881 no-migration boundary: flipping the *product default* to ON
        // must NOT rewrite an existing user who explicitly persisted OFF.
        // `load_or_default` returns a valid persisted file verbatim, so a saved
        // `identity_discoverable: false` stays false — the new ON default only
        // reaches fresh profiles (missing file) and mint reset.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let persisted = ConnectivitySettings {
            identity_discoverable: false,
            ..Default::default()
        };
        persisted.save(&path).expect("save");
        assert!(
            !ConnectivitySettings::load_or_default(&path).identity_discoverable,
            "ZEB-881: a persisted opt-out must survive the default flip, not silently migrate to ON"
        );
    }

    #[test]
    fn persist_fail_closed_writes_invisible_but_keeps_relays() {
        // ZEB-881 mint-recovery: the reset-failure path must write an EXPLICIT
        // non-discoverable state (not rely on delete → Default, which is now ON).
        // The written file must load back as invisible/opted-out while retaining
        // the relay pool so connectivity is not bricked.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        ConnectivitySettings::persist_fail_closed(&path).expect("write fail-closed");
        let loaded = ConnectivitySettings::load_or_default(&path);
        assert!(
            !loaded.identity_discoverable,
            "fail-closed recovery must leave the new identity NOT discoverable"
        );
        assert!(loaded.presence_invisible, "fail-closed recovery must be invisible");
        assert!(!loaded.friend_auto_accept_known, "fail-closed recovery must not auto-accept");
        assert!(!loaded.relays.is_empty(), "fail-closed must keep the relay pool, not brick connectivity");
    }

    #[test]
    fn legacy_file_omitting_discoverable_loads_off_not_default_on() {
        // A pre-ZEB-881 settings file has no `identity_discoverable` key. The
        // field's `#[serde(default)]` fills `bool::default()` = FALSE — the
        // struct's Default (now ON) does NOT apply to individual omitted fields.
        // This is the intended no-migration contract: an established identity
        // that predates the flag stays private until it opts in, rather than
        // being silently broadcast on the next launch.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("legacy.json");
        std::fs::write(&path, r#"{"friend_auto_accept_known":true}"#).expect("write");
        let loaded = ConnectivitySettings::load_or_default(&path);
        assert!(
            !loaded.identity_discoverable,
            "ZEB-881: a legacy file omitting the field must load OFF (serde field default), \
             never silently inherit the new ON struct default"
        );
    }

    #[test]
    fn parse_error_fails_closed_not_open() {
        // A corrupt settings file must fail CLOSED on EVERY privacy/trust toggle,
        // never open. Privacy-fail-open would silently violate a real opt-out —
        // both `identity_discoverable` (don't broadcast) and
        // `friend_auto_accept_known` (don't auto-accept) must land restrictive.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("connectivity-settings.json");
        std::fs::write(&path, b"{ this is not valid json").unwrap();
        let settings = ConnectivitySettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
        assert!(!settings.friend_auto_accept_known);
        // ZEB-881 guard: the ON default must NOT leak into the fail-closed path.
        assert!(!ConnectivitySettings::fail_closed_defaults().identity_discoverable);
    }

    #[test]
    fn unreadable_file_fails_closed_not_first_run() {
        // An existing-but-unreadable path must NOT be mistaken for first-run
        // (which would re-enable auto-accept). A directory at the settings path
        // yields a non-NotFound read error, exercising the fail-closed branch.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("connectivity-settings.json");
        std::fs::create_dir(&path).unwrap();
        let settings = ConnectivitySettings::load_or_default(&path);
        assert!(!settings.identity_discoverable);
        assert!(!settings.friend_auto_accept_known);
    }

    #[test]
    fn missing_file_returns_default() {
        // A genuinely-absent file IS first-run: the product default
        // (auto-accept ON) applies, distinct from the fail-closed paths above.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let settings = ConnectivitySettings::load_or_default(&path);
        assert!(settings.identity_discoverable);
        assert!(settings.friend_auto_accept_known);
    }

    #[test]
    fn defaults_to_auto_accept_known_on() {
        // ZEB-371 spec §7.1: auto-accept KNOWN requesters defaults ON.
        let settings = ConnectivitySettings::default();
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
        let loaded = ConnectivitySettings::load_or_default(&path);
        assert!(loaded.identity_discoverable);
        assert!(loaded.friend_auto_accept_known);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("nonexistent.json");
        let settings = ConnectivitySettings::load_or_default(&path);
        assert!(settings.identity_discoverable);
    }

    #[test]
    fn round_trip_save_then_load() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let settings = ConnectivitySettings {
            identity_discoverable: true,
            friend_auto_accept_known: false,
            relays: vec!["https://relay.pkarr.org".to_string()],
            iroh_relays: Vec::new(),
            presence_invisible: false,
            peer_intro_policy: crate::friend_graph::PeerIntroPolicy::FriendsOfFriends,
        };
        settings.save(&path).expect("save");

        let loaded = ConnectivitySettings::load_or_default(&path);
        assert_eq!(loaded, settings);
    }

    #[test]
    fn defaults_to_recommended_relays() {
        let settings = ConnectivitySettings::default();
        assert_eq!(settings.relays, default_pkarr_relays());
        assert!(settings.relays.len() >= 2, "must ship a >=2 relay default");
    }

    #[test]
    fn self_hosted_relay_leads_default_pool() {
        // The Zeblithic-operated relay leads the pool so the fleet shares one
        // deterministic rendezvous (ZEB-513); the public relays stay behind it
        // as redundancy fallbacks (ZEB-330/380).
        let relays = default_pkarr_relays();
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
        let loaded = ConnectivitySettings::load_or_default(&path);
        assert_eq!(loaded.relays, default_pkarr_relays());
    }

    #[test]
    fn round_trips_custom_relays() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let settings = ConnectivitySettings {
            identity_discoverable: false,
            friend_auto_accept_known: true,
            relays: vec!["https://relay.pkarr.org".to_string()],
            iroh_relays: Vec::new(),
            presence_invisible: false,
            peer_intro_policy: crate::friend_graph::PeerIntroPolicy::FriendsOfFriends,
        };
        settings.save(&path).expect("save");
        assert_eq!(
            ConnectivitySettings::load_or_default(&path).relays,
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
        let settings = ConnectivitySettings::load_or_default(&path);
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
        // The caller (effective_pkarr_relays) maps empty → default_pkarr_relays().
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

    #[test]
    fn presence_invisible_defaults_visible() {
        // First-run product default: presence broadcasts (invisible = false).
        assert!(!ConnectivitySettings::default().presence_invisible);
    }

    #[test]
    fn presence_invisible_missing_field_defaults_visible() {
        // A pre-ZEB-600 settings file has no `presence_invisible` key; serde's
        // field default must fill it FALSE so existing users keep broadcasting.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("legacy.json");
        std::fs::write(&path, r#"{"identity_discoverable":true}"#).expect("write");
        assert!(!ConnectivitySettings::load_or_default(&path).presence_invisible);
    }

    #[test]
    fn presence_invisible_fails_closed_to_invisible() {
        // A corrupt settings file must fail CLOSED = INVISIBLE: never silently
        // re-broadcast a user who had opted to appear offline. NB this is the
        // INVERSE direction of identity_discoverable (whose closed value is false).
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        std::fs::write(&path, b"{ not valid json").expect("write");
        assert!(ConnectivitySettings::load_or_default(&path).presence_invisible);
    }

    #[test]
    fn presence_invisible_round_trips() {
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        let s = ConnectivitySettings {
            presence_invisible: true,
            ..Default::default()
        };
        s.save(&path).expect("save");
        assert!(ConnectivitySettings::load_or_default(&path).presence_invisible);
    }

    // ---- ZEB-376: peer_intro_policy ----

    #[test]
    fn peer_intro_policy_defaults_to_fof_and_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("connectivity-settings.json");
        // Fresh install default = FriendsOfFriends.
        assert_eq!(
            ConnectivitySettings::load_or_default(&path).peer_intro_policy,
            crate::friend_graph::PeerIntroPolicy::FriendsOfFriends,
        );
        let s = ConnectivitySettings {
            peer_intro_policy: crate::friend_graph::PeerIntroPolicy::AskMe,
            ..Default::default()
        };
        s.save(&path).unwrap();
        assert_eq!(
            ConnectivitySettings::load_or_default(&path).peer_intro_policy,
            crate::friend_graph::PeerIntroPolicy::AskMe,
        );
    }

    #[test]
    fn corrupt_settings_fails_closed_to_closed_policy() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("connectivity-settings.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        // A corrupt file must NOT silently widen the policy: fail closed to Closed.
        assert_eq!(
            ConnectivitySettings::load_or_default(&path).peer_intro_policy,
            crate::friend_graph::PeerIntroPolicy::Closed,
        );
    }

    // ---- ZEB-796: reset privacy posture on mint ----

    #[test]
    fn reset_privacy_posture_resets_toggles_preserves_relays() {
        // A freshly-minted identity must not inherit the previous identity's
        // privacy posture, but MUST keep the machine's relay infrastructure.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        // Seed an "inherited" file: every privacy/trust toggle flipped AWAY from
        // its product default, plus a custom relay pool the machine configured.
        let inherited = ConnectivitySettings {
            identity_discoverable: true,
            friend_auto_accept_known: false,
            presence_invisible: true,
            peer_intro_policy: crate::friend_graph::PeerIntroPolicy::Closed,
            relays: vec!["https://relay.pkarr.org".to_string()],
            iroh_relays: vec!["https://use1-1.relay.n0.iroh.link".to_string()],
        };
        inherited.save(&path).expect("seed inherited settings");

        ConnectivitySettings::reset_privacy_posture_for_new_identity(&path).expect("reset");

        let after = ConnectivitySettings::load_or_default(&path);
        // All four privacy/trust toggles back to product Default.
        // ZEB-881: mint resets to the product Default, which is now ON.
        assert!(after.identity_discoverable, "discoverable must reset ON");
        assert!(
            after.friend_auto_accept_known,
            "auto-accept-known back to product default ON"
        );
        assert!(!after.presence_invisible, "presence back to visible");
        assert_eq!(
            after.peer_intro_policy,
            crate::friend_graph::PeerIntroPolicy::FriendsOfFriends,
        );
        // Machine relay infra preserved verbatim (not reset to the default pool).
        assert_eq!(after.relays, vec!["https://relay.pkarr.org".to_string()]);
        assert_eq!(
            after.iroh_relays,
            vec!["https://use1-1.relay.n0.iroh.link".to_string()]
        );
    }

    #[test]
    fn reset_privacy_posture_on_missing_file_writes_clean_default() {
        // No prior file (a genuine first-run mint): the reset writes product
        // Default — discoverable OFF, the vetted default relay pool.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        assert!(!path.exists(), "precondition: no settings file");
        ConnectivitySettings::reset_privacy_posture_for_new_identity(&path).expect("reset");
        assert_eq!(
            ConnectivitySettings::load_or_default(&path),
            ConnectivitySettings::default()
        );
    }

    #[test]
    fn reset_privacy_posture_on_corrupt_file_writes_clean_default() {
        // A corrupt inherited file must not block the reset and must not carry a
        // stale toggle across: load_or_default fails closed (discoverable OFF,
        // default relay pool), then the reset writes product Default over it.
        let td = TempDir::new().expect("tempdir");
        let path = td.path().join("connectivity-settings.json");
        std::fs::write(&path, b"{ not valid json").expect("write corrupt");
        ConnectivitySettings::reset_privacy_posture_for_new_identity(&path).expect("reset");
        let after = ConnectivitySettings::load_or_default(&path);
        // ZEB-881: reset writes product Default (now discoverable ON) over the
        // fail-closed load; relays fall back to the default pool.
        assert!(after.identity_discoverable);
        assert_eq!(after.relays, default_pkarr_relays());
        assert_eq!(after, ConnectivitySettings::default());
    }
}
