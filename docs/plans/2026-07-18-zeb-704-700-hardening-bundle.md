# ZEB-704 + ZEB-700 — hardening bundle (dial-route freshness + friend/v1 rate limiter)

**Branch:** `zeb-704-700-dial-freshness-friend-rate-limiter` off `main@bcccb2f9` (post-#489).
**Tickets:** ZEB-704 (CodeRabbit finding from PR #481, pre-existing), ZEB-700 (ZEB-680 final-review finding). Both Low, bundled per the one-PR-per-repo rule.

## Verified current state (first-hand on this branch)

| # | Fact | Where |
|---|------|-------|
| V1 | `resolve_by_node_id` / `resolve_entry_by_node_id` use `.find()` on the `BTreeMap<(OwnerAddr, [u8;32]), ResolverSlots>` — FIRST matching `(owner, node_id)` key in owner-address order wins, then `freshest()` runs only within that entry's slots | `reachability_resolver.rs:522-549` |
| V2 | `boot_seed_node_ids_by_recency` dedupes keep-freshest-across-owners per node-id off `list_dialable_peers` — so seed ranking and dial resolution can disagree when one node-id exists under two owners | `iroh_zenoh_registration.rs:92-104` |
| V3 | `freshest()` orders by `effective_announced_at_ms`, ties → source rank (durable 2 > pkarr 1 > fleet 0); the rank fn is local to `freshest()` | `reachability_resolver.rs:160-188` |
| V4 | ZEB-694 gave the PEX ALPN a two-tier `IntroRateLimiter`: Tier 1 pre-auth `admit_connection(remote_id)` shield, Tier 2 post-auth per-owner window+dedupe; shed = log + SAME benign ack (no oracle); `KeyedSlidingWindow`/`KeyedDedupe` primitives are private to `friend_intro.rs`, bounded (`MAX_WINDOW_KEYS`/`MAX_DEDUPE_ENTRIES`), locks never held across `.await` | `friend_intro.rs:571-856`, `iroh_pex_acceptor.rs:598-659` |
| V5 | friend/v1 (`handle_friend_handshake_inbound`) has NO limiter: accept_bi → bounded read → decode → `authenticate_friend_request` (cert chain + sig + ≤32 carried revocation attestations ≈ up to ~64 ed25519 verifies) → consent tree | `iroh_friend_acceptor.rs:2021-2068` |
| V6 | friend/v1's benign outcome for an unknown owner is the `Pending` reply (record + prompt); the reply path is `encode_friend_response(&FriendLinkResponse::Pending)` + `write_friend_response` | `iroh_friend_acceptor.rs:2225-2243,1918-1951` |
| V7 | Legit re-dial flows exist within minutes: request → `Pending` → user approves → requester re-dials → inline accept; and `Pending` → token obtained → re-dial token path | `iroh_friend_acceptor.rs:1137-1157,2136-2139` |
| V8 | Acceptor is builder-constructed (`with_config` + `with_*` chain); live-endpoint serve tests exist (`serve_refuses_revoked_requester` et al.) | `iroh_friend_acceptor.rs:1673-1860,5141-5330` |

## ZEB-704 design

Make both reverse lookups select the **globally freshest** matching entry across owners:

- Hoist the source-rank fn to module scope (`source_rank`), keep `freshest()` using it.
- Shared helper: filter all `(owner, node_id)` keys matching the target node-id → `freshest()` per entry → reduce keeping the candidate only on **strictly greater** `(effective_announced_at_ms, source_rank)`; full ties keep the FIRST owner in BTreeMap order (today's behavior for the degenerate all-tie case — deterministic, pinned by test).
- `resolve_by_node_id` + `resolve_entry_by_node_id` both delegate; callers unchanged.

Cross-owner tie → source rank mirrors the within-entry slot semantics (a verified durable record beats an unsigned fleet one at equal freshness).

## ZEB-700 design

New `FriendRateLimiter` in `friend_intro.rs` (same module → reuses the private audited primitives), disjoint budgets from the intro limiter:

- Tier 1 `admit_connection(remote_id, now_ms)`: `KeyedSlidingWindow<[u8;32]>`, cap `FRIEND_HANDSHAKE_PER_CONNECTION_MAX = 40` / 1h (`FRIEND_HANDSHAKE_WINDOW_MS`) — parity with the intro shield's rationale (one endpoint may retry; a flood is still shed).
- Tier 2 `admit_owner(owner, now_ms)`: `KeyedSlidingWindow<OwnerAddr>`, cap `FRIEND_HANDSHAKE_PER_OWNER_MAX = 20` / 1h.
- **Deliberately NO dedupe tier** (divergence from the intro limiter, documented in-code): V7's legit flows re-dial the same `(requester, acceptor)` pair within minutes; a `(owner, owner)` dedupe TTL would shed the approval re-dial and the post-`Pending` token redemption. The 20/h window never sheds those (2-3 dials/h) while still bounding a flood.

Wiring in `handle_friend_handshake_inbound`:

- Tier 1 after the bounded body read, BEFORE decode + all crypto (cheaper than the PEX placement, which only sits post-decode because it must select an arm; friend/v1 has one arm).
- Tier 2 immediately after `authenticate_friend_request` succeeds (owner now authenticated), before any lock/consent work.
- Shed = `tracing::warn!` ("no silent truncation") + write the SAME `Pending` reply V6 identifies as the benign outcome + `Ok(())`. **Zero state effect**: nothing recorded in `pending_requests`, no token consumed, no friend written — an honest peer that somehow trips the cap self-heals by re-dialing after the window.
- Field `rate_limiter: Arc<FriendRateLimiter>` defaulted in `with_config`; `with_rate_limiter` builder for tiny-cap tests.

No-oracle note: a shed `Pending` is network-indistinguishable from the Path-A "recorded, awaiting accept" outcome, and reveals nothing about consent/referral state. For a token-carrying requester it is a lie that self-heals (token stays live and unconsumed; re-dial redeems it).

## Test plan (red-first where the behavior exists to pin)

1. ZEB-704 (`reachability_resolver.rs`): two-owner same-node-id, LATER owner fresher → reverse lookups must return the later owner (RED vs `.find()`); mirror variant (earlier owner fresher → earlier owner, guards accidental last-wins); full-tie determinism pin (first owner). Agreement test: `boot_seed_node_ids_by_recency` ranking and `resolve_entry_by_node_id` freshness agree for a two-owner node-id.
2. ZEB-700 unit (`friend_intro.rs`): connection cap sheds at max+1; owner cap sheds; window slides → re-admits; conn/owner budgets disjoint; zero-cap admits nothing.
3. ZEB-700 acceptor-level (live-endpoint serve test pattern, tiny caps via `with_rate_limiter`): Tier-1 shed → `Pending` on the wire + NO pending-request recorded + no friend written; Tier-2 per-owner shed likewise. Honest single-handshake behavior unchanged (existing serve tests keep passing untouched).

## Out of scope

- Secondary index for the reverse lookup (Phase-2 profiling note at `reachability_resolver.rs:517-521` stands).
- Rate limiting other ALPNs (invite/handshake-v1 has the token gate; separate family).
- Multi-owner same-node-id prevention upstream (the pathological state itself) — this bundle makes the resolution deterministic-freshest, not impossible.
