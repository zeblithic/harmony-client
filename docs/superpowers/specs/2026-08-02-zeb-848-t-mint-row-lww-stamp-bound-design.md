# ZEB-848 T-MINT — bound peer `updated_at` in mint-row LWW (design)

**Ticket:** ZEB-848 (ZEB-831 wall-clock threat model §4 CRITICAL, C3).
**Branch:** `zeblith/zeb-848-t-mint-row-lww-bound` off `main@34464b31`.
**Status:** design approved 2026-08-02 (Option A; reject unparseable + forward-bound at ingest; cover the 3 upserts AND the deletion-floor).

## Problem (confirmed against source)

`apply_remote_snapshot` (`src-tauri/src/mint_sync.rs:89`) applies a remote
`MintSnapshot` from a peer device. It resolves per-row conflicts by a **raw
lexicographic `String` compare** on the peer-supplied `updated_at`:

- `upsert_account_lww` — LWW at `:189` (`r.updated_at > local`), plus a
  deletion-floor suppress compare at `:171` (`r.updated_at <= floor_ts`).
- `upsert_transaction_lww` — LWW at `:232`; a win overwrites
  `amount`, `account_id`, `deleted_at` (the ticket's high-value fields).
- `upsert_setting_lww` — SQL `WHERE excluded.updated_at > settings.updated_at`
  (`:261`), same lexicographic compare in SQLite `TEXT`.
- **Deletion-floor block** (`:124-150`) — for each
  `remote.account_deletion_floor` entry, `if local_ts <= remote_ts` **hard-deletes**
  the local account and its transactions, then merges the `remote_ts` into the
  persisted floor (which subsequently suppresses honest rows at `:171`).

Every one of these consumes an unbounded, unparsed peer string. All `updated_at`
fields are `String` (`mint_sync_types.rs:25-52`); there is **no per-row HLC** and
**no signature anywhere in mint sync** — rows travel in a fleet-symmetric
AEAD blob, so any fleet-key holder (a skewed or compromised owner/sibling device)
rewrites `updated_at` freely.

**Attack:** a sibling publishes a row stamped `"9999-12-31T23:59:59Z"` — or
`"zzzz"` (`z` = 0x7A sorts above every digit, so it beats *even* `9999`). It wins
LWW forever; every subsequent honest local edit stamps `now` (< poison) and is
**reverted on the next sync on every device**. Via the deletion-floor path a
poison `remote_ts` **hard-deletes** honest accounts and permanently suppresses
their re-creation. POISON-SQUAT / GRIEF-LOCKOUT / data integrity.

**Path is ingest-only.** `apply_remote_snapshot` runs solely from
`EngineShared::handle_incoming_decoded` (`:561`, inbound peer ingest). Local
edits bypass it entirely — they write directly in `mint.rs`, each stamping
`chrono::Utc::now().to_rfc3339()`. So an ingest-side bound can never reject the
user's own `now`-stamped row.

**ZEB-845 is a separate axis** — it gated only the envelope HLC
(`MintRootPublishPayload.at.wall_ms`); the per-row string LWW is untouched
(proven by the existing `poisoned_envelope_wall_does_not_affect_row_merge_lww`
test at `:2118`). No existing test feeds a poisoned *row* `updated_at` — that is
the coverage gap.

## Approach

**Option A — bound `updated_at` at ingest (chosen).** Parse each peer
`updated_at` as RFC-3339 and drop the remote row when the stamp is unparseable or
lands beyond `now + MAX_FORWARD_SKEW_MS`. No wire change; reuses the shared
`clock_trust` policy module.

**Option B — move row LWW onto a per-row HLC (rejected).** No per-row HLC exists;
this would add an HLC field to every row DTO = a flag-day wire-schema bump
(`MINT_SCHEMA_VERSION`/`LOCAL_MAX_SCHEMA_VERSION` = 1) with a migration for
persisted rows. High cost, no offsetting benefit for this fix.

### Policy

- **REJECT, not clamp.** The mint store is a *stored, replicated* LWW register
  (SQLite, re-synced to other devices). Clamp-and-store would persist a
  receiver-specific `min(stamp, now)` that then re-replicates — each device
  clamps to its own `now` → divergence. Rejecting drops the offending remote row
  and leaves honest state to re-propagate. (Same reasoning as T-DISCOVERY's
  D2-MERGE `merge_from` reject; the reject-never-clamp rule for stored replicated
  registers.)
- **Reject unparseable stamps.** `"zzzz"` is the sharpest vector *because* it is
  unparseable and sorts above every real stamp. An unparseable string is never a
  valid ordering key. Honest producers only emit `to_rfc3339()` (`+00:00`,
  variable fractional precision) or the fixed `Z` epoch literal — both parse via
  `chrono::DateTime::parse_from_rfc3339` — and pre-launch there is no legacy
  non-RFC-3339 corpus. Rejecting unparseable only ever drops a *remote* poison
  row, never a local edit.
- **Fail-open on an unreadable receiver clock.** `receiver_now_ms() == None` ⇒
  skip the forward-skew check (a bad *local* clock must never drop honest
  state — the documented `receiver_now_ms` contract). The unparseable check still
  applies (it needs no clock), so `"zzzz"` is dropped even with `None`.
- **Control tier.** Mint LWW is a control-plane decision (which data wins), so
  the bound is `MAX_FORWARD_SKEW_MS` (5 min), not the display tier.
- **Inclusive boundary.** `reject_future(stamp, now, tol) = stamp.saturating_sub(now) > tol`
  — a stamp exactly at `now + tol` is accepted (series-consistent).

### New shared helpers (`clock_trust`)

```rust
/// Parse an RFC-3339 timestamp to non-negative epoch milliseconds.
/// Accepts both `Z` and `+00:00` offsets and variable fractional precision
/// (the two honest encodings mint produces). Pre-epoch instants floor to 0
/// (ancient — never future). `None` if the string is not valid RFC-3339.
pub fn parse_rfc3339_ms(stamp: &str) -> Option<u64>;

/// Reject a peer-supplied RFC-3339 wall stamp used as a stored replicated LWW
/// ordering key. Returns true when the remote row MUST be dropped at ingest:
///   - the stamp does not parse as RFC-3339 (never a valid key; e.g. "zzzz"), or
///   - it parses to more than `tol_ms` beyond the receiver clock `now_ms`.
/// `now_ms == None` (receiver clock unreadable) ⇒ the forward-skew half is
/// skipped (fail-open on skew) but an unparseable stamp is still rejected.
pub fn reject_rfc3339_future(stamp: &str, now_ms: Option<u64>, tol_ms: u64) -> bool;
```

`clock_trust` currently is raw-`u64` only; `chrono` 0.4 is already a workspace
dependency. Placing the RFC-3339 helpers here keeps the policy DRY and reusable
(the series has many string-timestamp sites; cf. ZEB-855).

### Where the gate lives (all in `apply_remote_snapshot`)

Thread `now_ms: Option<u64>` from `handle_incoming_decoded`
(`clock_trust::receiver_now_ms()`, read once per snapshot) →
`apply_remote_snapshot` → the three upsert fns. At each consumer, drop the item
when `reject_rfc3339_future(stamp, now_ms, MAX_FORWARD_SKEW_MS)`:

1. `upsert_account_lww` — gate `r.updated_at` at the top (before the floor
   suppress at `:171` and the LWW at `:189`); return `Applied` (no-op, not
   `Suppressed` — a poison-rejected account must not cascade-skip its
   transactions).
2. `upsert_transaction_lww` — gate `r.updated_at` at the top (before `:232`).
3. `upsert_setting_lww` — gate `r.updated_at` before the SQL statement.
4. Deletion-floor loop (`:124-150`) — gate each `remote_ts` before the
   hard-delete (`:134`) and before merging into `floor_to_merge` (`:147`).

`#[cfg(test)]` tests call `apply_remote_snapshot` directly and pass an explicit
`now_ms` (no wall-clock flakiness).

## Tests (discrimination, revert-sensitive)

Each must fail with the gate neutralized:

- **Poison far-future loses** — `"9999-12-31T23:59:59Z"` remote vs `now`-stamped
  local → local survives (account, transaction, setting).
- **Poison unparseable loses** — `"zzzz"` remote vs local → local survives.
- **Legit-newer remote wins (control)** — remote at `now + 1s` (within skew) vs
  older local → applies. Proves the gate does not over-reject honest edits.
- **`None`-clock fail-open** — `now_ms = None`: `"9999-…"` remote applies
  (fail-open on skew) but `"zzzz"` remote is still dropped.
- **Deletion-floor poison** — a `remote.account_deletion_floor` entry stamped
  `"9999-…"` for a local account → the account is NOT hard-deleted and the poison
  is NOT merged into the floor; an honest recent floor entry still deletes.
- **Near-boundary tier** — `now + (MAX_FORWARD_SKEW_MS − ε)` accepted,
  `now + (MAX_FORWARD_SKEW_MS + ε)` rejected. Pins the control tier (a
  display-tier mutation would fail) and the inclusive edge.

## Non-goals

- **Not** moving the winner-compare off strings. The mixed `Z`/`+00:00`
  lexicographic fragility is pre-existing and only diverges at exact-instant ties
  (already "keep local"). Noted; out of scope.
- **No** per-row HLC (Option B).
- **Already-persisted** poison (a future stamp written before this fix) is healed
  by any local edit to that row (direct write bypasses LWW) but not purely by
  sync. Noted; not separately remediated.

## Files

- `src-tauri/src/clock_trust.rs` — add `parse_rfc3339_ms`,
  `reject_rfc3339_future`.
- `src-tauri/src/mint_sync.rs` — thread `now_ms`; gate the 3 upserts + the
  deletion-floor loop; source `now_ms` in `handle_incoming_decoded`.
- Tests in `mint_sync.rs`'s `#[cfg(test)] mod tests` (`:1284`) plus a
  `clock_trust` unit test for the two new helpers.
