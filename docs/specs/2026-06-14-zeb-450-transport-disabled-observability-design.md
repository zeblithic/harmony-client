# ZEB-450: surface a boot-time transport-disabled reason to the UI

**Status:** approved (scope confirmed with Jake 2026-06-14 — full UI surface + doc fix)
**Branch:** `zeb-450-transport-disabled-observability` off main `a53ebfb9`

## Problem

Setting `HARMONY_DISABLE_KEYCHAIN=1` on a real launch with no
`HARMONY_PASSPHRASE`/`_FILE` (or any boot where the iroh secret key can't be
loaded/created, or the endpoint can't bind) brings the node up **with no
transport** and the only signal is a buried `tracing::warn!` at the boot site
(`lib.rs`, the `load_or_create_secret_key()` match). The node looks healthy; it
just can't network. A fleet operator or agent who set the kill-switch by analogy
with the test guidance gets a silently non-networking instance.

The root cause — the iroh key being keychain-only — was already fixed by
**ZEB-449** (encrypted-file fallback for the app-local/iroh key). With a
passphrase configured, `HARMONY_DISABLE_KEYCHAIN=1` now degrades to file storage
with transport intact. What remains is **observability**: the genuinely-disabled
case (no keychain AND no passphrase) is loud at the boundary
(`load_or_create_secret_key` returns an actionable error) but that error is
swallowed into a log line and never reaches the UI.

`NetworkHealthSnapshot.my_network: None` is overloaded — it means *both* "iroh
still initializing" and "transport disabled this session" — so the Network
Health panel can't tell a transient startup from a permanent failure.

## Decision

Carry the boot-time failure reason to the UI as an explicit, orthogonal field
rather than overloading `my_network`. The reason is written **once** at boot
(never mutated), so no interior mutability / telemetry type is needed — a plain
`Option<String>` in `NodeState`, stamped onto the snapshot at the single IPC
seam.

## Scope

In:
- `NetworkHealthSnapshot.transport_disabled_reason: Option<String>` (serde
  camelCase, `#[serde(default)]` for forward-compat).
- Capture the actionable reason in the two boot `Err` arms (key load/create
  fail; endpoint bind fail); stash in `NodeState.transport_disabled_reason`.
- Stamp it onto the snapshot in `network_health_snapshot_impl` via a pure
  `stamp_transport_status` helper — covers BOTH the live-service and
  service-absent (`empty()`) paths, since the disabled case has no
  `NetworkHealthService` (it's set `None` when iroh bind fails).
- Frontend: `transportDisabledReason?: string | null` on the type; a persistent
  "This node can't network" banner in `NetworkHealthView.svelte`; suppress the
  pointless 30s startup auto-retry when a reason is present (a restart is
  required).
- Doc: correct the inaccurate `headless-install.md` note ("blocks mint" — it
  does not; the owner seed has its own file fallback) and document that the
  kill-switch must never be set on a real launch without a passphrase.

Out (deferred / already covered):
- The transport-level streaming key load is unchanged (ZEB-449 owns the
  fallback).
- A startup modal/toast — the persistent Network Health banner is the surface;
  a global toast can be a later follow-up if wanted.
- De-overloading `my_network: None` elsewhere — not needed; the new field is
  additive.

## Testing

- Rust: `empty()` defaults the reason to `None`; `stamp_transport_status`
  sets/overwrites/clears; the field serializes as `transportDisabledReason`
  (camelCase) and a pre-field payload deserializes to `None`.
- Frontend: the banner renders with the reason and `role="alert"` and replaces
  the "starting up…" placeholder when a reason is set; the placeholder (not the
  banner) shows when no reason is set.

## Risk

Low — additive field, single stamp point, no change to the transport or
identity-persistence paths. The boot-arm reason strings are best-effort UI copy
wrapping the existing actionable error; the underlying loud-at-the-boundary
behavior (and its `iroh_key_file_fallback.rs` coverage) is unchanged.
