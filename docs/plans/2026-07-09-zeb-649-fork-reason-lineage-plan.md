# ZEB-649 Implementation Plan — fork reason + genealogy graph

> Executing inline (Koya). Spec: `docs/specs/2026-07-09-zeb-649-fork-reason-lineage-design.md`
> (approved 2026-07-09). Design detail lives in the spec; this plan is
> sequencing + verification. One harmony-client PR, three phases (spec §9).

## Global constraints

- Wire shape (spec §2.1): `reason: Option<String>`, key `"rs"`,
  `skip_serializing_if = "Option::is_none"`, `default`. The existing
  `fork_event_canonical_cbor_pinned` fixture MUST stay byte-identical.
- Mandatory at IPC layer only: `fork_community` rejects empty/oversize
  reason before mint (spec §3). Cap = `MAX_MODERATION_REASON_CHARS` (280
  codepoints) in `verify_event` Fork arm → `VerifyError::ReasonTooLong`.
- No sage/clay dispute coding, no member counts, no display names (ledger).
- Iterative gates: `scripts/test-select --context task` + targeted nextest
  `-E` filters; full `--workspace --all-targets` is CI's job. Frontend:
  `npx tsc --noEmit` + targeted vitest per task, full run at the end.
- Commit per task; trailers as always.

## Phase A — capture + event + descendant projection

- [ ] **A1. Wire field + fixtures** (`community_membership.rs`,
  `tests/wire_format/zeb285_fixtures.rs`)
  Red: extend `fork_event_cbor_roundtrip` to assert `rs`; add
  `fork_event_with_reason_canonical_cbor_pinned` (new fixture, generated
  bytes); add `fork_event_without_reason_decodes_none_and_reencodes_identical`
  (old-event compat); add verify_event reject test at 281 codepoints
  (mirror `kick_event_rejected_when_reason_exceeds_max_chars`); assert
  existing pinned hex untouched (it must not change — compile guard).
  Green: add field per spec §2.1 code; extend `all_variants_cbor_roundtrip`
  literal; add Fork arm cap check in `verify_event` beside the Kick check.
  Gate: `cargo nextest run --locked --features test-fixtures -E
  'test(fork_event) or test(all_variants) or test(reason_exceeds)'` + fmt.

- [ ] **A2. IPC mandatory reason** (`community_fork.rs`)
  `ForkCommunityOpts` gains `reason: String` (no default — TS always sends
  it). `fork_community` validates trim-non-empty + ≤280 codepoints before
  Step 7 mint; `mint_fork_event` threads `Some(reason)`. Tests: opts CBOR/
  JSON deserialization with reason; validation reject (empty, 281); mint
  carries reason into the event (existing fork-flow test extension).
  Gate: targeted nextest `-E 'test(fork)'` on the touched crate + clippy.

- [ ] **A3. Descendant projection** (`lib.rs`)
  `ForkDescendantDto.reason: Option<String>` + populate in
  `build_fork_descendants`. Test: descendant DTO carries reason; old
  events → `None`.

- [ ] **A4. Dialog capture** (`ForkConfirmDialog.svelte`,
  `community-service.ts`, `CommunityView.svelte` onFork chain, tests)
  Mandatory textarea (maxlength 280, label "Why is this fork happening?"),
  `reasonValid` disabled-gate joined with `nameValid`; payload
  `{name, silent, alsoLeave, reason}`; service opts type + invoke
  passthrough. Tests: payload pin updated, gate test (empty reason blocks),
  maxlength attr, existing tests keep passing.

## Phase B — fork-side state + snapshot threading

- [ ] **B1. Own reason + per-hop lineage** (`community_state_crdt.rs`,
  `community_fork.rs`, `community_invite.rs`)
  `CommunityState.fork_reason` (`"fr"`, Option+skip+default) stamped at
  fork creation; `ParentLineageEntry.reason` (`"rs"`, same shape);
  `PreForkSnapshot`/invite payload threading so joiners inherit; the fork's
  own lineage entry handed to ITS forks carries `fork_reason` forward.
  Verify no manual sig preimage (`canonical_invite_token_bytes`) embeds
  lineage entries (spec §2.3 checkpoint). Tests: state roundtrip with/
  without `fr`; snapshot pin with-reason added, existing pins untouched;
  joiner-inherits test.

- [ ] **B2. Lineage DTOs + surfacing** (`lib.rs`, `types.ts`,
  `ChannelMessageFeed.svelte`, `ForkLineageTree.svelte`, tests)
  `CommunityLineageDto.forkReason` + `ParentLineageDto.reason`; Phase-1
  lineage `forkReason`; divider quote line under "Forked from {name}";
  ForkLineageTree reason sub-lines on descendant + ancestor cards
  (null-safe). Tests per spec §8.

## Phase C — genealogy graph

- [ ] **C1. Layout helper** (`src/lib/fork-genealogy-layout.ts` + test)
  Pure function: `(lineage, descendants) → {nodes: [{spaceId, kind:
  root|ancestor|self|descendant, x, y, …}], edges: [{fromXY, toXY,
  elbowPath, label}]}`. Chain vertical, fan horizontal, deterministic
  spacing constants. Unit tests pin coordinates for 0/1/3-ancestor ×
  0/1/4-descendant matrices.

- [ ] **C2. `ForkGenealogyGraph.svelte`** — SVG connector layer under
  absolutely-positioned HTML card buttons (reuse ForkLineageTree card/badge
  classes); inspect panel (selected node, dates, badge, self-only "Direct
  forks: N" + reason cards); `onNavigate` for locally-known. DOM tests,
  DelegationGraph-style (counts/classes/selection, no position pins).

- [ ] **C3. Settings integration** — "View genealogy →" button in Forks
  section opening a large `Modal` hosting the graph; CommunitySettingsPanel
  test.

## Finish

- [ ] Local iterative gates before the PR: fmt + clippy --all-targets +
  targeted nextest sweeps + tsc + full vitest. `scripts/test-select` is a
  LOCAL ITERATIVE aid only — final validation is CI's full
  `cargo nextest run --locked --workspace --all-targets --features
  test-fixtures` (the rust-test job), never test-select.
- [ ] PR + `@coderabbitai review` once + converge (all three buckets, one
  commit/push per round).
