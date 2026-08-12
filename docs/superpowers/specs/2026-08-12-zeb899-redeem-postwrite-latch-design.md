# ZEB-899: Latched-pending parity for iroh redeem post-write failures — design

**Ticket:** ZEB-899 — invite redeem reports `inviter_unreachable` while the join actually
completes asynchronously (false-negative onboarding UX).
**Branch:** `zeblith/zeb-899-harmony-client-invite-redeem-reports-inviter_unreachable`
**Premise audit:** posted on ZEB-899 (2026-08-12). Verdict: the false negative is real and
structural; the "background async completion" hypothesis was refuted — completion required a
retry or the LAN fallback.

## 1. Context — the confirmed mechanism

The iroh first-contact redeem (`connectivity_redeem_invite_iroh_inner`, `src-tauri/src/lib.rs`)
is a non-atomic request/response over one bi-stream, and the acceptor **commits before it
responds**:

* Joiner: pkarr resolve → dial (B4 retry) → witness ladder → `open_bi` → **mint (+ ZEB-889
  cache)** → build packet → **write request** → `finish()` → **read response**
  (`response_read_timeout`, default 30 s).
* Acceptor (`iroh_invite_acceptor.rs::handle_invite_handshake_inbound`): `handle_unicast`
  **inserts the PendingJoin into the community engine** → auto-countersign hook fires →
  polls its own CRDT (10 s production deadline) for the **JoinCountersign — already inserted
  into CommunityState** → writes the response → burns the invite only after a successful
  write (ZEB-874).

Every joiner-side failure **after the request write** — response-read timeout/error, response
CBOR-decode failure, chain-bounds failure, foreign `community_id`, countersign-target
mismatch — returns `post_dial_failure_outcome()` = `inviter_unreachable` (admin rung) or
`no_member_reachable` (witness rung), and **commits nothing locally**, while the host's CRDT
already (or imminently — the auto-countersign task is async and can land after the acceptor's
poll deadline) contains the joiner as a countersigned member. The UI then claims "Couldn't
reach the inviter… They may be offline" — factually wrong in this window.

The LAN/reticulum path already solved this shape in ZEB-501: an un-countersigned redeem
**commits a latched-pending join** (`pending == true`, `pending_join_at == Some`, greyed in
nav) and converges when the JoinCountersign arrives — pinned by
`redeem_invite_only_commits_pending_join_when_inviter_unreachable`. ZEB-902 Pt 1 gave the
dialog an honest rendering for it ("Join request sent — unlocks once the admin approves").
The iroh path lacks the latch; this design adds it.

## 2. Design

### 2.1 The post-write boundary

A failure branch is **post-write** iff both `write_all`s (length-prefix and body) returned
`Ok`. From that point the acceptor's `read_exact` of the full packet may have completed, so
host-side commit is possible and "the inviter was NOT reached" is unknowable-at-best.
`send.finish()` failure is post-write (it only signals end-of-stream locally; the acceptor's
`read_exact(len)` does not need EOS).

* **Latch (post-write):** `finish()` failure; response-read error/timeout; response
  CBOR-decode failure; chain length out of bounds; foreign `community_id` in chain;
  countersign-target mismatch; countersign `community_id` mismatch.
* **Keep today's outcomes (pre-write):** resolve failures, connect failures (both attempts),
  witness-ladder exhaustion, `open_bi` failure/timeout, either `write_all` failure/timeout.
  These still return `inviter_unreachable` / `no_member_reachable` /
  `relays_warming_up` exactly as today (pinned by the existing zeb911 fallback tests).

Decision on the garbage-response branches (audit open question 1): **latch**. A decoded-but-
wrong response proves the inviter was reached; a malformed one strongly implies it. The latch
commits only local pending state for the community the *user chose to redeem*, every synced
event is re-verified by `verify_event` at insert, and a pending community can be left — a
hostile responder gains nothing.

### 2.2 Single-funnel restructure

Steps 10–11 (read + decode + verify, currently seven `return Ok(post_dial_failure_outcome())`
sites) are restructured to produce

```rust
// None = post-write failure (each branch keeps its warn! + labelled conn.close()).
let delivered: Option<(Vec<SignedMembershipEvent>, SignedMembershipEvent)>
```

and fall through to the **single existing** `redeem_invite_inner_with_overrides` call:

```rust
let overrides = RedeemInviteOverrides {
    pre_minted: Some(minted),
    pre_delivered_countersign: delivered.as_ref().map(|(_, cs)| cs.clone()),
    pre_delivered_chain: delivered.map(|(chain, _)| chain).unwrap_or_default(),
    admin_identity_pub: Some(admin_id_pub),
    redeem_timeout: None,   // env-or-5s production default in BOTH modes
    open_join_iroh: None,
};
```

Latch mode (`delivered == None`) is exactly the inner's existing ZEB-501/ZEB-902 machinery:
it inserts the local PendingJoin, **deposits it via the DM outbox** (post-ZEB-474
DepositOnlyDmTransport) so the host can recover it even if the handshake request was written
but never delivered, awaits the countersign oneshot for the redeem window (a live Zenoh
session may complete the join in-band → `pending == false` immediately — the ZEB-908
already-connected case), and otherwise commits the latched-pending Space and spawns the
engine.

### 2.3 Outcome mapping (the tail)

* `delivered == Some` → **unchanged**: `Ok(dto)` → evict the ZEB-889 mint-cache entry, fence
  flush (ZEB-427), `emit_stage(Joined)`, `nav-updated` with `pending: Some(dto.pending)`,
  return `joined(dto.community_id, dto.pending)`; `Err` → `join_failed` (inviter reached,
  local failure — copy and fallback-suppression stay correct).
* `delivered == None`, `Ok(dto)` → fence flush, `emit_stage(Joined)`, `nav-updated` with
  `pending: Some(dto.pending)`, return `joined(dto.community_id, dto.pending)` — and **do
  NOT evict the mint cache**: no countersign was applied and the invite was not burned; the
  cached mint is what makes a later retry hit the host's AlreadyKnown-retransmit path
  instead of minting fresh and dying on the P6 already-engaged reject.
* `delivered == None`, `Err(e)` → `warn!` + return `post_dial_failure_outcome()` — the
  latch could not commit, so nothing landed locally and today's honest unreachable outcome
  (with the LAN-fallback affordance) is the right degrade. NOT `join_failed`: that status
  suppresses the LAN fallback and asserts the inviter was reached, neither of which holds
  here.

`RedemptionOutcome::joined(...)` + `pending` is already rendered honestly by
`RedeemInviteDialog.svelte` (ZEB-902 Pt 1): **no frontend change**.

### 2.4 Retry-after-latch semantics (audit open question 2)

A retry after a latch re-runs the whole redeem:

* The ZEB-436 orphan short-circuit does not trigger (it requires the persisted dir to
  materialize self as **Joined**; a latched-pending self is pending).
* The mint cache (whole-payload digest key, preserved by §2.3) returns the same
  `bootstrap_join.id` → the host's `handle_unicast` hits AlreadyKnown → the countersign
  (already in the host CRDT) is returned in-band → the inner runs with
  `pre_delivered_countersign` over the **existing** engine/Space: PendingJoin insert is
  AlreadyKnown, the countersign inserts, the post-Inserted hook resolves the oneshot →
  `dto.pending == false`, cache evicted, full join. Pinned by T2.
* A retry after the mint-cache TTL mints fresh and is P6-rejected host-side — but unlike
  today, the latched Space + deposit keep converging, so the user is not stranded.

### 2.5 Convergence (audit open question 3)

The latch relies on the shipped ZEB-254/501/902 convergence: the deposited/synced PendingJoin
reaches the host (deposit recovery or Zenoh CRDT sync), the host's auto-countersign hook
fires (or already fired), and the countersign syncs back → the Space ungreys to full member.
The acceptor's oversize-chain degrade already relies on this exact path in production
("joiner degrades to pending + Zenoh"). Cross-WAN pending-join convergence *latency* is
ZEB-903's existing scope; this change aligns the iroh path with the LAN path's behavior and
adds no new exposure.

## 3. What does not change

* Pre-write failure outcomes and their typed errors (`relays_warming_up`, etc.).
* The witness-ladder decision table (`witness_ladder_fallback_outcome`) and its tests.
* The acceptor (`iroh_invite_acceptor.rs`) — commit-before-respond ordering, ZEB-874
  burn-after-delivery, AlreadyKnown-retransmit.
* The frontend dialog, RPC/IPC signatures, wire formats, `RedemptionOutcome`'s status union.
* The `RedemptionOutcome::unreachable()` doc comment IS updated: "The inviter was NOT
  reached" now holds again, because post-write failures no longer map to it.

## 4. Testing

Integration tests live beside the existing two-endpoint harness in
`src-tauri/tests/pkarr_net/pkarr_invite_redemption_integration.rs` (the
`pkarr_iroh_redeem_full_integration` fixture), which already drives a real acceptor +
real iroh endpoints:

* **T1 — post-write latch:** joiner runs with `HandshakeDialConfig { response_read_timeout:
  ~50ms, .. }` against an acceptor whose countersign poll is slow (long `poll_interval`), so
  the request is written and committed host-side but the response misses the window. Assert:
  outcome `joined` + `pending == true`; joiner Space row exists with
  `pending_join_at == Some`; joiner engine registered; **mint-cache entry still present**;
  host CommunityState contains the PendingJoin (and, after its poll, the countersign).
* **T2 — retry-after-latch completes:** after T1's latch, run the same redeem again with
  production timeouts. Assert: outcome `joined` + `pending == false`; same
  `bootstrap_join.id` (cached mint); mint-cache entry evicted; Space row no longer pending.
* **T3 — latch-mode local failure degrades to today's outcome:** force the latch call to
  fail (fence_check returns `Err` — flip a shared flag after the write completes, before the
  inner runs, e.g. from the progress-sink's `AwaitingCountersig` stage callback). Assert:
  outcome status is `inviter_unreachable` (not `join_failed`, not `joined`), and no Space row
  was committed.
* **Existing pins unaffected:** zeb911 fallback-outcome tests (pre-write classification),
  `redeem_invite_only_commits_pending_join_when_inviter_unreachable` (LAN latch),
  `zeb911_no_records_at_all_stays_inviter_unreachable`, full-integration positive path.

Gates: fmt, clippy `--all-targets -D warnings`, scoped nextest per task, full
`--workspace --all-targets --features test-fixtures` sweep before PR.

## 5. Out of scope

* Frontend changes (ZEB-902 rendering already covers the new outcomes).
* Acceptor-side changes; response-delivery hardening.
* Cross-WAN pending-convergence latency (ZEB-903).
* Distinct status string for "request delivered, confirmation pending" — the honest
  `joined`+`pending` state supersedes the need for it.
* The open-join (`connectivity_open_join_iroh`) path — different protocol (typed
  `OpenJoinResponse`, `searching` retry state already exists there).
