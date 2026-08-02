# ZEB-852 T-DISCOVERY — bound peer stamps in discovery/reachability registers (design)

**Ticket:** ZEB-852 (ZEB-831 wall-clock threat model, §4 HIGH/MEDIUM — findings RB, D2, D4, C7, ABK; D1 already fixed).
**Series:** fourth fix after ZEB-846 T-GOV (#581), ZEB-847 T-OWNER (#582), ZEB-849 T-CARD (#583). Same
`clock_trust` machinery.

## Threat

Discovery / reachability registers rank-or-LWW on **unbounded peer wall-clock stamps**. A peer or sibling with a
fast (future) clock can pin a slot for process life, rank itself first, or make its entry immune to eviction —
poisoning routing, censoring inbound state, or evicting honest entries. Cross-user forgery is already blocked by
signatures; the attacker is a validly-enrolled member/sibling whose signed row simply carries a skewed stamp. Six
findings, five in scope (D1 already closed).

## The central distinction — clamp vs reject vs derive

This ticket looks *opposite* to T-OWNER/T-CARD, which said **reject, never clamp**. The rule from those tickets
applies to **stored replicated newer-wins registers**, where a clamped value is receiver-dependent and would
diverge across peers. Almost every register here is the reverse: a **local, non-replicated projection** rebuilt at
each receiver from the replicated source (the community address book / the pkarr blob). Clamping such a projection
is receiver-*independent in effect* — every peer re-applies the same clamp at its own ingest, nothing propagates,
nothing diverges. That is exactly why the ticket prescribes "clamp" / "rank on `min(stamp, now)`" for these where
the earlier fixes rejected. Three shapes, chosen per register:

- **Clamp** (shape B) — for local non-replicated projections (resolvers, transient sort/filter keys). Use
  `clock_trust::clamp_future` or `stamp.min(now)`. RB, D2, D4.
- **Reject at ingest verify** (shape C) — for a verify function that gates admission, exactly like `verify_card`.
  Use `clock_trust::reject_future` with the receiver clock; `None ⇒ apply-all`. C7.
- **Derive-local** (shape D) — for a stored field that lives *outside* the signed preimage and is therefore
  attacker-rewritable: stop trusting the wire value; stamp it locally at receipt. ABK.
- Plus one genuine **stored replicated register** in scope — the D2 merge register — which takes the
  T-OWNER-style **reject** (shape A). D2-MERGE.

## Global constraints

- One house module: `clock_trust`. Constants `MAX_FORWARD_SKEW_MS` (5 min, control tier) and
  `DISPLAY_SKEW_TOLERANCE_MS` (30 min, display tier). Never introduce a new constant or a parallel skew value.
- **Tier by concern, not by convenience** (T-CARD lesson). A stamp that gates a *control / routing* decision takes
  the control tier (5 min); a stamp that only orders a display / discovery list takes the display tier (30 min).
  The `clock_trust` module doc is explicit: *do not point a new control consumer at the display tier.*
- **A bad LOCAL clock must never drop honest state** (fail-open). Ingest-reject sites read the receiver's own
  clock via `receiver_now_ms() -> Option<u64>` and treat `None` (unreadable) as **apply-all** — never substitute
  `0`. Clamp sites inherit whatever the resolver's existing `now_ms()` returns (symmetric with the clamp already
  present in the same function).
- **Reject, never clamp — only for stored replicated registers** (D2-MERGE). Local projections clamp (see above).
- **Do not grow a signed preimage** (ABK). A preimage change is a flag-day that breaks cross-peer compatibility
  (`reachability_record.rs:94-96`). Prefer relocating provenance to the receiver.
- Units: HLC `wall_ms`, `announced_at_ms`, `seen_at.wall_ms`, `ad_at`, `listed_at.wall_ms`, `stamped_at_ms` are
  all milliseconds. The resolver clamps compare ms-to-ms.

## Fixes

### RB — reachability resolver durable-slot HLC (clamp)

`ReachabilityResolver::update_with_source` (`reachability_resolver.rs`, the `DurableCrdt | FleetSibling` HLC match
arm, ~line 428). The forward-skew clamp already covers `effective_announced_at_ms` for all sources and the
`PkarrLive` HLC arm, but the `DurableCrdt | FleetSibling` arm passes the HLC through **untouched**. The stale code
comment ("Durable-CRDT HLCs are authored by the owner's own device") predates **ZEB-815**, which rerouted durable
rows through the peer-signed address book (`ingest_verified_row` feeds `DurableCrdt` with a peer-signed `row.at`).
`lww_newer` compares `hlc.wall_ms` first, so a future HLC pins the durable **slot occupancy** (what gets dialed /
re-served) for process life; `durable_butler_set` is window-exempt. The display view (`freshest()`) already reads
the clamped `effective_announced_at_ms`, so only the routing slot is exposed.

**Fix:** clamp `hlc.wall_ms` in the `DurableCrdt | FleetSibling` arm to `now + FUTURE_SKEW_TOLERANCE_MS`, mirroring
the `PkarrLive` arm directly above it (and the sibling relay-copy clamp at `address_book_sync.rs:225`). Update the
stale comment. **Shape B** (the resolver is a terminal, in-memory, non-replicated projection — clamping is
receiver-independent in effect). **Tier: control** — `FUTURE_SKEW_TOLERANCE_MS` at `reachability_resolver.rs:46`
already aliases `clock_trust::MAX_FORWARD_SKEW_MS`; reuse it. This is a routing decision.

### D2 — fleet-net butler-set order (upper-bound filter + sort clamp)

`butler_set_order` (`fleet_net.rs`, filter ~line 199, sort ~line 208). The freshness filter has only a **lower**
bound (`seen_at.wall_ms >= stale_before_ms`); a fast-clocked sibling passes it trivially and the descending sort
ranks it slot 0. `build_butler_set` embeds the result in the signed pkarr blob published to **other owners**, who
then route butler **deposits** (message delivery) to the top-ranked device — a dead device.

**Fix:** (a) add the missing **upper** bound to the filter, mirroring `fresh_butler_set`
(`reachability_record.rs:37`, which bounds `bs_at` on both sides); (b) clamp the sort key to
`min(seen_at.wall_ms, now)`. **Shape B** (read-side filter+sort, persists nothing). **Tier: control** — butler
deposit routing is the operational decision the module doc reserves the 5-min tier for.

### D2-MERGE — fleet-net stored row LWW (reject) — folded in beyond the ticket's stated scope

`FleetNetDoc::merge_from` (`fleet_net.rs:142`) LWWs stored rows by `seen_at.is_strictly_newer_than`. A future
`seen_at` freezes the replicated row so honest later updates from the same actor/device can never replace it. This
is the one genuine **stored replicated register** in scope. The D2 read-side upper-bound filter already sweeps a
frozen-future row out of the *ranking*, so this is defense-in-depth — but the register itself should refuse to
adopt a future stamp.

**Fix:** at the merge LWW, **reject** (do not adopt) an incoming row whose `seen_at.wall_ms` exceeds the receiver's
forward-skew ceiling — the T-OWNER pattern. **Shape A** (stored replicated → reject, never clamp: a clamped stored
value is receiver-dependent and diverges). **Tier: control** (5 min), symmetric with D2. Fail-open on unreadable
receiver clock.

### D4 — community relay resolver (sort clamp + store clamp)

`community_relay_resolver.rs` — read sort `relays_for_community` (~lines 62-68, sort by raw `ad_at` desc, truncate
to `COMMUNITY_RELAY_ADVERTISERS_MAX = 4`); store LWW `update` (~line 39). The sole `.update` caller
(`address_book_sync.rs:229`) already clamps `ad_at` to `now + 5 min`, but `fresh_relay_entry` accepts up to
`now + 15 min` freshness and the read sort still ranks by raw `ad_at` — so four advertisers stamped at the 5-min
ceiling outrank honest advertisers at `now` and fill all four rendezvous slots, censoring inbound community state.

**Fix:** rank on `min(ad_at, now)` in `relays_for_community` (collapses any forward advantage to zero); add the
"B6" store-side clamp at `:39` as defense-in-depth (safe — the resolver is a local non-replicated projection).
**Shape B.** **Tier:** the sort-key clamp to `now` is tier-agnostic; the existing ingest clamp stays at the
control-tier magnitude (5 min).

### C7 — library directory `listed_at` (ingest reject)

`library_directory.rs::verify_announce` (~line 437) bounds name/description and verifies the signature over the
canonical CBOR, but places **no** bound on `announce.listed_at` and has **no** receiver-clock parameter today.
`listed_at` is inside the signed CBOR (authenticated) but self-attested and unbounded against the receiver. A
future `listed_at` (a) wins the per-community LWW (`is_strictly_newer_than`) → pins the top of discovery, and (b)
is never the min in the cap-eviction "oldest" selection → **immune to eviction**, so it evicts genuine libraries
instead.

**Fix:** thread a receiver clock into `verify_announce` via `clock_trust::receiver_now_ms()` and reject when
`announce.listed_at.wall_ms` exceeds `now + DISPLAY_SKEW_TOLERANCE_MS`; `None ⇒ apply-all` (unreadable local clock
never rejects an honest announce). Thread the clock through the (few) call sites. **Shape C** (ingest reject, like
`verify_card`). **Tier: display** — pure discovery ranking + cap eviction, no operational control gated.

### ABK — address-book `stamped_at_ms` (derive-local)

`community_address_book.rs` — `AddressBookRow.stamped_at_ms` (field ~line 69), `upsert` clamp (~lines 175-178), LWW
+ TTL key (~line 201). `verify_inner_signature` (`address_book_sync.rs:196`) covers only `(payload, actor, at)`,
so `stamped_at_ms` is **outside the signed preimage**. A malicious co-member re-seals another member's
validly-signed row (via the snapshot-serve path) with a bumped `stamped_at_ms`; the inner signature still verifies.
Even clamped to `now + 5 min` it wins the book LWW over the honest current row and refreshes the TTL clock
indefinitely, keeping a stale row alive.

**Fix — Option 2 (derive-local), not Option 1.** On the **peer-ingest** path (`ingest_verified_row` → `upsert`),
stamp `stamped_at_ms = now_ms` at receipt and ignore the wire value. Leave self-authored and disk-loaded rows
as-is (they must not be re-stamped, or every load would refresh the TTL). **Shape D** (preimage integrity resolved
by relocating provenance to the receiver).

Why not Option 1 (bind `stamped_at_ms` into the signed bytes): it is a **flag-day** preimage change
(`reachability_record.rs:94-96` documents that a peer on old code rejects the extended preimage and vice-versa) →
version bump, breaks existing signed rows, cross-peer incompatibility. It is also semantically wrong —
`stamped_at_ms` is doc-commented as "the store's own admission stamp," a per-receiver value redundant with the
already-signed `announced_at_ms`/`ad_at` in the inner payload; the signer has no authority over the receiver's
admission time. Option 2 collapses the attack at a one-line boundary change with no flag-day, no compat break, no
signature-scheme growth. No routing regression: the authoritative ordering is the signed HLC (`row.at`) /
`announced_at_ms` that feeds the resolver; the book's `stamped_at_ms` is only a local freshness / TTL / LWW
heuristic, which merely becomes receipt-ordered rather than author-stamp-ordered. Keep the existing 5-min ingest
clamp as belt-and-suspenders.

## D1 — out of scope (already fixed)

`network_health.rs::list_records` already reads `list_active_peers_effective()` (the clamped
`effective_announced_at_ms`), not the raw `announced_at_ms`. This landed in commit `8e32f2c0` ("ZEB-831: bounded-
time trust policy module + three prior-fix gap closures (ZEB-818/711/621) (#580)") — the same commit that added
`clock_trust`. A regression test pins it (`prod_reachability_snapshot_last_seen_is_future_skew_clamped`,
`network_health.rs:4623`). **Do not re-fix.**

## Tests (discrimination pairs — series discipline)

Every register ships both halves: poison is stopped **and** an honest in-range entry still wins / still ranks
(proving the bound does not over-reject), plus a fail-open pin where a clock is threaded.

- **RB**: future-dated `DurableCrdt` HLC does not pin the durable slot over an honest in-range record; an honest
  newer in-range record still wins the slot.
- **D2**: a fast-clocked sibling is filtered out (upper bound) and does not rank slot 0 (sort clamp); an honest
  fresh sibling still ranks ahead of an honest stale one.
- **D2-MERGE**: `merge_from` does not adopt a future-`seen_at` row; an honest newer in-range row is adopted;
  unreadable receiver clock ⇒ apply-all (adopts).
- **D4**: four ceiling-stamped advertisers do not evict honest `now`-stamped advertisers from the four slots; an
  honest fresher advertiser still ranks ahead of an honest staler one.
- **C7**: `verify_announce` with a future `listed_at` + real `now` → rejected; an in-range announce verifies; an
  older in-range announce verifies too; `receiver_now_ms() == None` ⇒ the future announce verifies (fail-open).
- **ABK**: a replayed row with a bumped `stamped_at_ms` does not win the book LWW / refresh the TTL past an honest
  row (peer-ingest stamps receipt time); a self-authored row keeps its own stamp; a disk-loaded row is not
  re-stamped.

## Out of scope (explicit)

- D1 network-health (already fixed, #580).
- Option 1 signed-preimage growth for ABK (flag-day; rejected in favor of Option 2).
- Full re-verify-on-load of any resolver (layering; the projections rebuild from the address book).
- Tracing / metrics on the new bound sites — folds under the already-filed **ZEB-855** (uniform observability
  across all `clock_trust` reject/clamp boundaries).
