# ZEB-669 Slice 2 — Buddy-Pact Backend Domain Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the storage-buddies backend per spec §3 (`docs/specs/2026-07-11-zeb-669-storage-buddies-design.md`): three signed LWW wire records, verify-first ingest, pact state, refcounted hosting ledger, serialized-budget auto-pin engine, settings, six IPCs, two frontend events.

**Architecture:** Clone the ZEB-671 follow-list machinery (signing → bounded LWW store → publish clocks → choke points) for three new `harmony/storage/{owner}/…` records, then drive an auto-pin engine from a new event-loop interval arm whose budget state is loop-local (single-threaded ⇒ serialized admission for free). The hosting ledger refcounts physical CIDs across pacts and is the meter's numerator; a spawned lib.rs task republishes hosting reports on change/refresh.

**Tech Stack:** Rust (Tauri backend only — no frontend changes this PR), ed25519-dalek via `harmony_identity`, zenoh pub/sub, serde JSON camelCase wire records.

## Global Constraints (from spec §0/§3/§8 — copied verbatim)

- Domains: `harmony-storage-pledges-v1`, `harmony-storage-backup-set-v1`, `harmony-storage-hosting-v1`.
- Topics: `harmony/storage/{owner}/pledges`, `…/backup-set`, `…/hosting`.
- Caps: PledgeList ≤ 64 pledges; BackupSet ≤ 1000 entries, 96 KB wire cap; HostingReport ≤ 64 reports (aggregate per beneficiary — bytes + CID count, never per-CID).
- Verify-first ingest order: byte cap → parse → sig → pubkey→address binding → topic-shape → caps — all before any state effect. LWW replace by `updatedAt`, strictly-greater wins (`>=` existing ⇒ `IgnoredOlder`).
- BackupSet eligibility enforced **at ingest**: any entry whose CID header is not `ContentClass::PublicDurable` ⇒ `Rejected` (a hostile signed record must never induce fetches of encrypted/ephemeral content).
- Physical-CID refcounting: bytes stored once, meter numerator = Σ distinct pinned sizes, per-pact attribution counts the entry against each pledging pact's slice, unpin only at last release.
- Budget admission serialized: reserve claimed bytes before fetch, reconcile to actual after, release on failure. Budget is enforced.
- Default shared budget **10 GB**. Health rule verbatim: Healthy / Catching up / Over budget per spec §3.
- Announcements stay anonymous; owner identity appears only in these signed records (§0.2).
- Never fabricate ledger entries for unfetched CIDs (honesty rule). RAM-only cache ⇒ at boot, ledger entries missing from the cache are dropped (they re-enter on successful re-pin) so hosting reports only ever claim actually-held bytes.
- Gates per CLAUDE.md: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `scripts/test-select --context task` per task (copy the printed `round=… bucket=…` line into the task report); full `cargo nextest run --locked --workspace --all-targets --features test-fixtures` before PR.
- Commit trailers: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` + `Claude-Session: https://claude.ai/code/session_01MsT6ZD7kqbpbKoeenyQPtc`.

## File Structure

| File | Responsibility |
|---|---|
| Create `src-tauri/src/storage_signing.rs` | Wire payload structs (3), canonical bytes, sign/verify. Reuses `vine_signing` primitives. |
| Create `src-tauri/src/storage_records.rs` | Bounded LWW store for **remote** records + verify-first ingest + persistence (`storage_records.json`). |
| Create `src-tauri/src/storage_ledger.rs` | Refcounted hosting ledger + persistence (`storage_ledger.json`) + newest-first eviction. |
| Create `src-tauri/src/storage_settings.rs` | `storage_settings.json`: budget, my pledges, dismissed invites, publish floors. |
| Create `src-tauri/src/buddy_pin_planner.rs` | Pure reconciliation planner (desired vs held vs budget → fetch/attribute/release plan). |
| Modify `src-tauri/src/vine_signing.rs` | `pub(crate)` the shared primitives (`push_str`, `push_u64`, `verify_signed`). |
| Modify `src-tauri/src/content_index.rs` | Additive `#[serde(default)] backup: bool` + `set_backup` mutator. |
| Modify `src-tauri/src/lib.rs` | NodeState fields, boot wiring, publish choke points, 6 IPC `*_impl` + commands, hosting publisher task. |
| Modify `src-tauri/src/event_loop.rs` | 3 subscriptions, ingest routing, `buddy_sync_tick` engine arm, fetch-completion arm, `run()` params. |
| Modify `src-tauri/src/api/rpc.rs` | 6 `rpc!` registrations + camelCase arg structs (headless parity). |

Constants introduced (all in the module that owns them):
`MAX_PLEDGES_PER_LIST=64`, `MAX_PLEDGE_LIST_WIRE_BYTES=16*1024`, `MAX_BACKUP_ENTRIES=1000`, `MAX_BACKUP_SET_WIRE_BYTES=96*1024`, `MAX_HOSTING_REPORTS=64`, `MAX_HOSTING_REPORT_WIRE_BYTES=16*1024`, `MAX_TRACKED_OWNERS=1024`, `BUDDY_SYNC_INTERVAL_MS=30_000`, `HOSTING_REFRESH_INTERVAL_MS=300_000`, `HOSTING_REPORT_STALE_MS=900_000` (3× refresh, per spec ≥3), `BUDDY_FETCH_MAX_INFLIGHT=4`, `BUDDY_FETCH_BACKOFF_BASE_MS=60_000`, `BUDDY_FETCH_BACKOFF_MAX_MS=3_600_000`, `DEFAULT_SHARED_BUDGET_BYTES=10_000_000_000`.

Plan-time facts this plan relies on (from survey, main @ 0d355065):
- `vine_signing.rs:45-68` primitives; `:156-181` `verify_signed` core; `:121` `signer_address`.
- Ingest template `vine_feed_cache.rs::on_follow_list_sample` (`:962-1041`), outcome enum `:180`.
- Publish clock pattern `lib.rs:14070-14085`; signer-authority guard `lib.rs:14046-14060`; boot republish `lib.rs:10559-10571`.
- Descriptor publish channel: `NodeState::publish_tx` → `event_loop::PublishRequest { key_expr, payload, reply }` (arm at `event_loop.rs:3879`).
- Subscribe block `event_loop.rs:2935`; routing `emit_frontend_event` `event_loop.rs:7665-7788`.
- `write_atomic_0600` at `identity.rs:133`; settings template `vine_settings.rs`.
- `ContentId` re-export `crate::owner_state_types::ContentId`; class check `cid.content_class() == ContentClass::PublicDurable` (header-only, `cid.rs:129`); `verify_checksum()` header-only.
- Remote admit+pin: fetch (`fetch_via_zenoh` `event_loop.rs:6403` / recursive `fetch_recursive` `:6719` + `wrap_fetch_one_with_admission` `:6798`) → verify → admit → `runtime.pin_content` (`runtime.rs:1134`); pins for CIDs without sidecar entries are explicitly supported (`event_loop.rs:4228-4232`).
- Pin quota is count-based (`pin_limit()` = 256 with capacity 512); `max_pinned_bytes` is dead — this PR's ledger is the first real byte accounting.
- Cache is RAM-only in this client (`storage_tier.rs:337`) — pins don't survive restart.
- Interval-arm template: `reannounce_tick` (`event_loop.rs:3370-3373`, arm `:5745-5773`) — std-mutex guards dropped before await, `MissedTickBehavior::Skip`.
- IPC seam pattern `set_vine_settings` (`lib.rs:14199-14243`), handler list `lib.rs:54376`, `rpc!` macro `api/rpc.rs:51-76`.
- Verify at implementation time (small, non-blocking): `ContentClass` re-export alongside `ContentId` (add re-export if missing); FriendNicknames keyspace = pledge owner-address hex (both `hex(address_hash)`); non-mutating cache-presence check for the boot sweep; the per-generation guard pattern for spawned boot tasks; `ContentVerbRequest::Unpin` descendant behavior (mirror it).

---

### Task 1: `storage_signing` — payloads, canonical bytes, sign/verify

**Files:** Create `src-tauri/src/storage_signing.rs`; Modify `src-tauri/src/vine_signing.rs` (visibility only); Modify `src-tauri/src/lib.rs` (`mod storage_signing;` next to `mod vine_signing;`).

**Interfaces produced:** `PledgeEntry{to,bytes}`, `PledgeListPayload`, `BackupEntry{cid,size}`, `BackupSetPayload`, `HostingReportEntry{beneficiary,bytes,cids}`, `HostingReportPayload` (all camelCase serde, `Option<identity_pub>/Option<sig>` tail); `sign_pledge_list/sign_backup_set/sign_hosting_report(&PrivateIdentity, &mut P)`; `verify_pledge_list/verify_backup_set/verify_hosting_report(&P) -> Result<(),String>`; `*_canonical_bytes(&P) -> Vec<u8>`; the three domain consts.

- [ ] **Step 1: visibility change in `vine_signing.rs`** — change `fn push_str`, `fn push_u64`, `fn verify_signed` to `pub(crate) fn` (no body changes; `push_opt_str`/`push_bool` stay private — unused here).

- [ ] **Step 2: write failing tests** (bottom of new `storage_signing.rs`, `#[cfg(test)] mod tests`) — mirror `vine_signing.rs:265-582`:
  - `pledge_list_sign_verify_roundtrip`, `backup_set_sign_verify_roundtrip`, `hosting_report_sign_verify_roundtrip` (mint `PrivateIdentity` via OsRng as vine tests do; owner = `signer_address`).
  - `unsigned_record_rejected_with_unsigned_message` (each type; expect `"is unsigned"`).
  - `tampered_field_invalidates_signature` — per payload, mutate each field post-sign (owner, updated_at, one entry value) and expect `"signature invalid"`.
  - `forged_signer_pubkey_address_mismatch` — sign with key A, claim owner B ⇒ `"pubkey does not match claimed address"`.
  - `serde_camel_case_pin` per type — assert exact keys: `ownerAddress`, `pledges`/`entries`/`reports`, `updatedAt`, `identityPub`, `sig`; entry keys `to`/`bytes`, `cid`/`size`, `beneficiary`/`bytes`/`cids`. Also decode-old pin: a literal JSON string (no sig fields) parses and `verify_*` returns unsigned error.
  - `canonical_entry_boundaries_pinned` — pledges `[{to:"ab",bytes:1}]` ≠ `[{to:"a",…},{to:"b",…}]` canonical bytes; count prefix pins list boundary.
  - `canonical_bytes_golden_pin` per type — hex-pin the full canonical byte string for a fixed payload (fixed owner `"aa".repeat(32)`-style, fixed entries, `updated_at: 42`). This is the decode-old/wire-format fixture required by spec §8.
  - `domains_are_distinct_across_record_types` — same logical content under two record types ⇒ different canonical bytes.

- [ ] **Step 3: run to verify failure** — `cd src-tauri && cargo nextest run --locked -p harmony-app --features test-fixtures -E 'test(storage_signing)'` → compile error (module empty). 

- [ ] **Step 4: implement** — module header doc comment (“ZEB-669 slice 2 … same posture as vine_signing; records are public signed wire records, §0.2”):

```rust
use serde::{Deserialize, Serialize};
use crate::vine_signing::{push_str, push_u64, verify_signed, signer_address};

pub const PLEDGE_LIST_DOMAIN: &str = "harmony-storage-pledges-v1";
pub const BACKUP_SET_DOMAIN: &str = "harmony-storage-backup-set-v1";
pub const HOSTING_REPORT_DOMAIN: &str = "harmony-storage-hosting-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PledgeEntry { pub to: String, pub bytes: u64 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PledgeListPayload {
    pub owner_address: String,
    pub pledges: Vec<PledgeEntry>,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_pub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}
// BackupEntry{cid: String /*64-hex ContentId*/, size: u64}, BackupSetPayload{owner_address, entries, updated_at, tail}
// HostingReportEntry{beneficiary: String, bytes: u64, cids: u32}, HostingReportPayload{owner_address, reports, updated_at, tail}
// (same derives/tail as PledgeListPayload — write all three out fully)

pub fn pledge_list_canonical_bytes(p: &PledgeListPayload) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + p.pledges.len() * 48);
    push_str(&mut out, PLEDGE_LIST_DOMAIN);
    push_str(&mut out, &p.owner_address);
    push_u64(&mut out, p.updated_at);
    out.extend_from_slice(&(p.pledges.len() as u32).to_le_bytes());
    for e in &p.pledges { push_str(&mut out, &e.to); push_u64(&mut out, e.bytes); }
    out
}
// backup_set_canonical_bytes: domain, owner, updated_at, count, per entry: push_str(cid); push_u64(size)
// hosting_report_canonical_bytes: domain, owner, updated_at, count, per report:
//     push_str(beneficiary); push_u64(bytes); push_u64(u64::from(r.cids))

pub fn sign_pledge_list(private: &harmony_identity::PrivateIdentity, p: &mut PledgeListPayload) {
    let bytes = pledge_list_canonical_bytes(p);
    p.sig = Some(hex::encode(private.sign(&bytes)));
    p.identity_pub = Some(hex::encode(private.public_identity().to_public_bytes()));
}
pub fn verify_pledge_list(p: &PledgeListPayload) -> Result<(), String> {
    verify_signed(p.identity_pub.as_deref(), p.sig.as_deref(),
                  &p.owner_address, &pledge_list_canonical_bytes(p), "pledge list")
}
// sign/verify for backup set ("backup set") and hosting report ("hosting report") — same shape.
```

`signer_address` re-exported use is for callers (Task 6); keep the import even if only tests use it here (or drop and import in Task 6 — whichever clippy prefers).

- [ ] **Step 5: run tests to green** — same nextest filter; all storage_signing tests pass.
- [ ] **Step 6: task gates + commit** — `cargo fmt --all`; `scripts/test-select --context task` (record the `round=…` line); commit `ZEB-669 S2: storage_signing — signed wire payloads for pledges/backup-sets/hosting`.

### Task 2: `storage_records` — bounded LWW store + verify-first ingest

**Files:** Create `src-tauri/src/storage_records.rs`; Modify `src-tauri/src/lib.rs` (`mod storage_records;`).

**Interfaces produced:**
```rust
pub enum RecordOutcome { Inserted, UpdatedNewer, IgnoredOlder, Rejected(String) }
pub struct PledgeListRecord { pub pledges: Vec<PledgeEntry>, pub updated_at: u64 }
pub struct BackupSetRecord { pub entries: Vec<BackupEntry>, pub updated_at: u64 }
pub struct HostingReportRecord { pub reports: Vec<HostingReportEntry>, pub updated_at: u64, pub received_at_ms: u64 }
pub struct StorageRecordStore { /* pledge_lists/backup_sets: HashMap<String, _> (persisted),
                                   hosting_reports: HashMap<String, _> (in-memory only), path: Option<PathBuf> */ }
impl StorageRecordStore {
    pub fn new(path: Option<PathBuf>) -> Self            // loads from disk, re-applies caps
    pub fn on_pledge_list_sample(&mut self, key_expr: &str, payload: &[u8]) -> RecordOutcome
    pub fn on_backup_set_sample(&mut self, key_expr: &str, payload: &[u8]) -> RecordOutcome
    pub fn on_hosting_report_sample(&mut self, key_expr: &str, payload: &[u8], now_ms: u64) -> RecordOutcome
    pub fn pledge_list(&self, owner: &str) -> Option<&PledgeListRecord>
    pub fn backup_set(&self, owner: &str) -> Option<&BackupSetRecord>
    pub fn hosting_report(&self, owner: &str) -> Option<&HostingReportRecord>
    pub fn owners_pledging_to(&self, me: &str) -> Vec<(String, u64)>  // (owner, bytes) where their list names me
    pub fn sweep_hosting(&mut self, now_ms: u64)          // drops reports older than HOSTING_REPORT_STALE_MS
}
```

- [ ] **Step 1: write failing tests** (`#[cfg(test)]` mod, helper `signed_pledge_bytes(owner_signer, pledges, updated_at)` etc. minting identities like `vine_feed_cache.rs:3410` follow-list tests):
  - `signed_pledge_list_inserts` / `…backup_set…` / `…hosting_report…` (outcome `Inserted`, readable back).
  - `unsigned_record_rejected`, `tampered_record_rejected` (each family).
  - `record_on_foreign_or_misshapen_topic_rejected` — signed by A, delivered on `harmony/storage/{B}/pledges`; also `…/pledges/extra` trailing segment; also wrong kind segment.
  - `lww_keeps_newest_ignores_equal_and_older` — `>=` existing ⇒ `IgnoredOlder`.
  - `oversized_record_rejected` — wire-byte cap (pre-serde) and entry-count cap per family.
  - `backup_set_encrypted_or_ephemeral_cid_rejected` — build a real encrypted-class and an ephemeral-class ContentId (via `ContentId::for_book`-family constructors with flags, or hand-assemble header bits per `cid.rs:377`), hex them into entries ⇒ `Rejected` mentioning eligibility; store unchanged.
  - `backup_set_malformed_or_bad_checksum_cid_rejected`, `backup_set_duplicate_cid_rejected`.
  - `pledges_and_backup_sets_survive_disk_reload`; `hosting_reports_are_not_persisted`.
  - `owner_cap_evicts_stalest` (insert `MAX_TRACKED_OWNERS+1` pledge lists ⇒ smallest `updated_at` evicted).
  - `hosting_sweep_drops_stale_reports` (fresh kept at `STALE_MS-1`, dropped at `STALE_MS`).
  - `owners_pledging_to_filters_by_beneficiary`.
- [ ] **Step 2: run to verify failure** — `-E 'test(storage_records)'`.
- [ ] **Step 3: implement.** Ingest = the 8-step follow-list shape (`vine_feed_cache.rs:962` template): byte cap → `serde_json::from_slice` → `storage_signing::verify_*` → topic shape (`strip_prefix("harmony/storage/")`, split ⇒ `[owner, kind]`, owner == payload owner, exact kind, no trailing) → entry cap → **(backup-set only) eligibility loop**:

```rust
let mut seen = std::collections::HashSet::new();
for e in &set.entries {
    let Ok(raw) = hex::decode(&e.cid) else { return RecordOutcome::Rejected(format!("backup set cid not hex: {}", e.cid)); };
    let Ok(bytes32): Result<[u8; 32], _> = raw.try_into() else { return RecordOutcome::Rejected("backup set cid wrong length".into()); };
    let cid = crate::owner_state_types::ContentId::from_bytes(bytes32);
    if !cid.verify_checksum() { return RecordOutcome::Rejected("backup set cid checksum invalid".into()); }
    if cid.content_class() != crate::owner_state_types::ContentClass::PublicDurable {
        return RecordOutcome::Rejected("backup set entry is not public durable content".into());
    }
    if !seen.insert(bytes32) { return RecordOutcome::Rejected("backup set contains duplicate cid".into()); }
}
```
→ LWW compare (`existing.updated_at >= incoming` ⇒ `IgnoredOlder`) → insert → stalest-evict while over `MAX_TRACKED_OWNERS` → `self.save()` (pledges/backup-sets only). Persistence: `StorageRecordsDiskV1 { version: 1, pledge_lists: Vec<…OnDisk>, backup_sets: Vec<…OnDisk> }` camelCase, `crate::identity::write_atomic_0600`, tolerant load (missing/corrupt/version≠1 ⇒ empty, WARN). Hosting reports never touch disk (staleness-pruned; freshness is the point).
- [ ] **Step 4: run tests to green.**
- [ ] **Step 5: task gates + commit** — fmt, test-select task line, commit `ZEB-669 S2: storage_records — bounded verify-first LWW store for buddy records`.

### Task 3: `storage_ledger` — refcounted hosting ledger

**Files:** Create `src-tauri/src/storage_ledger.rs`; Modify `src-tauri/src/lib.rs` (`mod storage_ledger;`).

**Interfaces produced:**
```rust
pub enum ReleaseOutcome { NotHeld, StillReferenced, LastReference }
pub struct LedgerEntry { pub cid: String, pub size: u64, pub pinned_at_ms: u64 }
pub struct StorageLedger { /* per_buddy: BTreeMap<String, Vec<LedgerEntry>>, path: Option<PathBuf> */ }
impl StorageLedger {
    pub fn new(path: Option<PathBuf>) -> Self
    pub fn record_pin(&mut self, buddy: &str, cid: &str, size: u64, now_ms: u64) -> bool // false if already held for buddy
    pub fn release(&mut self, buddy: &str, cid: &str) -> ReleaseOutcome
    pub fn release_buddy(&mut self, buddy: &str) -> Vec<String>           // cids that hit last-reference
    pub fn evict_newest_first(&mut self, target_bytes: u64) -> Vec<String> // last-ref cids released to reach target
    pub fn holds(&self, buddy: &str, cid: &str) -> bool
    pub fn bytes_for_buddy(&self, buddy: &str) -> u64
    pub fn cid_count_for_buddy(&self, buddy: &str) -> u32
    pub fn distinct_pinned_bytes(&self) -> u64                            // meter numerator
    pub fn distinct_cids(&self) -> Vec<String>
    pub fn buddies(&self) -> Vec<String>
    pub fn drop_cid_everywhere(&mut self, cid: &str)                      // boot sweep: cache-missing cid
    pub fn save(&self)
}
```

- [ ] **Step 1: failing tests:** `record_and_release_roundtrip`; `same_cid_across_two_buddies_counted_once_in_distinct_bytes_but_attributed_to_both`; `release_with_remaining_reference_is_still_referenced_and_last_gets_last_reference`; `release_buddy_returns_only_last_ref_cids`; `evict_newest_first_reaches_target_and_returns_last_ref_cids` (mixed pinned_at ordering, shared cid must not double-free); `record_pin_duplicate_for_same_buddy_is_noop_false`; `ledger_survives_disk_reload`; `drop_cid_everywhere_removes_all_attributions`.
- [ ] **Step 2: run to failure.** Step 3: implement (refcount = number of buddies whose Vec contains the cid; `distinct_pinned_bytes` = Σ over the distinct-cid set using any holder's recorded size; disk `{version:1, buddies:[{owner, entries:[{cid,size,pinnedAtMs}]}]}` camelCase via `write_atomic_0600`; every mutator saves). Step 4: green. Step 5: fmt + test-select + commit `ZEB-669 S2: storage_ledger — refcounted buddy-pin ledger`.

### Task 4: `storage_settings` — budget, pledges, dismissals, floors

**Files:** Create `src-tauri/src/storage_settings.rs`; Modify `src-tauri/src/lib.rs` (`mod storage_settings;`).

**Interfaces produced** (clone `vine_settings.rs` shape: version-1 flatten envelope, `load_or_default(path)`, `save(path, &settings)` via `write_atomic_0600`):
```rust
pub const DEFAULT_SHARED_BUDGET_BYTES: u64 = 10_000_000_000;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageSettings {
    pub shared_budget_bytes: u64,                          // Default impl: DEFAULT_SHARED_BUDGET_BYTES
    #[serde(default)] pub my_pledges: std::collections::BTreeMap<String, u64>,   // to → bytes (0 valid)
    #[serde(default)] pub dismissed_invites: std::collections::BTreeMap<String, u64>, // owner → dismissed updatedAt
    #[serde(default)] pub pledge_floor: u64,               // lastPublishedUpdatedAt per record type
    #[serde(default)] pub backup_set_floor: u64,
    #[serde(default)] pub hosting_floor: u64,
}
pub fn settings_path(data_dir: &Path) -> PathBuf           // data_dir.join("storage_settings.json")
```
- [ ] Steps: failing tests (`defaults_are_10gb_and_empty`, `roundtrip_save_load`, `corrupt_or_missing_file_yields_default`, `unknown_version_yields_default`) → red → implement → green → fmt + test-select + commit `ZEB-669 S2: storage_settings — shared budget, pledges, dismissals, publish floors`.

### Task 5: boot wiring — NodeState, subscriptions, ingest routing, events

**Files:** Modify `src-tauri/src/lib.rs`, `src-tauri/src/event_loop.rs`.

**Interfaces produced:** `NodeState` fields `storage_records: Arc<Mutex<StorageRecordStore>>`, `storage_ledger: Arc<Mutex<StorageLedger>>`, `storage_settings: Arc<Mutex<StorageSettings>>`, `storage_settings_path: Option<PathBuf>`, `pledge_clock/backup_set_clock/hosting_clock: AtomicU64`; `run()` gains the three Arc params (appended, mirroring `observed_holders`); frontend event names `"storage-buddies-updated"`, `"contribution-updated"`.

- [ ] **Step 1: failing test** — routing-level unit test on a new extracted helper in `event_loop.rs`:
```rust
/// Routes a harmony/storage/* sample into the record store.
/// Returns true when the store changed (Inserted | UpdatedNewer) so the
/// caller can emit "storage-buddies-updated" only on real change.
pub(crate) fn note_storage_record_sample(
    store: &Arc<std::sync::Mutex<crate::storage_records::StorageRecordStore>>,
    key_expr: &str, payload: &[u8], now_ms: impl FnOnce() -> u64,
) -> bool
```
Tests (`note_storage_record_sample_tests`): signed pledge/backup-set/hosting samples route to the right ingest fn and return true; rejected/older return false; non-storage key returns false without locking side effects.
- [ ] **Step 2: red.** **Step 3: implement:**
  - `note_storage_record_sample` matches the kind suffix (`/pledges`, `/backup-set`, `/hosting`) and dispatches; hosting passes `now_ms()`.
  - Three `RuntimeAction::Subscribe` entries after the vines block (`event_loop.rs:2935` area): `harmony/storage/*/pledges`, `harmony/storage/*/backup-set`, `harmony/storage/*/hosting`.
  - In `emit_frontend_event` (or its caller arm, matching where vines route): `if key_expr.starts_with("harmony/storage/") { if note_storage_record_sample(...) { emit_ser(app, "storage-buddies-updated", &serde_json::Value::Null); } return; }`.
  - NodeState: fields + `Default` sites (`lib.rs:1631` area and the test-default at `:59543`); start_node loads all three stores from `app_data_dir` (paths: `storage_records.json`, `storage_ledger.json`, `storage_settings.json`), installs Arcs, seeds the three clocks from floors (`AtomicU64::new(settings.pledge_floor)` etc. — `lib.rs:9597` follow-clock pattern), passes Arc clones into `run()`.
  - **Boot ledger honesty sweep** stub: pass ledger Arc into `run()`; actual sweep logic lands in Task 8 (first tick).
- [ ] **Step 4: green** (routing tests + full storage filter). **Step 5:** fmt + clippy scoped + test-select + commit `ZEB-669 S2: wire storage records into boot, subscriptions, routing, events`.

### Task 6: publish side — signed builds, clocks, choke points, boot republish

**Files:** Modify `src-tauri/src/lib.rs`.

**Interfaces produced:**
```rust
fn next_storage_clock(clock: &AtomicU64, now_secs: u64) -> u64   // strictly-increasing; caller persists floor
pub(crate) fn build_signed_pledge_list(guard: &mut NodeState) -> Result<(String /*topic*/, Vec<u8>), String>
pub(crate) fn build_signed_backup_set(guard: &mut NodeState, index: &ContentIndex) -> Result<(String, Vec<u8>), String>
pub(crate) fn build_signed_hosting_report(guard: &mut NodeState) -> Result<(String, Vec<u8>), String>
pub(crate) fn publish_pledge_list_update(guard: &mut NodeState)   // best-effort choke point → publish_tx
pub(crate) fn publish_backup_set_update(guard: &mut NodeState)
pub(crate) fn publish_hosting_report_update(guard: &mut NodeState)
```
Rules (all three builders): signer-authority guard first (`guard.node_addr == vine_signing::signer_address(identity)` — `lib.rs:14046` template; error `refusing to sign: storage record owner … does not match signer identity …`); `updated_at = next_storage_clock(...)`; persist the matching floor into `storage_settings.json` **before** returning bytes (floor-then-publish, matching the follow-list ordering); truncate to caps. Content per builder:
- Pledge list: `settings.my_pledges` → sorted-by-key `Vec<PledgeEntry>`.
- Backup set: index entries where `backup && !archived`, deduped by cid (first occurrence), filtered to `PublicDurable` class (defense in depth at build too), ordered by `stored_at_ms` ascending then cid (deterministic priority order — oldest flagged first), truncated to `MAX_BACKUP_ENTRIES`, each `{cid: hex, size: size_bytes}`.
- Hosting report: ledger → per-buddy `{beneficiary, bytes: bytes_for_buddy, cids: cid_count_for_buddy}`, sorted by beneficiary, only buddies with ≥1 entry.
Dispatch: send `event_loop::PublishRequest { key_expr, payload, reply: None-style }` through `guard.publish_tx.try_send` exactly as descriptors do; failure logs WARN (best-effort, republish heals).
Boot republish: in the `lib.rs:10559` once-per-generation block, after the follow-list republish add `publish_pledge_list_update` + `publish_backup_set_update` (hosting comes from the Task 8 publisher task).

- [ ] **Step 1: failing tests:** `pledge_clock_is_strictly_monotonic_within_session`; `pledge_floor_persists_across_settings_reload` (build twice, reload settings file, floor advanced); `refuses_to_sign_for_foreign_owner` (guard with mismatched node_addr); `backup_set_build_dedupes_and_orders_and_filters` (index fixture with duplicate-cid sidecars, an archived flagged entry, an encrypted-class cid ⇒ excluded); `hosting_report_build_aggregates_per_beneficiary`.
- [ ] **Step 2: red. Step 3: implement. Step 4: green. Step 5:** fmt + test-select + commit `ZEB-669 S2: signed publish paths for pledges/backup-set/hosting with persisted clocks`.

### Task 7: pact state + IPCs

**Files:** Modify `src-tauri/src/lib.rs` (DTOs, `*_impl`, `#[tauri::command]`, `generate_handler!` at `:54376`), `src-tauri/src/api/rpc.rs` (arg structs + 6 `rpc!` lines).

**Interfaces produced (DTOs camelCase — e2e assertions key off these exact names):**
```rust
#[derive(Serialize)] #[serde(rename_all = "camelCase")]
pub struct StorageBuddyDto {
    pub owner_address: String,
    pub pet_name: Option<String>,
    pub status: BuddyStatus,                    // Active | PendingIncoming | PendingOutgoing (camelCase serde)
    pub my_pledge_bytes: u64,                   // 0 when we haven't pledged (incoming invite)
    pub their_pledge_bytes: Option<u64>,        // their pledge naming me, if any
    pub hosted_for_them_bytes: u64,             // ledger.bytes_for_buddy
    pub they_report_holding_bytes: Option<u64>, // their HostingReport entry naming me
    pub report_age_ms: Option<u64>,
}
#[derive(Serialize)] #[serde(rename_all = "camelCase")]
pub struct ContributionSummaryDto {
    pub hosted_bytes: u64,        // ledger.distinct_pinned_bytes()
    pub budget_bytes: u64,
    pub buddy_count: u32,         // active pacts only
    pub health: BuddyHealth,      // Healthy | CatchingUp | OverBudget (camelCase serde)
}
```
IPC set (each: `*_impl(state: &Mutex<NodeState>, …)` + thin command + `rpc!`):
1. `get_storage_buddies() -> Vec<StorageBuddyDto>` — active = my_pledges ∩ owners_pledging_to(me); pending incoming = they pledge to me, I don't, and `dismissed_invites.get(owner) < their updated_at` (or absent); pending outgoing = I pledge, they don't. Pet-names via the FriendNicknames loader (`lib.rs:50121` pattern). Sorted by owner.
2. `set_buddy_pledge(owner_address: String, bytes: u64) -> Result<(), String>` — validate owner hex (reuse the follow_vine_creator address validation), upsert `my_pledges`, clear any `dismissed_invites` entry for that owner (pledging IS accepting), save settings, `publish_pledge_list_update`, emit `storage-buddies-updated`.
3. `remove_storage_buddy(owner_address: String)` — remove from `my_pledges`; if they still pledge to us record a dismissal at their current `updated_at` (removal also dismisses the residual invite); save + republish pledges; ledger release happens in the engine tick (it sees the pact gone). Emit `storage-buddies-updated`.
4. `set_shared_budget(bytes: u64)` — set, save, emit `contribution-updated` (eviction to the new budget happens in the engine tick).
5. `get_contribution_summary() -> ContributionSummaryDto` — health: `OverBudget` if `hosted > budget`; else `CatchingUp` if any active pact has an eligible in-slice BackupSet entry not in the ledger (reuse the Task 8 planner in dry-run — `plan(...)` with empty in-flight; nonempty fetch/attribute lists or any truncation ⇒ catching up); else `Healthy`.
6. `set_backup_flag` — Task 9 (registered there).
Decline path: dismissal is written by `remove_storage_buddy` when no pledge exists — i.e. Decline in the UI calls `remove_storage_buddy(owner)` (works for pure invites: no pledge to remove, records dismissal at the invite's `updated_at`).

- [ ] **Step 1: failing tests** (unit tests on `_impl` with a default NodeState + seeded stores — the `set_vine_settings_impl` test pattern): `pact_and_invite_classification` (all three statuses + dismissed invite suppressed + re-issued newer invite resurfaces); `set_buddy_pledge_accepts_invite_and_clears_dismissal`; `remove_storage_buddy_records_dismissal_at_current_updated_at`; `zero_byte_pledge_is_a_valid_accept` (0-byte pact classifies Active); `contribution_summary_health_over_budget_beats_catching_up`; `summary_counts_distinct_bytes_once`.
- [ ] **Step 2: red. Step 3: implement (including `generate_handler!` additions and `rpc!` registrations `get_storage_buddies`, `set_buddy_pledge`, `remove_storage_buddy`, `set_shared_budget`, `get_contribution_summary`). Step 4: green. Step 5:** fmt + test-select + commit `ZEB-669 S2: buddy-pact IPCs — pacts, invites, budget, contribution summary`.

### Task 8: auto-pin engine — planner + event-loop tick + fetch/pin/ledger

**Files:** Create `src-tauri/src/buddy_pin_planner.rs`; Modify `src-tauri/src/event_loop.rs`, `src-tauri/src/lib.rs` (hosting publisher task + `mod buddy_pin_planner;`).

**Planner (pure — all engine policy decisions live here so they're unit-testable):**
```rust
pub struct PlannerInputs<'a> {
    pub me: &'a str,
    pub my_pledges: &'a BTreeMap<String, u64>,
    pub pledgers_to_me: &'a [(String, u64)],       // from store.owners_pledging_to(me)
    pub backup_sets: &'a dyn Fn(&str) -> Option<&'a BackupSetRecord>,
    pub ledger: &'a StorageLedger,
    pub shared_budget: u64,
    pub inflight_claims: &'a HashMap<String /*cid*/, u64>, // reservations (claimed bytes)
    pub inflight_buddy_attr: &'a HashMap<String /*cid*/, String /*buddy*/>,
    pub backoff_blocked: &'a dyn Fn(&str) -> bool, // cid → retry not yet due
}
pub struct PinPlan {
    pub fetch: Vec<FetchCandidate>,                 // {buddy, cid, claimed}
    pub attribute_only: Vec<(String, String, u64)>, // (buddy, cid, size): pinned via another pact
    pub release: Vec<(String, String)>,             // (buddy, cid): no longer desired
    pub release_buddies: Vec<String>,               // pact gone entirely
    pub evict_to: Option<u64>,                      // budget shrunk below distinct bytes
    pub catching_up: bool,
}
pub fn plan(inputs: &PlannerInputs) -> PinPlan
```
Rules (deterministic; buddies in sorted order; entries in BackupSet list order): active pact = both sides pledge (0 valid). Per buddy: slice = my pledge to them; attributed = `ledger.bytes_for_buddy + Σ inflight claims attributed to buddy`; entry not held for this buddy: over slice ⇒ stop this buddy + `catching_up`; held for another pact ⇒ `attribute_only` (counts against slice, not global); global `distinct_bytes + Σ inflight_claims + claimed > shared_budget` ⇒ stop globally + `catching_up`; inflight or backoff ⇒ skip + `catching_up`; else fetch candidate. Releases: ledger entries whose buddy pact is inactive ⇒ `release_buddies`; entries absent from the buddy's current set ⇒ `release`. `evict_to = Some(budget)` when `distinct_bytes > budget`.

**Event-loop engine:** new `buddy_sync_tick` interval (30 s, `MissedTickBehavior::Skip`) + `buddy_fetch_rx` arm + loop-local state `BuddyEngineState { inflight: HashMap<String, InflightFetch{buddy, claimed}>, backoff: HashMap<String, (u32, u64)>, booted: bool }`.
Tick sequence (std-mutex guards NEVER held across await — compute under short locks, then act):
1. First tick only (`!booted`): honesty sweep — for each `ledger.distinct_cids()`, check cache presence via the storage-tier cache; missing ⇒ `ledger.drop_cid_everywhere(cid)` (re-enters as fetch candidates). Set `booted`.
2. `store.sweep_hosting(now)`.
3. Snapshot inputs → `plan(...)`.
4. Apply releases/evictions: `release`/`release_buddy`/`evict_newest_first` → for every last-ref cid: `runtime.unpin_content(&cid)` (+ descendants, mirroring the Unpin arm) and `pin_intent.remove`; ledger saves; if anything changed emit `contribution-updated`.
5. `attribute_only` → `ledger.record_pin` (bytes stored once — no fetch); emit `contribution-updated` on change.
6. Spawn fetches up to `BUDDY_FETCH_MAX_INFLIGHT - inflight.len()`: reserve (insert into `inflight`), spawn task cloning `session` + `cas_op_tx` + `buddy_fetch_tx`: `fetch_recursive(wrap_fetch_one_with_admission(fetch_one, cas_op_tx, /*serveable*/ false), root_cid, max_bytes = remaining_global_budget_at_reservation)` → send `BuddyFetchResult { buddy, cid, total_bytes: Option<u64> }`.
`buddy_fetch_rx` arm: remove reservation; `None` ⇒ backoff bump (`min(BASE << attempts, MAX)`) — pact shows Catching up; `Some(actual)` ⇒ recheck `distinct+actual ≤ budget` (else skip: admitted-unpinned content just becomes evictable — never enters the ledger); `runtime.pin_content` root + `collect_descendants` (a `false` return = pin-count quota exhausted ⇒ treat as failure + backoff, unpin nothing); `pin_intent.insert`; `ledger.record_pin(actual)`; clear backoff; emit `contribution-updated`.
**Hosting publisher task (lib.rs):** `spawn_hosting_report_publisher(app_handle)` spawned once per generation next to the boot-republish block (never inline-awaited — start_node hazard): every 30 s, exit if generation superseded; build the aggregate from the ledger Arc; publish via `publish_hosting_report_update` when the aggregate differs from last-published or `HOSTING_REFRESH_INTERVAL_MS` elapsed.

- [ ] **Step 1: failing planner tests** (pure — the bulk of engine coverage): `fetches_in_list_order_within_pledge_slice`; `slice_exhaustion_stops_buddy_and_flags_catching_up`; `global_budget_stop_spans_buddies_and_counts_reservations`; `shared_cid_across_pacts_attributes_without_refetch_or_double_budget`; `inactive_pact_releases_buddy`; `entry_removed_from_backup_set_releases`; `budget_shrink_sets_evict_target`; `zero_pledge_pact_fetches_nothing_but_is_active`; `backoff_blocked_cid_skipped_and_flags_catching_up`; `deterministic_order_across_calls`.
- [ ] **Step 2: red. Step 3: implement planner. Step 4: green. Step 5: implement loop wiring** (tick arm, fetch arm, channels, publisher task). Loop-level tests are thin by design (policy is in the planner): reuse the slice-1 loopback-test pattern only if a cheap seam exists; otherwise the planner tests + Task 10's full sweep carry it — document the boundary in the module docstring.
- [ ] **Step 6:** fmt + clippy `--all-targets` + test-select + commit `ZEB-669 S2: auto-pin engine — planner, budget-serialized fetch/pin, hosting publisher`.

### Task 9: backup flag — index field, IPC, republish

**Files:** Modify `src-tauri/src/content_index.rs`, `src-tauri/src/lib.rs`, `src-tauri/src/api/rpc.rs`.

- [ ] **Step 1: failing tests:** index — `backup_flag_defaults_false_on_old_sidecars` (deserialize a pre-field JSON entry), `set_backup_persists_and_is_idempotent`; IPC — `set_backup_flag_rejects_encrypted_and_ephemeral_with_ineligible_error` (error string starts `ineligible:`), `set_backup_flag_republishes_backup_set` (floor advances / publish attempted), `set_backup_flag_unknown_sidecar_errors`.
- [ ] **Step 2: red. Step 3: implement:**
  - `#[serde(default)] pub backup: bool` on `ContentIndexEntry` (`content_index.rs:105` block, after `kind`) + `backup: false` at every creation seam (`lib.rs:15661`, `lib.rs:16436`, `lib.rs:17920`, test helpers `lib.rs:55347`, `content_index.rs:512`).
  - `set_backup(&mut self, id: &SidecarId, backup: bool) -> bool` — clone of `set_pinned` (`content_index.rs:397`).
  - `set_backup_flag_impl(state, sidecar_id: String, backup: bool) -> Result<(), String>`: `parse_sidecar_id` → entry lookup → eligibility `ContentId::from_bytes(entry.cid).content_class() == PublicDurable` else `Err("ineligible: encrypted or ephemeral content cannot be backed up by buddies")` → `set_backup` → `publish_backup_set_update(guard)` → emit `storage-buddies-updated`. Command + `generate_handler!` + `rpc!("set_backup_flag", …)`.
- [ ] **Step 4: green. Step 5:** fmt + test-select + commit `ZEB-669 S2: backup flag — sidecar field, eligibility-gated IPC, backup-set republish`.

### Task 10: final gates + PR

- [ ] `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings` (clean).
- [ ] Full sweep: `cargo nextest run --locked --workspace --all-targets --features test-fixtures` (green; background with wall-clock net).
- [ ] Frontend untouched — confirm `git diff --stat main` shows no `src/` (TS) changes; skip tsc/vitest per §8 note.
- [ ] Push branch; open PR `--repo zeblithic/harmony-client` titled `ZEB-669 S2: storage buddies — signed pledge/backup/hosting records + budget-enforced auto-pin engine`; body: spec link, record table, engine design (serialized budget, refcounting, honesty sweep), test inventory, gates incl. test-select round lines; attribution footer.
- [ ] Fire `@coderabbitai review` once; converge per standing loop.

## Self-Review (performed at write time)

- Spec §3 coverage: wire records ✅ (T1/T2), pact semantics + decline ✅ (T7), ingest eligibility ✅ (T2), auto-pin + refcount + serialized budget + unpin triggers + failure posture ✅ (T3/T8), ledger persistence ✅ (T3), settings + default 10 GB ✅ (T4), all 6 IPCs ✅ (T7/T9), events ✅ (T5/T7/T8), health rule ✅ (T7), boot republish ✅ (T6), staleness pruning ✅ (T2), fixture coverage ✅ (T1 golden pins).
- Honesty additions beyond spec letter: RAM-only-cache boot sweep (T8 step 1) — spec's "never fabricate ledger entries" extended to restart; documented in Global Constraints.
- Type consistency: `RecordOutcome` shared across families; planner consumes `BackupSetRecord`/`StorageLedger` types produced by T2/T3; DTO names match spec meter fields.
- Known deferrals (spec §6): sharedWith ACL ticket filed at ZEB-669 completion; multi-device hosting future work (code comment in ledger).
