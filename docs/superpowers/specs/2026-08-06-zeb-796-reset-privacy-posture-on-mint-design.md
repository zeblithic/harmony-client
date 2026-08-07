# ZEB-796 — Reset privacy posture on mint (design)

**Status:** approved (Jake, 2026-08-06). Decision-request ticket; this file is the decision on the record.

## Problem

`connectivity-settings.json` is keyed to the **app-data dir**, not to the identity, so a
newly-minted identity silently inherits the *previous* identity's privacy/trust posture —
most consequentially `identity_discoverable`, which is documented as *identity-keyed* yet
persisted in an identity-agnostic file. "Mint a fresh identity to get a clean slate" does
not actually produce a clean slate; during ZEB-770 this produced a false conclusion (a
2-minute-old identity reading `identity_discoverable: true` was taken as evidence nobody
enabled it, when the flag simply predated the identity by weeks).

### Sharpened premise (verified against source)

There are **two** "start fresh" flows and they already disagree:

| Flow | `connectivity-settings.json` | Re-mint result |
|---|---|---|
| ZEB-842 clean-slate wipe (`remove_dir_children_except`, excludes only `profiles`/`logs`/`api`) | **deleted** | fresh defaults ✓ |
| Boot-failure reset (`reset_local_identity`, ZEB-835/6 — moves only `OWNER_RESET_FILES`) | **preserved** | inherits posture ✗ |
| Fresh profile reusing an old dir | preserved | inherits ✗ |

So the leak is real on the boot-reset and profile paths, and the codebase's own two flows
are inconsistent. Normalizing at the **mint choke point** fixes all three at once.

## Decision

**A freshly-minted identity's connectivity *posture* equals a genuine fresh install's; the
machine's *relay infrastructure* is preserved.**

On mint, reset the four privacy/trust toggles to their product `Default` values while
preserving `relays` + `iroh_relays`:

| Field | Reset to (product `Default`) | Nature |
|---|---|---|
| `identity_discoverable` | `false` (OFF) | identity-keyed |
| `friend_auto_accept_known` | `true` (ON) | identity posture (vacuous on a friendless new identity) |
| `presence_invisible` | `false` (visible) | node/identity posture |
| `peer_intro_policy` | `FriendsOfFriends` | identity posture |
| `relays` / `iroh_relays` | **preserved from existing file** | machine/operational infra |

Rationale: extends the existing principle *"never silently restore a privacy posture the
user did not choose"* (today wired only to the corrupt-file trigger in
`fail_closed_defaults`) to the identity-rotation trigger. Uses product `Default`, not the
restrictive `fail_closed` floor, because a deliberate mint is a fresh *install*, not
untrusted state — and the one safety-critical field (`identity_discoverable`) is OFF in
both, so the fail-safe direction is covered regardless.

## Design

### 1. Helper — `connectivity_settings.rs`

```rust
/// Reset the identity-scoped privacy/trust posture to product first-run
/// defaults for a freshly-minted identity, preserving the machine-level relay
/// infrastructure (relays / iroh_relays are operational, not a user opt-out).
pub fn reset_privacy_posture_for_new_identity(path: &PathBuf) -> std::io::Result<()>
```

- Load the existing file (if any) to capture `relays` + `iroh_relays` (sanitized as on the
  normal read path); a missing/corrupt file falls back to the default pool.
- Write `ConnectivitySettings { ..Default::default(), relays, iroh_relays }` via the
  existing atomic `save()`.

### 2. Hook — `mint_owner_identity_inner` (owner_commands.rs)

Inside the Phase-2 `OWNER_STATE_WRITE_LOCK` window, immediately after
`save_owner_state_atomic` succeeds, call the helper with
`resolve_app_data_dir()?.join("connectivity-settings.json")`.

**Failure posture:** the identity is already persisted (no rollback — spec §7.1). A
settings-reset write failure must **not** fail the mint. Log loudly and best-effort remove
the inherited file so the next `load_or_default` fails safe to `Default` (discoverable OFF).
Mirrors the keychain-clear "log, don't fail the reset" posture, nudged fail-safe.

## Scope / non-goals

- **In:** the fresh-mint path only (`mint_owner_identity_inner`), harmony-client, single PR.
- **Out:** restore-from-phrase (recovers a *known* identity — posture should not be
  force-reset); no core-crate change (`mint_owner` untouched); no first-boot UX surface
  (that was the rejected Option 3).

## Test plan

- Unit (`connectivity_settings.rs`): helper over a seeded file with
  `identity_discoverable:true`, `friend_auto_accept_known:false`, `presence_invisible:true`,
  `peer_intro_policy:Closed`, and a custom relay pool → asserts the four toggles land at
  `Default` and the custom relays survive. Plus: missing file → writes clean `Default`;
  corrupt file → writes clean `Default` (relays fall back to the default pool).
- Integration (owner_commands mint lifecycle): mint over a dir seeded with an inherited
  discoverable-true settings file + custom relays → after mint, on-disk toggles are reset,
  custom relays preserved.

## Docs

- Testing docs note (the ZEB-770 lesson): a freshly-minted identity is now a genuine
  clean slate for privacy posture; relays persist. So "mint a fresh profile" IS a valid
  clean-slate control for discoverability again.
