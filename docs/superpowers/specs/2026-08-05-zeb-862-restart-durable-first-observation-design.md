# ZEB-862 — Restart-durable local first-observation clock (relay-hold + DM-inbox)

**Status:** approved (Jake, 2026-08-05)
**Ticket:** ZEB-862 (Medium) — follow-up to ZEB-851 / the ZEB-831 wall-clock threat model
**Branch:** `zeb-862-restart-durable-first-observed` (off `main` @ `16c0470e`)
**Scope:** client-only, symmetric across relay-hold and DM-inbox

## Goal

Make the per-replica `first_observed_ms` TTL clock survive process restart so that
**never-covered** relay-hold blobs and DM-inbox entries age out on their intended
TTL instead of receiving a fresh full TTL after every reboot. Persist the **local**
clock only — no peer-supplied wall stamp (`held_at` / `deposited_at`) re-enters a
TTL decision, keeping the ZEB-831 "no peer stamp in a time decision" mandate intact.

## Background / premise (verified against current source, 2026-08-05)

ZEB-851 (PR #587) re-keyed both GC paths off a per-replica LOCAL `first_observed_ms`
side-map instead of the peer stamp, closing a backdate-to-instant-GC grief. That map
is `#[serde(skip)]`:

- `RelayHoldDoc.first_observed_ms` — `community_relay_hold_crdt.rs:51`
- `DmInboxDoc.first_observed_ms` — `dm_inbox_crdt.rs:100`

Both `RelayHoldDoc::gc(now_ms)` (`community_relay_hold_crdt.rs:180`) and
`DmInboxDoc::gc_expired(now_ms, covered)` (`dm_inbox_crdt.rs:129`) lazy-stamp
`first_observed_ms.entry(key).or_insert(now_ms)`, expire entries past
`observed + TTL` (`RELAY_HOLD_TTL_MS` / `INBOX_TTL_MS`), then `retain` the side-map
to live keys. On restart the doc's `entries` deserialize back but the side-map is
empty (serde-skip), so the first sweep re-stamps every entry at `now` — resetting
the TTL clock. A device that restarts frequently never ages never-covered entries
out.

The most recent commit touching both files is the ZEB-851 PR (#587) itself — no
sibling PR has closed this. Bounded by caps (`RELAY_HOLD_PER_SENDER_CAP` + overall
bounds; the inbox is bounded), not remote-attackable, self-inflicted (crash loops /
reboots). This is a duration-extension refinement, not a DoS.

(Note: the ticket cites `dm_inbox_ingest.rs` for the DM side-map; the field actually
lives in `dm_inbox_crdt.rs`. `dm_inbox_ingest.rs` only references it in comments.)

## Decision: sidecar local file (one per subsystem)

Persist each doc's `first_observed_ms` as its own local-only CBOR file, mirroring the
**existing replay-tracker sibling** (`relay_hold_replay.cbor` / `dm_inbox_replay.cbor`,
a `BTreeMap<String, Hlc>` persisted via the same atomic-write + quarantine-recovery
pattern and threaded through `FleetPersist::persist`). `first_observed_ms:
BTreeMap<String, u64>` is the same structural shape, so the sidecar is a direct clone
of that proven pattern.

The `Doc` keeps `#[serde(skip)]`, so the CRDT canonical wire bytes stay
byte-identical and `PartialEq` (entries-only) is unchanged; durability lives entirely
in the persist layer.

### Rejected alternatives

- **In-file schema V2** (`FileV2 { doc, first_observed_ms }` under a new version
  byte). Same ZEB-831 posture, one file, atomic doc+clock. Rejected: adds a versioned
  migration/read path to each persist module (more churn than the sidecar), and a
  corrupt file loses entries AND clock together rather than degrading independently.
  The sidecar reuses an established pattern verbatim and quarantines independently.

- **Anchor-floor `max(first_observed_ms, peer_stamp.wall_ms)`** (ticket option 2, no
  persistence). Rejected on two grounds: (1) it re-admits peer wall-time into the TTL
  decision — the exact thing the ZEB-831 series exists to forbid — and a future-dated
  peer stamp extends retention, requiring the ZEB-846 forward-skew-gate posture; and
  (2) it does not even meet the goal: after restart `first_observed` re-stamps to
  `now` and honest peer stamps are in the *past*, so `max(now, past) = now` for
  exactly the old entries we want to age out. It cannot recover the real deadline
  without extra clamping logic.

## Components (symmetric: `relay_hold` and `dm_inbox`)

### 1. Doc accessors (field stays private)

On `RelayHoldDoc` (`community_relay_hold_crdt.rs`) and `DmInboxDoc`
(`dm_inbox_crdt.rs`):

- `pub fn first_observed_ms(&self) -> &BTreeMap<String, u64>` — read snapshot for
  persist.
- `pub fn restore_first_observed(&mut self, map: BTreeMap<String, u64>)` — replace the
  side-map on boot-load (overwrites the default-empty map).

### 2. Sidecar persist functions

In `relay_hold_persist.rs` and `dm_inbox_persist.rs`, cloned from the replay-tracker
code in the same file:

- Filename const: `RELAY_HOLD_FIRST_OBSERVED_FILENAME = "relay_hold_first_observed.cbor"`
  / `DM_INBOX_FIRST_OBSERVED_FILENAME = "dm_inbox_first_observed.cbor"`.
- Schema const `*_FIRST_OBSERVED_SCHEMA_V1: u8 = 1` and newtype
  `FirstObservedFileV1(BTreeMap<String, u64>)`.
- `save_first_observed(path, &BTreeMap<String, u64>) -> Result<(), SyncError>` —
  `[schema_v1][CBOR]`, atomic write.
- `load_first_observed(path) -> Result<BTreeMap<String, u64>, SyncError>` — missing
  file → `Ok(BTreeMap::new())`; strict trailing-byte rejection; unknown version →
  `CborDecode`.
- `load_first_observed_or_recover(path)` — `CborDecode` corruption → quarantine
  `.corrupt-<ms>` + `Ok(BTreeMap::new())`; transient `Persist` I/O → propagate `Err`
  (ZEB-460 contract).

### 3. FleetPersist wiring

Add `first_observed_path: std::path::PathBuf` to `RelayHoldPersist` /
`DmInboxPersist`. In `persist(state, tracker)`, after saving the doc and replay
tracker, also `save_first_observed(&self.first_observed_path, state.first_observed_ms())`.

### 4. Boot wiring (`lib.rs`, two blocks)

- DM-inbox: `lib.rs:6002–6051`. Build `dm_inbox_first_observed_path =
  identity_dir.join(DM_INBOX_FIRST_OBSERVED_FILENAME)`; after `load_doc_or_recover`,
  `restore_first_observed(load_first_observed_or_recover(&path)?)` on the doc before
  the `Arc<Mutex>` wrap; add `first_observed_path` to the `DmInboxPersist` literal
  (line ~6047).
- Relay-hold: `lib.rs:6262–6303`, same shape; add `first_observed_path` to the
  `RelayHoldPersist` literal (line ~6299).

### 5. Test-site touch-up

The `#[cfg(test)]` `DmInboxPersist` at `dm_inbox_ingest.rs:2181` gets the new
`first_observed_path` field (a tempdir path). Any other `RelayHoldPersist` /
`DmInboxPersist` struct literals surfaced by the compiler get the field too.

## Data flow

- **Boot:** doc file → doc (entries, empty clock) → sidecar file → `restore_first_observed`
  → doc has the durable clock → GC sees real ages, old never-covered entries expire.
- **Runtime:** engine mutates the doc (deposit / merge / gc), calls `persist(doc,
  tracker)` → snapshots `doc.first_observed_ms()` (reflecting the latest GC stamps and
  prune) into the sidecar alongside the doc + replay files.

## Error handling / compatibility

- **Missing sidecar** (fresh install OR first boot after upgrading from a pre-862
  build) → empty map → today's re-stamp behavior, then the clock starts persisting
  going forward. **No versioned doc-file migration** — the file-presence check is the
  migration.
- **Corrupt sidecar** → quarantine `.corrupt-<ms>` (bytes preserved) + empty map; the
  doc file loads independently and is untouched.
- **Transient I/O on sidecar load** → propagate `Err` (fail boot loudly, retry next
  launch with the file intact), matching the doc/replay `load_*_or_recover` contract
  (ZEB-460).
- **Non-atomic multi-write:** doc, replay, and sidecar are written as three separate
  atomic files (the replay tracker already accepts this). A crash between doc-write
  and sidecar-write degrades to today's re-stamp for the affected keys; a sidecar
  written ahead of the doc leaves orphan stamps that GC's `retain(|k,_|
  live.contains(k))` prunes. Both directions are safe.

## Testing

**Persist-module unit tests** (mirror the existing replay-tracker set, per file):
round-trip; missing file → empty; corrupt → quarantine + empty; trailing-byte
rejection → `CborDecode`; transient I/O (path is a dir) → `Persist` propagated, no
quarantine. Extend the existing `*_persist_writes_both_files` test to assert the
sidecar is written too ("writes all three files").

**CRDT unit tests** (the core regression, per doc):
- **Durability closes the bug:** construct a doc with one never-covered entry,
  `restore_first_observed` an OLD stamp (`now - TTL - 1`), `gc(now)` → entry expires.
- **Negative / today's behavior:** same doc, empty clock, `gc(now)` → entry survives
  (re-stamped at `now`), proving the sidecar is what makes the difference.
- Accessor round-trip: `restore_first_observed(m)` then `first_observed_ms() == &m`
  (after a GC prunes to live keys, as applicable).

**Integration:** save doc + sidecar via the `FleetPersist` impl, reload through the
boot path shape (`load_doc_or_recover` + `load_first_observed_or_recover` +
`restore_first_observed`), and confirm the restored clock drives expiry.

## Global constraints (CI gates, run from `src-tauri/`)

- `cargo fmt --all -- --check`
- `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`
- `cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- Frontend (repo root): `npx tsc --noEmit` — no frontend change expected, but the gate
  runs. (No `network-health.ts` / DTO change in this work.)

## Out of scope

- No change to CRDT wire format, `merge_from`, or `PartialEq` (entries-only equality
  preserved).
- No new IPC surface / `NetworkHealthSnapshot` field. (Operator observability for
  never-covered retention is a separate concern; not this ticket.)
- No change to TTL values or the covered/pulled-by semantics.
- Anchor-floor / peer-stamp involvement explicitly rejected (see above).
