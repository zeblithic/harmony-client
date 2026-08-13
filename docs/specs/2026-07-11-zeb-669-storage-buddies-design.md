# ZEB-669 — Storage Buddies: real hosting accounting + sharing model (design)

**Date:** 2026-07-11 · **Author:** Koya (with Jake's product decisions) · **Status:** approved forks, spec for implementation

Restores the Files storage-buddies surfaces removed in ZEB-612 S3 (PR #441) on top of a **real** hosting domain. Reference drawing: `docs/design/commons/references/Harmony Vines & Files.dc.html` Frame C. Honesty rule (ZEB-610 §0) governs throughout: every rendered number has a real, named source.

## §0 Product decisions (Jake, 2026-07-11)

1. **Buddy model = mutual pact.** Explicit invite/accept between people. Each side pledges a byte budget of its own choosing and auto-pins what the other flags for backup. Reciprocity is social, not enforced (a 0-byte reciprocal pledge is a valid accept).
2. **Attribution = hybrid.** Content announcements stay **anonymous** (session-level zid only — and slice 1 fixes the wiring so they actually count). Owner identity appears **only in signed buddy-protocol records** (pledges, backup sets, hosting reports). Strangers see counts; names exist only between consenting parties' records.
3. **Meter denominator = new shared-budget setting.** A user-configurable "storage I share with buddies" budget, persisted and **enforced** against buddy-pins. The drawn "8.2 of 20 GB shared" becomes `buddy-hosted bytes / shared budget`.

Note on pledge visibility: pledge/backup-set/hosting records are public signed wire records (same posture as ZEB-671 follow lists). Making a pledge is itself opt-in consent to that visibility; record contents are addresses and byte totals only — never file names (backup sets carry CIDs + sizes, which for public durables are already announced).

## §1 Ground truth (survey 2026-07-11, main @ 6f98b0dc)

**What the drawing asks for** (narrower than the deleted mocks): one aggregate **"Your contribution"** meter in the Files nav rail — big number "8.2 **of 20 GB shared**", 41% fill bar, caption *"You host pieces for 3 storage buddies. Healthy."* — plus the already-shipped rows/detail anatomy. The drawing has **no** per-buddy edit lists, **no** ShareList, **no** origin row; the old `origin` field was never rendered anywhere.

**What exists:** opportunistic content serving (queryable `harmony/content/*/**`, hash-verified, public freely / encrypted allowlist-only); announcements `harmony/announce/{cid}` carrying a bare unsigned u32 size; `ObservedHolders` (in-memory, zid-keyed, TTL-swept 180 s, caps 4096 CIDs × 32 holders); `get_storage_budget` reporting **configured** limits only; pin machinery for self-ingested content (`ContentIndexEntry.pinned`); `FriendNicknames` keyed by owner address.

**What does NOT exist** (all confirmed absent): per-peer byte metering in either direction; any hosting request/grant concept; auto-fetch or pin-for-others; zid→owner→pet-name resolution; enforcement of `max_pinned_bytes` (dead code, count-based `pin_limit` only); persistence of observed holders; any introducer/provenance record at ingest.

**Latent bug (slice 1):** `ObservedHolders` is fed from the Zenoh sample *attachment* zid (`event_loop.rs:6274-6276`), but the publish path attaches the zid **only** for `harmony/compute/capacity/*` keys (`event_loop.rs:6184-6192`). Announce publishes attach nothing, so real cross-peer announces arrive `source_zid = None` and are skipped — the shipped "×N copies seen" chip sits at ×1 (self) in production. The S3 feature is wired but unfed.

## §2 Slice 1 — feed the observed-holders counter (bugfix, own PR)

* Attach the local zid attachment to **all** `harmony/announce/*` publishes, in every publish site (the 60 s re-announce loop **and** the admit-time initial announce path — enumerate publish sites at plan time; the capacity-publish attachment machinery is reused, the condition extends from `CAPACITY_PREFIX` to announce keys).
* Receiver side is already correct (`note()` on attachment zid ≠ own zid; TTL sweep).
* Additive on the wire: older clients ignore attachments.
* Tests: publish-arm attachment pins (announce key → attachment present; unrelated key → absent), end-to-end observed-holders feed test (announce sample with foreign-zid attachment increments `replica_count`), regression pin that self-announces don't self-count.

## §3 Slice 2 — buddy pact domain (backend)

All wire records follow the ZEB-671/673 machinery verbatim: Ed25519 via `vine_signing`-style module (`storage_signing`), length-prefixed injective canonical bytes with count prefixes, strict verify-first ingest (byte cap → parse → sig → pubkey→address binding → topic-shape → caps) before any state effect, whole-record LWW replace by `updatedAt` (strictly-greater wins), session-monotonic + restart-persisted publish clocks, signer-authority guard on publish.

### Wire records (public content only; encrypted CIDs are rejected at flag time — v1)

| Record | Topic | Payload (camelCase serde) | Domain | Caps |
|---|---|---|---|---|
| **PledgeList** | `harmony/storage/{owner}/pledges` | `{ ownerAddress, pledges: [{to, bytes}], updatedAt, identityPub, sig }` | `harmony-storage-pledges-v1` | ≤ 64 pledges |
| **BackupSet** | `harmony/storage/{owner}/backup-set` | `{ ownerAddress, entries: [{cid, size}], updatedAt, identityPub, sig }` | `harmony-storage-backup-set-v1` | ≤ 1000 entries, 96 KB wire cap |
| **HostingReport** | `harmony/storage/{owner}/hosting` | `{ ownerAddress, reports: [{beneficiary, bytes, cids}], updatedAt, identityPub, sig }` | `harmony-storage-hosting-v1` | ≤ 64 reports |

* **Pact** = A's PledgeList names B **and** B's names A. One-sided = pending invite (surfaced to the named party). Revoke = publish a list without that entry (LWW clears it everywhere).
* **BackupSet eligibility is enforced at ingest, not just at local flag time** (PR #448 review): a signed record authenticates the sender, not policy compliance — a remote BackupSet entry whose CID header flags mark an encrypted or ephemeral class is `Rejected` before any state effect, so a hostile record can never induce fetches of never-announced content classes.
* **HostingReport** is **aggregate per beneficiary** (bytes + CID count), never per-CID — keeps the record tiny at scale and matches the drawn UI, which never shows per-file buddy status. Republished on ledger change + periodic refresh; receiver-side staleness pruning (constant at plan time, ≥ 3 refresh intervals).
* **ZEB-923 (2026-08-12): PledgeList and BackupSet are leased too.** While non-empty they are re-minted + re-signed + republished hourly (`STORAGE_RECORD_REFRESH_INTERVAL_MS`) by the storage-record publisher task; receivers decay them after `STORAGE_RECORD_TTL_MS` (3 days, keyed on the persisted local receipt clock, boot-grace floored at load). A permanently-dark buddy's pact therefore self-releases via the existing 30 s reconciliation; an empty family stays silent so its receiver rows decay away entirely.
* Global ingest caps mirror the follow-list pattern (bounded record store, stalest evicted).

### Auto-pin engine + hosting ledger

* For each active pact, fetch-and-pin the buddy's BackupSet CIDs **in list order** (deterministic), stopping at `min(my pledge to that buddy, remaining shared budget)`. Claimed `size` is a budget-admission hint; actual bytes are verified after fetch (CAS already hash-verifies) and the ledger records actuals; a fetch whose actual size exceeds remaining budget is unpinned and skipped.
* Fetched buddy content is admitted **serveable** (public durables serve freely anyway) and **pinned** (protected from cache eviction — exact CAS pin seam for non-self-ingested CIDs is a plan-time verification item, alongside fetch-concurrency bounding).
* **Hosting ledger** (new persisted store, `storage_ledger.json`, atomic writes via the `write_atomic_0600` pattern): `{ buddy_owner → [{cid, size, pinnedAt}] }` — the local source of truth for the meter's numerator and for HostingReport publishes.
* **Physical-CID dedup** (PR #448 review): the same CID may appear in several buddies' BackupSets. The ledger refcounts physical CIDs across pacts — bytes are stored once, the meter numerator counts each distinct pinned CID once (Σ distinct sizes), per-pact attribution still counts the entry against each pledging pact's budget slice, and the CID is unpinned only when the **last** referencing pact releases it (revoke/removal/backup-set change for one pact never evicts content another pact still requires).
* **Budget admission is serialized** (PR #448 review): a single budget gate reserves claimed bytes **before** each fetch and reconciles to actual size after; failed fetches release the reservation. Concurrent per-pact fetch tasks never observe stale remaining-budget, so the enforced budget cannot be transiently over-admitted.
* Unpin triggers: pact revoked (either side), CID removed from the buddy's BackupSet, shared budget shrunk (evict newest-pinned-first until within budget), our own removal of the buddy.
* Failure posture: fetch failures retry with backoff and leave the pact "catching up" — never fabricate ledger entries for unfetched CIDs.

### Settings + IPC surface

* `storage_settings.json` (vine_settings pattern): `{ sharedBudgetBytes, lastPublishedUpdatedAt floors per record type }`. Default budget **10 GB**.
* IPCs (all `*_impl` seams + registered): `get_storage_buddies` (pacts + pending invites, pet-names via `FriendNicknames`, my pledge, their pledge, ledger bytes I host for them, bytes they report holding for me + report freshness), `set_buddy_pledge(owner, bytes)` (0 allowed; creates/accepts/updates), `remove_storage_buddy(owner)`, `set_shared_budget(bytes)`, `get_contribution_summary` (numerator = ledger total, denominator = budget, buddy count, health), `set_backup_flag(sidecar_id, bool)` (rejects encrypted/ephemeral CIDs with a typed error).
* Backup flag is a new additive `ContentIndexEntry` field (`#[serde(default)]`); flag changes republish our BackupSet.
* Frontend events: `storage-buddies-updated` (pacts/invites/reports changed), `contribution-updated` (ledger/budget changed) — emitted only on real change.

### Health (concrete rule, rendered verbatim)

* **Healthy** — ledger ≤ budget and every active pact's BackupSet is fully pinned.
* **Catching up** — fetches pending/retrying, or a pact truncated by pledge/budget.
* **Over budget** — budget shrunk below ledger (until eviction completes).

## §4 Slice 3 — contribution meter + manage UI

* **Nav rail meter** (Files mode, restores the `StorageBuddySummary` slot per the drawing): `{X} of {Y} shared` + fill bar + *"You host pieces for {K} storage buddies. {Healthy|Catching up|Over budget}."* Renders **only after** `get_contribution_summary` succeeds (the `shareFollowsLoaded` gating pattern); zero-buddies state renders the meter with an invite affordance, never fabricated numbers. **Manage** button opens the sheet.
* **Manage sheet** (Tune-sheet a11y pattern: `role="dialog"`, `aria-modal`, focus-on-open, Escape): active pacts (pet-name, my pledge — **slider paired with a number input, bidirectionally synced**, their reported holdings + freshness), pending invites (Accept → `set_buddy_pledge`; Decline → local persisted dismissal in `storage_settings.json` — the inviter's record isn't ours to remove, so a dismissed invite simply stops surfacing unless re-issued with a newer `updatedAt`), invite-a-friend picker (friends list), shared-budget control (slider + number input).
* **Detail panel:** "Back up with buddies" toggle for eligible (public, non-ephemeral) files; disabled with reason copy otherwise. No per-file buddy list (reports are aggregate — honesty ledger).
* Tier-2 (click) confirm on buddy removal — replicas need re-homing; no typed confirm (reversible).
* Styles token-clean; style-token-guard budget-0.

## §5 Slice 4 — origin/provenance ("From" row)

* Additive `ContentIndexEntry.origin: Option<OriginInfo>` (`#[serde(default)]`): `{ kind: SelfIngest | ChannelDownload | BuddyPin, introducer: Option<owner_addr> }`, recorded at index-entry creation seams (enumerate at plan time; `download_channel_artifact` carries the author, buddy pins carry the buddy).
* Detail panel "From" row renders only when origin is present (pet-name fallback to truncated address); legacy entries show nothing. Never inferred retroactively.

## §6 Honesty ledger (deviations from drawing / old mocks)

| Element | Treatment | Why |
|---|---|---|
| Old per-buddy "storing N bytes" + online dots | manage sheet shows ledger/report bytes; **no online dots** | no per-buddy presence surface; drawing draws none |
| Old ShareList / `sharedWith` viewer ACL | **deferred to a new ticket** (file at implementation end, use the assigned ID) | it is an encrypted-content key-sharing problem; drawing dropped it |
| "Healthy" rollup | concrete 3-state rule (§3) | no invented health scoring |
| Per-file "backed up by K buddies" | not rendered | reports are aggregate by design |
| Encrypted/ephemeral content backup | rejected at flag time with typed error | never announced/served without allowlist; key sharing is future work |
| Multi-device owners | v1 ledger/pins are device-local | fleet-unified hosting is future work (note in code) |
| "20 GB" denominator | real persisted setting, enforced | was fiction; now the budget |

## §7 PR map (sequential, one open PR at a time)

1. **PR-1 (slice 1):** announce-attribution bugfix. Small, ships immediately, makes ×N real.
2. **PR-2 (slice 2):** backend domain — signing, records, ingest, pact state, auto-pin engine, ledger, settings, IPCs, events.
3. **PR-3 (slices 3+4):** meter + manage sheet + backup toggle + origin row.

Boundaries may merge (PR-2+3) if plan-time sizing says the combined diff stays reviewable; never split below slice granularity.

## §8 Gates

Per PR: `npx tsc --noEmit`; `npx vitest run`; style-token-guard budget-0; `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `scripts/test-select --context task|round` iteratively; final full sweep `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. Wire-format fixture coverage for every new record type (decode-old / round-trip-new).
