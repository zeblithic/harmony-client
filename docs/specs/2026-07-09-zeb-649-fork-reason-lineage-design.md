# ZEB-649: Fork reason + 2D genealogy graph

**Status:** DRAFT — awaiting review
**Ticket:** [ZEB-649](https://linear.app/zeblith/issue/ZEB-649/fork-reason-and-richer-lineage-capture-the-mandatory-why-build-the-2d)
**Origin:** ZEB-609 (Commons F) deferral; design reference `docs/design/commons/references/Harmony Forks & Lineage.dc.html` Frame A + Frame D; premise ledger `docs/specs/2026-07-06-zeb-609-commons-f-fork-lineage-design.md` §0.

## 0. Premise corrections (vs the ticket text)

1. **Single-repo, not cross-repo.** The ticket says "a new field on the
   harmony-core Fork event," but the Fork membership event lives in
   harmony-client's own workspace: `MembershipEventKind::Fork`
   (`src-tauri/src/community_membership.rs:224-228`). The `harmony` git-dep
   crates (runtime/identity/content/…) are not involved. Everything ships in
   one harmony-client PR.
2. **Silent forks emit no event.** `silent: true` skips the mint/publish
   block entirely (`community_fork.rs:735`) — so a silent fork's reason never
   reaches the parent community. The reason is still captured (mandatory in
   the dialog regardless) and still lands on the fork's own state (§2.3):
   "silent" means *don't notify the parent*, not *the fork has no why*.
3. **Descendants are derived, not materialized.** "Forks of this community"
   is an on-demand projection over the membership log's Fork events
   (`build_fork_descendants`, `lib.rs:20081`), so surfacing reason there is a
   pure projection change. Ancestor lineage, by contrast, flows through the
   invite snapshot (`PreForkSnapshot` → `CommunityState.parent_lineage`), a
   separately wire-pinned path — that's why §2.3 is its own slice.

## 1. Goal

Capture a **mandatory fork reason** (the design's centerpiece: "⑂ Treasury
split. A faction wanted a larger reserve floor…") at fork time, persist it on
the Fork event, and surface it everywhere the lineage renders. Then build the
deferred **2D genealogy graph + inspect panel** (design Frame A), which the
reason field finally makes compelling.

## 2. Data model

### 2.1 Wire change — the one field, and the one trap

Mirror the Kick/Unban moderation-reason precedent exactly
(`community_membership.rs:93-99`):

```rust
#[serde(rename = "x")]
Fork {
    #[serde(rename = "fs")]
    fork_space_id: SpaceId,
    /// ZEB-649: forker's stated reason. Optional ON THE WIRE for
    /// backward-decode-compat; mandatory at the IPC layer (§3).
    #[serde(rename = "rs", skip_serializing_if = "Option::is_none", default)]
    reason: Option<String>,
},
```

- `rs` reuses the established moderation-reason key and keeps the variant's
  same-length-keys invariant (`fs`/`rs`, both 2 chars).
- **Why `Option` + `skip_serializing_if` is load-bearing:** `verify_signature`
  re-encodes the decoded `EventPayload` and `verify_strict`s it
  (`community_membership.rs:1152`). A mandatory bare `String` with
  `#[serde(default)]` would decode old events to `""` but re-encode them
  *with* `rs: ""` → byte divergence → **every pre-existing Fork event fails
  signature verification**. With `Option` + skip, old events decode to `None`
  and re-encode byte-identically. The existing pinned fixture
  (`tests/wire_format/zeb285_fixtures.rs:89-100`) must stay byte-identical —
  that unchanged fixture IS the compat proof; a new fixture pins
  Fork-with-reason.
- **Length cap:** reuse `MAX_MODERATION_REASON_CHARS` (280 codepoints,
  `community_membership.rs:3924`) enforced in the Fork arm of `verify_event`
  → existing `VerifyError::ReasonTooLong`, identical to Kick
  (`:3196-3200`). Defense-in-depth: a peer can't bypass the UI cap.

### 2.2 Known protocol consequence (accepted)

Because `kind` sits inside the signed preimage, a node on the **current**
build receiving a Fork event *with* a reason drops the unknown `rs` key on
decode, re-encodes without it, and rejects the event as `SignatureInvalid`.
Degradation is benign and bounded: the old node misses that fork's row in
"forks of this community" until it upgrades (fork materialization is a no-op
annotation; nothing else breaks). Per the ZEB-623 versioning policy this is
an additive feature needing no ALPN bump. Fleet nodes upgrade together;
accepted as-is.

### 2.3 Fork-side state (divider + ancestor reasons)

The fork's own members are the audience who most needs the "why". Two
additions, same additive `Option` + skip pattern:

- **Own reason:** new `fork_reason: Option<String>` (key `"fr"`) on
  `CommunityState` beside `forked_from`/`"ff"`, stamped at fork creation in
  `community_fork.rs` (the forker has it locally; no event round-trip).
- **Per-hop ancestor reasons:** `reason: Option<String>` (key `"rs"`) on
  `ParentLineageEntry` (`community_invite.rs`: `si`/`nm`/`at`), threaded
  through `PreForkSnapshot` → `CommunityInvitePayload` so joiners inherit it.
  The fork's own entry in the lineage it hands to *its* future forks carries
  its `fork_reason` forward — reasons accumulate down the chain.

Invite/snapshot fixtures gain new with-reason pins; existing pins stay
byte-identical (same `Option` argument as §2.1). Old-node consequence: an
invite payload with `rs`/`fr` present — the invite app-sig wrapper verifies
the signature over `signed_bytes` **exactly as transmitted**
(`community_invite.rs:679-681`, `verify_strict(signed_bytes, …)` at `:1899`),
not a decode-re-encode like membership events, so old nodes verify fine, drop
the unknown keys at decode, and join with reason-less lineage. Strictly
better than §2.2. (Plan phase re-verifies no *manual* sig preimage —
`canonical_invite_token_bytes` — embeds lineage entries; ZEB-623 rule 3.)

## 3. IPC + read path

- `ForkCommunityOpts` (`community_fork.rs:135-148`) gains
  `reason: String` (**mandatory here** — plain field, no default). The
  `fork_community` command validates `!reason.trim().is_empty()` and the
  280-codepoint cap *before* `mint_fork_event`, returning a string error
  (matches existing fork error surfacing in the dialog's parent). The wire
  event gets `Some(reason)` always; `Option` is purely a decode-compat shape.
- `ForkDescendantDto` (`lib.rs:20058`) gains `reason: Option<String>`,
  populated in `build_fork_descendants` from the matched event. TS
  `ForkDescendantDto` (`types.ts:375`) gains `reason: string | null`.
- `PhaseTwoCommunityLineageDto` (→ TS `CommunityLineageDto`) gains
  `fork_reason: Option<String>` (self's own why, from §2.3) and
  `ParentLineageDto` gains `reason: Option<String>` (per-hop).
- Phase-1 lineage (`{originalCommunityName, forkedAtMs, snapshotMessageCount}`
  consumed by the divider) gains `forkReason: string | null`.
- Fork/lineage commands are Tauri-only today (no `*_impl`/rpc.rs seam —
  confirmed; the ZEB-445 registry doesn't carry them). No rpc.rs change.

## 4. Capture UI — ForkConfirmDialog

New **mandatory** field between the name input and the toggles: a labeled
`<textarea maxlength="280">` — "Why is this fork happening?" with helper copy
"Shown in both communities' lineage. Be specific — this is the permanent
record of the split." Validation follows the dialog's existing idiom (derived
`reasonValid = reason.trim().length > 0`; confirm button disabled-gate; no
inline error text). `onConfirm` payload becomes
`{name, silent, alsoLeave, reason}`; `CommunityService.forkCommunity` opts
type and the `fork_community` invoke pass it through. Mandatory even for
silent forks (§0.2). (UI `maxlength` counts UTF-16 units vs the wire's 280
codepoints — same accepted skew as moderation reasons,
`community_membership.rs:3911-3923`.)

## 5. Surfacing (existing components)

1. **Settings → "forks of this community"** (`ForkLineageTree` descendant
   cards): reason line under each descendant row, clay-tinted quote per the
   design's reason cards. Falls back to nothing when `null` (old events).
2. **Fork divider band** (`ChannelMessageFeed.svelte:901-914`): the design's
   reason quote — a second line `"{forkReason}"` under "Forked from {name}"
   when present.
3. **Ancestor lineage cards** (`ForkLineageTree` ancestor rows): per-hop
   reason as the card's sub-line when present.

## 6. The 2D genealogy graph (design Frame A)

New `ForkGenealogyGraph.svelte`, opened as a large `Modal` from a new
"View genealogy →" button in the settings Forks section (Frame D's deferred
button; the vertical `role=tree` list stays — it's the compact, a11y-solid
in-settings view and the graph's accessible sibling).

- **Honest topology** (what our DTOs actually know): a vertical **chain** of
  ancestors (root → … → parent) down to **self**, then self's direct
  descendants **fanned horizontally** below — a caterpillar, not the mock's
  full tree. We cannot query descendants of ancestors or of descendants
  (`list_community_forks` is Joined-gated), so the mock's sibling branches
  are unknowable; the layout renders what's real.
- **Rendering:** the mock's own structure — a `position:relative` stage with
  one `<svg>` connector layer (clay elbow paths + junction dots,
  `M x y V … H …` segments; stroke
  `color-mix(in srgb, var(--gov-clay) 35%, transparent)`) under
  absolutely-positioned **HTML node cards** (avatar chip, name, date sub-line,
  membership badge — reusing `ForkLineageTree`'s card/badge classes). HTML
  cards over SVG keeps nodes real `<button>`s (keyboard + vitest-queryable,
  the `DelegationGraph` lesson) while connectors stay pure SVG. Layout is
  deterministic (chain + fan) — computed in a pure TS helper
  (`fork-genealogy-layout.ts`) with unit tests; **no d3, no force
  simulation**.
- **Edge labels:** mono chips on each edge — `⑂ {Mon YYYY}` plus the reason
  snippet (truncated ~40 chars) when known. Clay structural accent only —
  **no sage/clay dispute coding** (ledger §0.2 stands).
- **Inspect panel** (right column of the modal, `INSPECTING` eyebrow):
  selected node's name + avatar, founded/forked date, `You are here /
  Member / not joined` badge, and — for **self only** (the only node whose
  descendants we know) — a "Direct forks: N" stat and the per-fork **reason
  cards** (name + date + full reason text). Node click selects; second
  click/`Open` action navigates via the existing `onNavigate` chain for
  locally-known nodes.
- **Dropped from the mock, per the ledger:** member counts everywhere
  ("142 members", the Members stat tile), dispute/amicable edge colors,
  "signed by N founders", forker display names (still `null` pending
  ZEB-281 → "an unknown member").

## 7. Explicitly not in scope

Everything in the ticket's NOT-in-scope list (dispute classifier, member
counts, founder signatures, display names), plus: reason editing after fork
(it's a signed event — immutable by construction), reason on the rpc.rs
headless surface, and any route-level lineage page (modal only).

## 8. Tests

- **Wire:** existing `fork_event_canonical_cbor_pinned` stays byte-identical
  (the compat proof — assert untouched); new fixture pins Fork-with-reason
  bytes; `all_variants_cbor_roundtrip` + `fork_event_cbor_roundtrip` extended;
  verify_event reject test at 281 codepoints (mirror
  `kick_event_rejected_when_reason_exceeds_max_chars`); old-event-decodes-to-
  None round-trip; invite-snapshot with-reason pin beside existing pins.
- **Rust flow:** `fork_community` rejects empty/oversize reason pre-mint;
  `build_fork_descendants` projects reason; lineage DTOs carry
  `fork_reason`/per-hop reasons; joiner inherits reasons via snapshot.
- **Frontend:** ForkConfirmDialog payload test extended (`{name, silent,
  alsoLeave, reason}`), mandatory-gate test, maxlength attr;
  ForkLineageTree reason lines (present/null); divider quote
  (present/absent); `fork-genealogy-layout.ts` unit tests (chain+fan
  coordinates, deterministic); `ForkGenealogyGraph` DOM tests
  (DelegationGraph style: counts + classes + selection + navigate, no
  position pinning); CommunitySettingsPanel opens the modal.
- **Gates:** cargo fmt/clippy `--all-targets --features test-fixtures`;
  `scripts/test-select --context task` iteratively; full frontend gates;
  CI full sweep.

## 9. Delivery

One harmony-client PR (single-repo per §0.1), built as three plan phases:
**A** event field + IPC validation + descendant projection + dialog capture
(the irreducible core), **B** fork-side state + snapshot threading (divider +
ancestor reasons), **C** the genealogy graph + inspect panel. A and B are
Rust-heavy with fixture work; C is frontend-only. If review finds the PR too
large, C splits cleanly into a follow-up PR — A+B stay together (one wire
story, one fixture review).

## 10. Follow-ups (not this ticket)

- ZEB-281 display names auto-upgrade the graph's "an unknown member" labels.
- Reason on the headless rpc.rs surface if agents ever fork communities.
- Full sibling-branch topology if a future descendant-gossip primitive lands.
