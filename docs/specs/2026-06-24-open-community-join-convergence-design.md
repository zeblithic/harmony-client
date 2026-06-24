# Open-Community Join Convergence — Design Spec

**Ticket:** ZEB-558 (open-community join-by-URL deadlocks). Parent: ZEB-327 (alpha). Found in the ZEB-557 fleet live smoke.
**Status:** design approved (B1 chosen over B2 after scope discovery); pending spec review.
**Author:** Koya, 2026-06-24.

---

## Goal

Make **open-community join by URL converge** between two real nodes. Today a joiner who redeems an open invite and the community's creator **mutually reject** each other's community-state publishes (`publisher_not_joined`) and never converge — the joiner is permanently "joined but empty" (no channels, no roster, no messaging). Fix it so that, once a joiner redeems an open invite, both nodes materialize each other as `Joined` and the channel/roster/message flow comes up — **without requiring the admin to be online or a working pkarr resolve**.

## Background — the deadlock

Community state syncs as encrypted **root publishes** over the relay/zenoh transport. Each receiver runs `handle_incoming_publish` (`community_state_sync.rs:2991`), whose **membership gate** (lines 3127–3159) rejects a publish unless the publisher is already `Joined` in the receiver's **local** event log, evaluated at the publish HLC via `prior_state_at_hlc`. An unknown publisher materializes to the synthesized sentinel `status: Left, left_at: None` (`status_now.unwrap_or(MemberStatus::Left)`, line 3152) and is rejected.

For an **open** community this deadlocks every brand-new join, symmetrically:

- **Joiner → creator's publish:** the joiner's CRDT has no `Join` for the creator (the open redeem path seeds only the joiner's own self-Join, `lib.rs:25920`; the open invite ships `admin_bootstrap: None`, `lib.rs:22326`), so the joiner rejects the creator's publish.
- **Creator → joiner's publish:** the creator's CRDT has no `Join` for the joiner; the joiner's self-Join lives only inside the joiner's own encrypted blob, so the creator rejects the joiner's publish.

Neither can learn the other is joined, because that fact lives only in the state each is rejecting. The in-code comment at `community_state_sync.rs:3105–3123` names this **Case B** (open self-Join only in own blob) and marks it **DEFERRED**, noting it "require[s] a gate redesign (blob pre-decrypt or self-publisher-bootstrap)."

Invite-only doesn't hit this: it seeds `admin_bootstrap` from the URL (closing the joiner→creator direction, "Case A"), and an iroh counter-sign handshake delivers the joiner's Join to the admin (closing the creator→joiner direction). Open communities use neither mechanism.

## Why B1, not B2 (the unicast mirror)

The originally-favored fix (B2 — mirror invite-only's "unicast" of the joiner's Join to the admin) was reassessed after code mapping:

- The Reticulum unicast is **dormant** (removed in ZEB-473/Move-1a). The live invite-only delivery is an ~800-line iroh handshake (`connectivity_redeem_invite_iroh_inner`, `lib.rs:42964`) whose Case-A guards **require `invite_token` + `admin_identity_pub`** — open communities don't route through it at all.
- B2 would therefore mean: populate the open invite, relax those guards for open, add an open packet variant, add an open branch to `handle_unicast`, change frontend routing — all in the most-scarred subsystem (ZEB-260/325/339/427/473/501) — and it still depends on the **flaky pkarr resolve** (ZEB-557 finding #4) and an **online admin**.

B1 fixes **both** rejection directions in **one** place, with no invite/handshake/frontend churn, and is robust to an offline admin and to pkarr flakiness. The membership gate is explicitly a **DoS pre-filter, not the authoritative security boundary** (the authoritative check is the publisher-sig verify at step 4), which makes a scoped, open-only relaxation defensible.

## Architectural constraint that shapes the design

`CommunityRootPublishPayload` (`community_state_sync.rs:220–253`) carries only a **`root_cid`** plus `publisher_addr`, `at`, `publisher_sig`, `epoch`. The publisher's membership events are **not** in the payload — they live in the encrypted `CommunityState` blob, which is fetched from CAS (line ~3207), decrypted (~3224), and decoded into `remote.events` (~3269) **after** the membership gate and the publisher-sig check.

So the publisher's self-`Join` is **not visible at the gate**. B1 cannot "peek at the payload"; it must **defer** the unknown-open-publisher decision until after the blob is decoded, then validate the self-Join carried inside it.

## Design — deferred self-Join admission (open + unknown publisher only)

Restructure `handle_incoming_publish` so the membership decision has three branches at the gate:

1. **Publisher is `Joined` locally** → unchanged. Use the materialized `MemberState` for the existing publisher-sig verify; proceed on the current cheap path. (No extra cost for the common case.)
2. **Publisher not `Joined` AND (invite-only OR publisher is known-but-`Left`/`Banned`)** → unchanged: reject `PublisherNotJoined` immediately. Invite-only retains the strict pre-decrypt reject; Case C (re-join after Leave) is **out of scope** here (see below).
3. **Publisher not `Joined` AND community is open AND publisher is entirely unknown** (`members.get(publisher) == None`, `!ctx.is_invite_only`) → **defer**. Do not reject and do not run the prior-state publisher-sig check yet. Fetch + decrypt + decode the blob (the step that already exists later in the function), then run the **bootstrap-admission** check below before merging.

### Bootstrap-admission check (the new logic)

After the blob is decoded to `remote: CommunityState`, for the deferred case:

1. **Locate the candidate self-Join:** find an event in `remote.events` where `actor == payload.publisher_addr` and `kind == Join`. If none → reject `PublisherNotJoined` (an open publisher must carry their own Join).
2. **Validate the Join under the open-join rule:** reuse the existing authorization path so gate and merge can never diverge —
   - `enrolled_key_from_cert(&candidate_join)` (`community_membership.rs:1314`) — verifies the `EnrollmentCert`, binds `cert.owner_id == actor`, and yields the device ed25519 key.
   - Confirm the candidate is a valid **open** Join (signature-alone authorization, no countersig/power gate — exactly what `verify_event` applies for `is_invite_only == false`). Prefer running the candidate through the same `verify_event`/materialize path used at merge (e.g. materialize `local_events + candidate` via `prior_state_at_hlc` and assert the publisher resolves to `Joined` with non-empty `enrolled_device_keys`) rather than re-deriving the rule inline.
3. **Verify the root publish is authored by that device:** run the existing `verify_publisher_sig` (`community_state_sync.rs:2964`) against an enrolled-key set seeded from the validated candidate Join. This proves the same device that holds the self-Join also signed this root publish — i.e. the admission is exactly as strong as a normal member publish.
4. **Admit** → fall through to the normal merge (lines ~3293+), which inserts the publisher's self-Join via `insert_event` → `verify_event`. After merge the publisher is `Joined` locally, so all subsequent publishes take the cheap path (branch 1).

If any of steps 1–3 fail → reject `PublisherNotJoined` (unchanged error surface; no new leniency beyond a cert-valid, signature-valid open self-Join).

### Convergence (why both directions close)

With branch 3 active on **both** nodes:

- Joiner receives creator's publish → creator unknown → creator's blob carries the creator's bootstrap self-Join → validated → admitted → joiner now has creator `Joined`.
- Creator receives joiner's publish → joiner unknown → joiner's blob carries the joiner's self-Join → validated → admitted → creator now has joiner `Joined`.

Both rosters now contain both members; subsequent publishes pass branch 1; channels/roster/messages come up. No `admin_bootstrap` seed, no invite change, no handshake, no online-admin or pkarr dependency.

### Integration points to handle carefully

- **TOCTOU re-check (~line 3308):** the pre-merge re-materialize currently re-asserts the publisher is `Joined` under the state lock. For the deferred case the publisher is *being* admitted by this very merge, so the re-check must treat the validated candidate self-Join as establishing membership (e.g. include the candidate in the re-materialized set, or skip the redundant assert when bootstrap-admission already passed). Must not reintroduce the rejection it just bypassed.
- **Replay tracker (~line 3196):** unchanged semantics (never advances on rejection).
- **Scope guard:** every new branch is gated on `!ctx.is_invite_only` **and** publisher-unknown. Invite-only and known-but-not-Joined publishers are untouched.

## Security & DoS analysis

- **Authorization unchanged in strength.** Admission requires (a) a valid `EnrollmentCert`-bearing self-`Join` for the publisher (cert verified, owner-bound) and (b) a `publisher_sig` over the root that verifies against that Join's device key. An attacker cannot forge either. The only new *capability* is that an unknown party can introduce themselves — which is the definition of an open community.
- **Gate role.** The membership gate is documented as a cheapest-first **DoS pre-filter**; the authoritative security gate is the publisher-sig verify, which we still run (against the in-blob-sourced key). Relaxing the pre-filter for open + unknown publishers does not weaken the authoritative gate.
- **DoS surface.** The deferred path pays a CAS fetch + decrypt for an unknown open publisher. Bound: the wire must first decrypt under the community epoch key (line ~3037), so only parties holding the open invite's epoch key can reach this path — the same exposure as any open member. **Hardening considered:** cap concurrent deferred admissions / rate-limit per publisher_addr. Deferred to a follow-up unless review deems it alpha-blocking; the epoch-key gate is the operative bound for alpha.

## Out of scope

- **Case C** (self re-join after `Leave`): shares the root cause but the publisher is *known-but-Left*, not unknown; folding it widens the branch-3 predicate and the re-materialize semantics. File as a focused follow-up if needed; **not** in this change.
- **`join_open_community` pkarr warm-up 500** (ZEB-557 finding #4): orthogonal transport flakiness; separate ticket. B1 makes it non-load-bearing (open join converges via relay without it), but does not fix it.
- **B2** (iroh-handshake direct delivery): superseded (ZEB-563 canceled).
- No frontend, `generate_invite`, invite-payload, or handshake changes.

## Components / files

| File | Change |
|---|---|
| `src-tauri/src/community_state_sync.rs` | `handle_incoming_publish`: add the deferred bootstrap-admission branch (open + unknown publisher); seed enrolled keys from the in-blob self-Join; adjust the TOCTOU re-check for the deferred case. Likely a small private helper `bootstrap_admit_open_publisher(remote_events, publisher_addr, admin_addr) -> Option<MemberState>` for testability. |
| `src-tauri/src/community_membership.rs` | Reuse `enrolled_key_from_cert`, `prior_state_at_hlc`/`materialize_with_now`. Add a helper only if needed to materialize "local + one candidate Join" cleanly. No rule changes. |
| `src-tauri/tests/community_sync/community_open_flow_integration.rs` | New two-node convergence test (centerpiece). |
| `src-tauri/tests/community_sync/community_sync_engine_unit.rs` (or a new unit module) | Unit tests for the bootstrap-admission helper: valid self-Join admits; missing Join rejects; bad cert rejects; wrong publisher_sig rejects; invite-only never admits. |

## Error handling

- All failure paths in the deferred branch return the existing `CommunitySyncError::PublisherNotJoined { addr, status, left_at }` so the frontend's `community-state-sync-degraded` surface is unchanged. No new error variants required.
- The blob fetch/decrypt failures in the deferred path reuse the existing `Crypto`/`CborDecode`/CAS error handling already present after the gate.

## Testing

**Centerpiece — two-node open-join convergence integration test** (mirror the harness in `community_open_flow_integration.rs::open_community_create_redeem_leave_round_trip`):

1. Build two engines (`is_invite_only: false`) over a shared in-memory CAS with bidirectional publish↔subscriber forwarding and **separate** `CommunityState` per engine.
2. Node A (admin) mints creation (`mint_community_creation`), inserts its self-Join, publishes.
3. Node B (joiner) mints an open redemption (`mint_redemption`, open path), inserts its self-Join, **without** any pre-seed of A's Join — the real deadlock setup.
4. Drive both engines and `wait_until` (poll, no fixed sleep — per the logical-time/condition-wait convention) until **both** states materialize **both** members as `Joined`.
5. Assert: A's CRDT contains B's Join and vice-versa; both rosters = {A, B}; no lingering `PublisherNotJoined`.
6. A pre-fix run of this test must **fail** at the deadlock (proves the test has teeth) before the gate change makes it pass.

**Unit tests** for `bootstrap_admit_open_publisher`: valid self-Join → `Some(MemberState)` with seeded enrolled keys; no Join for publisher → `None`; tampered/invalid cert → `None`; publisher_sig mismatch → reject; **invite-only community → never admits** (regression guard).

**Regression:** the existing `community_open_flow_integration.rs` + `community_sync_integration.rs` + invite-only integration tests must stay green (invite-only strict-reject behavior unchanged).

**Gates** (per `CLAUDE.md`): `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked -p harmony-app --features test-fixtures` (scope to the touched tests during dev, full `--all-targets` for the final sweep); frontend gates are unaffected but run `tsc`/`vitest` once to confirm no accidental coupling.

## Definition of done

- The two-node open-join convergence test passes (and demonstrably failed pre-fix).
- Invite-only and known-member paths are untouched (regression suite green).
- ZEB-558 repro (open create → open redeem → both rosters populate, channels instantiate) converges in a local two-engine run.
- All gates green; PR opened (branch `open-community-gate-self-join-admission`, no ZEB id in branch/commit/PR-title; PR body references ZEB-558 descriptively, not as a Linear magic-word close, to avoid the parent-cascade).
