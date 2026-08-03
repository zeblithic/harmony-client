# ZEB-853 T-MISC — bounded-time cleanup (design)

**Ticket:** ZEB-853 — the last direct T-\* implementation ticket of the ZEB-831
wall-clock threat model (§4 MEDIUM/LOW + NEEDS-VERIFICATION). All Urgent+High
tiers (846/847/848/849/850/851/852) already merged.

**Goal:** close five bounded-but-real clock-trust / ordering defects, each a
peer-supplied stamp (or a unit-confused stamp) influencing an ordering, freeze,
or admission decision. All in `harmony-client` → one PR.

**Clock-trust model (unchanged, from ZEB-831 PR #580):** control-tier stamps are
gated at `clock_trust::MAX_FORWARD_SKEW_MS` (5 min); ordering that must be
DoS-proof keys off a **local** receipt/first-observed clock, never a peer wall
stamp. Peer stamps remain FWW display metadata. No wire-format or
CRDT-convergence change in any task.

All five sites verified against `main@d1f84fa3` (recon 2026-08-02; ticket line
numbers had drifted and were re-located by content).

---

## C2 — voice-moderation slot freeze  (FAIL-OPEN → forward-skew reject)

**Defect (real):** `ActiveModeration::apply` (`voice_moderation.rs:377-399`)
resolves same-slot directives by `strictly_newer` (`:344-354`) = HLC
(`is_strictly_newer_than`, lexicographic over `(wall_ms, logical, device_id)`)
then `seq`. `issued_hlc` (`:100`) is an attacker-controlled field on the signed
directive. A far-future `issued_hlc.wall_ms` wins forever → every later
legitimate directive hits the `false // stale → ignore` arm → the
`(community,channel,target,class)` slot is frozen permanently. The ingest path
(`event_loop.rs:5665-5702` `open_directive` → `directive_signer_is_authorized`
→ `apply`) passes `now` only to compute the TTL `enforce_until_ms`; it is never
compared to `issued_hlc.wall_ms`. No forward-skew gate exists.

**Fix:** forward-skew **reject** at the directive ingest boundary, mirroring
ZEB-846 T-GOV (`community_membership.rs:1194`). Before `apply`, if the
directive's `issued_hlc.wall_ms` exceeds the receiver's wall clock by more than
`clock_trust::MAX_FORWARD_SKEW_MS`, drop the directive (do not apply, do not
advance the slot). Use `clock_trust::reject_future(issued_hlc.wall_ms, now_ms,
MAX_FORWARD_SKEW_MS)` where `now_ms` is the receiver wall clock — reuse the
ingest path's existing wall `now` if it is wall-epoch, else
`clock_trust::receiver_now_ms()`. Control tier (5 min), so honest skew still
applies; a re-issued directive after clock correction applies normally. Fail
**closed** on the skew.

**Not doing:** same-actor arrival-order LWW (a larger ordering-semantics change);
the forward-skew reject bounds the freeze, which is the exploit.

**Test:** a directive stamped `now + MAX_FORWARD_SKEW_MS + 1` is rejected and
does not freeze the slot; a later in-window directive still applies. A directive
exactly at `now + MAX_FORWARD_SKEW_MS` is accepted (inclusive ceiling, matching
`clock_trust` convention).

---

## D5 — raised-hand speaker-queue squat  (POISON-SQUAT → local first-observed)

**Defect (real):** the speaker queue orders by the peer-supplied `hand` wall
stamp (`Option<u64>`), copied verbatim from the beacon to `PresenceEntry.hand`
(`voice_presence.rs:252`, no clamp) and surfaced as `RosterEntry.hand_raised_at`
(`:279`). The oldest-first sort lives in the **frontend**
(`src/lib/voice-session.ts:48-58` `speakerQueue`): `a.handRaisedAt! -
b.handRaisedAt! || cmp(ownerHex) || cmp(deviceHex)`. `hand=1` (epoch) jumps
ahead of every honest raise forever. No usable local timestamp exists —
`PresenceEntry.last_seen_ms` is monotonic but overwritten every heartbeat;
`joined_hlc` is peer-supplied.

**Fix (decided: local first-observed-at ordering):** stamp the raise locally.
- `PresenceEntry`: add `hand_first_observed_ms: Option<u64>` (local monotonic).
  On apply, when `hand` transitions **None→Some** (or the entry is newly created
  with `hand=Some`), set it to the injected monotonic `now_ms`; when `hand`
  transitions **Some→None** (hand lowered), clear it. When `hand` stays `Some`
  across heartbeats, leave it unchanged (stable key).
- `RosterEntry`: add `hand_first_observed_ms` (JSON `handFirstObservedMs`);
  `roster()` copies it.
- `voice-session.ts` `speakerQueue`: filter on `handRaisedAt !== null` (a peer
  still asserts intent), but **sort** on `handFirstObservedMs` (falling back to
  `handRaisedAt` only if the local stamp is somehow absent), then the existing
  `ownerHex`/`deviceHex` tiebreaks. Peer wall becomes display-only.

Ordering is now DoS-proof (attacker cannot pre-date their raise) at the cost of
per-client order divergence, which is correct for an advisory, non-consensus
queue. Monotonic reset on restart is fine — the roster is ephemeral, rebuilt
from live beacons.

**Test (Rust):** None→Some stamps once; repeated Some heartbeats keep the stamp;
Some→None clears; a beacon carrying `hand=1` gets a first-observed stamp at
receipt, not epoch. **Test (TS):** `speakerQueue` orders by `handFirstObservedMs`
so an attacker's `handRaisedAt=1` does not jump the queue.

---

## D6 — presence `lastSeenMs` unit mismatch  (INERT dead code → honest removal)

**Defect (real, fails safe):** `NetworkHealthService::snapshot`
(`network_health.rs:2527-2540`) max-merges a presence cache value into
`record.last_seen_ms`, but the presence side (`cache.last_seen(...)`, fed by
`CommunityPresenceMap::apply` → `event_loop.rs:3946` `voice_now_ms =
start.elapsed()`) is **monotonic loop-relative ms**, while the record side is
**wall-epoch ms** (resolver announce clamped by `SystemTime::now()`; liveness
`since_ms` from `peer_liveness.rs:437`). A monotonic value (~1e7) can never
exceed a wall-epoch value (~1.7e12), so the presence contribution is **100%
inert**. The cache doc-comments (`network_health.rs:2035-2041, 2063-2064`)
falsely claim wall-clock. A masking test
(`last_seen_prefers_freshest_source`, `:5728-5772`) injects all three sources in
the same fake unit, hiding the mismatch.

**Fix (decided: drop the dead merge + fix docs):** remove the inert presence
contribution to `last_seen_ms` and any wiring that becomes unused as a result
(the merge is the cache's **sole** consumer — verified — so the
`PresenceLastSeenCache`, its `presence` field/setter, and the
`community_presence.rs` `note_seen` feed become dead once the merge is gone;
remove them under compiler/clippy guidance so nothing wired-but-inert lingers).
Correct/soften the doc-comments that claimed wall-clock. Drop the presence
assertion in the masking test. **Zero production behavior change** (production
already behaves as if the merge is absent).

**Out of scope (flag only):** the community-roster DTO
`PresenceMemberDto.last_seen_ms` also carries the monotonic value on the wire but
has no `Date.now()` math against it (TTL-consistent, monotonic sweep) — not
misused; untouched.

**Follow-up:** if the cross-WAN "heard-via-presence freshness" capability
ZEB-622 intended is later wanted, it is a *feature* (feed a real wall closure to
the presence side) — file as a ZEB-622 completion, not smuggled into this
cleanup.

**Test:** the masking test loses its presence assertion; add/adjust so
`last_seen_ms` reflects only resolver + liveness (both wall-epoch).

---

## E9 — pending-join nondeterministic sort  (DISPLAY → full tuple)

**Defect (real, display-only):** `filter_pending_joins` (`lib.rs:47861`) sorts on
`(pending_at_hlc.wall_ms, pending_at_hlc.logical)`, dropping `device_id` →
nondeterministic cross-replica render when two joins share `(wall_ms, logical)`.
The sole consumer (`PendingJoinsPanel.svelte`) keys `{#each}` by `eventId` and
acts by `joinerAddr` — no positional/index action, so this is a reproducibility
concern only, not a mis-target risk.

**Fix:** extend the key to the full `(wall_ms, logical, device_id)` tuple,
mirroring the existing ascending pattern at `lib.rs:44266`:
```rust
out.sort_by(|a, b| {
    a.pending_at_hlc.wall_ms.cmp(&b.pending_at_hlc.wall_ms)
        .then(a.pending_at_hlc.logical.cmp(&b.pending_at_hlc.logical))
        .then(a.pending_at_hlc.device_id.cmp(&b.pending_at_hlc.device_id))
});
```

**Test:** two pending joins sharing `(wall_ms, logical)` with different
`device_id` sort deterministically (device_id ascending).

---

## B7 — open-join pre-auth flood + global-budget lockout  (DoS → shield + per-source)

**Defect (both halves real):** the open-join acceptor
(`IrohInviteHandshakeAcceptor::handle_open_join_inbound` →
`open_join_admit::verify_and_admit_open_join`) checks its rate limiter at **step
7**, *after* two ed25519 operations (cert-chain verify step 5, sig verify step
6). The only prior gate is the `epoch_auth` MAC — but `epoch_key` **is** the
public open-invite-link capability, so anyone with the link + self-minted
throwaway Master identities + fresh nonces forces unbounded pre-consent ed25519
per connection. Separately, `OpenJoinRateLimiter` (`open_join_admit.rs:94-188`,
20/60s) is **keyless/global**: one source that passes crypto exhausts the shared
budget and sheds the 21st legitimate open-joiner (`RateLimited`) — the B1
lockout amplifier.

**Fix (decided: shield + per-source budget — defense-in-depth):**

1. **Tier-1 pre-auth shield (load-bearing).** Mirror the ZEB-700 friend/v1
   hardening (`iroh_friend_acceptor.rs:2110-2123`). Give
   `IrohInviteHandshakeAcceptor` a per-source connection limiter
   (`KeyedSlidingWindow<[u8;32]>`, or reuse `FriendRateLimiter`) and call
   `admit_connection(*conn.remote_id().as_bytes(), monotonic_now_ms())` in
   `handle_invite_handshake_inbound` **right after `accept_bi`**, before any
   stream read / `decode_packet` / crypto. Placing it there shields **both** the
   open-join (`0x11`) and the also-un-hardened invite (`0x10`) arms in one move.
   On shed: log + write the SAME benign reply the path already sends (no oracle)
   + return. Key on the un-spoofable transport `remote_id()`; use the monotonic
   clock (ZEB-711), never wall.

2. **Per-source coarse budget (defense-in-depth).** Re-key `OpenJoinRateLimiter`
   from a single global `count_in_window` to a per-source
   `KeyedSlidingWindow`-style counter (keyed on the connecting `remote_id`), so a
   single source can no longer exhaust the shared 20/60s budget for everyone.
   The nonce-replay half stays as-is (already per-request). Keep the pure
   `verify_and_admit_open_join` kernel's testability — thread the source key
   through its signature.

**Caps:** reuse the friend/v1 precedent shape
(`FRIEND_HANDSHAKE_PER_CONNECTION_MAX = 40`, 1h window) for Tier-1, or
open-join-appropriate values; keep the existing 20/60s semantics for the
per-source Tier-2 counter.

**Wiring:** acceptor struct field + constructor + one gate call
(`iroh_invite_acceptor.rs`), production build (`lib.rs:10253`), plus the
`OpenJoinRateLimiter` re-key in `open_join_admit.rs`.

**Risk:** LOW for Tier-1 (additive, mirrors two audited precedents:
`iroh_friend_acceptor`, `iroh_pex_acceptor`). MEDIUM-scoped for the per-source
re-key (touches the pure kernel + its tests) — keep behavior for the
single-source honest case identical.

**Tests:** (1) a flood of connections from one `remote_id` is shed by
`admit_connection` **before** decode/crypto (assert no verify work / benign
reply); a connection from a fresh `remote_id` still admitted. (2) the per-source
`OpenJoinRateLimiter` lets source A exhaust its own budget without shedding
source B (no cross-source lockout). (3) the Tier-1 shed reply is byte-identical
to the honest-pending reply (no oracle).

---

## Task decomposition (for the plan)

Independent, mostly one-file tasks — implement in this order (cheapest / most
isolated first, B7 last as the largest):

1. **E9** — pending-join full-tuple sort (`lib.rs`). One-liner + test.
2. **C2** — voice-mod forward-skew reject (`voice_moderation.rs` /
   `event_loop.rs` ingest) + test.
3. **D6** — remove dead presence→lastSeenMs merge + wiring + docs
   (`network_health.rs`, `community_presence.rs`, `event_loop.rs`/`lib.rs` boot)
   + test adjust.
4. **D5** — local first-observed hand-raise ordering (`voice_presence.rs` +
   `RosterEntry` DTO + `voice-session.ts`) + Rust & TS tests.
5. **B7** — Tier-1 pre-auth shield in `IrohInviteHandshakeAcceptor` + per-source
   `OpenJoinRateLimiter` re-key (`iroh_invite_acceptor.rs`, `open_join_admit.rs`,
   `lib.rs` wiring) + tests.

## Global constraints

- No wire-format or CRDT-convergence change. (D5 and B7 add/adjust local-only
  fields and a local limiter; the D5 `RosterEntry` DTO gains a new optional
  field — additive, backward-compatible; peer beacon shape unchanged.)
- Control-tier skew = `clock_trust::MAX_FORWARD_SKEW_MS` (5 min); do not point a
  control consumer at the 30-min display tier.
- Limiters use the monotonic clock (ZEB-711), never wall; shed replies must not
  be an oracle (byte-identical to the benign path).
- CI parity: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets
  --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked
  --workspace --all-targets --features test-fixtures --no-fail-fast`; plus the
  frontend test suite for the D5 TS change.
