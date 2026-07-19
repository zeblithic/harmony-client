# ZEB-212: M-of-N community admin recovery — design

**Status:** accepted (PR #496 merged) — D1 implemented under ZEB-713, D2
(IPC + UI + finality-gated rotation synthesis) under ZEB-714; see the
implementation notes in §3.3 and §4.3
**Ticket:** ZEB-212 (harmony: M-of-N community admin recovery — governance failure case)
**Author:** Koya, 2026-07-19
**Builds on:** ZEB-250 (admin quorum, PR #128), ZEB-173 (identity recovery principles), ZEB-677 (fleet quorum ceremonies), ZEB-249 (epoch rotation / backward secrecy)

## 1. Premise correction: what already exists

ZEB-212 was filed (2026-04-30) before ZEB-250 shipped. A current-state trace against
`main` (`cfbc7431`) shows most of the ticket's sketch is already production machinery
in `src-tauri/src/community_membership.rs`:

| Shipped (ZEB-250, PR #128) | Where |
|---|---|
| Per-community `admin_quorum: u8` (default 1), changed only via quorum | `CommunityState.admin_quorum`, `ProposalKind::ChangeQuorum`, gate AP5 (`new_quorum` ≤ live admin count) |
| `AdminProposal { proposal_kind }` — power-100 proposer counts as signature 1 | `MembershipEventKind::AdminProposal`, gates AP1–AP5 (§4.1 of the ZEB-250 spec) |
| `AdminCountersign { target_event_id }` — admin-tier co-signature, lenient forward-ref, paired at materialize | `MembershipEventKind::AdminCountersign` |
| Admin-affecting `SetPower`/`Kick` hard-rejected when `admin_quorum > 1` (must route via proposal) | `VerifyError::SetPowerRequiresQuorum` / `KickRequiresQuorum` |
| 30-day proposal expiry evaluated against the materialize now-floor | `quorum_signers` pre-pass + threshold-crossing application |
| Conviction-voting auto-execution defers to quorum (`SkippedRequiresQuorum`) | `community_voting_tick.rs` |

**Consequence:** a community with ≥2 admins and `admin_quorum` ≤ (live admins − 1)
can **already** recover from one lost admin with shipped machinery — the surviving
admins quorum-`SetPower` a replacement and quorum-`Kick` the lost `OwnerAddr`.

**The genuinely unsolved case is sole-admin key loss** (or loss of enough admins to
make quorum unreachable). `AdminProposal` gate **AP2 requires the proposer to hold
power 100**, so a community whose only admin's owner identity is gone has no actor
who can even *initiate* recovery. That community is bricked at the governance layer —
exactly the ticket's headline. This design addresses that case and only that case.

Note on "key loss" under the owner/device split: losing a *device* is already
recoverable at the fleet layer (ZEB-668/677 — quorum enrollment/revocation). The
community-layer problem is loss of the **owner identity** (master seed + fleet all
gone, or below the fleet's own K=2 recovery quorum). Per ZEB-173, that identity is
unrecoverable by design; its `OwnerAddr` is permanently dead. Community recovery
therefore means **transferring admin power to a different `OwnerAddr`** — usually the
same human's freshly-minted identity after they re-join.

## 2. Design principles (inherited, non-negotiable)

1. **No escrow, no platform admin** (ZEB-173, polycentric governance): recovery must
   be executable entirely inside the community by its own members. No Harmony-level
   override exists or is added.
2. **Recovery capability is pre-provisioned or absent** (ZEB-173 A+C pattern): the
   admin opts in *while healthy*. A community whose sole admin never configured
   recovery remains bricked on loss — the honest fallback is "re-create the
   community", the community-level analogue of fresh-identity-on-loss. No default
   grants non-admins a latent takeover path the admin didn't choose.
3. **Deterministic materialization with the existing now-floor** (ZEB-250 / R4-6
   precedent): every recovery state transition is a deterministic function of
   `(event log, caller-supplied now-floor)` — the exact contract
   `materialize_with_now(events, admin_addr, now_ms)` already uses for PendingJoin's
   30-day expiry. No wall-clock reads *inside* materialization; the runtime passes
   `now_ms` and the function compares against `max(max(event.wall_ms), now_ms)`.
   This is what makes time-locked execution live in an idle community (see §4.1).
4. **Loss is treated as potential compromise**: recovery execution removes the
   named lost admin and triggers epoch rotation. A *hostile but active* key cannot
   be silently removed — it must publicly veto to survive, which is itself signal
   (§6 T9, §9.3).

## 3. Mechanism: Recovery Designates + time-locked, veto-able recovery

### 3.1 Configuration (healthy-admin ceremony)

New community-state config, set and changed **only through the existing ZEB-250
quorum machinery** (a new `ProposalKind` routed through `AdminProposal` /
`AdminCountersign` — reusing AP1–AP5 and the 30-day proposal expiry unchanged):

```text
ProposalKind::SetRecoveryDesignates {
    designates: Vec<OwnerAddr>,   // currently-Joined members, no admins required
    threshold: u8,                // R: co-signatures required to initiate, 1 ≤ R ≤ len
    veto_window_ms: u64,          // W: default 30 days; floor 7 days (see §6 T6)
}
```

Materialized as `CommunityState.recovery_designates: Option<RecoveryDesignates>`
(absent = recovery disabled = today's behavior; `skip_serializing_if` keeps old
snapshots byte-identical, same pattern as `admin_quorum`'s default-elision).

**Wire-format constraints (canonical CBOR, same-length keys per nesting level):**
the `CommunityState` field takes a 2-char serde code at that struct's level (e.g.
`"rd"`, matching its documented 2-char key contract); the nested
`RecoveryDesignates` struct uses same-length keys at its own level (e.g. `"ds"`,
`"th"`, `"vw"`); new `MembershipEventKind` variant codes are 1-char picks from the
unused pool (as for `"q"`/`"n"`). Exact codes are fixed at implementation time
against `canonical_cbor_encode`'s contract; the invariant, not the letters, is
normative here.

The materialized config also carries a derived **`config_digest`**: the canonical-CBOR
hash of `(designates, threshold, veto_window_ms, generation-HLC of the proposal that
set it)`. Every recovery event binds to it (§3.2), making "the config changed under
you" a mechanical digest mismatch rather than a special-cased rule.

Verify gates (RD1–RD4): designates non-empty and deduped; every designate currently
Joined; `1 ≤ threshold ≤ designates.len()`; `floor ≤ veto_window_ms ≤ ceiling`
(ceiling 365 d, added in D1 — bounds the `t_R + W` deadline arithmetic away from
u64 wrap and keeps the value JS-number-exact on the DTO boundary). An admin may
name themselves a designate but it is pointless (they can already act); UI discourages.

### 3.2 Initiation (the lost-admin flow)

Three new membership event kinds (variant codes chosen at implementation time from
the unused 1-char pool, per the §3.1 wire constraints):

```text
RecoveryProposal {
    lost_admin: OwnerAddr,        // the admin identity being declared lost (RP4)
    new_admin: OwnerAddr,         // must be currently Joined (RP3)
    config_digest: [u8; 32],      // binds to the RecoveryDesignates generation (RP5)
}
RecoveryCosign  { target_event_id: EventId }   // designate co-signature, forward-ref
RecoveryVeto    { target_event_id: EventId }   // admin-tier, single signature kills
```

The proposal **names the lost admin explicitly**. The designates know who is lost —
that is why they were contacted out-of-band — and naming them makes the execution
effect (kick + rotation, §3.3) deterministic instead of inferred from activity
heuristics. Recovering from multiple simultaneous admin losses = one proposal per
lost admin (each independently vetoable); communities with surviving admin quorum
should use the ZEB-250 path instead.

Gates:

* **RP1** — actor is a member of `recovery_designates.designates` AND currently
  Joined. (Not an admin gate: this is precisely the event non-admins may author.)
* **RP2** — `recovery_designates` is configured (absent ⇒ reject).
* **RP3** — `new_admin` is currently Joined, is not currently power-100, and
  `new_admin ≠ lost_admin`.
* **RP4** — `lost_admin` currently holds power 100.
* **RP5** — `config_digest` equals the live config's digest.
* **RP6** — the actor has no other open (collecting or time-locked) proposal.
  Structural spam bound: at most `|designates|` open proposals community-wide,
  each visible to every member (§5), each expiring per §3.3.
* **RC1** — cosigner is a designate, Joined, distinct from prior signers (proposer
  counts as co-signature 1, mirroring `AdminProposal`).
* **RC2** — the cosign is valid only while the live config digest equals the
  proposal's `config_digest` (evaluated at the cosign's HLC).
* **RV1** — vetoer holds power 100 and is Joined, and the veto's **authored HLC**
  lies in `[t₀, deadline]` (§3.3). **One veto suffices** — deliberately not
  quorum-gated: a veto is a liveness proof and restores the status quo ante; it
  cannot escalate anyone's power, so requiring quorum would only help an attacker
  who already silenced most admins.

### 3.3 Lifecycle (deterministic in the event log + now-floor)

A proposal with event HLC `t₀`, Rth-signature HLC `t_R`, and digest-bound config
`(R, W)` defines `deadline_ms = t_R.wall_ms + W`. All phase comparisons use the
materialize time-reference `T = max(max(event.wall_ms), now_ms)` — the same
now-floor `materialize_with_now` already applies to PendingJoin expiry, so an
otherwise-idle community still advances (§4.1).

1. **Collecting** — until `R` distinct designate signatures accumulate. Initiation
   expiry: if `R` signatures are not reached within 30 days of `t₀` (same constant
   as ZEB-250 proposal expiry, same now-floor), the proposal is dead.
2. **Time-locked** — from `t_R` until `T` passes `deadline_ms`. Loudly surfaced to
   every member (§5). Any `RecoveryVeto` whose authored HLC lies in
   `[t₀, deadline_ms]` kills the proposal **permanently** — veto-wins is the
   convergence rule even when the veto is *delivered* late (§4.2).
3. **Executed** — derived state, once `T > deadline_ms` with no qualifying veto
   observed: `new_admin` → power 100; `lost_admin` is **kicked** (the named target,
   not an activity heuristic), which marks `pending_rotation_for` and so triggers
   the existing ZEB-249 epoch rotation path. Execution is *pure derived state* —
   no synthetic Kick/SetPower events are authored — so a late-delivered veto
   re-derives it away cleanly (§4.2). Side-effectful follow-ons are finality-
   gated (§4.3).
4. **Terminal** — an executed / vetoed / expired proposal is terminal by
   `event_id`; late cosigns and duplicate executions are no-ops (mirrors ZEB-250's
   expired-proposal handling). A `SetRecoveryDesignates` or `ChangeQuorum` that
   lands during collecting/time-lock changes the live digest, so RC2 fails and
   lifecycle evaluation kills the proposal — config-change-kills-proposal falls
   out of RP5/RC2 mechanically.

Rival concurrent proposals: at most one proposal may execute; deterministic
tie-break is lowest `(t_R, event_id)` — every replica picks the same winner, losers
die terminal.

> **D1 implementation note (ZEB-713).** "At most one" is scoped **per
> `lost_admin`**: candidates are grouped by the admin they recover, and the
> tie-break picks one winner per group. This is the §3.2 multi-loss semantics
> (one proposal per lost admin, each independently executable) — a global
> single-winner rule would force serial W-day windows for multi-admin loss.
>
> **D1 implementation note (PR #497 R2).** Execution is **atomic** and
> additionally requires `new_admin` to be Joined **as of the deadline**
> (a replay-time snapshot, log-derivable). If they left mid-window the
> proposal ends **Stalled** — terminal, no promotion, no kick: kicking the
> sole lost admin without the paired promotion would leave the community
> with no power-100 member, bricked by the recovery mechanism itself. A
> later rejoin does not revive a Stalled proposal (that could retroactively
> flip a rival group's executed winner); the designates simply run a fresh
> proposal (Stalled does not count against RP6).

## 4. Convergence, liveness & partition analysis

### 4.1 Liveness in an idle community (why the now-floor)

A rule of the form "execute once a later event's HLC passes the deadline" would
strand an idle community in Time-locked forever — the exact pathological case the
repo already fixed for PendingJoin (R4-6: `materialize_with_now`'s doc comment).
Recovery reuses that mechanism verbatim: callers pass `now_ms`, materialization
compares phases against `max(events_max, now_ms)`, and determinism is preserved
because the function stays pure in `(events, now_ms)`. The §5 UX promise
("executes on DATE") is honest under this rule: any replica evaluating at or after
the deadline executes, with or without new traffic.

### 4.2 Veto-wins under late delivery

Veto *authorship* is only valid inside `[t₀, deadline]` (RV1) — there is no
veto-after-the-fact right. The convergence question is a veto authored inside the
window but **delivered** to some replicas after they already passed the deadline
and materialized execution. Because execution is derived state (§3.3.3), those
replicas simply re-derive on the veto's arrival: promotion reverts, the derived
kick reverts, `pending_rotation_for` reverts, and any admin-tier events the
briefly-promoted `new_admin` authored in the divergence window retroactively fail
power validation — the same re-materialization class the CRDT already handles for
late-arriving kicks.

### 4.3 Irreversible side effects are finality-gated

The one irreversible follow-on is an `EpochRotation` **event** authored by a
then-admin in response to the derived `pending_rotation_for`. Two containments:

1. **Finality margin F:** clients do not act on a `pending_rotation_for` that was
   produced by recovery execution until `T > deadline_ms + F` (default F = 48 h).
   A veto delivered within F therefore reconverges before any rotation event
   exists. F bounds delivery delay, not authorship — it can be generous because
   the window W (≥ 7 d) already did the waiting.
2. **Heal-by-superseding rotation:** in the residual case (veto delivered later
   than F), a rotation event authored during the divergence excluded the — now
   restored — vetoing admin. That event cannot and need not be erased: the
   restored admin holds power 100 again after re-derivation and simply authors a
   **fresh** `EpochRotation` (UI-prompted), which supersedes the divergent epoch;
   stragglers converge via the existing `EpochCatchup` machinery. Governance
   never depends on possessing the divergent epoch's key.

D1 ships a test vector for exactly this path: derived execution → divergent
rotation → late veto delivery → membership reconverges → superseding rotation.

> **D1 implementation note (ZEB-713) — the heal is stronger than described
> above.** A recovery rotation has no Kick event to cite, so its
> `triggered_by` names the RecoveryProposal itself, and materialize validates
> the rotation against the proposal's executed-ness (evaluated
> position-locally at the rotation's wall clock — stable, because every
> in-window cosign/veto sorts before it). Consequence: on late veto delivery
> the divergent rotation's trigger is no longer executed, so **the epoch
> advance itself re-derives away** along with the membership effects — the
> materialized state needs no manual superseding rotation at all. The
> restored admin is not epoch-excluded on the membership layer; the §4.3
> superseding-rotation story survives only as the crypto-layer cleanup for
> peers who already ingested the divergent epoch KEY (a D2/D3 validation
> point), with `EpochCatchup` as the delivery vehicle. The F=48h finality
> margin remains the client-behavior gate before authoring any
> recovery-triggered rotation.

HLC skew: all comparisons are between event HLCs and the now-floor; a skewed
initiator only shifts its own wait relative to honest events, and veto validity is
HLC-interval-based, never local-clock-based.

## 5. UX flow ("I lost my admin key")

1. **Provision (day 0, healthy):** Community Settings → Governance → *Admin
   recovery*: pick designates + threshold R + window W. Routed through the quorum
   proposal flow (self-satisfies when `admin_quorum == 1`). Sole-admin communities
   get a persistent, dismissible settings nudge: *"If you lose your identity, this
   community cannot replace you. Configure recovery designates."*
2. **Loss:** the human re-mints a fresh identity (ZEB-173), re-joins the community
   as a regular member (invite from any member / open join), then contacts
   designates out-of-band (the ZEB-204 "reach out" backstop).
3. **Initiate:** each designate: community settings → *Initiate admin recovery* →
   select **which admin is lost** and the proposed replacement from the roster →
   sign. Signatures accumulate asynchronously as CRDT events — designates never
   need to be online together (the ZEB-677 lesson: ceremonies must survive async
   fleets).
4. **Pending:** all members see a banner from the moment a proposal exists —
   *collecting* ("Recovery of ADMIN proposed by NAME — R−k more signatures
   needed") and *time-locked* ("NAME becomes admin on DATE unless a current admin
   vetoes") phases both surface; power-100 members additionally get the ZEB-356
   OS-notification treatment in both phases. The banner is deliberately loud —
   social detection is a first-class defense layer (§6 T2), and it starts before
   the threshold is reached (§6 T1).
5. **Veto:** any current admin: one click → `RecoveryVeto` → proposal dead, banner
   resolves to "vetoed by NAME". No quorum, no ceremony.
6. **Execute:** automatic once the window passes (§4.1): `new_admin` promoted; the
   named `lost_admin` kicked; epoch rotation prompted after the finality margin
   (§4.3); banner resolves. The new admin is nudged to immediately reconfigure
   `SetRecoveryDesignates` (the old config may name members loyal to the old
   key-holder — see §6 T4).

## 6. Threat model

| # | Threat | Outcome |
|---|---|---|
| T1 | **Rogue designate minority** (< R) | Can author a *collecting* proposal (RP1 gates initiation on designate status, not on R) but cannot reach threshold or execute. Collecting proposals are banner-visible to all members and admins from creation, vetoable early, expire in 30 days, and are bounded by RP6 (one open proposal per designate ⇒ ≤ |designates| community-wide). Nuisance, not takeover. |
| T2 | **Rogue designate quorum** (≥ R) against a *live* admin | Proposal is loudly visible for ≥ W to every member incl. all admins; a single one-click veto kills it. Succeeds only against an admin silent for the full window — which is the designed function, not a bypass. Defense-in-depth: designate choice is the admin's own trust decision; banner gives the social layer W days. |
| T3 | **Rogue admin solo-claim** (ticket's named threat) | An admin cannot use recovery to escalate: RP3 forbids proposing a current admin as `new_admin`, RP1 forbids non-designate initiation, and admin-affecting direct actions already require quorum (ZEB-250). A rogue admin vetoing legitimate recovery = status quo ante, resolvable only socially (fork the community — polycentric governance's ultimate backstop). |
| T4 | **Captured designate set after recovery** | New admin inherits a designate config chosen by the old key-holder; §5.6 nudges immediate reconfiguration. Config changes flip the digest, so a stale set cannot race the new admin (RP5/RC2). |
| T5 | **Full-window eclipse of all admins** | Out of threat budget (multi-transport replication, W ≥ 7d); recorded as residual risk. |
| T6 | **Window-shortening** | `veto_window_ms` floor of 7 days is enforced at RD4 verify time on every replica — a malicious client build cannot make honest replicas accept a 1-hour window. |
| T7 | **Replay of a recovery artifact** | There is no bearer artifact to replay (declined design, §7). Proposals are one-shot by `event_id`, bound to `community_id` (event envelope) and to the config generation via `config_digest` (RP5/RC2). Terminal states never re-arm; a re-run requires fresh designate signatures on a fresh proposal, in public, again. |
| T8 | **Kicked/left designate** | RP1/RC1 require currently-Joined at signature HLC; kick strips initiation power at materialize like every other power check. |
| T9 | **Lost key is actually compromised** | The named `lost_admin` is kicked at execution regardless of activity — a thief emitting traffic does not immunize the key — and the kick always triggers ZEB-249 rotation, cutting the stolen key's forward read access. A thief can survive only by authoring a public `RecoveryVeto`, which announces "this key is active" to the whole community and routes to the T3 social/fork resolution. What recovery cannot do: silently defeat an actively hostile admin key (§9.3). |

## 7. Considered and declined

* **Out-of-band signed recovery artifact** (the ticket's original sketch): a
  pre-signed artifact is a bearer instrument — unrevocable once distributed,
  stealable, and its redemption needs exactly the in-CRDT proposal/veto machinery
  anyway to be safe. In-CRDT designate config is revocable, auditable, and has no
  offline secret to leak. Declined.
* **True threshold signatures (FROST-Ristretto255 / D-FROST) for v1**: the fleet
  already standardizes on N-separate-signature CRDT events for quorum flows
  (ZEB-250, ZEB-677) because co-signers are rarely co-online and CRDT events are
  the native async transport. FROST would compress R signatures into one and hide
  the signer set, but recovery *wants* a public signer set (auditability), and the
  interactive 2-round ceremony fights the async model. Revisit only if/when Phase-4
  governance lands D-FROST infrastructure and a compactness need appears. Declined
  for v1 — this resolves the ticket's "threshold signature scheme" criterion by
  reasoned decision rather than adoption.
* **Silent-vs-active kick heuristic** (R0 of this doc): kicking "admins who were
  silent all window" made compromise containment activity-dependent — a thief
  could keep the stolen key admin by emitting any event — and left executions with
  no kick and therefore no rotation trigger. Superseded by naming `lost_admin`
  explicitly (PR #496 review). Declined.
* **Default designates (e.g. all members over power X)**: a latent takeover path
  the admin never chose violates §2.2. Declined.
* **Quorum-gated veto**: see RV1 rationale. Declined.
* **Member recovery for non-admins**: out of scope per ticket (fresh identity +
  re-join suffices below power 100).

## 8. Interactions with existing machinery

* **ZEB-250 quorum:** `SetRecoveryDesignates` is just a new `ProposalKind` — AP
  gates, countersigning, expiry, and the voting-tick `SkippedRequiresQuorum`
  behavior apply unchanged. Recovery events are a parallel track with their own
  gates precisely because AP2 (power-100 proposer) must NOT apply to them.
* **ZEB-249 epoch rotation:** execution reuses the kick → `pending_rotation_for`
  path; no new rotation code. The only recovery-specific addition is the §4.3
  finality margin before clients act on it.
* **ZEB-677 fleet quorum:** orthogonal layers — fleet quorum recovers a *device*
  under a living owner; this recovers a *community* from a dead owner. The UX
  copy must keep them distinct ("your devices" vs "your community").
* **Open-join / invite:** the returning human needs ordinary membership first;
  no recovery-specific join path is added.

## 9. Honesty ledger (what this does NOT give)

1. A community whose sole admin configured no designates and then lost their
   identity stays bricked. By design. The remedy is re-creation.
2. Recovery cannot restore the *old* admin identity — it replaces it. Content
   authored by the dead `OwnerAddr` keeps its authorship; nothing is re-attributed.
3. Recovery defeats **absent** admin keys, not **actively hostile** ones: a
   compromised key that publicly vetoes survives the attempt, and the community's
   recourse is social (fork). The veto's visibility is the consolation prize —
   the attack stops being silent.
4. A unanimous rogue designate set + a genuinely absent admin = successful
   takeover after W days, in public. Designate choice is the trust decision.
5. Window-length backward secrecy is bounded by ZEB-249 epoch semantics, not
   improved by this design.

## 10. Phasing (3 PRs, each independently green)

* **D1 — CRDT core:** event variants + RD/RP/RC/RV gates + materialize (pre-pass
  mirroring `quorum_signers`; phase evaluation on the `materialize_with_now`
  now-floor) + derived-execution lifecycle; red-first unit tests for every gate,
  the late-delivered-veto re-materialization vector incl. the divergent-rotation
  heal (§4.3), rival-proposal tie-break, digest-mismatch kill, idle-community
  execution (now-floor), and terminal-state replay no-ops.
* **D2 — IPC + UI:** `set_recovery_designates` / `initiate_admin_recovery` /
  `cosign_admin_recovery` / `veto_admin_recovery` / `get_recovery_state` IPCs
  (+ headless RPC registry per ZEB-445 parity); Governance settings section;
  collecting + time-locked banners; admin OS notification; sole-admin nudge;
  finality-margin gating of the rotation prompt.
* **D3 — e2e:** two-node scenarios: designate-initiate → veto (liveness path) and
  designate-initiate → time-locked execute with now-floor-driven time control (no
  wall-clock sleeps, per the wall-clock-budget testing rule).

## 11. Acceptance criteria mapping (ticket → this doc)

| Ticket criterion | Where |
|---|---|
| Design doc: threshold scheme + recovery flow | §3, §7 (FROST decision) |
| UX flow "I lost my admin key", bounded time + counter-sign | §5, §3.3 |
| Replay-attack analysis (no artifact reuse) | §6 T7, §7 |
| Threat model: rogue admin solo-claim | §6 T3 (plus T1–T9) |
