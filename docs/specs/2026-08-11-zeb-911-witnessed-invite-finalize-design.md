# ZEB-911: Witnessed invite finalize — design

**Status:** Approved design, pre-implementation
**Ticket:** ZEB-911 (parent epic ZEB-909, Freenet architecture review R2)
**Related:** ZEB-254 (shipped pending-join CRDT), ZEB-888 (canonical-claimant fence), ZEB-889 (mint cache), ZEB-906 (PendingJoin stall — member-side sweep moves there), ZEB-908 (live-session addressing reuse), ZEB-918 (rendezvous epoch-key spawn-pinning, filed out of this design), ZEB-874 (deferred invite burn), ZEB-827 (identified rendezvous resolution)
**Research basis:** `docs/research/2026-08-11-freenet-architecture-review.md` §2.1, §5 (R2)

## 1. Problem

Invite-only redemption today couples the joiner's success to the liveness of **one specific peer** — the community's founding admin (who is also the only valid token minter, so "inviter" and "admin" coincide). The coupling exists at exactly two points, both verified to file:line during design research:

1. **Acceptance policy.** The invite-handshake acceptor is installed once per node and is community-agnostic (`lib.rs:10937-10960`), dispatching off the packet's own `community_id` (`community_invite.rs:2590-2599`). A non-admin Joined member dialed with a valid redeem packet passes every check and fails at exactly one line: `verify_packet_pure` step 4, `signed.invite_token.inviter != self_owner` → `InviteSignerMismatch` (`community_invite.rs:2271-2277`).
2. **Discovery.** The cold cross-WAN path (`connectivity_redeem_invite_iroh_inner`, `lib.rs:62717`) resolves only the admin's Case-A pkarr record (keyed by `HKDF(token.sig, epoch)`, verified against `payload.admin_identity_pub`, `lib.rs:62959-62963`). A joiner has no way to find any other member's endpoint. ZEB-908 improved the addressing of that single dial; it did not add targets.

Everything downstream of those two points is **already general**. Since ZEB-254: any Joined member with power ≥ `invite_threshold` (default 0) validly counter-signs a `PendingJoin` (`community_membership.rs:5069-5072`), and every Joined member's engine auto-counter-signs on both the local-insert and CRDT-merge hooks (`community_state_sync.rs:1508`, `2444`). Since ZEB-888: multiple members racing to counter-sign the same claim converge via the canonical-claimant pre-pass in `materialize_with_now` (`community_membership.rs:2706-2773`). Since ZEB-889: joiner retries re-send the same `PendingJoin` event id via the mint cache (`community_invite.rs:287-290`), so retransmits converge instead of getting P6-rejected.

ZEB-906 (joiner permanently stranded at PendingJoin) and ZEB-908 ("inviter offline" despite a live link) are both downstream symptoms of the two couplings above.

## 2. Design overview

Two slices. No new event kinds, no CRDT merge-rule changes, no change to single-use or token-minting invariants.

- **Slice 1 — Witness acceptance:** any Joined member's node can accept the redeem handshake, verify the packet, perform the claim-bound insert, auto-counter-sign, and return the countersign — using the machinery that already exists behind the one policy line.
- **Slice 2 — Witness discovery:** the joiner's dial logic becomes a bounded ladder — admin first (unchanged fast path), then the community's rendezvous advertiser slots as witness candidates.

Explicitly **out of scope** (see §8): member-side level-triggered countersign sweep (ZEB-906), the rendezvous epoch-key spawn-pinning fix (ZEB-918), delegated token minting, multi-epoch slot publishing.

### 2.1 The three authorities, and which one changes

| Authority | Today | After ZEB-911 |
|---|---|---|
| Mint a valid `InviteToken` | Founding admin only — `verify_event` P2: `invite_token.inviter == ctx.admin_addr` (`community_membership.rs:4368-4371`), enforced on every replica | **Unchanged.** P2 is the issuance invariant and the root of admission integrity |
| Accept the redeem handshake | Admin's own nodes only — `verify_packet_pure` step 4 | **Any Joined member with power ≥ `invite_threshold`** (same predicate counter-signing already uses) |
| Counter-sign `PendingJoin → Joined` | Any Joined member, power ≥ `invite_threshold` (default 0) | **Unchanged** (already general) |

A valid invite-only token always carries `inviter == admin_addr` (the admin minted it, possibly long before going offline), so P2 passes regardless of which node transports or witnesses the packet. The witness change is transport-layer policy only; the trust boundary — event verification P1–P6 replayed independently on every replica against the admin-rooted signature chain — is untouched.

## 3. Slice 1 — Witness acceptance

All changes in `community_invite.rs` (`verify_packet_pure`, `handle_unicast`) plus error-code surface.

### 3.1 Relax step 4 to witness eligibility

Replace the identity check with the standing eligibility predicate:

- **Today:** `signed.invite_token.inviter == self_owner`, else `InviteSignerMismatch` (`community_invite.rs:2271-2277`).
- **New:** the accepting node must be a currently-Joined member of `signed.community_id` with `power ≥ invite_threshold`, evaluated against its own materialized state — the same predicate `handle_unicast` already applies at `community_invite.rs:2611-2647` and that `JoinCountersign` verification applies at `community_membership.rs:5069-5072`. The admin trivially satisfies it, so the existing path is a special case of the new rule and no behavior changes for admin-accepted redeems.

Since the self-eligibility check already exists later in `handle_unicast`, the concrete change is *removing* the identity equality from `verify_packet_pure` and letting the eligibility check be the gate — plus keeping a pure-function seam so eligibility remains unit-testable with a raised `invite_threshold` (the per-community materialized value via `actor_power_meets_invite_tier`, `community_membership.rs:5940`, not the global default).

### 3.2 Retarget the token-signature check

Today the acceptor verifies `InviteToken.sig` against `self_device_ed25519` — its own device key — which is only correct because self == admin. A witness must instead verify the token signature against the **admin's enrolled device keys from its own materialized CRDT state**, i.e. the same check `verify_event` P5 performs authoritatively at insert/merge (`verify_invite_token_sig_with_enrolled`, `community_membership.rs:4363-4399` region). The witness necessarily has the admin's enrolled keys: it is a Joined member, and the admin's self-Join bootstraps every replica.

Failure of this check on a witness gets a **new, distinct error** (§3.4) rather than reusing the admin-path variant, since it means "token not signed by this community's admin" — a materially different diagnosis than a key mismatch on the admin's own device.

### 3.3 Burn stays acceptor-local; single-use stays at materialize

The ZEB-874 deferred burn (`iroh_invite_acceptor.rs:529-540`, fired only after the countersign response reaches the transport) remains acceptor-local. A witness burning a token only prevents *that witness* from re-accepting it — there is deliberately **no cross-witness burn coordination**. The authoritative single-use fence is unchanged: `insert_local_claim_bound_pending_join` (`community_state_sync.rs:1715`) at insert, and the canonical-claimant pre-pass with its wall-clock causality guard at materialize (ZEB-888). Two witnesses accepting the same token race safely:

1. Same joiner retrying → same `PendingJoin` event id (ZEB-889 mint cache) → second insert is `AlreadyKnown`, converges.
2. Different actors with the same leaked token → claim-bound insert refuses the second locally (`LocalInsertError::InviteAlreadyClaimed`), and even under partition the canonical-claimant pass picks one winner deterministically on convergence.

### 3.4 Error-code surface

Acceptor side (`CommunityInviteVerifyError`, drives `community-state-sync-degraded` reason tags): add variants for witness-specific rejections — the node is not Joined in the packet's community, power below the community's invite threshold, and admin-enrolled-key token verification failure. Exact naming settled in the implementation plan; the requirement is that a witness's refusal is distinguishable in degraded-event telemetry from the legacy admin-path failures, and that `InviteSignerMismatch` disappears from the accept path (its semantic is deleted).

Joiner side (`RedeemInviteErrorCode`, `community_invite.rs:1530-1583`): add one code for "full ladder exhausted" (admin + all witnesses unreachable), distinct from today's single-target `InviterUnreachable`, so the GUI can render "no community member reachable" instead of misattributing to the inviter. Existing codes unchanged.

## 4. Slice 2 — Witness discovery ladder

All changes in `connectivity_redeem_invite_iroh_inner` (`lib.rs:62717+`) and a small resolver helper. **No publisher-side changes**: opted-in Joined members of invite-only communities already publish rendezvous slots today — the publish path has no `is_invite_only` gate (`lib.rs:12034-12283`, gated only on `RelayOptInDoc` opt-in + `is_joined`, `lib.rs:12121,12143-12145`). The `community_rendezvous.rs` module doc's "open-community" framing is design intent from ZEB-458-era work, not an enforced restriction; this design promotes the invite-only usage to supported behavior.

### 4.1 Slot derivation from the invite

`rendezvous_slot_key(epoch_key, slot_index, epoch_id)` (`community_rendezvous.rs:107-114`) needs exactly: the community epoch key (the joiner decrypts `epoch_snapshot.sealed_epoch_key` from the invite), a slot index in `0..COMMUNITY_RELAY_ADVERTISERS_MAX` (= 4, `community_relay_announce.rs:24`), and the wall-clock 7-day bucket `epoch_id` (`current_epoch_id(now_ms)`, with the existing `epoch_tolerance_window` covering bucket boundaries). No `community_id`, no membership secret. An invite holder can therefore derive all four slot keys with zero new material.

### 4.2 The ladder

Restructure the current single-target dial (one `alice_addr` from Case-A, `for attempt in 0..2` with a diverse-relay re-resolve of the same identity, `lib.rs:63009,63031-63180`) into an outer loop over **candidate identities**:

1. **Rung 0 — admin (unchanged fast path):** Case-A resolve + ZEB-908 `merge_live_endpoint_addrs` enrichment, 2 attempts with the existing diverse-relay retry. Preserves today's behavior and latency for the common case exactly.
2. **Rungs 1–4 — rendezvous witnesses:** resolve slots 0–3 via the plain `resolve_rendezvous` (the ZEB-827 `IdentifiedSlotResolver` requires an enrolled-key roster the joiner does not have pre-admission — see §6). Build an `EndpointAddr` per record via the existing `endpoint_addr_from_routing` shape (`lib.rs:62445-62464`), **dedup against the admin's node id and against each other** (the admin may be an advertiser; multiple slots may be one node), 1 attempt each.

Bounded worst case: 2 + 4 connection attempts before returning ladder-exhausted. Slot resolution runs once, after rung 0 fails, not per-rung. The *request* wire protocol is byte-identical regardless of rung — same `HARMONY_HANDSHAKE_V1` ALPN, same `CommunityInviteSigned` packet (discriminant `0x10`), same countersign-response wait against the acceptor's `poll_deadline`. On a witness, the countersign arrives from the witness's own auto-counter-sign hook firing on its local claim-bound insert — typically within milliseconds, same as the admin path.

**Response shape (implementation delta, discovered in the three-party e2e):** a fresh joiner's CRDT knows only the admin, so a witness-authored countersign alone is unverifiable on arrival (signer unresolvable + `JoinCountersignActorNotJoined`). The acceptor therefore computes `admission_chain_for(self)` — its own cert-carrying `PendingJoin` plus the ratifying countersigns, recursed up the countersigner graph, admin-terminated — and, when non-empty, responds with a CBOR **array** `[chain…, countersign]` instead of the legacy single event (a CBOR map). The two shapes are distinguished by CBOR major type; the admin's chain is empty by construction, so the admin path stays byte-identical and old acceptors interoperate unchanged (old joiners never dial witnesses — the ladder is joiner-side). The joiner bounds the chain (≤ 64 events, same-community) and inserts it before the countersign; every chain event is re-verified by `verify_event` against the admin-rooted signature chain, so a malicious witness still cannot fabricate admission. This is the River `ensure_members_for_message_authors` lesson applied: the response delta must be self-contained.

### 4.3 Post-handshake session seeding

Today the post-redeem Zenoh link seed is hardcoded to `payload.admin_addr` (`lib.rs:62989-62991`, re-seeded at `63137-63143`). Change: seed `ReachabilityResolver` under the identity **actually dialed successfully** — for a witness rung, the owner identity embedded in the resolved slot record. Precedent: the gateway dial driver already seeds under a resolved non-admin beacon identity and lets standard downstream machinery take over (`community_gateway_dial_driver.rs:518-521`). The joiner's subsequent state-root sync rides the `HARMONY_ZENOH_V1` link to the witness; roster-wide reachability then arrives via ordinary `ReachabilityAnnounce` CRDT deltas.

### 4.4 The LAN path is untouched

`redeem_invite_inner` (the Reticulum/LAN CRDT-sync path, `lib.rs:40477+`) already dials no one — the PendingJoin rides the engine's state-root publisher (ZEB-473/474 removed the unicast fan-out, `lib.rs:41211-41262`). Nothing to change there; the 5s oneshot + `pending: true` fallback (`lib.rs:41276-41435`) remains the shared terminal behavior when no countersign arrives in time on either path.

## 5. Accepted limitation: membership-epoch rotation

Slot keys derive from the community *membership* epoch key, which rotates on revocation events. An invite minted at epoch N whose community has since rotated to N+k yields slot keys current publishers (post-restart — see ZEB-918) no longer use. **v1 accepts this:** a rotation-crossing invite loses rungs 1–4 and degrades exactly to today's admin-only behavior; rotations are rare, and the invite's admin Case-A record is epoch-independent. Documented follow-up options if this bites: publishers additionally advertise under the previous K epoch keys for a bounded window (natural extension once ZEB-918 makes derivation live-keyed), or admins re-mint invites after rotating. Note that a rotation-crossing invite's *sealed epoch key* is stale for state decryption too, and admission already self-heals that via `EpochCatchup` (`community_membership.rs:251,1854`) — the limitation here is discovery-only.

## 6. Security analysis

- **Admission integrity — unchanged.** P2 (token minted by admin) and the full P1–P6 verify replay on every replica are untouched. A witness cannot fabricate an admission: it transports and counter-signs a token only the admin could have signed, and every replica independently re-verifies both signatures at merge. A node that is not genuinely Joined cannot produce a countersign that survives `verify_event` on anyone else's replica (its actor fails the Joined-at-prior-state check against the admin-rooted chain).
- **Malicious witness.** Worst case: accept the handshake, then drop it (one wasted ladder rung — the joiner proceeds to the next rung / pending fallback), or refuse. It cannot steal the claim for a different identity (claim-bound insert + canonical-claimant fence), cannot replay the packet usefully (same event id → `AlreadyKnown`; envelope is joiner-signed), and learns only what any Joined member learns days later from the roster (joiner identity, community id) plus the joiner's IP — see next point.
- **Decoy beacons.** The plain resolver's records are BEP44-authenticated only by possession of the epoch key (`community_rendezvous.rs:172-174` documents this deliberately: "trust is established at the handshake/admission layer"), so any epoch-key holder — member, invite holder, or leaker thereof — can publish a decoy slot. Blast radius: a wasted dial and exposure of the joiner's IP/relay to the decoy operator. This is the same posture the open-community join has shipped with since ZEB-458-era; nothing else in the system trusts these records (confirmed: consumers are `open_join_dial.rs:102` and the gateway driver only). Upgrading joiners to ZEB-827 identified resolution is structurally impossible pre-admission (no roster) and is explicitly out of scope.
- **No new metadata leak from publishing.** Invite-only communities' advertisers already publish these slots today; this design adds resolvers, not publishers.
- **Threshold interaction.** A community that later raises `invite_threshold` (ZEB-251, deferred) gates witnesses exactly as it gates counter-signers, because both use the materialized per-community value — no second knob to forget.

## 7. Failure modes

1. **Admin and all witnesses unreachable / no slots published** (small community, no opt-ins): ladder exhausts → new joiner-side error code → GUI keeps today's pending/retry UX; LAN path and ZEB-889-convergent manual retry remain. Net behavior: never worse than today.
2. **Witness accepts, response lost in transit:** ZEB-874 deferred burn means the witness burns only after transport handoff; the joiner retries (same event id via mint cache) on the next rung or later — converges via `AlreadyKnown` retransmit re-delivery.
3. **Two rungs both eventually deliver** (e.g., admin was slow, witness fast): same event id, both inserts converge; two countersigns for one PendingJoin are already legal and race-safe (production reality since ZEB-254, documented in the ZEB-888 design).
4. **Stale slot record (advertiser went offline within TTL):** dead dial, one rung wasted; pkarr TTL (7d) and re-publish triggers bound the staleness window; acceptable inside a 4-rung bounded ladder.
5. **Rotation-crossing invite:** §5 — degrades to today's behavior.

## 8. Out of scope

1. Member-side level-triggered sweep for unpaired `PendingJoin`s (edge-trigger miss class) — **ZEB-906**, where it fixes already-ingested stranded joins.
2. Rendezvous derivation reading the live epoch key + rotation overlap window — **ZEB-918**.
3. Multi-epoch slot publishing for rotation-crossing invites — noted follow-up, no ticket until evidence it bites.
4. Delegated token minting (non-admin `InviteToken.inviter`) — ZEB-250/251 governance territory; P2 unchanged.
5. ZEB-827 identified resolution for pre-admission joiners — structurally impossible without a roster.
6. Reticulum-path changes — the LAN path is already target-free.

## 9. Test plan

**Unit — acceptance (Slice 1):** witness-eligible accept end-to-end through `handle_unicast` (non-admin Joined node, valid packet → claim-bound insert + countersign response); reject non-member witness; reject Joined witness under a raised materialized `invite_threshold`; token-sig verification against admin enrolled keys (valid, and forged-token reject with the new error); admin self-accept unchanged (regression); burn remains local (witness A burns, witness B still accepts the same joiner's retry).

**Unit — ladder (Slice 2):** rung ordering and bounded attempt count; dedup of admin node id appearing in a slot; slot resolution only after rung 0 exhausts; ladder-exhausted maps to the new error code; post-handshake seed uses the dialed identity (witness rung) vs `admin_addr` (rung 0).

**Wire/format:** no new event kinds — nothing to pin in `zeb254_fixtures.rs`; new error-code strings snapshot-tested alongside the existing `RedeemInviteErrorCode::as_str()` mapping.

**E2E (headless stack):** three-profile scenario — admin mints invite for an invite-only community with one additional Joined member opted into relay advertising; admin goes offline; cold joiner redeems via `connectivity_redeem_invite_iroh` → lands Joined via the witness; admin returns and converges to the same roster. Negative: no advertisers → pending fallback (assert DTO camelCase `pending` key per e2e conventions). Rotation-crossing invite → rung 0 only, graceful degrade.
