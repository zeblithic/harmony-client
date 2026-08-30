# ZEB-1031: Authenticated D-FROST Committee-Reset Ceremony — Design

Status: draft for review (brainstormed with Jake 2026-08-30; sections approved in-session)
Author: Koya
Tickets: ZEB-1031 (this), closes the deeper gap disclosed by ZEB-1034, descends from ZEB-1029
Baseline: main @ `36f470c1`; anchor survey in session scratchpad (`zeb1031-anchor-survey.md`)

## 1. Problem

The D-FROST committee's `CommitteeState.active` flag is a one-way latch: nothing in
production ever clears it, and three gates are built on that assumption
(`check_ceremony_init_admissible` rejects `di` while active; `apply_dkg_complete` has an
`active`-guarded vk-immutability check and the R5 covert-replacement gate). Sealed-share
persistence (ZEB-1029) covers restarts, but two cases remain structurally unsolvable:

- **(a) Membership change.** There is no path to change the member set of an active
  committee — proactive refresh rotates shares under the SAME members and vk.
- **(b) True data loss.** If more than n−t members lose their sealed
  `dfrost_share.cbor` files, the committee is permanently signing-dead. ZEB-1027's
  `pending_repair` handles losses up to n−t; beyond that, nothing.

Additionally, ZEB-1034 disclosed a provenance gap: no verifier records vk lineage, so a
fresh joiner cannot distinguish a legitimate committee succession from a covert swap or a
stale-committee replay by colluding ex-members.

This design adds an authenticated, governance-gated reset ceremony that deactivates the
committee, records provenance, and permits exactly one successor DKG under a pinned new
member set — with the membership log as the tamper-proof provenance anchor.

### Non-goals

- Multi-epoch committee oracle / historical vk lookup (still the deferred v2 work noted at
  `community_voting_log_engine.rs:99-103`). Pre-reset Tier-3 polls are voided, not
  preserved.
- Serving or adopting pre-reset `vb` beacons across the boundary (they fail signature
  verification against the new vk and die naturally; not serving them is a select-side
  optimization, not correctness).
- Tier-3-vote-gated resets. Tier 3 hard-depends on the committee (circular for the
  disaster case), and no vote→effect bridge exists for Tier 1/2. Revisit when one does.
- Share repair for ≤ n−t losses (ZEB-1027 owns that).

## 2. Approved shape (decision record)

Decisions made with Jake in the 2026-08-30 brainstorm:

1. **Purpose: both cases, two speeds.** One event family; cooperative resize (live
   committee endorses — no delay) and disaster reset (admin quorum + veto window).
2. **Poll fallout: void + prompt relaunch.** Pre-reset Tier-3 polls are explicitly voided
   naming the reset; the creator gets a one-click relaunch under the new epoch. No silent
   stalls; no auto-relaunch on a human's behalf.
3. **Veto window: 72h default, per-proposal, clamped 24h–30d.**
4. **Structure: hybrid.** Governance lifecycle in the membership log (ZEB-212-shaped
   parallel track); ONE reset-marker event in the dfrost log performs deactivation and
   carries provenance mechanics.
5. **Veto form: threshold liveness proof.** The committee vetoes (or endorses) by
   threshold-signing the reset digest — a cryptographic proof it still functions.
6. Sections 1–5 of the in-chat design approved as presented; the `consumed` verdict
   (§4.3) was added during spec writing to close the lapse/stale-replay hole and is
   called out for review.

## 3. Membership-log event family (governance track)

Three new `MembershipEventKind` variants using the free 1-char tags `o`, `w`, `z`.
All follow the house wire invariants: 1-char variant tag values under `tg`/`vl`,
2-char inner-field keys, canonical CBOR. Like the ZEB-212 recovery family, they are a
parallel track to `AdminProposal` (they need their own lifecycle, not the generic
proposal effect machinery).

### 3.1 `DfrostResetProposal` (tag `o`)

Authored by a power-100 admin. Fields:

| Key | Field | Type | Meaning |
|---|---|---|---|
| `tv` | `target_vk` | `[u8; 32]` | the joint vk being reset (claimed; see below) |
| `te` | `target_epoch` | `u64` | the committee epoch being reset |
| `nm` | `new_members` | `Vec<OwnerAddr>` | pinned successor member set |
| `nt` | `new_threshold` | `u16` | pinned successor threshold |
| `vw` | `veto_window_ms` | `u64` | per-proposal window, clamped (§8) |

- `tv`/`te` keep membership-log verification a **pure function of the membership log**:
  every threshold signature in §3.3 verifies against the proposal's claimed vk, never
  against live dfrost state. A proposal lying about `tv` is self-defeating: the dfrost
  marker (§5) cross-checks against the real committee state and refuses to apply, so the
  lie is a dead end that honest members can also see and flag in UI.
- `nm`/`nt` pin the successor: after deactivation, only a `di` claiming exactly this
  shape is admissible (§5.3). This is the post-deactivation covert-replacement
  protection.
- Per-proposal `vw` (instead of a community config knob): the cosigning quorum approves
  the window together with the reset. One fewer `ProposalKind` variant and no config
  migration, same configurability.

**Verify gates (RS-P):**

- RS-P1: actor is Joined with power 100 (this is an admin action; unlike ZEB-212 the
  admins exist).
- RS-P2: `nm` sorted ascending, deduped, `len >= 2`; every member Joined at the
  proposal's HLC.
- RS-P3: `2 <= nt <= nm.len()`.
- RS-P4: `vw` within `[RESET_VETO_WINDOW_FLOOR_MS, RESET_VETO_WINDOW_CEILING_MS]`.
- RS-P5: actor has no other open (Collecting/Window/Authorized) reset proposal
  (structural spam bound, mirrors RP6).

### 3.2 `DfrostResetCosign` (tag `w`)

| Key | Field | Type |
|---|---|---|
| `ti` | `target_event_id` | `EventId` (bstr) |

ZEB-250 semantics verbatim: power-100 cosigners, proposer counts as signature 1,
effective at `admin_quorum` total distinct admin signatures. Lenient forward-ref (pairing
at materialize), like `AdminCountersign`.

**Verify gates (RS-C):** RS-C1 actor Joined power-100; RS-C2 not a duplicate signer for
this target (evaluated at materialize; duplicates are no-ops).

### 3.3 `DfrostResetCommitteeResponse` (tag `z`)

The committee's voice — one kind, three verdicts:

| Key | Field | Type | Meaning |
|---|---|---|---|
| `ti` | `target_event_id` | `EventId` | the reset proposal |
| `vd` | `verdict` | 1-char: `e` / `v` / `c` | endorse / veto / consumed |
| `sg` | `group_sig` | `[u8; 64]` | threshold Schnorr signature (R ‖ z) |
| `nv` | `new_vk` | `Option<[u8; 32]>` | present iff `vd == c` (skip-if-none) |

The **reset digest** is `dg = blake3(canonical_cbor({space_id, proposal_event_id, tv,
te, nm, nt}))` — recomputed by verifiers, never trusted from the wire. Space id inclusion
prevents cross-community replay of responses; fail-closed on encode error (ZEB-212
digest discipline).

Signed messages (32-byte message hash handed to the ordinary threshold-sign ceremony
machinery, new domain tags):

- endorse: `blake3("harmony-dfrost-reset-endorse-v1" ‖ dg)`, verified against `tv`.
- veto: `blake3("harmony-dfrost-reset-veto-v1" ‖ dg)`, verified against `tv`.
- consumed: `blake3("harmony-dfrost-reset-consumed-v1" ‖ dg ‖ nv)`, verified against
  `nv` — the successor committee attesting its own birth (§4.3).

**Verify gates (RS-R):**

- RS-R1: actor is Joined. For `vd == c`, actor must additionally be a member of the
  proposal's pinned `nm`. (The author is a courier; the threshold signature is the real
  authorization. The `c` author gate limits consumption-griefing to pinned successor
  members, who already hold equivalent obstruction power by refusing the DKG — §10.)
- RS-R2: target proposal exists (lenient forward-ref acceptable, matching house
  precedent; pairing at materialize).
- RS-R3: `sg` verifies as a Schnorr signature over the verdict's domain-tagged message
  against `tv` (`e`/`v`) or `nv` (`c`).
- RS-R4: `nv` present iff `vd == c`.

## 4. Lifecycle (pure derived state)

Evaluated with the recovery evaluator's now-floor `T = max(max(event.wall_ms), now_ms)`,
so an otherwise-idle community still progresses. No synthetic events are authored;
everything below is derived and re-derives cleanly on late delivery.

### 4.1 Phases

- **Collecting** — from `t0` (proposal) until `admin_quorum` distinct admin signatures
  at `t_q`. Dead (→ **Expired**) if quorum not reached within `ADMIN_PROPOSAL_EXPIRY_MS`
  (30d, reused) of `t0`.
- **Window** — `(t_q, t_q + vw]`. Ends early on an effective response.
- **Authorized** — either `T > t_q + vw + RESET_FINALITY_MS` with no effective veto
  (disaster path — the 48h finality margin gives any in-window veto two days to
  propagate before a marker becomes appliable anywhere), or immediately upon an
  effective endorse (cooperative path — the endorsement IS the committee's consent).
- **Consumed** — a valid `c` response exists (§4.3). Terminal.
- **Vetoed** — terminal.
- **Lapsed** — Authorized but unconsumed for `RESET_AUTHORIZED_LAPSE_MS` (30d) past
  authorization. Terminal. Lapse exists so a failed reset (e.g. a mid-flight refresh
  killed the marker, proving the committee alive) cannot permanently freeze joiner
  adoption of the living committee (§6.1).

### 4.2 Response effectiveness

- **First-response-wins**: among valid `e`/`v` responses with HLC ≤ `t_q + vw`, the
  earliest by `(HLC, event_id)` decides. Honest committees produce one response; a
  committee that threshold-signs both made its choice at the first. A veto is effective
  even during Collecting (the committee saying "we're alive, stop" does not wait for
  admins to finish cosigning). An endorse observed during Collecting takes effect when
  quorum lands (no window ever opens).
- Responses with HLC past `t_q + vw` are inert. Late *delivery* of in-window responses
  is what the finality margin absorbs.

### 4.3 The `consumed` verdict (added during spec writing — review this)

Membership cannot see dfrost state, so without an explicit record, "was this Authorized
reset actually executed?" is invisible to the membership log — and after Lapsed cleared
the adoption freeze, colluding ex-members could replay their genuine pre-reset dk quorum
to a fresh joiner (the exact ZEB-1034 stale-committee shape, one layer up). The fix:
after the successor DKG completes, a pinned successor member authors a `c` response
carrying the new vk and a threshold signature under it. Effects:

- The reset becomes **Consumed**: `tv` is **permanently** rejected for joiner adoption
  (§6.1), and Lapse no longer applies.
- The membership log thereby becomes the complete, self-contained vk-lineage registry
  (`tv → nv` per consumed reset), readable without any dfrost state.

A fake `c` (wrong `nv`) requires a pinned successor member as author (RS-R1) plus a
threshold signature under the claimed `nv`; such an insider can already sabotage the
reset by refusing to run the DKG, so no new capability is granted (honesty ledger, §10).

## 5. Dfrost-log reset marker (`rs`)

One new committee-event kind. `ResetMarkerPayload`:

| Key | Field | Type |
|---|---|---|
| `ri` | `reset_proposal_id` | `EventId` (bstr) |
| `dg` | `reset_digest` | `[u8; 32]` |
| `ov` | `old_vk` | `[u8; 32]` |
| `oe` | `old_epoch` | `u64` |
| `sp` | `space_id` | `SpaceId` (bstr(16)) |

`sp` is **mandatory** (post-ZEB-1034 discipline; a brand-new kind carries no legacy
tolerance). Authored by any power-100 admin or any member of the pinned `nm` — a
mechanical bridge; the authorization already happened in the membership log.

### 5.1 Apply gates (RS-M)

- RS-M1: `sp` equals this log's community (the ZEB-1034 check, unconditional).
- RS-M2: committee is `active`, `joint_verifying_key == Some(ov)`, and
  `current_epoch == oe`. The epoch equality is where a mid-flight refresh kills a stale
  reset — membership cannot see dfrost state, so staleness is enforced HERE, not in the
  lifecycle. A marker that fails RS-M2 is permanently inapplicable; the membership-side
  proposal eventually Lapses.
- RS-M3: the membership log materializes `ri` as Authorized-consumable or Consumed (not
  Vetoed / Expired / Lapsed), **evaluated at the marker's own envelope HLC** — the
  at-event-HLC discipline, so the verdict is deterministic across replicas regardless of
  arrival order. Consumed is accepted here because a *genuine* consumption implies the
  state already moved (RS-M2 blocks), so the only marker that reaches apply under
  Consumed is one racing a forged `c` — which must not be able to block a legitimately
  authorized reset. (A malicious author cannot stamp a future HLC to skip the finality
  margin: the ZEB-1035 forward-skew gate bounds envelope HLCs to now + 5 min, which is
  negligible against the 48h margin.)
- RS-M4: recomputed digest for `ri` equals both the marker's `dg` and the proposal's
  content; the proposal's `tv`/`te` equal the marker's `ov`/`oe`.
- RS-M5: actor gate as above (power-100 or ∈ `nm`), at the marker's HLC.
- RS-M6: duplicate and late markers are benign. Once the state has moved (deactivation
  happened, or the successor promoted), RS-M2 no longer matches; such markers MUST be
  treated as idempotent no-ops rather than log-poisoning errors, because catch-up replay
  legitimately re-delivers them.

### 5.2 Apply effects (the deactivation event)

- `active = false`; `joint_verifying_key = None`; push
  `VkLineageEntry { old_vk: ov, old_epoch: oe, reset_id: ri, digest: dg, hlc }` onto a
  new `vk_history: Vec<VkLineageEntry>` field (`#[serde(default)]`, the
  `pending_repair` snapshot-compat pattern; growth is bounded by resets, which are rare).
- Set `pending_reset: Option<PendingReset { ri, nm, nt }>` (`#[serde(default)]`).
- Clear `pending_dkg`, `pending_sign` (all sessions), `pending_refresh`,
  `pending_repair`, and associated in-memory secrets — every in-flight ceremony under the
  old vk is dead by definition.
- `current_epoch` stays at `oe`, so the existing `epoch == current_epoch + 1` gate on
  `di` naturally yields the successor DKG at `oe + 1`. `verifying_shares` / `members` /
  `threshold` / `max_signers` remain as historical residue until promotion overwrites
  them (they are only consulted behind `active` / adoption paths).
- Engine hook: notify the voting engine to void polls (§7).

### 5.3 Successor DKG

Existing gates work unchanged: `check_ceremony_init_admissible` admits the `di` because
`!active`; vk-immutability and R5 are `active`-guarded and correctly dormant. One
addition: **while `pending_reset` is set, the `di` must claim exactly `nm` / `nt`
(and therefore `max_signers == nm.len()`)** — otherwise reject. On `dk` promotion,
`pending_reset` clears, `active = true`, and the chain closes:
genesis dk → … → marker(`ov`, `oe`) → dk(`oe + 1`, new vk). A pinned successor member
then authors the `c` response (§4.3); the engine automates this the way DkgDriver
auto-drives ceremonies.

## 6. Adoption and provenance across a reset

The membership log — fully replicated, admin-signed, already every joiner's trust root —
is the provenance anchor. Markers are local state-transition mechanics.

### 6.1 Fresh joiner (`adopt_initial_quorum`)

One new check, against the joiner's **own** materialized membership state:

> Reject any dk quorum whose `joint_verifying_key` equals the `tv` of a reset that is
> **Authorized** or **Consumed**.

- Live Authorized: the old committee is being replaced; adopting it would wedge the
  joiner against the successor (vk-immutability). Reject and await the post-reset chain.
- Consumed: permanent — closes stale-committee replay by colluding ex-members forever.
- Lapsed (unconsumed): no rejection — the reset failed or was abandoned and the old
  committee may be the living one.
- **Supersession**: the Consumed rejection for `tv` is lifted by any LATER (by HLC)
  valid veto response threshold-signed under that same `tv`, on any reset proposal. A
  committee cryptographically proving it lives under `tv` is strictly stronger evidence
  than a courier's consumption claim; this is the recovery path from a forged `c`
  (§4.3) that would otherwise permanently poison adoption of a living committee — admins
  author a fresh proposal to give the committee a veto vehicle if none is open.
- The responder controls none of this evidence; that is the point.

The lone-responder **denial** residual from ZEB-1030 (serve nothing, or serve only
staleness the rule catches) remains disclosed and unchanged.

### 6.2 Straggler (holds active pre-reset state)

`adopt_refresh_quorum` is untouched — its held-vk pin correctly refuses cross-reset
adoption, which is what forces the marker path. Catch-up responses gain a **reset
section**: the marker(s) and successor dk quorum(s), interleaved for multi-reset chains
(`marker₁, quorum₁, marker₂, quorum₂, …`). The straggler:

1. Applies each marker through the ordinary `rs` apply path — RS-M1..M6 verify against
   its own membership state (which syncs independently via the community state root).
2. Adopts each successor quorum via `adopt_initial_quorum` (it is now `!active`), with
   the `pending_reset` pin enforcing `nm`/`nt`.

A responder cannot invent a link: markers verify against membership evidence, quorums
against member signatures and the pin.

### 6.3 Catch-up wire changes

The ZEB-1030 catch-up response gains an optional reset-chain field (same optional-key
evolution as `sp`: absent = no resets, legacy responders byte-identical). Request side
is unchanged — the responder knows from its own `vk_history` whether the requester's
implied epoch predates a reset.

## 7. Tier-3 poll fallout (void + prompt relaunch)

On marker apply, an engine hook (ZEB-1033 Weak-hook pattern) tells the voting engine to
move every open Tier-3 poll with `community_epoch <= oe` to a new terminal
`Voided { reset: ri }` state — the ballots are ElGamal-encrypted to `ov` and the
decryption shares are gone with the old committee; there is nothing to preserve
(single-epoch oracle, hazard #2 of the survey). UI: the poll shows a banner naming the
reset; the creator (or an admin) gets a one-click **relaunch** that authors a fresh
PollCreate copying the parameters, stamped at the current epoch, linking its
predecessor. Re-voting is honest — the old votes are cryptographically unrecoverable.
Tier-1/2 polls are untouched. Voiding is idempotent and replay-safe (derived from the
marker, which is itself in the log).

## 8. Constants and config

| Constant | Value | Notes |
|---|---|---|
| `RESET_VETO_WINDOW_FLOOR_MS` | 24h | clamp on `vw` |
| `RESET_VETO_WINDOW_CEILING_MS` | 30d | clamp on `vw` |
| default `vw` (UI-suggested) | 72h | per-proposal, quorum-approved |
| `RESET_FINALITY_MS` | 48h | disaster path only; same value as `RECOVERY_ROTATION_FINALITY_MS`, separate constant |
| `RESET_AUTHORIZED_LAPSE_MS` | 30d | Authorized→Lapsed if unconsumed |
| proposal expiry | `ADMIN_PROPOSAL_EXPIRY_MS` (30d) | reused |

No new community-config field; no new `ProposalKind` variant.

## 9. IPC / UI surface

IPCs (registry-parity asserted, headless-driveable for e2e):
`get_dfrost_reset_state` (takes `now_ms` for as-of evaluation, like
`get_recovery_state`), `propose_dfrost_reset`, `cosign_dfrost_reset`,
`respond_dfrost_reset` (drives the endorse/veto sign ceremony, then authors the
response), `author_dfrost_reset_marker`, `relaunch_voided_poll`. UI: an admin-panel
reset section (proposal status, window countdown, response state) and the voided-poll
banner + relaunch button. The engine auto-authors the marker once a reset it can apply
becomes Authorized-consumable, and auto-authors the `c` response after successor
promotion — humans initiate and veto; mechanics are automated.

## 10. Honesty ledger (what this does NOT defend)

- **A compromised admin quorum can reset a live-but-silent committee** after
  `vw + 48h`. Mitigation is exactly the veto: a functioning committee proves liveness
  cryptographically. A committee that cannot produce a threshold signature is, by
  definition, the thing the reset exists to replace. Recourse beyond that is social
  (fork), as with ZEB-212.
- **A lone catch-up responder can still deny** (serve nothing). Unchanged ZEB-1030
  disclosed residual; this design removes its ability to serve *stale or foreign*
  committees undetected.
- **A pinned successor member can grief consumption** (author a fake `c` with a wrong
  `nv` — the threshold signature is under `nv` itself, which any keypair satisfies).
  Roughly equivalent power to refusing the DKG, which they already hold (dealer-based
  DKG needs all `nm` members); a forged `c` cannot block a legitimately authorized
  marker (RS-M3) and cannot make anything adoptable, and its one lasting effect — a
  poisoned permanent `tv` rejection if the reset later fails — is recoverable via the
  veto-supersession rule (§6.1).
- **Single surviving committee member cannot veto** (threshold liveness proof requires
  t live members with shares). Chosen deliberately over single-signature veto: a lone
  signature proves an owner key is alive, not that the committee works, and enables
  costless obstruction. The survivor's recourse is the UI objection surface (advisory,
  not consensus) and admin persuasion during the window.
- **First emit is a hard fork** for un-upgraded peers (loud state-root rejection) —
  established ChangeThresholds/recovery posture; upgrade-before-adopt.

## 11. Testing

- **Verify-gate unit matrices** per event (RS-P/RS-C/RS-R/RS-M), each case failing on
  its own defect (the ZEB-1034 intent-pinning discipline: assert the error is the
  expected one, not just `is_err`).
- **Lifecycle evaluation**: now-floor progression on idle communities;
  first-response-wins ordering incl. `(HLC, event_id)` tie-break; veto during
  Collecting; endorse-before-quorum; finality-margin late-veto reconvergence; Expired /
  Lapsed transitions; `c` after Lapse does not resurrect.
- **Marker apply matrix**: wrong `sp` / vk / epoch / digest; unauthorized actor;
  not-yet-consumable (pre-finality HLC); Vetoed/Lapsed proposal; idempotent re-apply;
  pending-slot + secret clearing.
- **Successor DKG**: `pending_reset` pin (wrong members / threshold rejected); promotion
  clears the pin; vk_history chain correctness.
- **Adoption suite**: straggler across one and two resets; fresh joiner post-reset;
  stale-committee replay rejected (Authorized and Consumed); Lapsed unfreezes; legacy
  no-reset communities unaffected (absent optional field byte-identical).
- **Poll voiding integration**: open se-mode poll voided on marker apply with reason;
  relaunch produces a new poll at the new epoch; Tier-1/2 untouched.
- **Wire pins**: new zeb303-style byte-pin fixtures for `o`/`w`/`z` (membership fixture
  file) and `rs` (dfrost fixture file) + structural key-shape assertions; existing pins
  never edited.
- **Two-node headless e2e**: disaster flow to completion; disaster flow vetoed;
  cooperative flow (endorse, immediate); joiner bootstrap after reset.
