# ZEB-855 — Uniform forward-skew reject tracing (design)

**Ticket:** ZEB-855 — *Observability: forward-skew reject sites are silent — add uniform tracing across all `clock_trust` reject boundaries*
**Branch:** `zeb-855-forward-skew-reject-tracing` (off `main` @ `7e311d7a`)
**Lineage:** ZEB-831 wall-clock threat model → ZEB-846 (T-GOV) / ZEB-847 (T-OWNER) reject-at-ingest guards → this follow-up.
**Kind:** Observability only. No correctness change, no skew-policy change, no behavior change on any merge/ingest outcome.

## Problem

Every forward-skew guard in the tree **rejects silently**. When a remote entry's wall stamp exceeds the receiver-relative forward-skew ceiling and the merge/ingest boundary drops it, nothing is logged. A silent drop is operationally indistinguishable between two very different situations:

1. **Working as designed** — an honest peer whose clock is skewed (e.g. AVALON's ~1 s/day drift, ZEB-788) has a stamp legitimately rejected. Benign, self-healing next sync round.
2. **A bug** — a guard is over-rejecting and dropping honest present-day state.

With no log line the only symptom of either is "state that should have merged didn't," discovered downstream or never. A single low-noise `tracing::debug` at each reject turns an invisible drop into a diagnosable, greppable event, and lets us confirm the guards actually fire in the field rather than inferring it from absence.

## Premise, as verified against `main` @ `7e311d7a`

No forward-skew **ingest/merge** reject site emits a `tracing` event today — **with one exception**: `owner_trust_sync.rs:126` (ZEB-854, added after this ticket was filed) already emits `tracing::warn!` with a raw `device` hex attribution. That site is **out of scope** here: it is already observable, and its richer warn-level operator attribution is a deliberate ZEB-854 choice, not a gap. Harmonizing it down to the uniform `debug` baseline would *lose* signal. It is documented as the intentional exception below.

## Design

### Shared emission point

A single private emit in `clock_trust.rs` is the one place the event format lives:

```rust
fn emit_forward_skew_reject(field: &str, skew_ms: u64, receiver_now_ms: u64, tolerance_ms: u64) {
    let tier = if tolerance_ms <= MAX_FORWARD_SKEW_MS { "control" } else { "display" };
    tracing::debug!(
        target: "clock_trust::forward_skew",
        field,
        skew_ms,
        receiver_now_ms,
        tolerance_ms,
        tier,
        "forward-skew reject: peer stamp beyond receiver clock tolerance",
    );
}
```

- **`debug`, not `warn`** — a skewed peer is expected, not exceptional (ticket requirement).
- **No raw peer identity** — `field` is a static `<subsystem>.<register>.<stamp_field>` discriminator (e.g. `"owner_state.space.updated_at"`), never a device/owner id. This keeps the privacy surface unchanged.
- **`tier` + `tolerance_ms`** — `tolerance_ms` is the actual budget that was exceeded; `skew_ms` vs `tolerance_ms` tells an operator exactly how far past budget the stamp was. `tier` (`"control"` / `"display"`) is derived from the budget so the events are greppable by tier without an operator memorising the numeric constants. No mapping table — the budget *is* the tier.
- All magnitudes are reported in **milliseconds**, uniformly, regardless of each site's native unit.

### Logged predicate wrappers

Each wrapper is a sibling of an existing `clock_trust` predicate. It calls the plain predicate for the decision (single source of truth for the skew policy), and when — and only when — that predicate says "reject", it computes the ms-normalized skew and emits. This co-location makes it structurally impossible for the log to drift from the reject decision.

```rust
/// Control-tier (MAX_FORWARD_SKEW_MS), ms stamp, `Option` receiver-now.
pub fn wall_exceeds_forward_skew_logged(
    wall_ms: u64, receiver_now_ms: Option<u64>, field: &str,
) -> bool {
    match receiver_now_ms {
        Some(now) if reject_future(wall_ms, now, MAX_FORWARD_SKEW_MS) => {
            emit_forward_skew_reject(field, wall_ms.saturating_sub(now), now, MAX_FORWARD_SKEW_MS);
            true
        }
        _ => false,
    }
}

/// Unit: ms stamp, ms now, explicit tolerance. For the raw `reject_future` sites.
pub fn reject_future_logged(stamp_ms: u64, now_ms: u64, tolerance_ms: u64, field: &str) -> bool {
    if reject_future(stamp_ms, now_ms, tolerance_ms) {
        emit_forward_skew_reject(field, stamp_ms.saturating_sub(now_ms), now_ms, tolerance_ms);
        true
    } else {
        false
    }
}

/// ms stamp, epoch-**seconds** receiver-now, explicit tolerance_ms.
pub fn wall_exceeds_forward_skew_secs_logged(
    wall_ms: u64, now_secs: u64, tolerance_ms: u64, field: &str,
) -> bool {
    if wall_exceeds_forward_skew_secs(wall_ms, now_secs, tolerance_ms) {
        let now_ms = now_secs.saturating_mul(1000).saturating_add(999);
        emit_forward_skew_reject(field, wall_ms.saturating_sub(now_ms), now_ms, tolerance_ms);
        true
    } else {
        false
    }
}
```

The 4th predicate, `secs_exceeds_forward_skew`, gets **no** logged variant — its only caller (`owner_trust_sync.rs:126`) is the documented already-logged exception.

The single seconds-domain `reject_future` caller (`vine_feed_cache.rs:729`, `created_at` in seconds) routes through `reject_future_logged` by rescaling its args to ms at the call site: `created_at*1000`, `now_secs*1000`, `DISPLAY_SKEW_TOLERANCE_MS`. This is an **exact behaviour-preserving** linear rescale (`a − b > T ⟺ 1000a − 1000b > 1000T`), so the reject decision is byte-identical while the emitted magnitude is ms like every other site.

### Call-site change

Purely mechanical, one line per site — swap the predicate for its logged sibling and pass the discriminator:

```rust
// before
if crate::clock_trust::wall_exceeds_forward_skew(space.updated_at.wall_ms, receiver_now) { continue; }
// after
if crate::clock_trust::wall_exceeds_forward_skew_logged(
    space.updated_at.wall_ms, receiver_now, "owner_state.space.updated_at",
) { continue; }
```

OR-combined guards keep their `||` structure; each term takes its own discriminator, and Rust's short-circuit means the first-exceeded bound is the one that logs (one event per rejected entry):

```rust
if wall_exceeds_forward_skew_logged(g.granted_at, receiver_now, "owner_state.grant.granted_at")
    || wall_exceeds_forward_skew_logged(g.revoked_at, receiver_now, "owner_state.grant.revoked_at")
{ continue; }
```

Sites that compute their own receiver-now inline (`community_voting_log_engine`, `open_join_admit`) keep their existing `now`/`!= 0` guard unchanged and only swap the inner predicate — no refactor of the now-computation (that would exceed the observability-only scope).

## In-scope reject sites (~22 blocks / 24 predicate calls)

### `wall_exceeds_forward_skew_logged` — control tier, ms

| Site | Field (discriminator) |
|---|---|
| `owner_state_sync.rs:316` | `owner_state.space.updated_at` |
| `owner_state_sync.rs:329` | `owner_state.read_marker.last_read_at` |
| `owner_state_sync.rs:357` | `owner_state.owner_device.learned_at` |
| `owner_state_sync.rs:380` | `owner_state.library.added_at` |
| `owner_state_sync.rs:384` (OR term) | `owner_state.library.removed_at` |
| `owner_state_sync.rs:424` | `owner_state.friend.learned_at` |
| `owner_state_sync.rs:520` | `owner_state.grant.granted_at` |
| `owner_state_sync.rs:521` (OR term) | `owner_state.grant.revoked_at` |
| `owner_state_sync.rs:578` | `owner_state.dismissed_grant.dismissed_at` |
| `owner_state_sync.rs:610` | `owner_state.received_grant.received_at` |
| `notes_crdt.rs:99` | `notes.note.updated_at` |
| `fleet_net.rs:163` | `fleet_net.device.seen_at` |
| `fleet_net.rs:219` | `fleet_net.petname.set_at` |
| `voice_moderation.rs:444` | `voice_moderation.directive.issued_hlc` |

### `reject_future_logged` — ms

| Site | Tier | Field |
|---|---|---|
| `community_membership.rs:4072` | control | `community_membership.event.at` |
| `community_channel_log.rs:1413` | control | `channel_log.event.at` |
| `community_invite.rs:1871` | control | `community_invite.join_event.at` |
| `open_join_admit.rs:396` | control | `open_join_admit.join_event.at` |
| `community_voting_log_engine.rs:2924` (inbound) | control | `voting_log.inbound.event.hlc` |
| `community_voting_log_engine.rs:3082` (backfill) | control | `voting_log.backfill.event.hlc` |
| `library_directory.rs:481` | display | `library_directory.announce.listed_at` |
| `vine_feed_cache.rs:729` (secs→ms at call) | display | `vine_feed.descriptor.created_at` |

### `wall_exceeds_forward_skew_secs_logged` — ms stamp, secs now

| Site | Tier | Field |
|---|---|---|
| `profile_broadcast.rs:596` | display | `profile_broadcast.shared_at` |
| `profile_card_broadcast.rs:225` | control | `profile_card.shared_at` |

## Deliberately excluded (and why)

| Site | Why excluded |
|---|---|
| `owner_trust_sync.rs:126` | Already emits `tracing::warn!` + `device` attribution (ZEB-854). Already observable; deliberately richer. Left as the one intentional warn exception. |
| `community_state_crdt.rs:634` | Predicate reused inside a `.filter().min()` to schedule a **recompute** — nothing is dropped at this call. Logging a "reject" here would be false. |
| `persistent_card_store.rs:253`, `:274` | Read-time **view gates** (ZEB-849 "gate the view, not the store"). Fire on *every read* of a skewed card → logging would be per-read noise, and these are not ingest boundaries. |
| `fleet_net.rs:198`, `vine_feed_cache.rs:813`, `community_membership.rs:2336` | Negated **accept**-guards (`!predicate`) — no reject branch to log. |
| test / doc-comment references | Not reject sites. |

## Testing

The correctness contract is **behaviour parity**, not log capture. (The repo has no `tracing-test` harness and this design adds no test dependency for one; an assertion that a `tracing::debug!` fired would require a capturing subscriber that doesn't exist.)

- **Parity tests** (in `clock_trust`'s unit module): for each `*_logged` wrapper, assert it returns an **identical bool** to its plain predicate across a matrix — present, past, exactly-at-boundary (inclusive), far-future, `None`/`0`-sentinel receiver-now. This pins the load-bearing property: *observability must never change a reject decision.*
- **Seconds→ms normalization test**: one focused test on `wall_exceeds_forward_skew_secs_logged`'s reported magnitude path (the `now_secs*1000 + 999` compensation), via the same pure arithmetic the emit uses.
- The full CI gate (fmt, clippy `--all-targets`, nextest `--workspace --all-targets`, tsc) backstops the ~22 mechanical call-site swaps — a wrong discriminator or a broken swap surfaces as a compile error, and the existing reject-behaviour tests at each subsystem continue to pass unchanged (proving no decision drift).

## Non-goals

- No change to any reject **decision**, skew constant, or policy (ZEB-831/846/847 unchanged).
- No counter / metric sink — the repo has no metrics facade; the greppable `debug` event satisfies the acceptance bar. (Explicitly declining the ticket's stretch goal as YAGNI.)
- No refactor of sites' existing receiver-now computation.
- No harmonization of the `owner_trust_sync` warn site.

## Files touched

- `src/clock_trust.rs` — 3 logged wrappers + 1 private emit + parity/normalization unit tests.
- 12 call-site files (one-line swaps each): `owner_state_sync.rs`, `notes_crdt.rs`, `fleet_net.rs`, `voice_moderation.rs`, `community_membership.rs`, `community_channel_log.rs`, `community_invite.rs`, `open_join_admit.rs`, `community_voting_log_engine.rs`, `library_directory.rs`, `vine_feed_cache.rs`, `profile_broadcast.rs`, `profile_card_broadcast.rs`.

No frontend change.
