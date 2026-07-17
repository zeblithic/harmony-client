# ZEB-705: bounded fetch retry for fleet-sync inbound publishes — plan

**Ticket:** ZEB-705 (High). Live D3 (2026-07-17, `main@999c5f3d`) proved the ZEB-702
republish mechanisms work but showed the receiver drops an incoming state-root
publish permanently when the content-blob fetch behind it fails — observed as a
zenoh queryable-declaration race on a ~1s-old link, fatal when the publisher
(the only blob holder) departs seconds later.

## Design

### Component 1 — retry re-injection channel (fleet_sync)

`handle_incoming_publish` step 5 failures (`content_store.get` → `Err(_)` or
`Ok(None)`) become a new `Inbound::FetchMiss` outcome carrying the original wire
bytes. The engine loop schedules a bounded retry: spawn `sleep(FETCH_RETRY_DELAY_MS)`
then re-send `(wire, attempts_left - 1)` on a new internal mpsc retry channel the
loop also selects on. NOT an inline sleep: the inbound arm shares the `select!`
loop with `flush_now` (whose oneshot callers include the shutdown flush fence),
so the loop must stay live during the backoff.

- `FETCH_RETRY_ATTEMPTS = 3`, `FETCH_RETRY_DELAY_MS = 2000` (declaration
  propagation is ~1s; the D3 window this must win is "publisher alive", measured
  in seconds-to-minutes).
- A retried wire re-enters the FULL pipeline (decrypt → replay check → fetch →
  merge). If a newer root applied meanwhile, the replay check kills the stale
  retry as `Duplicate` — supersession for free. Tracker stays un-advanced on
  every failure (existing apply-before-advance invariant), so retries are
  idempotent.
- Deterministic failures (decrypt/decode) do NOT retry — only fetch-class.
- Retry channel bounded (cap 8); on full, drop with WARN (backstop, should never
  happen at fleet scale).
- Covers 11 of the 12 owner datasets (owner-state — the D3 roster — is
  `FleetSyncEngine<OwnerState>` internally). `mint_sync` has a hand-rolled twin
  handler: mirrored here only if the diff stays small; otherwise noted for the
  ZEB-705 follow-up ticket.

### Component 2 — D3 harness hardening (scripts/gce-xwan/run-tests.sh)

1. **Roster-converged barrier** between pin-B2 and SIGKILL-P: poll B2's friend
   list until A's owner appears (own timeout + label). Separates "roster
   converges while P is alive" (the ZEB-702 promise) from "deposit HELD"
   (transport+auth) — today HELD requires winning two races in a ~3-5s window.
2. **Snapshot on HELD timeout**: capture B2's `network_health_snapshot`
   (`butlerDeposits` section) into the artifacts dir for counter-first triage.
3. Runbook: measured-results row for the 2026-07-17 session; D3 section status
   updated (ZEB-702 landed → residual = ZEB-705).

## Testing

- Paused-time engine tests via a `FlakyStore` ContentStore wrapper (fail first N
  gets): retry-then-apply; exhaustion leaves tracker un-advanced (same wire
  re-sent later still applies); newer root supersedes a pending retry
  (Duplicate); decode failures schedule no retry (cfg(test) retry counter).
- Gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets
  --features test-fixtures --no-deps -- -D warnings`; `scripts/test-select
  --context task`; `shellcheck scripts/gce-xwan/run-tests.sh`; final full sweep
  `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
  pre-PR.
- **Live validation (post-merge): D3 re-run** — the standing gate; expected
  green with the barrier making the window deterministic.

## Out of scope

- Query-side fetch driver for owner datasets (ZEB-705 finding 2) — follow-up.
- In-session post-enrollment engine spawn (finding 3) — follow-up.
- community_state_sync / channel backfill paths (have query-driver recovery).
