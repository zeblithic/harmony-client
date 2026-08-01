# ZEB-831 — Wall-clock threat model & bounded-time trust design

**Goal:** Enumerate every place a faulty or malicious wall-clock timestamp can degrade correctness
or liveness for *other* participants (or bypass a control that should bind the timestamp's author),
state a system-wide clock-trust model, and design the defense.

**Status:** Investigation complete (5-domain parallel audit + 2 deep LWW sub-audits). This document
is the threat model, the trust model, and the defense design ZEB-831 was chartered to produce.

**Trigger:** ZEB-770 fleet session — AVALON's wall clock drifts ~1 s/day (ZEB-788), which corrupted
cross-clock measurement for the whole session. Two members of this bug class have already shipped and
been caught failing open (ZEB-791, ZEB-792). This is a systemic pass over the whole class.

---

## 1. The threat class

A wall-clock timestamp is **untrusted input the moment it crosses a device boundary.** It becomes a
cross-participant attack surface wherever such a stamp reaches a decision that affects shared state or
gates a control. Every timestamp that feeds a decision is tagged by **whose clock produced it**:

- **(P) Peer-supplied** — the stamp arrives in a payload/envelope from another device
  (`event.at.wall_ms`, `payload.at`, an LWW `updated_at`). A malicious peer sets it directly.
- **(A) Adoption-nudged local** — a local `wall_now_ms` read passed through `HlcAdoptFloor::merged_now`;
  post-ZEB-790 a verified peer can pull it forward by ≤ `HLC_ADOPT_FORWARD_CAP_MS` (5 s).
- **(L) Purely-local** — `SystemTime::now()` / monotonic `Instant`, no adoption path. Self-affecting
  unless the local decision is published and others rely on it.

Four failure modes, each a different reading of the same comparison:

- **FAIL-OPEN** — a skewed/malicious stamp makes a control *stop applying* (expiry never fires, a
  revoke is silently undone, a limiter never trips). Safety degrades silently. (ZEB-791/792 class.)
- **GRIEF-LOCKOUT** — a stamp makes a control *over-apply against others* (drops valid events, locks
  peers out, evicts honest state, makes fresh peers look stale). Liveness / DoS.
- **POISON-SQUAT** — a *future-dated* write wins an LWW / freshest-wins register "forever," blocking
  every legitimate later update. (ZEB-817/820 class.)
- **SAFE** — monotonic clock; bounded/clamped stamp; (L) and self-affecting; or the control is
  authenticated by something other than time.

---

## 2. Root cause (all auditors converged)

The codebase **owns** the correct primitive — a bounded-future check
(`harmony-pkarr::PkarrRoutingRecord::verify_freshness`, `reachability_record::fresh_butler_set`,
`ADMIN_PROPOSAL_MAX_FORWARD_SKEW_MS` at `community_membership.rs:5930`) — and applies it at a handful
of network-**admission** boundaries. Everywhere else, a (P) stamp reaches a decision *without* the
bound, because the site is either:

- **(a)** inside a `materialize`/replay/apply/merge function that trusts admission to have bounded the
  walls — but **no membership or voting verify arm bounds `event.at.wall_ms`**
  (`community_membership.rs::verify_event`, `community_voting_core.rs` inbound eligibility); or
- **(b)** an LWW / freshest-wins / eviction / ordering register that never got the check.

The **adoption floor (ZEB-790/843/845) bounds exactly one consumer** — local *mint* ordering, +5 s,
confirmed genuinely bounded (`hlc_adopt_floor.rs:62`, re-clamped against current `now` on every read,
no cross-node accumulation) and rejection-inert (every `observe` sits post-verify+apply+record). It
does **not** touch any finding below, because those read the peer stamp *directly* from a payload, not
via a mint. A structural consequence worth stating: because honest local mints are capped at `now+5 s`,
**an honest device can never out-stamp a poisoned register until the real wall clock catches up** — so
every unbounded future-dated register is effectively permanent.

---

## 3. Clock-trust model (the policy this project commits to)

1. **Own devices are in scope.** A sibling/own device with a skewed *or* compromised clock is a
   defended-against adversary. Justification: ZEB-788 proved benign drift alone (no compromise)
   poisons these registers; the clamp is cheap and correct in both cases.
2. **Bounded-skew tolerance, not trust.** Any (P) or (A) stamp that gates a control or enters a shared
   register is accepted only within a bounded forward window of the *receiver's* own clock; beyond it,
   the stamp is rejected or clamped (never silently trusted).
3. **One house constant for control/security decisions.** A single `MAX_FORWARD_SKEW_MS`
   (~5 min, matching the existing `FUTURE_TOLERANCE_MS`) governs every expiry/admission/revocation/
   governance-ordering decision. A separate, looser `DISPLAY_SKEW_TOLERANCE_MS` may apply only to
   pure display/discovery ordering where no control is gated. Both live in one module so the policy is
   auditable in one place.
4. **Controls prefer monotonic or causal time.** Rate limiters, cooldowns, backoff, and TTL-of-a-
   locally-observed-event should key on `Instant`/monotonic or on a *local receipt* stamp, never on a
   peer-reported wall. Ordering that must be causal uses the full HLC tuple with a bounded wall.
5. **Signed ≠ bounded.** A stamp being inside a verified signature makes it *attributable*, not
   *plausible*. Authentication and bounding are independent obligations.

---

## 4. Findings inventory

Deduplicated across all audits. `file:line` is the decision site. Full per-domain tables and the two
deep LWW sub-audits are archived alongside this spec's working notes. Severity reflects blast radius ×
reachability (who can mount it) × recoverability.

### CRITICAL — governance / identity / revocation integrity; any member or one own-device can mount

| ID | Site | Class | One-line |
|---|---|---|---|
| A1 | `community_membership.rs:2482` | FAIL-OPEN | Future-dated `RecoveryProposal` makes the victim admin's honest veto fail `wall >= t0` and vanish silently; veto is the sole non-quorum control on a recovery takeover. |
| SS | `community_membership.rs:2250` (`event_sort_key`, used `:2692/:5088/community_state_sync.rs:4181`) | POISON-SQUAT | Unbounded peer wall is the LWW comparator for every membership/power/channel register; `materialize` never re-verifies, so a future-dated `Kick`/`SetPower` sorts last **forever** and no honest event can override it. |
| FR | `owner_state_crdt.rs:1099` (`FriendEntry`) | FAIL-OPEN | Sibling writes `status:Active` at wall+1 yr → friendship can never be revoked; **DM cutoff defeated** (blocked party keeps DM access). |
| GR | `owner_state_types.rs:2578` / `owner_state_sync.rs:471` (`GrantEntry`) | FAIL-OPEN | Per-field `max` join; a skewed re-share (`ga=now+5m`) makes a later revoke lose → **file-share revoke silently undone**. |
| C3 | `mint_sync.rs:189/232/261` (`upsert_*_lww`) | POISON-SQUAT | Row LWW is a raw lexicographic **String** compare on peer `updated_at`; `"9999-…"` reverts every local edit (incl. transaction amount / deletion) on every device forever. Separate axis from ZEB-845. |
| C4 | `persistent_card_store.rs:213` | POISON-SQUAT | `shared_at` unbounded, disk-persisted, **no TTL, not re-verified on load** → a future-dated card pins the attacker's name/avatar/profile-page on every peer permanently. |
| E1 | `community_voting_log_engine.rs:1108` → `community_voting_tier3.rs:1068` | FAIL-OPEN | One future-dated vote event (accepted *unconditionally* — no stage/authz gate) makes every signing replica auto-mint PollClose+PollResult → poll finalizes instantly with whatever ballots exist. |
| SP | `owner_state_crdt.rs:1226` (`Space.shared_in_profile`) | FAIL-OPEN | Future-dated Space pins the community as publicly listed; the user's later privacy opt-out is discarded. **Privacy-control bypass.** |

### HIGH — durable lockout / broad griefing, cross-participant

| ID | Site | Class | One-line |
|---|---|---|---|
| E2 | `community_voting_tier3.rs:457/1049` | GRIEF-LOCKOUT | One event stamped `now+1h` (need not be accepted) advances the global per-poll `last_received_hlc` → every mini-public ballot rejected `IllegalTransition` for the skew duration, durable across restart. Fix model = channel-log per-(author,device) lane (E7). |
| A6/CF | `community_membership.rs:2593` → `:3326` | GRIEF-LOCKOUT | `max`-register over all event walls; one `now+31d` stamp drops every in-flight PendingJoin community-wide + forces recovery `Expired`/`Executed` transitions. |
| ES | `storage_records.rs:772` (`evict_stalest`) | GRIEF-LOCKOUT | Victim = `min(peer updated_at)` under a wildcard self-signed topic → far-future flood evicts every honest buddy's pledge/backup set from the 1024 cap. Sibling `evict_pins:744` is the flood-proof template. |
| C11 | `dm_inbox_crdt.rs:217` + `dm_inbox_ingest.rs:412` | GRIEF-LOCKOUT | Earliest-wins on butler-minted `deposited_at`; a backdated stamp makes the recipient drop the DM as pre-expired. Silent message loss; sender gets a valid ack. |
| RH | `community_relay_hold_crdt.rs:104` | GRIEF-LOCKOUT | First-writer-wins on `held_at`; a **backdated** stamp wins and `gc` evicts the live held blob immediately → recipient never receives it. Needs a *backward* bound. |
| D2 | `fleet_net.rs:199/206` | FAIL-OPEN | Sibling butler-set staleness has only a lower bound; a fast-clocked sibling is never swept, ranks slot 0, and is **published to other owners** → they route deposits to the dead device. |
| C7 | `library_directory.rs:987/1005` | POISON+GRIEF | Future `listed_at` pins top of every peer's discovery list + is immune to cap eviction → genuine libraries evicted. |
| D4 | `community_relay_resolver.rs:62` | GRIEF-LOCKOUT | Sort key unclamped within the freshness window; 4 skewed advertisers fill all 4 slots → censor inbound community state for a node. |
| A5 | `community_voting_tick.rs:166` | FAIL-OPEN | Future-dated poll-create → no replica ever auto-closes the poll; tally never finalizes. |
| RB | `reachability_resolver.rs:456` ← `address_book_sync.rs:213` | POISON-SQUAT | **ZEB-815 regression** — the RCH4 ±30-min gate went dead (`lib.rs:7590` no-op) and the DurableCrdt LWW clamp was never restored; a future-dated addrbook HLC freezes a peer's durable slot for process life (butler set pinned to a dead device). |

### MEDIUM — real but bounded (in-memory, own-device-only, narrow radius, or self-healing)

`A2` admin-proposal apply-path expiry never fires (`community_membership.rs:3538`, ZEB-792 gap) ·
`A3` recovery-init window immortal (`:2464/:2475`) · `A4` PendingJoin expiry never fires (`:3326`) ·
`DEV` owner-device set poison → new device unlearnable, DMs `UnknownSigningKey` (`owner_state_crdt.rs:886`) ·
`RG` received-grant dismiss + ZEB-730 granter-revoke defeated (`owner_state_sync.rs:570`; sibling `revoke_grant_inner:708` has the clamp) ·
`LE` LibraryEntry trust pinned (`owner_state_types.rs:2548`) ·
`CD` future-dated channel `deleted_at` stops gating writes (`community_channel_log.rs:1526`) ·
`C1` durable butler-set future-unbounded (`butler_deposit.rs:626`, PR-#221 sibling) ·
`C2` voice-moderation directive freezes a mute slot (`voice_moderation.rs:387`; in-memory) ·
`D5` voice speaker-queue jump (`voice_presence.rs:251`) ·
`D1` network-health false "seen 0 s ago" (`network_health.rs:3604→:1812`, ZEB-621 gap; diagnostics-only) ·
`C8` pkarr vines slot LWW (`pkarr_vines_publisher.rs`, ZEB-820; multi-device, no attacker needed) ·
`VF` vine-feed squat/eviction (`vine_feed_cache.rs:701/787/735`, ZEB-818 gap) ·
`C9` notes CRDT silent data loss (`notes_crdt.rs:57`, own devices) ·
`B6` community-relay resolver LWW clamp-at-caller latent (`community_relay_resolver.rs:39`) ·
`B1` OpenJoinRateLimiter on wall clock (`open_join_admit.rs:92`, ZEB-711 gap; (L) local, availability) ·
`ABK` addrbook `stamped_at_ms` unsigned & attacker-rewritable (`community_address_book.rs:201`).

### LOW / display-only / latent
`RM` read-marker poison (`owner_state_crdt.rs:694`) · `C10` profile-broadcast in-memory (`profile_broadcast.rs:607`) ·
`E5` governance sort display-only (`CharterView.svelte`/`StatementVoteList.svelte`) ·
`E9` `list_pending_joins` partial-tuple render (`lib.rs:47859`) ·
`BW` channel-log backfill watermark (`community_channel_log.rs:2189`; ~1 h reconcile heals) ·
`BS` backup-staleness banner never clears (`backup_state.rs:69`).

### NEEDS-VERIFICATION (resolve before ticketing)
- **RR** `community_membership.rs:3113` — can a recovery-rotation author future-date `at` to clear the
  48 h ZEB-212 finality window early? Does a later-arriving veto (sorting *before* the future rotation)
  re-materialize to `Vetoed` and retract the derived kick, or is the epoch rotation already
  irreversibly applied by the deposed admin's devices? **If not self-healing → CRITICAL.**
- **B7** `iroh_invite_acceptor` open-join has no pre-auth tier-1 limiter — unbounded pre-consent crypto
  per connection (ZEB-700 class) + one attacker can exhaust the global 20/60 s budget (raises B1).
  Deliberate capacity limit or unswept ZEB-694/700 sibling?
- **E10** `set_butler_pin`/`set_device_petname`/`set_community_relay_opt_in` (`lib.rs:69086/69274/69619`)
  inherit a sibling's future-dated fleet-doc stamp unbounded (bypass `merged_now`). In scope per §3.1;
  contained (own fleet-doc register, no leak to shared lanes).
- **D6** ZEB-622 presence→`lastSeenMs` merges monotonic-ms into wall-epoch-ms → inert in production
  (`network_health.rs:2534`). Fails safe, but a designed input is silently absent.
- **FSN** `fleet_sync.rs:585` `synced_device_count` on sibling-controlled `seen[our_device]` — no
  production consumer found; bound if one appears.

---

## 5. Incomplete prior fixes (regressions of shipped tickets — fix in THIS ticket)

| Ticket | Shipped | Gap | Site |
|---|---|---|---|
| **ZEB-792** | forward bound on admin-proposal *planner* (`:5932`) | authoritative *apply* path unbounded → 30-day expiry never fires (A2) | `community_membership.rs:3538` |
| **ZEB-818** | forward bound on vine *pull cursor* (`vine_pull_driver.rs:311`) | vine_feed_cache admission/ordering/eviction (VF) | `vine_feed_cache.rs:701` |
| **ZEB-711** | Intro/Friend limiters → monotonic | `OpenJoinRateLimiter` still wall-clocked (B1) | `open_join_admit.rs:92` |
| **ZEB-621** | reachability future-skew clamp *computed* (`reachability_resolver.rs:421`) | `list_active_peers` returns the raw value; network-health reads unclamped (D1) | `network_health.rs:3604` |

---

## 6. Defense design (the spine)

1. **One forward-skew gate on `event.at.wall_ms` at membership/voting sync ingest.** Reject (or clamp)
   any inbound signed governance event whose wall exceeds the receiver's `now + MAX_FORWARD_SKEW_MS`.
   This single gate closes **A1, SS, A2, A3, A4, A6/CF, CD, E1** and shrinks **RR** — the "one gate at
   admission, not six patches inside the replay" the audit repeatedly recommended. Highest leverage.
2. **`reject_future(stamp, now, MAX_FORWARD_SKEW_MS)` at each self-ingesting LWW/verify boundary** so
   the poisoned value never enters a register: owner-state CRDT cluster (FriendEntry, GrantEntry,
   LibraryEntry, Space, owner-device set, notes, read markers, received-grant dismiss), profile card
   (`verify_card`, C4), mint rows (parse + bound `updated_at`, C3), reachability DurableCrdt (RB),
   library directory (`verify_announce`, C7), vine feed (VF), storage `evict_stalest` (ES), butler-set
   (C1). For **relay-hold (RH)** the bound is *backward* (backdating is the winning direction).
3. **Structural fixes** where a clamp is insufficient:
   - tier-3 `last_received_hlc` → per-`(actor, device_id)` watermark (E2), mirroring the channel-log
     lane keying (`community_channel_log.rs:999`) that ZEB-585 already proved.
   - tier-3 engine auto-trigger → clamp the "now" reference to `min(t3.last_hlc.wall_ms, local_now)` (E1).
   - order presence/speaker-queue and rank relay advertisers by **local receipt time** or
     `min(peer_stamp, now)` (D4, D5, B6); push the relay-resolver clamp *into* the store (B6).
   - close the tier-3 apply-path authz gap (verify_ss/sf/sr not called on apply) that makes E1/E2 cheap.
4. **Monotonic migration:** `OpenJoinRateLimiter` → `tokio::time::Instant` epoch, completing ZEB-711;
   split its single `now_ms` arg into `wall_now_ms` (freshness) + `limiter_now_ms` (window/nonce) (B1).
5. **One policy module** owning `MAX_FORWARD_SKEW_MS`, `DISPLAY_SKEW_TOLERANCE_MS`, and the shared
   `reject_future`/`clamp_future` helpers, so the trust model is auditable and testable in one place.

Every fix ships with a positive-discrimination test (a poisoned stamp higher than a legit one, so a
leak clamps visibly) mirroring the ZEB-790 T5–T7 pattern.

---

## 7. Remediation plan (ticket map)

**In ZEB-831 (this effort), per the scope decision:** close the 4 incomplete-prior-fixes
(§5) — each is a shipped-ticket regression, highest-confidence, cheapest. Each is a small, testable
forward-bound. Introduce the §6.5 policy module as the shared home for the constant + helper so the
CRITICAL tickets build on it.

**Spawn as prioritized implementation tickets (do NOT fix here):**
- **T-GOV (CRITICAL):** the §6.1 membership/voting ingest forward-gate → closes A1/SS/A2/A3/A4/A6/CD/E1.
  Superset of the ZEB-792 regression; the single highest-value follow-up.
- **T-OWNER (CRITICAL):** owner-state revocation cluster — FriendEntry, GrantEntry, Space privacy,
  received-grant/ZEB-730, LibraryEntry, owner-device set (§6.2).
- **T-MINT (CRITICAL):** mint-row `updated_at` parse+bound (C3).
- **T-CARD (CRITICAL):** profile-card `verify_card` bound + re-verify-on-load (C4).
- **T-VOTE-LANE (HIGH):** tier-3 per-(actor,device) watermark + engine-trigger clamp + apply-path
  authz gap (E1/E2).
- **T-STORAGE (HIGH):** `evict_stalest` → local-stamp newest-first (ES); relay-hold backward bound (RH);
  DM-inbox deposited_at clamp (C11).
- **T-DISCOVERY (HIGH):** reachability DurableCrdt / ZEB-815 (RB); fleet butler-set bound (D2); relay
  advertiser ranking (D4); library directory (C7); addrbook `stamped_at_ms` signing (ABK).
- **T-VINE (MEDIUM):** pkarr vines slot device-scoping (C8); vine-feed already folded into ZEB-818 fix.
- **T-MISC (MEDIUM/LOW):** voice moderation (C2), speaker queue (D5), notes (C9), read markers (RM),
  presence unit-mismatch (D6), list_pending_joins tuple (E9), B7 pre-auth shield decision.

**Cross-repo (harmony-owner) — Linear dossier, NO upstream comms:**
- **D3** `harmony-owner` `trust.rs:44` / `state.rs:539`: sibling liveness certs never age out when
  future-stamped (one-sided lower bound). The client already guards its OWN cert (`ClockRegressed`,
  ZEB-721) but accepts a sibling's — asymmetry. Fleet trust state stays "fresh" forever.

**Resolve the NEEDS-VERIFICATION items (§4) before ticketing** — RR in particular may be CRITICAL.

---

## 8. Confirmed-SAFE registry (the templates to copy, and closed holes to not re-litigate)

- **`community_membership.rs:5930`** — ZEB-792's two-sided bound: the template for the ingest gate.
- **`owner_state_crdt.rs:375-382`** — rejects `created_at`-backdating hijack of the epoch-key
  creator-pin. The correct pattern for immutable-field defense.
- **`storage_records.rs:744` (`evict_pins`)** — local stamp + newest-first: flood-proof eviction.
- **`harmony-pkarr::verify_freshness`**, **`reachability_record::fresh_butler_set`**,
  **`community_relay_announce::fresh_relay_entry`**, **`open_join_admit` freshness arm**,
  **`community_invite` redemption expiry** — bidirectional bounds done right.
- **ZEB-817 pkarr freshest-by-seq** (verify-gated highwater), **`feed_authority`** (sticky-revoke
  evaluated before the clock comparison; verifier's clock, never the record's), **adoption-floor
  ratchet** (bounded, no cross-node accumulation), **floor feed sites** (rejection-inert).
- **ZEB-804 staleness tier** and **ZEB-829 sync staleness** — locally-stamped, unfakeable by a peer.
- Self-affecting-only registers: outbox tombstones (ULID-keyed), inbox min-wins, channel-log
  per-(author,device) lanes, reaction cells, community-relay-opt-in / pin / petname (prev-anchored
  re-stamp always beats a poisoned future stamp).

---

## 9. Residual to record in the ZEB-790 spec
The adoption floor caps *magnitude* at +5 s but not *duration*: one hostile verified frame pins this
device's floor at `now+5 s` for the **rest of the process lifetime** (session-max register, no decay).
Every wall-coupled mint is shifted +5 s in the permissive direction for the session. Magnitude-bounded
(hence SAFE), but the ZEB-790 spec should say "for the session," not "for 5 s," if it reads otherwise.

---

## 10. Testing & verification posture
Each shipped bound gets a discrimination test (poisoned-higher-than-legit → visible clamp). The ingest
gate (§6.1) additionally needs a replay/restart test (the poisoned frame is persisted; the bound must
hold after reload). The policy module (§6.5) gets a single-source constant test pinning
`MAX_FORWARD_SKEW_MS` below every consumer budget it must not cross (mirror
`adopt_cap_stays_far_below_consumer_budgets`).
