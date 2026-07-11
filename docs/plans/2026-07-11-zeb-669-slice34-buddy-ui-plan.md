# ZEB-669 Slices 3+4 — Contribution Meter + Manage Sheet + Backup Toggle + Origin Row (PR-3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore the Files-mode storage-buddy surfaces honestly on top of the slice-2 backend (#449): a nav-rail contribution meter, a Manage sheet for pacts/invites/budget, a per-file "Back up with buddies" toggle, and an origin ("From") row — per spec §4+§5 (`docs/specs/2026-07-11-zeb-669-storage-buddies-design.md`).

**Architecture:** Small Rust additions first (additive `origin` on `ContentIndexEntry`, recorded at the two real self-ingest seams + carried through the Case-D re-mint; `backup`+`origin` exposed on `ContentItemWire`), then a fresh `storage-buddy-service.ts` binding the six slice-2 IPCs + both events, then three UI surfaces (meter component in the NavPanel slot, Modal-based Manage sheet, detail-panel toggle + From row) wired prop-down from App.svelte like every other service.

**Tech Stack:** Rust (Tauri backend), Svelte 5 runes + TypeScript, Vitest + @testing-library/svelte, cargo-nextest.

## Global Constraints

- **Honesty rule (ZEB-610 §0):** no fabricated data. Meter renders only after `get_contribution_summary` succeeds; origin renders only when recorded at creation (legacy entries show nothing); no per-file buddy list (reports are aggregate); no online dots.
- **Style tokens:** all new component styles use `var(--…)` tokens from `src/app.css`; style-token-guard budget-0 (the guard is a Vitest test, `src/style-token-guard.test.ts`).
- **Sliders:** every slider ships paired with a typeable number input, bidirectionally synced (`bind:value` to shared `$state`; blur-clamp per `SetPowerDialog.svelte:35-49`).
- **Tier-2 confirm** (click-confirm, no typed confirm) on buddy removal — the inline arm-token pattern from `VoiceChannelView.svelte:79-92`.
- **Error extraction:** `e instanceof Error ? e.message : String(e)` (prod rejections are strings).
- **Wire keys are camelCase** (serde `rename_all = "camelCase"`); Rust command args snake_case → JS args camelCase.
- **Never bump `content-index.json` `FILE_VERSION`** — `read_file` discards the whole index on version mismatch (`content_index.rs:208-221`). Additive fields use `#[serde(default)]` only.
- **Iterative gates:** `scripts/test-select --context task` per task; final sweep in Task 9. Frontend gates: `npx tsc --noEmit`, `npx vitest run`.
- Spec §8 "wire-format fixture coverage for every new record type": `origin` is a **local sidecar field, not a network record** — the decode-old/round-trip-new coverage lives in `content_index.rs` unit tests (precedent: `backup_flag_defaults_false_on_legacy_entries` at 804-816, `save_persists_kind_field` at 844-858).

## Plan-time facts (ground truth, main @ 9c47b16e)

- **Only self-ingest creates `ContentIndexEntry`.** Three prod construction sites: `send_ingest_with_name` (`lib.rs:17000`, literal at `17016`), `create_folder_at_root_with_children` (`lib.rs:17771`, literal `17792`), `move_case_d` re-mint (`lib.rs:19223`, literal `19277`, deliberately resets intent flags — `_src_pinned` unused). `download_channel_artifact` writes to a user path (no index entry, author discarded at `find_attachment`); the buddy-fetch arm only pins+caches. → `ChannelDownload`/`BuddyPin` are **reserved variants no seam emits yet**; note this in code.
- Rekey paths (`content_index.rs:339-367`) preserve unknown fields → renames + moves A/B/C need zero origin work.
- `ContentItemWire` (`lib.rs:15819-15843`, camelCase) lacks `backup` and `origin`; built in `list_root` (`~16000-16013`, from sidecar entries) and `list_folder` (`~16085-16100`, manifest rows, `sidecar_id: ""`). `list_content` is GUI-only — **no rpc.rs / curated-surface change**.
- Frontend already gates pin/burn/archive on `sidecarId !== ''` — the backup toggle uses the same gate.
- Slice-2 DTOs (all camelCase): `StorageBuddyDto { ownerAddress, petName, status: 'active'|'pendingIncoming'|'pendingOutgoing', myPledgeBytes, theirPledgeBytes, hostedForThemBytes, theyReportHoldingBytes, reportAgeMs }`; `ContributionSummaryDto { hostedBytes, budgetBytes, buddyCount, health: 'healthy'|'catchingUp'|'overBudget' }`. Verbs: `get_storage_buddies()`, `set_buddy_pledge {ownerAddress, bytes}` (0-byte accept valid; accept = pledge; clears dismissal), `remove_storage_buddy {ownerAddress}` (doubles as decline), `set_shared_budget {bytes}`, `get_contribution_summary()`, `set_backup_flag {sidecarId, backup}` (rejects with stable `ineligible:` prefix; clearing always allowed).
- Events: `storage-buddies-updated` (from set_buddy_pledge / remove_storage_buddy / set_backup_flag), `contribution-updated` (from set_shared_budget). **The meter must refetch on BOTH** (hostedBytes changes on pledge/backup changes too). Pinning is async (30 s engine tick) — the meter lags until the tick lands; that is honest, not a bug.
- No TS bindings exist for any of the six verbs — all fresh.
- Meter slot: `NavPanel.svelte` between `</nav>` (~line 420) and `<div class="nav-footer">` (~421), gated `{#if appMode === 'files'}`. The deleted `StorageBuddySummary` was "{N} buddies · {M} online" — **the fill-bar meter is a new component**, only the slot + Manage affordance carry over.
- Data flow is prop-driven from App.svelte (no stores): service instances near `App.svelte:1767`, adapter connect in the `isTauri` block `~1946-1951`, `$derived`/`$state` pushed down as props (`contentItems={allFileContents}` at `~3520`).
- Fill-bar idiom: `QuotaBar.svelte:42-49` + CSS 79-95 (`--tally-track` rail, `--accent` fill, `.warning` → `--gov-clay`, `style:width="{pct}%"`).
- Dialog: `Modal.svelte` (`onCancel` required, `ariaLabelledby` required, `use:trapFocus` handles Escape/trap/restore).
- Slider pair precedents: `ChangeQuorumDialog.svelte:93-109` (pure `bind:value`), `SetPowerDialog.svelte:61-84` (+ blur clamp). CSS: `input[type="range"]{flex:1} input[type="number"]{width:5rem}`.
- Tier-2 inline confirm: `VoiceChannelView.svelte:79-92` + markup 184-190 (arm token = row id, window-click disarm via `.closest()`, `data-testid` `-confirm` suffix).
- Friend picker: `contactsFromFriends(friends)` (`friend-service.ts:129-141`, active-only, label ladder) → `Map<addr, Profile>`; `FriendDto.ownerIdHex` is the same 32-hex owner space as `StorageBuddyDto.ownerAddress` (the slice-2 pet-name join already relies on this). Picker shell precedent: `DmCreateDialog.svelte` (search + `filteredProfiles` `$derived.by`).
- Toggle + disabled-with-reason precedent: `IrohRelaySettings.svelte:146-230` (load gate, `toggle-hint`, `role="alert"` error, race-safe listen cleanup).
- Service test idiom: injected fake adapter (`friend-service.test.ts:10-20`); component tests `@testing-library/svelte` (jsdom).
- `formatBytes` lives in `src/lib/file-utils.ts:53`.
- `Sensitivity` wire values: `"private" | "confidential" | "public"`. Backup eligibility (PublicDurable) ⇒ frontend proxy gate = `sensitivity === 'public'`; the backend `ineligible:` error remains the authority.
- Rust literal fan-out for a new `ContentIndexEntry` field: `content_index.rs` `sample_entry` (~535); `lib.rs` 15209, 15360, 15419, 56711; `tests/content/content_index_integration.rs` 270, 642, 741, 1320, 1334, 1658; `folder_primitive_integration.rs` 24, 62, 458; `rename_content_integration.rs` 312; `move_content_integration.rs` 286. (`test-fixtures` compiles all of these under `--all-targets`.)

---

### Task 1: Rust — `OriginInfo` type + `ContentIndexEntry.origin` field

**Files:** Modify `src-tauri/src/content_index.rs`, `src-tauri/src/lib.rs` (test literals), `src-tauri/tests/content/*.rs` (literals).
**Produces:** `pub struct OriginInfo { pub kind: OriginKind, pub introducer: Option<String> }`, `pub enum OriginKind { SelfIngest, ChannelDownload, BuddyPin }` (both camelCase serde), `ContentIndexEntry.origin: Option<OriginInfo>`.

- [ ] Add types in `content_index.rs` (near Sensitivity/ContentKind):

```rust
/// ZEB-669 S4: provenance for the detail panel's "From" row. Recorded at
/// index-entry creation only — never inferred retroactively (honesty rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginInfo {
    pub kind: OriginKind,
    /// 32-char lowercase hex owner address of whoever introduced the
    /// content; `None` for self-ingest.
    #[serde(default)]
    pub introducer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OriginKind {
    SelfIngest,
    /// Reserved: channel downloads write to a user-chosen path and create
    /// no index entry today, so no seam emits this yet (ZEB-669 survey).
    ChannelDownload,
    /// Reserved: buddy pins are cache+pin only (no index entry); no seam
    /// emits this yet.
    BuddyPin,
}
```

- [ ] Add field after `backup` (`content_index.rs:139-140`): `#[serde(default)] pub origin: Option<OriginInfo>,` with a doc-comment noting legacy entries stay `None`. Do NOT bump `FILE_VERSION`.
- [ ] Fan out `origin: None` across every exhaustive struct literal (list in plan-time facts). Prod seams get `None` in this task (flipped in Task 2).
- [ ] Tests in `content_index.rs`: `origin_defaults_none_on_legacy_entries` (raw-JSON clone of the backup legacy test) and `origin_round_trips_through_save_and_reload` (entry with `Some(OriginInfo{ kind: SelfIngest, introducer: None })`, save, reload, assert equal).
- [ ] Gate: `cd src-tauri && scripts/test-select --context task` green; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`.
- [ ] Commit.

### Task 2: Rust — record `SelfIngest` at the real seams; carry through Case-D

**Files:** Modify `src-tauri/src/lib.rs` (three literals), `src-tauri/tests/content/move_content_integration.rs`.
**Consumes:** Task 1 types.

- [ ] `send_ingest_with_name` literal (`lib.rs:17016`): `origin: Some(content_index::OriginInfo { kind: content_index::OriginKind::SelfIngest, introducer: None }),`
- [ ] `create_folder_at_root_with_children` literal (`lib.rs:17792`): same value.
- [ ] `move_case_d` literal (`lib.rs:19277`): stays `origin: None` with a comment. **Implementation-time correction:** `moved_entry` is a `folders::ManifestEntry` — manifests carry no provenance (it is lost when a file enters a folder), so there is nothing to copy forward; inferring `SelfIngest` would violate the honesty rule. The plan's original `moved_entry.origin.clone()` assumed an index-entry source and was wrong.
- [ ] Tests: ingest path asserts the new entry's `origin == Some(SelfIngest, introducer: None)` (extend the nearest existing ingest test); the Case-D integration test asserts the re-minted entry's `origin.is_none()`.
- [ ] Gate: `scripts/test-select --context task`; commit.

### Task 3: Rust — expose `backup` + `origin` on `ContentItemWire`

**Files:** Modify `src-tauri/src/lib.rs` (`ContentItemWire` ~15819, `list_root` ~16000, `list_folder` ~16085; wire test).
**Produces:** wire fields `backup: bool`, `origin: Option<OriginInfo>` (camelCase: `backup`, `origin.kind = "selfIngest"`, `origin.introducer`).

- [ ] Append to `ContentItemWire`: `pub backup: bool,` (doc: root sidecar entries only; always false for manifest rows) and `pub origin: Option<content_index::OriginInfo>,`.
- [ ] `list_root`: `backup: e.backup, origin: e.origin.clone(),` · `list_folder`: `backup: false, origin: None,`.
- [ ] Test: entry with `backup: true` + `origin: Some(SelfIngest)` appears in `list_content` root rows with both fields (and serialized JSON uses `"selfIngest"`); a folder-child row carries `false`/`None`. Extend the nearest existing `list_content` test.
- [ ] No `rpc.rs` change (`list_content` is GUI-only — verified).
- [ ] Gate: `scripts/test-select --context task`; `cargo fmt --all -- --check`; commit.

### Task 4: TS — `storage-buddy-service.ts` + types + tests

**Files:** Create `src/lib/storage-buddy-service.ts`, `src/lib/storage-buddy-service.test.ts`.
**Produces:** `StorageBuddyService { connectAdapter(a), destroy(), listBuddies(): Promise<StorageBuddyDto[]>, setPledge(ownerAddress, bytes), removeBuddy(ownerAddress), setSharedBudget(bytes), getContributionSummary(): Promise<ContributionSummaryDto>, onChange(cb): () => void }` + exported DTO interfaces/`BuddyStatus`/`BuddyHealth` types (field lists in plan-time facts).

- [ ] Mirror `friend-service.ts` shape: injected `TauriAdapter`, no direct `@tauri-apps/api` import. `connectAdapter` subscribes BOTH `storage-buddies-updated` and `contribution-updated`, pushing unlisteners into an array torn down by `destroy()` (idiom: `file-manager-service.ts:205-230`); both events fire the registered `onChange` callbacks.
- [ ] Verb args exactly: `('get_storage_buddies', {})`, `('set_buddy_pledge', { ownerAddress, bytes })`, `('remove_storage_buddy', { ownerAddress })`, `('set_shared_budget', { bytes })`, `('get_contribution_summary', {})`.
- [ ] Tests (fake-adapter idiom from `friend-service.test.ts:10-20`): each verb + camelCase args; both event names subscribed; `onChange` fires when either event handler is invoked; `destroy()` unsubscribes.
- [ ] Gate: `npx vitest run src/lib/storage-buddy-service.test.ts`; `npx tsc --noEmit`; commit.

### Task 5: TS — `file-manager-service` carries `backup`/`origin`; `setBackupFlag`

**Files:** Modify `src/lib/types.ts`, `src/lib/file-manager-service.ts`, its test file.
**Produces:** `ContentItem.backup: boolean`, `ContentItem.origin: ContentOriginInfo | null`, `export interface ContentOriginInfo { kind: 'selfIngest' | 'channelDownload' | 'buddyPin'; introducer: string | null }`, `FileManagerService.setBackupFlag(sidecarId: string, backup: boolean): Promise<void>`.

- [ ] `types.ts`: add the two fields + `ContentOriginInfo` (`ContentDetail` is an alias of `ContentItem`, so the panel sees them for free).
- [ ] `file-manager-service.ts`: extend the internal `ContentItemWire` interface (~101-117) with `backup: boolean; origin?: ContentOriginInfo | null;`; `wireToContentItem` (~143-163) maps `backup: wire.backup ?? false, origin: wire.origin ?? null`. Mock seed data (`mock-file-data.ts` items flowing through the service in demo mode) gets `backup: false, origin: null` — never fabricated origins.
- [ ] `setBackupFlag`: `invoke('set_backup_flag', { sidecarId, backup })` then the same reload path the pin/unpin methods use (mirror their post-mutation refresh + `onChange` bump). Let rejections propagate to the caller (the panel renders the error).
- [ ] Tests: wire mapping carries `backup`/`origin` (and defaults when absent); `setBackupFlag` invokes the verb with camelCase args and refreshes.
- [ ] Gate: `npx vitest run src/lib/file-manager-service.test.ts`; `npx tsc --noEmit`; commit.

### Task 6: Svelte — `ContributionMeter` + NavPanel slot + App wiring

**Files:** Create `src/lib/components/ContributionMeter.svelte`, `src/lib/components/__tests__/ContributionMeter.test.ts`; modify `src/lib/components/NavPanel.svelte`, `src/App.svelte`.
**Consumes:** Task 4 service/DTOs.
**Produces:** `ContributionMeter` props `{ summary: ContributionSummaryDto | null; onManage: () => void }`.

- [ ] Component: renders nothing when `summary === null` (the honest loaded-gate — App only sets it after IPC success). Otherwise: QuotaBar-style track/fill (`--tally-track`/`--accent`; `.warning` → `--gov-clay` when `health !== 'healthy'`), `pct` clamped per `QuotaBar.svelte:22-31` (budget 0 with usage → 100); text `{formatBytes(hostedBytes)} of {formatBytes(budgetBytes)} shared`; status line `You host pieces for {K} storage {buddy|buddies}. {Healthy.|Catching up.|Over budget.}` — zero buddies renders `No storage buddies yet.` instead; one button: label `Manage`, or `Invite a friend` when `buddyCount === 0` (the invite affordance — both open the sheet via `onManage`). `aria-label="Storage buddies"` on the section. Tokens only.
- [ ] NavPanel: add props `contributionSummary`, `onManageBuddies`; render between `</nav>` and `.nav-footer`: `{#if appMode === 'files'}<ContributionMeter summary={contributionSummary ?? null} onManage={onManageBuddies} />{/if}`.
- [ ] App.svelte: `const storageBuddyService = new StorageBuddyService()` next to the other services; connect in the `isTauri` adapter block; `let contributionSummary = $state<ContributionSummaryDto | null>(null)`; after connect, fetch summary (failure → stays null, log once); `storageBuddyService.onChange(() => refetch summary (+ buddies while sheet open))`; pass props to `<NavPanel>`; `let manageBuddiesOpen = $state(false)` toggled by `onManageBuddies`.
- [ ] Tests: null summary → empty render; fill width + text for a known summary; warning class for `catchingUp`/`overBudget`; health copy variants; zero-buddy state (no fabricated count line, Invite label); `onManage` fires.
- [ ] Gate: `npx vitest run` (guard included); `npx tsc --noEmit`; commit.

### Task 7: Svelte — `StorageBuddySheet` (Manage sheet)

**Files:** Create `src/lib/components/StorageBuddySheet.svelte`, `src/lib/components/__tests__/StorageBuddySheet.test.ts`; modify `src/App.svelte` (render + callbacks).
**Consumes:** Task 4 DTOs; `Modal.svelte`; `contactsFromFriends`.
**Produces:** props `{ buddies: StorageBuddyDto[]; summary: ContributionSummaryDto | null; friendContacts: Map<string, Profile>; onClose(); onSetPledge(ownerAddress, bytes); onRemove(ownerAddress); onSetBudget(bytes); }` (presentational — App passes service-backed callbacks and refreshes on events).

- [ ] Shell: `<Modal onCancel={onClose} ariaLabelledby="storage-buddy-title">` with `<h3 id="storage-buddy-title">Storage buddies</h3>` (Escape/trap/restore come from Modal's `use:trapFocus`, satisfying the spec's dialog-a11y pattern).
- [ ] **Shared budget** section: GB-denominated slider (min 0, max 100, step 1) + number input (min 0, step 0.1, blur-clamp ≥ 0) both `bind:value` to one local `$state` seeded from `summary.budgetBytes / 1e9`; commit on slider `onchange` / number `onblur` → `onSetBudget(Math.round(gb * 1e9))`. (Decimal GB matches the backend's 10 GB = 10_000_000_000 default.)
- [ ] **Active pacts** (`status === 'active'`): per row — label (`petName ?? shortAddr(ownerAddress)`), report line (`They hold {formatBytes(theyReportHoldingBytes)} for you · {age}` from `reportAgeMs`, or `No report yet` when null — never fabricated), my-pledge slider+number pair (GB, max = budget GB, same commit-on-release idiom) → `onSetPledge`, and Remove with the tier-2 inline confirm (arm token = `ownerAddress`, `Remove` ⇄ `Confirm` swap, window-click disarm via `.closest('.buddy-row-actions')`, `data-testid="buddy-remove"`/`"buddy-remove-confirm"`).
- [ ] **Pending incoming** (`pendingIncoming`): label + `Offers {formatBytes(theirPledgeBytes)}` + **Accept** → `onSetPledge(ownerAddress, 0)` (0-byte accept is valid; the pledge slider appears in Active after refresh) + **Decline** → `onRemove(ownerAddress)` single-click (a dismissal is reversible — a re-issued invite re-surfaces).
- [ ] **Pending outgoing** (`pendingOutgoing`): label + `You pledged {formatBytes(myPledgeBytes)} · awaiting their pledge` + **Cancel** with tier-2 confirm (it removes our signed pledge) → `onRemove`.
- [ ] **Invite a friend**: search input + list from `friendContacts` excluding addresses already in `buddies` (`DmCreateDialog.svelte` `filteredProfiles` idiom, slice 50); selecting a friend reveals an inline pledge slider+number pair (GB, default 0 — no invented default pledge) + `Send invite` → `onSetPledge(addr, bytes)`. Empty-state copy when no eligible friends.
- [ ] App wiring: `{#if manageBuddiesOpen}<StorageBuddySheet … onClose={() => (manageBuddiesOpen = false)} />{/if}`; `buddies` loaded on open + refreshed by `onChange`; callbacks call the service and surface errors (`e instanceof Error ? e.message : String(e)`) via an inline `role="alert"` in the sheet (error prop or local state set by rejected callback promise — pick the simplest: callbacks return promises, sheet catches + renders).
- [ ] Tests: three buckets classify correctly; Accept → `onSetPledge(addr, 0)`; Decline single-click → `onRemove`; Remove requires arm→confirm (first click ≠ call); pledge slider and number stay in sync; budget commit converts GB→bytes; invite list excludes existing buddies; Send invite passes chosen bytes; report line shows `No report yet` for null report.
- [ ] Gate: `npx vitest run`; `npx tsc --noEmit`; commit.

### Task 8: Svelte — detail-panel backup toggle + "From" row

**Files:** Modify `src/lib/components/FileDetailPanel.svelte`, its test; `src/App.svelte` (pass handler).
**Consumes:** Task 5 (`detail.backup`, `detail.origin`, `setBackupFlag`).

- [ ] **Backup toggle** section (after the ReplicationStatus section), rendered only when `detail.sidecarId !== ''` (manifest rows have no sidecar to flag): checkbox `checked={detail.backup}`, label `Back up with buddies`. `eligible = detail.sensitivity === 'public'`; `disabled={(!eligible && !detail.backup) || backupPending}` (clearing an already-set flag is always allowed — backend contract). `{#if !eligible}<p class="toggle-hint">Only public files can be backed up by buddies.</p>{/if}`. On change: pending guard → `onSetBackup(detail.sidecarId, checked)` → catch: extract message, strip a leading `ineligible: ` prefix for display, render `role="alert"` inline (idiom: FileDetailPanel's own `copyError` at 40-66) and revert the checkbox.
- [ ] **From row**: own `panel-section`, rendered `{#if detail.origin}`: label `From`, value via `originLabel(origin)` — `selfIngest` → `Added by you`; otherwise `introducer ? shortAddr(introducer) : kindCopy` (`Channel download` / `Buddy pin`). Comment: pet-name resolution for introducers lands when a seam actually emits one (reserved variants, ZEB-669 survey). Legacy/manifest entries (`origin === null`) render nothing.
- [ ] App: pass `onSetBackup={(sid, b) => fileManagerService.setBackupFlag(sid, b)}` down the existing detail-panel prop path.
- [ ] Tests: toggle absent for manifest rows (`sidecarId: ''`); disabled + hint for `sensitivity: 'private'` unset flag; enabled for `'public'`; enabled-for-clearing when flag set on ineligible file; change calls handler; rejection with `ineligible: not public durable` shows friendly inline error and reverts; From row absent when `origin: null`; shows `Added by you` for selfIngest.
- [ ] Gate: `npx vitest run`; `npx tsc --noEmit`; commit.

### Task 9: Final gates + PR

- [ ] `npx tsc --noEmit` · `npx vitest run` (includes style-token-guard + commons-hex-guard) — both clean.
- [ ] `cargo fmt --all -- --check` · `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` — clean.
- [ ] `scripts/test-select --context round` green, then full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (backgrounded with a supervision net).
- [ ] Push branch, open PR (`gh --repo zeblithic/harmony-client`), body: what/why per slice, honesty-ledger notes (meter-lag-until-tick, reserved origin variants, no per-file buddy list), test inventory, gates. Fire `@coderabbitai review` once at open. Converge bots.
- [ ] After ZEB-669 completes (post-merge): file the deferred sharedWith viewer-ACL ticket in Linear (describe → file → use the assigned ID — never invent one).

## Self-review notes

- Spec §4 coverage: meter (T6), manage sheet with pacts/invites/picker/budget (T7), backup toggle w/ reason copy (T8), tier-2 removal confirm (T7), token-clean (all). §5: origin field + seams (T1-2), wire exposure (T3, T5), From row render-only-when-present (T8). §8 gates (T9, per-task).
- Deviation recorded: spec §5 names `download_channel_artifact` and buddy pins as introducer carriers, but plan-time enumeration (mandated by the spec itself) found neither path creates index entries — variants are reserved, seams documented, no fabricated origins. Surfaced in the PR body.
- Types consistent: `OriginInfo`/`OriginKind` defined once in `content_index.rs`, reused by `ContentItemWire`; TS mirror `ContentOriginInfo` in `types.ts`; camelCase everywhere on the wire.
