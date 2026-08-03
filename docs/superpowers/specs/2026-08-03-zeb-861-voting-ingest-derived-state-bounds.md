# ZEB-861 — Divergence-safe derived-state bounds for Tier-3 voting ingest

**Ticket:** ZEB-861 (Medium) — "Tier-3 receive-watermark lane map is keyed by unbounded, un-enrolled member-controlled `device_id`."
**Status:** design. Branch `zeblith/zeb-861-voting-ingest-volume-bound` off `main@49ef346c`.
**Relates to:** ZEB-860 (canonical-order materialization — just merged, the enabler), ZEB-868 (md/ss Ratification rebuild + verify-cache — the `md` materialization cap moves *here*; see §7), ZEB-850 (per-lane watermark), ZEB-846 (forward-skew reject), ZEB-585/ZEB-399 (channel-log precedent).

---

## 1. Problem

Tier-3 community-voting ingest verifies a signed voting event against the actor's **owner-level** identity key (`verify_voting_event`, `community_voting_core.rs:1362`). It never binds or bounds `hlc.device_id`, which is a free-form signed `String` inside the HLC. A single malicious member can therefore sign unbounded validly-signed events, each with a fresh `device_id`, and each event:

- mints a persistent `(actor, device_id)` lane in `Tier3PollState.last_received_hlc` (`community_voting_tier3.rs:221`) — the lane-mint tail runs for **every** dispatch, accept or silent-drop; and
- is appended to the poll's `events: Vec<SignedVotingEvent>` even when an apply rule silently drops it (`community_voting_log.rs:519-520`).

There is no per-member anti-spam bound anywhere in the ingest path.

### 1.1 What the premise recon corrected

Two load-bearing assumptions in the ticket text are wrong, and they reshape the fix (verified read-only against current code; recon retained at `scratchpad/zeb861-recon.md`):

1. **"Mirror channel-log's enrolled-device binding" does not bound anything.** Channel-log's watermark lane key is *also* the free-form `device_id` string, and its signature check accepts *any* of a member's ≤32 enrolled keys — it does **not** cryptographically bind `device_id` to a device. What actually bounds channel-log is a pair of **numeric** caps on its *sealed/wire* watermark vector (`MAX_WATERMARK_VECTOR_ENTRIES=4096`, `MAX_WATERMARK_VECTOR_BYTES=64KiB`), not a device binding. True binding would require switching voting to device-key signing — a large cross-cutting change that still would not bound event volume. **Direction 1 (enrolled-device binding) is rejected.**

2. **The lane map is not the dominant growth vector.** `last_received_hlc` is *never serialized* (a pure in-memory replay artifact; `community_voting_tier3.rs:219-220`), and it holds at most one entry per event. Its size is dominated by the `events` log. The ticket itself concedes this ("the map is not the dominant growth vector; the honest framing is 'voting has no per-member anti-spam bound on event volume'").

### 1.2 Scope decision (agreed)

**Bound the derived state, divergence-safely, at the voting layer.** Raw append-only `events`-log *storage* stays a documented residual (bounded by poll finalization/archival + the sparse-volume assumption), because a divergence-safe storage bound cannot be a naive ingest reject — canonical rank is unknown at append time, so an arrival-order reject is receiver-dependent → divergence (the ZEB-847 trap the ticket flags). See §6.

---

## 2. Invariant / design principle

Every bound in this fix is either (a) a **uniform decode-time predicate on the event itself** (identical admission on all replicas), or (b) a **pure function of the materialized state accumulated in canonical HLC order** (identical projection on all replicas, because `apply_event` is re-run in canonical order by `rebuild_from_events` and boot-restore — the ZEB-860 property). No bound depends on receiver arrival order in a way that survives a rebuild/restore. This is the same divergence-safety class the already-shipped `ds` 5-statement cap relies on.

The fix has three components plus a documented residual.

---

## 3. Component 1 — `device_id` length cap (decode-time admission)

**What:** reject any inbound voting event whose `hlc.device_id` byte-length exceeds `MAX_DEVICE_ID_LEN` at the two decode routes, before dedup/verify.

**Constant:** `MAX_DEVICE_ID_LEN: usize = 64`.
Justification: the canonical legit `device_id` is `hex::encode` of a 16-byte (128-bit) identity hash = **32 chars** (`owner_state.rs:770-773`, `owner_commands.rs:422-423`); engine-auto lanes are shorter (15 and 23 chars — `community_voting_log_engine.rs:871-877`, `3300-3304`). 64 = 2× the legit max, and also admits a hypothetical future 256-bit-hash migration (→ 64 hex) without a second bump. Any value ≥ 32 is memory-safe; 64 is a comfortable margin that still rejects the multi-KB `device_id` an attacker would use for decode-time memory amplification / lane-key bloat.

**Home:** a module const in `community_voting_log_engine.rs` (beside the ingest paths; near `clock_trust::MAX_FORWARD_SKEW_MS` in spirit).

**Where (both routes, immediately after `ciborium::from_reader`, before the skew block):**

- `process_inbound` — `community_voting_log_engine.rs:2890-2891`.
- `apply_backfilled_event` — `community_voting_log_engine.rs:3035-3036`.

Both return `Result<_, String>`. The reject:

```rust
if event.hlc.device_id.len() > MAX_DEVICE_ID_LEN {
    return Err(format!(
        "voting event device_id length {} exceeds MAX_DEVICE_ID_LEN {}",
        event.hlc.device_id.len(),
        MAX_DEVICE_ID_LEN
    ));
}
```

Use `.len()` (byte length) — the DoS bound is bytes, and legit ids are ASCII hex so bytes == chars.

**Divergence-safety:** a uniform predicate on the event, applied by every node at ingest before the event enters the canonical log — an over-length event is rejected identically everywhere. Mirrors the ZEB-846 forward-skew reject, which is likewise ingest-only (forward-looking admission control). It is **not** re-applied inside `apply_event`/`rebuild_from_events` — moot because no legit event exceeds 32 chars, and any hypothetical pre-existing persisted over-length event is a pre-fix log artifact, not a new divergence.

---

## 4. Component 2′ — make `max_received_hlc()` O(1) (remove the lane map's only super-linear cost)

**Rationale for replacing the ticket's lane-*count* cap.** The distinct-device lane-count cap the ticket sketches is not worth its cost:

- The "advance on every dispatch (accept OR drop)" behavior of `last_received_hlc` is a **deliberate, tested ZEB-850 anti-replay invariant** (`community_voting_tier3.rs:217`; test *"last_received_hlc must advance on every dispatch"*, `:5561`). It is the opposite of `last_hlc`, which must NOT advance on drop (ZEB-320, `:212`). So lane-minting **cannot** be gated on acceptance.
- A canonical-first-N device cap is therefore the only count-capping option, and it forces broadening the ZEB-860 rebuild trigger (fighting ZEB-868's tighten-intent) and reintroduces a transient divergence — disproportionate for bounding a structure (`last_received_hlc`) that is smaller than the events-log residual we already accept.

The lane map's only *independent* cost is a single **O(lanes) fold** on the ingest hot path:

```rust
// community_voting_tier3.rs:1168-1178 (current)
pub fn max_received_hlc(&self) -> Option<Hlc> {
    self.last_received_hlc.values().copied().max()
        .map(|(wall_ms, logical)| Hlc { wall_ms, logical, device_id: String::new() })
}
```

Called on the kd=rs mint-floor path (`community_voting_log_engine.rs:1288`, `:1768`) to get a floor strictly above every received event.

**Key fact:** this fold's `(wall_ms, logical)` output is **provably identical** to the `(wall_ms, logical)` prefix of the already-maintained `max_applied` watermark (`community_voting_tier3.rs:226`). Both advance on *every* dispatch at the same tail (`:1143` and `:1149`). `max_applied = max` over all dispatched `(wall_ms, logical, device_id)`; its `(wall_ms, logical)` prefix = the global max `(wall_ms, logical)` (key3 orders by `(wall_ms, logical)` first). The fold's result = `max` over per-lane maxima = the same global max `(wall_ms, logical)`. The returned `device_id` is documented-unused (the consumer synthesizes its own; `:1164-1167`). Empty on both sides.

**Change:**

```rust
pub fn max_received_hlc(&self) -> Option<Hlc> {
    self.max_applied.as_ref().map(|(wall_ms, logical, _device)| Hlc {
        wall_ms: *wall_ms,
        logical: *logical,
        device_id: String::new(),
    })
}
```

**Effect:** O(1). After this, **no consumer scans all lanes** — the per-lane monotonic guard (`:542`) and the tally-at read (`:4138`) are point lookups. The lane map's *count* then carries no per-operation cost; its memory joins the events-log residual (§6).

**Divergence-safety:** zero behavioral change — provably-equal output, a pure read, no admission/materialization effect. A test pins the equivalence against the old fold (§8) so the two watermarks can't silently drift in a future edit.

---

## 5. Component 3 — per-actor materialization caps (bound the tally-relevant projection)

`apply_event`'s kind dispatch has three arms that do an **unbounded per-actor push** with no cap (audit of every arm in `scratchpad/zeb861-spec-facts.md §B`; `ds` is already capped at 5, `dv`/`ts`/`da`/`ss`/`cl`/`rs`/`sf` are structurally bounded/scalar/idempotent):

| kind | field pushed | line | cap constant | limit |
|---|---|---|---|---|
| `md` MiniPublicDecline | `declines: Vec<(OwnerAddr, Hlc)>` (`:198`) | `:574` | `MAX_DECLINES_PER_ACTOR` | **2** |
| `dc` DraftCandidate | `candidates: Vec<DraftCandidateState>` (`:200`) | `:763` | `MAX_DRAFT_CANDIDATES_PER_ACTOR` | **5** |
| `rb` RatificationBallot | `ratification_ballots: Vec<RatificationBallotPayload>` (`:202`) | `:912`/`:915` | `MAX_RATIFICATION_BALLOTS_PER_ACTOR` | **2** |

`dc` and `rb` are **new findings** beyond the ticket's known `md` — the enumeration Jake pre-authorized. `MAX_RATIFICATION_CANDIDATES=5` (`:1737`) is only a read-time projection cap in `drafting_advancers` (`:2095-2098`); it does **not** bound the stored `candidates` Vec.

**Limits rationale (deliberately generous — bound, don't rate-limit):** a member legitimately declines once (`md`→2 tolerates an honest resubmit), casts one ratification ballot (`rb`→2 tolerates an LWW resubmit), and `dc`→5 matches the `ds` cap and the fact that ≤4 non-status-quo candidates can advance anyway. Each limit is ≥ any legitimate per-actor volume, so no honest flow breaks; the goal is converting "unbounded" to "O(members × constant)".

**Mechanism (mirror the shipped `ds` 5-cap exactly, `:609-650`):** add a sibling per-actor counter field for each capped kind and gate the push on it. Before each push, read the counter; if `>= LIMIT`, drop (`advance_last_hlc = false`) without pushing; else push and increment. Use explicit counter fields (not a Vec filter-count) because **`rb` cannot be derived** — `RatificationBallotPayload` carries no actor tag (`:2029`), so a stored `ratification_ballots` entry has no recoverable actor. For uniformity all three use the same pattern as `ds`'s `statements_per_author`:

- `declines_per_actor: BTreeMap<OwnerAddr, u8>` (sibling of `declines`)
- `candidates_per_actor: BTreeMap<OwnerAddr, u8>` (sibling of `candidates`)
- `ballots_per_actor: BTreeMap<OwnerAddr, u8>` (sibling of `ratification_ballots`)

```rust
// md arm (community_voting_tier3.rs:570-575), illustrative:
let prior = self.declines_per_actor.get(&ev.actor).copied().unwrap_or(0);
if prior >= MAX_DECLINES_PER_ACTOR {
    advance_last_hlc = false;
    tracing::debug!(actor = %ev.actor, "kd=md drop: per-actor decline cap reached");
} else {
    self.declines.push((ev.actor, ev.hlc.clone()));
    *self.declines_per_actor.entry(ev.actor).or_insert(0) += 1;
}
```

`dc` and `rb` follow identically against `candidates_per_actor` / `ballots_per_actor`, keyed on `ev.actor` (in scope at the apply site). Increment **only when the push actually happens** (as `ds` increments only on a real insert), so the counter equals the materialized count.

These three counter fields are direct fields of `Tier3PollState` (siblings of the Vecs they guard) and **must be reset to empty in `new_from_create`** (`:451-473`) — exactly as `deliberation`/`declines`/`candidates`/`ratification_ballots` already are — so the caps re-evaluate from scratch, in canonical order, on every rebuild. (`rebuild_from_events` reset-vs-preserve boundary: these counters are replay-derived ⇒ reset, never preserved.)

**Correct the inaccurate `rb` comment.** The comment at `community_voting_tier3.rs:2024-2030` claims "the apply path already enforces 1-per-actor via `current_mini_public` + monotonic-HLC" — false at the kernel level (the `rb` arm performs no mini-public check; the monotonic guard is per-`(actor,device)` ordering, not a count; `lww_dedup_se_ballots` is a pass-through). Update it to state that the kernel now enforces `≤ MAX_RATIFICATION_BALLOTS_PER_ACTOR` per actor.

**Constants home:** named `pub(crate) const`s alongside the `ds` cap in `community_voting_tier3.rs` (the `ds` limit is currently a `5` literal at `:618`; leave it as-is to avoid scope creep, but place the three new consts nearby with a comment tying them to the same anti-spam class).

**Divergence-safety and the trigger question.** These caps are evaluated inside `apply_event`, re-run in canonical order by `rebuild_from_events` and boot-restore, and derived from canonically-accumulated state ⇒ the materialized set is a pure function of the canonical-ordered event set = identical on every replica **after any rebuild/restore**. This is exactly the divergence property of the shipped `ds` 5-cap. Crucially, **no ZEB-860 trigger change is made**, and Component 3 introduces **no new divergence class beyond what `ds` already has**:

- For honest actors (≤ limit), the cap never binds ⇒ zero behavioral change ⇒ no order-dependence.
- Only an **abuser** (> limit) can observe the cap, and only across *their own* excess events; which of their > limit events materialize can differ between two *live* replicas until the next rebuild/restore re-canonicalizes — the identical, already-accepted property of the `ds` cap (an out-of-order cap-*drop* is `Dropped`, so it does not self-trigger under the existing `outcome==Applied` gate; boot-restore's full canonical fold is the convergence guarantee). It heals on restore and never affects honest users.

Deliberately **not** expanding the trigger set to `dc`/`rb` (which would give live-convergence parity with `ds`): for `rb` that would enlarge the Ratification crypto-rebuild surface ZEB-868 is trying to shrink, and restore-convergence already matches precedent. Noted as optional future hardening in §7, explicitly not done for `rb`.

---

## 6. Residual (documented, out of scope by decision)

- **Raw `events`-log storage** and, transitively, **lane-map *count*** (`last_received_hlc` grows ≤ one entry per event). Both grow together and are bounded only by poll finalization/archival + the codebase's sparse-voting-volume assumption — not by this fix. A divergence-safe storage bound is not a simple ingest reject (canonical rank unknown at append time), so it needs a separate mechanism (live-poll over-quota archival/GC or a new admission model) and its own ticket. **After Component 2′, this residual carries no per-operation cost** — it is a memory-footprint concern only.
- **Duplicate-event protection for the `md`/`dc`/`rb` pushes** relies on the existing ingest dedup (ZEB-731) plus `rebuild_from_events` folding an already-deduplicated stored set; the pushes are not made idempotent by `event_hash` here (would change the Vec to a keyed structure — a separate, larger change).

If the residual storage bound is later prioritized, file it as a follow-up referencing this spec.

---

## 7. ZEB-868 seam (re-scope)

Agreed split:
- **ZEB-861 (this)** owns the `md` (and `dc`, `rb`) **materialization** caps — bounding the `declines`/`candidates`/`ratification_ballots` *projection* growth (Component 3).
- **ZEB-868** keeps the **crypto re-run** mitigations: the apply-verdict verify-cache (crypto-free rebuilds) and the `current_stage_at(event.hlc) < Ratification` trigger-tightening for `ss`/`md`.

They compose: Component 3 bounds how much an actor can *push*; ZEB-868 bounds the *cost* of re-folding what is pushed. Update ZEB-868's description to reflect that the `md`-push bound lands in ZEB-861, and that ZEB-861 deliberately did **not** add `dc`/`rb` to the rebuild trigger (so ZEB-868's tighten work is unobstructed). Optional future hardening (either ticket): add `dc` to the ZEB-860 trigger for live-convergence parity — but **not** `rb` (Ratification crypto).

---

## 8. Testing strategy

All kernel tests live in `community_voting_tier3.rs` unit tests; ingest tests in the engine.

1. **Component 1 — length cap.** `process_inbound` and `apply_backfilled_event` each reject a `device_id` of length `MAX_DEVICE_ID_LEN + 1` with the expected `Err`, and accept length `MAX_DEVICE_ID_LEN` and a canonical 32-char id. (Both routes, since the guard is duplicated.)
2. **Component 2′ — O(1) equivalence.** For a spread of dispatch sequences (accepts, silent-drops, multiple lanes, out-of-order, empty), assert the new `max_received_hlc()` equals the old fold `self.last_received_hlc.values().copied().max().map(...)` — both `wall_ms` and `logical`, and `None` on empty. Keep the existing `max_received_hlc_is_max_over_lanes` test green.
3. **Component 3 — per-actor caps.** For each of `md`/`dc`/`rb`: an actor submitting `limit + 1` events materializes exactly `limit` (excess dropped, `advance_last_hlc == false`, still appended to `events`), and a *different* actor is unaffected. Assert honest volume (≤ limit) is untouched.
4. **Component 3 — divergence-safety (canonical convergence).** Two delivery orders of an over-cap `md`/`dc`/`rb` sequence converge to the identical materialized set after a canonical rebuild/restore (mirror the ZEB-860 `reconcile_converges_*` tests). Confirms the cap is a pure function of the canonical set.
5. **Regression.** Full `--workspace --all-targets` sweep (the 7 tier3 integration files exercise `apply_event`'s return path and the ingest routes).

---

## 9. Second-order correctness review

- **Does Component 2′ weaken the kd=rs mint floor?** No — it returns the identical `(wall_ms, logical)`; the floor stays strictly above every received event. `device_id` was already empty and unused.
- **Does Component 3 change any accept-path semantics for honest actors?** No — the caps only bind above legitimate per-actor volume; honest flows are byte-for-byte unchanged.
- **Does capping `rb` interact with the ZEB-867 (rs verify/apply TOCTOU) or tally math?** The cap bounds how many ballots an actor can *store*; the tally already re-derives from the stored ballots. A capped-out abuser simply has fewer stored ballots, identically on every replica after restore. No new invariant violation.
- **Does the length cap drop any legit event?** No — max legit `device_id` is 32 chars; cap is 64.
- **Does removing the fold in Component 2′ leave `last_received_hlc` with any remaining O(n) reader?** No — verified the only fold was `max_received_hlc`; all other reads are point lookups (§4).
- **Do the Component 3 counters stay in sync with their Vecs across a rebuild?** Yes — each counter is incremented only immediately after its push, and both are reset together in `new_from_create` and re-populated in the same canonical fold. A forgotten counter reset would over-count on rebuild; test 4 (two-order convergence through a rebuild) catches that.

---

## 10. Global constraints (carried into the plan)

- MSRV 1.91; CI gates = `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- All new constants are `pub(crate) const` with the exact values in §3/§5.
- The three new `Tier3PollState` counter fields (`declines_per_actor`, `candidates_per_actor`, `ballots_per_actor`) MUST be reset to empty in `new_from_create` (replay-derived; never preserved across a rebuild).
- No change to the ZEB-860 rebuild trigger set. No change to `last_received_hlc`'s advance-on-every-dispatch invariant (ZEB-850). No change to `last_hlc`'s advance-on-accept-only invariant (ZEB-320).
- Both ingest routes (`process_inbound`, `apply_backfilled_event`) must carry the Component 1 guard (parity requirement).
