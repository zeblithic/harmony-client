# ZEB-417 — SP1: Fleet Sync substrate (reusable per-owner replicated datasets)

**Status:** Design draft 2026-06-09 (awaiting review)
**Issue:** [ZEB-417](https://linear.app/zeblith/issue/ZEB-417) (SP1 of epic [ZEB-416](https://linear.app/zeblith/issue/ZEB-416))
**Framing:** `docs/specs/2026-06-09-multi-device-fleet-butler-framing.md`
**First consumer:** [ZEB-361](https://linear.app/zeblith/issue/ZEB-361) (Notes multi-device sync)

## Context — the engine already exists, three times

A code survey found that owner-private fleet sync is implemented as a single repeated pattern: **state-root CID publish → full encrypted-blob CAS fetch → per-entry CRDT/LWW merge, over a Zenoh-notified channel, gated by a device-keyed HLC replay tracker.**

| Subsystem | Trust domain | Notes |
|---|---|---|
| `owner_state_sync.rs` (2773 ln) | my own fleet | the reference implementation; merges a 9-collection `OwnerState` CRDT |
| `mint_sync.rs` (1668 ln) | my own fleet | **~95% byte-identical** transport/crypto/debounce/replay machinery (`encrypt_root_publish`, `encrypt_entry`, `next_hlc`, the `select!` debounce loop, CAS-fetch, replay-tracker, persist-after-advance) — differs only in snapshot shape, merge rule, and `lookup_key_tag` |
| `community_state_sync.rs` | cross-owner peers | same transport pattern + Ed25519 sig-verify + membership-at-HLC gates; `(owner,device)`-keyed tracker |
| `mail_sync.rs` | gateway authority | outlier: Merkle-tree walk, single publisher, no multi-writer merge — **not unified here** |

Crucially, `owner_state_sync` and `mint_sync` are *already separate engine instances on separate Zenoh topics with separate replay trackers*. The duplication is the copy-pasted engine, not the topology.

The user-facing **Notes** feature (`src/lib/notes-service.ts`, ZEB-334) is frontend-only `localStorage`, with a deliberately thin interface "so the persistence layer can later be swapped for a synced substrate without touching callers." It is the ideal first consumer of the extracted engine.

## Goal

Extract the repeated engine into one reusable `FleetSyncEngine`, prove it by migrating `owner_state_sync` onto it (preserving its exact wire/persist behavior), and add **Notes** as a second consumer with cross-device sync. Expose the narrow interface SP2 (the Butler) will deposit into. Surface a best-effort "synced to N devices" durability indicator.

## Scope

**In**
1. A generic `FleetSyncEngine<S>` (one instance per named dataset) factored from the owner/mint donors.
2. Migrate `owner_state_sync` to run on it — **wire format, Zenoh topic, on-disk format, and merge semantics unchanged.**
3. Add **Notes** as a Rust-backed synced dataset; migrate existing `localStorage` notes; owner-private.
4. A best-effort "synced to N devices" indicator.
5. The SP1↔SP2 seam: a per-dataset `write`/`notify_dirty` entry point plus a `list_online_devices()` fleet-presence query.

**Out**
1. Migrating `mint_sync` and `community_state_sync` onto the engine — fast-follow tickets (filed after this lands). The engine is designed so they *can* rebase later; `community_state_sync` keeps pluggable auth gates.
2. Willow / range-based set reconciliation — explicitly declined (see framing §8); full-blob-per-publish is adequate at these dataset sizes. Revisit only if a single dataset grows large.
3. Anything in SP2 (Butler), and `mail_sync` (gateway Merkle walk).

## Architecture

### `FleetSyncEngine<S>` — one instance per dataset

Generic over a snapshot type `S`, instantiated once per named dataset. Everything that is byte-identical between the owner and mint donors moves into the engine; everything that differs is a parameter.

**Shared (lifted verbatim from the donors):**
- the `notify_dirty` → debounce → `publish_root_now` loop (the pinned-`Notified` `select!` idiom);
- `flush_now` / `shutdown`;
- the device-keyed HLC replay tracker + `next_hlc` (wall-ms reckoning, logical-counter saturation on skew);
- the publish path: canonical-CBOR encode `S` → deterministic-nonce `encrypt_entry` → `ContentId` (BLAKE3 of ciphertext) → `content_store.put` → random-nonce `encrypt_root_publish` of `RootPublishPayload{ root_cid, at: Hlc }` → `publisher_tx`;
- the receive path: `decrypt_root_publish` → replay check (read-only, strictly-newer) → `content_store.get(root_cid)` (miss ⇒ drop, eventual-consistency retry) → `decrypt_entry` → decode `S` → **merge** → advance tracker → persist;
- the **CRITICAL ordering invariants** (from mint_sync's CRITICAL 1/3 + MAJOR 5): apply-before-tracker-advance, tracker-advance-before-persist, persist only post-mutation. Centralizing these is a correctness win — they stop being re-derived per subsystem.

**Parameters (per dataset):**

| Param | Owner-state | Notes |
|---|---|---|
| snapshot type `S` | `OwnerState` | `NotesDoc` |
| `merger: Fn(&mut S, S) -> MergeOutcome` | existing `merge_remote_into_local` | per-note LWW-by-`updated_at` + tombstone |
| `lookup_key_tag` | `b"owner-state-root-blob-v1"` (unchanged) | `b"notes-v1"` |
| Zenoh topic | `harmony/owner/{addr}/state-root-v1` (unchanged) | `harmony/owner/{addr}/ds/notes-v1` |
| persistence hook | existing `owner_state_persist` | new notes persist (CBOR file) |

```rust
pub struct FleetSyncEngine<S: Send + 'static> { /* notify/flush/shutdown handles */ }

impl<S: Serialize + DeserializeOwned + Send + 'static> FleetSyncEngine<S> {
    pub fn new(
        kt: Arc<KeyTree>, device_id: String,
        state: Arc<Mutex<S>>,
        merger: Arc<dyn Fn(&mut S, S) -> MergeOutcome + Send + Sync>,
        replay_tracker: Arc<Mutex<BTreeMap<String, Hlc>>>,
        content_store: Arc<dyn ContentStore>,
        publisher_tx: mpsc::Sender<Vec<u8>>, subscriber_rx: mpsc::Receiver<Vec<u8>>,
        lookup_key_tag: &'static [u8], debounce_ms: u64,
        persist: Arc<dyn FleetPersist>,
    ) -> Self;
    pub fn notify_dirty(&self);
    pub async fn flush_now(&self) -> Result<(), SyncError>;
    pub async fn shutdown(&self) -> Result<(), SyncError>;
}
```

Reuses the existing shared crypto/types unchanged: `KeyTree`, `space_lookup_key`, `encrypt_entry`/`decrypt_entry`, `encrypt_root_publish`/`decrypt_root_publish` (`owner_state_crypto.rs`), `Hlc`, `RootPublishPayload`, `ContentId` (`owner_state_types.rs`). No crypto is rewritten.

### Per-named-dataset model

Each dataset is one engine instance on its own topic with its own tracker — matching how owner-state and mint already run. Editing a note ships only the `notes` dataset; an owner-state change ships only `owner-state`. No cross-dataset coupling, no re-shipping unrelated state. New datasets register `(name, S, merger, tag, topic, persist)` and wire a `publisher_tx`/`subscriber_rx` pair through the event loop (the same adapter shape that owner/mint already use).

### The SP1↔SP2 seam

SP1 exposes to consumers (Notes today, the Butler in SP2):
- `write(dataset, op)` — applied locally via the dataset's merge, then `notify_dirty()` schedules the debounced publish/fan-out. (For owner-private datasets the existing per-collection `apply_*` methods are the `op` surface.)
- `list_online_devices()` — derived from the per-dataset replay tracker (devices seen publishing recently) plus the device cache; this is the butler-set source SP2 advertises.

The Butler never reaches past these two operations into replication.

### Durability indicator (best-effort)

Each root publish gains an optional `seen: BTreeMap<device_id, Hlc>` — the highest HLC this device has merged from each peer (bounded by `MAX_DEVICES_PER_OWNER = 32`). A device counts how many siblings report `seen[me] ≥ my_latest_published_hlc` ⇒ **"synced to N devices."** Non-blocking, observational, no acks on the write path. For a single-device owner it reads "1 device — not yet backed up," reinforcing the recovery-seed backup nudge.

Wire impact: `seen` is additive and `#[serde(default)]`. **Open item for the plan:** owner-state uses canonical CBOR with a same-length-keys constraint — confirm the additive field is canonical-compatible, else carry `seen` on a separate lightweight per-dataset presence message rather than in `RootPublishPayload`. Lean: piggyback if canonical-CBOR-safe.

## owner_state_sync migration (the validation)

Re-base `owner_state_sync` to construct a `FleetSyncEngine<OwnerState>` with its existing `merge_remote_into_local` as the `merger`, `b"owner-state-root-blob-v1"` tag, and current topic. Delete the now-duplicated loop/replay/publish/receive code from `owner_state_sync.rs`; keep `OwnerState`, the `apply_*` methods, and `owner_state_persist`. Net: same bytes on the wire and disk, fewer lines, shared engine. The existing owner-state two-engine convergence tests become the engine's regression harness.

## Notes consumer

1. **Model:** `NotesDoc = LWW-element-set` of `Note { id (ULID), text, created_at, updated_at, deleted_at: Option<Hlc> }`. Merge: per-`id` LWW on `updated_at`; delete = tombstone via `deleted_at` (mirrors mint's proven shape). Plain text in v1 (rich text/attachments out, per ZEB-361).
2. **Persistence:** owner-private CBOR file, encrypted via the same `encrypt_entry` path (`notes-v1` tag); never leaves the device set.
3. **Migration:** on first sync-capable launch, import existing `localStorage['harmony-notes:<ownerId>']` into the synced doc (one-time, idempotent, keyed by owner id), then the frontend `NotesService` swaps its persistence to Tauri IPC against the Rust dataset. No note loss; the thin `notes-service.ts` interface is preserved for callers.
4. **IPC:** `notes_list` / `notes_upsert` / `notes_delete` (snake_case Rust, camelCase JS args per repo convention).

## Crypto / auth

Owner-scoped exactly as today: keys derived from the owner `master_seed` via `KeyTree` (only bound devices hold the seed); state encrypted at rest (deterministic-nonce `encrypt_entry`) and in transit (random-nonce `encrypt_root_publish`); integrity via AEAD tags. No new key material. (Cross-owner signing/membership gates are a `community_state_sync` concern, out of scope here.)

## Testing

Mock `ContentStore` + in-memory channels (the existing owner/mint test harness pattern):
1. **Engine convergence:** two `FleetSyncEngine` instances, A writes → B converges; offline edits on both → deterministic convergence; blob-miss drop → retry on next publish.
2. **owner-state regression:** existing owner-state two-engine tests pass unchanged through the engine; a wire-format pin test proves the on-wire `RootPublishPayload` + blob bytes are identical pre/post migration.
3. **Notes:** note written on A appears on B; concurrent edit/delete converges (LWW + tombstone); `localStorage` import is idempotent and lossless.
4. **Durability indicator:** N-instance fan-out reports the correct "synced to N devices" count; single-device reports 1.
5. **CRITICAL ordering:** failure injected between apply and persist does not advance the tracker (peer republish retried).

## Gates (from `src-tauri/`)

Per repo policy (`--locked`, `--all-targets`, `--features test-fixtures` are load-bearing; harmony-app relink cost ⇒ scope `--lib` per task, reserve `--all-targets` for the final sweep):
- `cargo fmt --all -- --check`
- per-task: `cargo clippy --locked -p harmony-app --lib --features test-fixtures --no-deps -- -D warnings`
- per-task: `cargo nextest run --locked -p harmony-app --lib --features test-fixtures -E 'test(fleet_sync)+test(owner_state)+test(notes)'`
- final sweep: `--all-targets` clippy + nextest + MSRV `cargo check`
- frontend: `npx tsc --noEmit && npx vitest run` (Notes IPC swap)

## Risks

1. **owner-state migration regression.** Mitigated by wire/disk byte-pin tests + reusing the existing convergence suite as the engine harness; migration changes routing, not behavior.
2. **Canonical-CBOR field addition** for the `seen` vector — verified compatible or moved to a side channel (open item above).
3. **Notes localStorage→Rust migration data-loss.** Mitigated by idempotent import + keep-localStorage-until-confirmed.
4. **Generic ergonomics.** The `merger` closure + `FleetPersist` trait keep `S` opaque to the engine; heterogeneous per-field conflict rules stay inside each consumer's merge, not the engine.

## Open items deferred to the implementation plan

1. Exact `FleetSyncEngine`/`FleetPersist`/`MergeOutcome` signatures (read the donors' real signatures during planning).
2. `seen`-vector placement (piggyback vs side channel) pending the canonical-CBOR check.
3. Whether `list_online_devices()` reads the replay tracker, the `OwnerDeviceCache`, or both.
4. Event-loop wiring for the new `notes` publisher/subscriber pair (mirror the owner/mint adapter).
