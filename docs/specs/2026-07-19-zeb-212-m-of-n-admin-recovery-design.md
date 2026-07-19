# ZEB-212: M-of-N community admin recovery — design

**Status:** proposed (awaiting Jake's review)
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
| 30-day proposal expiry as a pure function of event HLCs (no wall-clock reads at materialize) | `quorum_signers` pre-pass + threshold-crossing application |
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
3. **Pure-function materialization** (ZEB-250 precedent): every recovery state
   transition is a deterministic function of the event log and each event's HLC.
   No wall-clock reads at materialize; replicas converge from any delivery order.
4. **Loss is treated as potential compromise**: a "lost" key might be in someone
   else's hands. Recovery execution removes the old admin and rotates the epoch.

## 3. Mechanism: Recovery Designates + time-locked, veto-able recovery

### 3.1 Configuration (healthy-admin ceremony)

New community-state config, set and changed **only through the existing ZEB-250
quorum machinery** (a new `ProposalKind` routed through `AdminProposal` /
`AdminCountersign` — reusing AP1–AP5 and the 30-day proposal expiry unchanged):

```
ProposalKind::SetRecoveryDesignates {
    designates: Vec<OwnerAddr>,   // currently-Joined members, no admins required
    threshold: u8,                // R: co-signatures required to initiate, 1 ≤ R ≤ len
    veto_window_ms: u64,          // W: default 30 days; floor 7 days (see §6 T6)
}
```

Materialized as `CommunityState.recovery_designates: Option<RecoveryDesignates>`
(absent = recovery disabled = today's behavior; `skip_serializing_if` keeps old
snapshots byte-identical, same pattern as `admin_quorum`'s default-elision).

Verify gates (RD1–RD4): designates non-empty and deduped; every designate currently
Joined; `1 ≤ threshold ≤ designates.len()`; `veto_window_ms ≥ floor`. An admin may
name themselves a designate but it is pointless (they can already act); UI discourages.

### 3.2 Initiation (the lost-admin flow)

Three new membership event kinds (variant codes chosen at implementation time from
the unused 1-char pool — the same-length-keys CBOR invariant at this nesting level
must hold, as for `"q"`/`"n"`):

```
RecoveryProposal {
    new_admin: OwnerAddr,         // must be currently Joined (RP gates below)
}
RecoveryCosign  { target_event_id: EventId }   // designate co-signature, forward-ref
RecoveryVeto    { target_event_id: EventId }   // admin-tier, single signature kills
```

Gates:

* **RP1** — actor is a member of `recovery_designates.designates` AND currently
  Joined. (Not an admin gate: this is precisely the event non-admins may author.)
* **RP2** — `recovery_designates` is configured (absent ⇒ reject).
* **RP3** — `new_admin` is currently Joined and is not currently power-100.
* **RC1** — cosigner is a designate, Joined, distinct from prior signers (proposer
  counts as co-signature 1, mirroring `AdminProposal`).
* **RV1** — vetoer holds power 100 and is Joined. **One veto suffices** — deliberately
  not quorum-gated: a veto is a liveness proof and restores the status quo ante; it
  cannot escalate anyone's power, so requiring quorum would only help an attacker
  who already silenced most admins.

### 3.3 Lifecycle (pure function of the event log)

A proposal with event HLC `t₀` and current config `(R, W)`:

1. **Collecting** — until `R` distinct designate signatures accumulate. Initiation
   expiry: if `R` signatures are not reached within 30 days of `t₀` (same constant
   as ZEB-250 proposal expiry), the proposal is dead.
2. **Time-locked** — from `t_R` (HLC of the Rth signature) until `t_R + W`. Loudly
   surfaced to every member (§5). Any `RecoveryVeto` with HLC in `[t₀, t_R + W]`
   kills the proposal **permanently and retroactively** — veto-wins is the
   convergence rule (§4).
3. **Executed** — at materialize, once observing an event with HLC `> t_R + W` and
   no qualifying veto: `new_admin` → power 100; every power-100 member whose
   `OwnerAddr` was admin at `t₀` and did NOT veto or otherwise author any event with
   HLC in `[t₀, t_R + W]` is **kicked** (loss-as-compromise, §2.4), which triggers
   the existing ZEB-249 epoch rotation. Admins who *were* active in the window but
   didn't veto are left untouched — their inaction is consent, their liveness is
   proven, and kicking them would let designates purge live admins.
4. **Terminal** — an executed / vetoed / expired proposal is terminal by
   `event_id`; late cosigns and late duplicate executions are no-ops (mirrors
   ZEB-250's expired-proposal handling).

Rival concurrent proposals: at most one proposal may execute; deterministic
tie-break is lowest `(t_R, event_id)` — every replica picks the same winner, losers
die terminal. A `SetRecoveryDesignates` change or a `ChangeQuorum` landing with HLC
inside an open proposal's window kills that proposal (config-change-as-veto): the
config the initiators acted under no longer holds.

## 4. Convergence & partition analysis

The dangerous interleaving: replicas that reach `t_R + W` without having seen a veto
materialize the new admin; the veto (HLC inside the window) arrives later. Because
execution is a pure function, those replicas **re-materialize and the veto wins**:
the new admin's power reverts, and any admin-tier events they authored in the
divergence window retroactively fail power validation — the same re-materialization
class the CRDT already handles for late-arriving kicks. `W` (≥ 7 days, default 30)
dwarfs realistic partition durations (hours), so the divergence window is a corner
case, not a likely state. The residual risk — an attacker who can *eclipse the
vetoing admin's entire fleet from the community for the whole window* — is recorded
honestly in §6 (T5); with zenoh peering + iroh + pkarr + fleet butlers all carrying
community state, sustained 30-day eclipse of a live owner is outside this design's
threat budget.

HLC skew: `t_R + W` is compared against other **event HLCs**, never wall clock, so a
proposal cannot be fast-forwarded by a lying clock beyond what HLC monotonicity
already permits; a skewed initiator only shortens/lengthens its own wait relative to
honest events, and the veto rule is HLC-interval-based, not clock-based.

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
   select the proposed new admin from the roster → sign. Signatures accumulate
   asynchronously as CRDT events — designates never need to be online together
   (the ZEB-677 lesson: ceremonies must survive async fleets).
4. **Pending:** all members see a banner: *"Admin recovery proposed: NAME becomes
   admin on DATE unless a current admin vetoes."* Power-100 members additionally get
   the ZEB-356 OS-notification treatment. The banner is deliberately loud — social
   detection is a first-class defense layer (§6 T2).
5. **Veto:** any current admin: one click → `RecoveryVeto` → proposal dead, banner
   resolves to "vetoed by NAME". No quorum, no ceremony.
6. **Execute:** automatic at window expiry: new admin promoted; silent-throughout
   old admins kicked; epoch rotates; banner resolves. The new admin is nudged to
   immediately reconfigure `SetRecoveryDesignates` (the old config may name members
   loyal to the old key-holder — see §6 T4).

## 6. Threat model

| # | Threat | Outcome |
|---|---|---|
| T1 | **Rogue designate minority** (< R) | Cannot initiate. Nothing to veto. |
| T2 | **Rogue designate quorum** (≥ R) against a *live* admin | Proposal is loudly visible for ≥ W to every member incl. all admins; a single one-click veto kills it. Succeeds only against an admin silent for the full window — which is the designed function, not a bypass. Defense-in-depth: designate choice is the admin's own trust decision; banner gives the social layer W days. |
| T3 | **Rogue admin solo-claim** (ticket's named threat) | An admin cannot use recovery to escalate: RP3 forbids proposing a current admin, RP1 forbids non-designate initiation, and admin-affecting direct actions already require quorum (ZEB-250). A rogue admin vetoing legitimate recovery = status quo ante, resolvable only socially (fork the community — polycentric governance's ultimate backstop). |
| T4 | **Captured designate set after recovery** | New admin inherits a designate config chosen by the old key-holder; §5.6 nudges immediate reconfiguration. Config changes kill in-flight proposals, so a stale set cannot race the new admin. |
| T5 | **Full-window eclipse of all admins** | Out of threat budget (multi-transport replication, W ≥ 7d); recorded as residual risk. |
| T6 | **Window-shortening** | `veto_window_ms` floor of 7 days is enforced at RD4 verify time on every replica — a malicious client build cannot make honest replicas accept a 1-hour window. |
| T7 | **Replay of a recovery artifact** | There is no bearer artifact to replay (declined design, §7). Proposals are one-shot by `event_id`, bound to `community_id` (event envelope) and the config generation they were initiated under (config change ⇒ dead). Terminal states never re-arm; a re-run requires fresh designate signatures on a fresh proposal, in public, again. |
| T8 | **Kicked/left designate** | RP1/RC1 require currently-Joined at signature HLC; kick strips initiation power at materialize like every other power check. |
| T9 | **Lost key is actually compromised** | Execution kicks the silent old admin and rotates the epoch (ZEB-249), cutting the stolen key's read access forward from execution. Backward secrecy for the window itself is bounded by existing epoch semantics. |

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
* **ZEB-249 epoch rotation:** execution reuses the kick path; no new rotation code.
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
3. A unanimous rogue designate set + a genuinely absent admin = successful
   takeover after W days, in public. Designate choice is the trust decision.
4. Window-length backward secrecy is bounded by ZEB-249 epoch semantics, not
   improved by this design.

## 10. Phasing (3 PRs, each independently green)

* **D1 — CRDT core:** event variants + RD/RP/RC/RV gates + materialize (pre-pass
  mirroring `quorum_signers`) + pure-function lifecycle; red-first unit tests for
  every gate, the veto-wins re-materialization vector, rival-proposal tie-break,
  config-change-kills-proposal, and terminal-state replay no-ops.
* **D2 — IPC + UI:** `set_recovery_designates` / `initiate_admin_recovery` /
  `cosign_admin_recovery` / `veto_admin_recovery` / `get_recovery_state` IPCs
  (+ headless RPC registry per ZEB-445 parity); Governance settings section;
  pending-recovery banner; admin OS notification; sole-admin nudge.
* **D3 — e2e:** two-node scenarios: designate-initiate → veto (liveness path) and
  designate-initiate → time-locked execute with HLC-driven time control (no
  wall-clock sleeps, per the wall-clock-budget testing rule).

## 11. Acceptance criteria mapping (ticket → this doc)

| Ticket criterion | Where |
|---|---|
| Design doc: threshold scheme + recovery flow | §3, §7 (FROST decision) |
| UX flow "I lost my admin key", bounded time + counter-sign | §5, §3.3 |
| Replay-attack analysis (no artifact reuse) | §6 T7, §7 |
| Threat model: rogue admin solo-claim | §6 T3 (plus T1–T9) |
