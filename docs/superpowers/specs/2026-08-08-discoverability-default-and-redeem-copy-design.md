# ZEB-881 + ZEB-879a: default discoverability ON + onboarding note + redeem error copy

Date: 2026-08-08
Tickets: ZEB-881 (default discoverability ON), ZEB-879 part (a) (misleading redeem error copy).
Deferred: ZEB-879 part (b) — runtime enable→republish stall — tracked separately.

## Problem

Surfaced during the ZEB-878 v0.2.4 fleet validation:

1. A fresh v0.2.4 identity mints with `identity_discoverable = false` (ZEB-796 resets the
   privacy posture to product `Default` on mint, and `Default` was OFF). With it off, the
   identity's pkarr case-B routing record is never published, so **first cross-WAN contact
   silently fails** for every fresh identity — both AVALON and Ildwyn hit this on their very
   first attempt. Product decision (Jake): default-off is **not a real privacy gain** and is a
   considerable usability loss — default should be **ON**.
2. On the redeem happy-path, a fresh identity that hasn't warmed its relay pool gets the raw
   `"no relays available (all on cooldown or unreachable)"` string, which is misleading — the
   relays are reachable; the pool is simply cold (or, pre-fix, discovery was off). The redeem
   dialog currently maps this to the generic FALLBACK ("Couldn't reach the network.").

## Scope

- **A. Backend default flip** — `identity_discoverable` default OFF → ON.
- **B. Onboarding privacy note** — a dismissible note that the identity is discoverable, with a
  pointer to go private.
- **C. Redeem error copy** — an actionable, non-misleading message for the "no relays available"
  warm-up case.

Out of scope (separate follow-up): the runtime enable→republish stall (ZEB-879b) — the
`PkarrIdentityPublisher::enable()` → `register()` path that stalled ~14 min on AVALON. Distinct
subsystem from the member-card publisher (ZEB-882-D1); not one shared fix. Non-blocking once the
default is ON.

## Design

### A. Backend default flip (ZEB-881)

- `connectivity_settings.rs` `impl Default`: `identity_discoverable: false → true`.
- This propagates automatically to the two creation paths:
  - `load_or_default(path)` — a fresh profile with no settings file returns `Self::default()`.
  - `reset_privacy_posture_for_new_identity(path)` — mint; builds `Self { relays, iroh_relays,
    ..Self::default() }`, so `identity_discoverable` is taken from `Default`.
- `fail_closed_defaults()` **stays `false`** — a corrupt/unreadable settings file must never
  silently become discoverable. Unchanged; this is the fail-safe, not first-run.
- **No migration of existing users.** `load_or_default` returns a persisted file verbatim, so
  users who already have a settings file keep their chosen value. Only fresh profiles and freshly
  minted identities get the new ON default. This is deliberate — we never flip a user's privacy
  posture out from under them.
- The ZEB-794 boot-log line that announces "identity discoverability OFF (default) …" is updated
  to reflect the ON default (it already has an ON-branch message; ensure the default path logs the
  resolvable/ON line).

### B. Onboarding privacy note (ZEB-881 Option A)

- Add a **dismissible** privacy note to `WelcomeModal`'s welcome/`explain` stage. Do **not** add a
  new wizard step — the mint hard-gate stays a 3-step wizard.
- Copy (mirrors the Settings → Network panel's own language so the terms stay consistent — the
  toggle reads "Allow discovery by identity address"): *"When you create your identity, you'll be
  discoverable by identity address — anyone who has it can connect to your devices. You can go
  private anytime in Settings → Network."* Two accuracy constraints: (1) discoverability is case-B
  identity discovery only — **invites/friend-tokens still work in private mode** (they resolve a
  separate case-A record keyed by the token secret), so the copy must not tie invites to
  discoverability; (2) the note renders on the shared `explain` chooser, which also offers Join /
  Restore (existing, possibly-private identities), so the promise is **scoped to "when you create
  your identity"** and must not unconditionally claim the reader will be discoverable.
- Dismissal is **local component state** (`$state`), not a persisted flag. The `WelcomeModal` is a
  one-time pre-mint hard-gate, so **no `ownerId` exists yet** to scope a durable flag by — and the
  existing onboarding-flags seam (`onboarding-backup-flags.ts`) is deliberately owner-scoped for
  exactly this reason: its ZEB-587 header documents that owner-agnostic global localStorage keys are
  bundle-scoped (not profile-isolated), so a device-global "dismissed" key would suppress the note
  for *every future identity* minted on the machine. Re-showing the note on each fresh mint is the
  intended behavior — a new identity's owner should be re-informed they'll be discoverable. Within a
  session the note stays dismissed after "Got it"; a remount (only reachable pre-mint) re-shows it.
- Reference Settings → Network (the existing `NetworkDiscoverabilitySettings` toggle) in copy
  rather than adding a new deep-link.

### C. Redeem error copy (ZEB-879a)

- Add a `VARIANT_PATTERNS` entry in `redeem-invite-errors.ts` matching `/no relays available/i`:
  - summary: *"The network is still warming up."*
  - hint: *"Discovery relays warm up for about a minute after launch — try again shortly. If it
    keeps failing, the inviter may not be discoverable yet."*
  - tag: `relays_warming_up`
- This replaces the generic FALLBACK for this case. Leads with the transient warm-up framing
  because default-ON makes "inviter not discoverable" rare. `mapRedeemInviteError` already routes
  the redeem dialog through this table (`RedeemInviteDialog.svelte`), so no wiring change.

## Testing

- **Rust** (`connectivity_settings.rs` tests):
  - Update assertions where the Default / mint-reset path asserted `!identity_discoverable` to
    assert it is now `true`. Audit each: fail-closed and explicit-persisted-file cases stay OFF.
  - The ZEB-796 reset test currently asserting "discoverable must reset OFF" flips to assert ON.
  - Assert `fail_closed_defaults().identity_discoverable == false` stays.
- **Frontend**:
  - `redeem-invite-errors` test: `mapRedeemInviteError("no relays available (all on cooldown or
    unreachable)")` returns the `relays_warming_up` variant (summary/hint/tag), not FALLBACK.
  - `WelcomeModal` test: the privacy note renders, names the private-mode escape hatch
    (`Settings → Network`), and is dismissible (dismissal holds for the mounted instance).
- Full CI-parity gate before PR:
  - From `src-tauri/`: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
  - From the repository root: `npx tsc --noEmit`; `npx vitest run`.

## Risks / notes

- The default flip is a privacy-posture change; the fail-closed and no-migration invariants above
  bound it to fresh identities only. Reviewers should confirm no test that intends to assert the
  *fail-safe* (corrupt file) path was flipped.
- Existing settings files across the fleet remain OFF unless the user toggles — expected.
