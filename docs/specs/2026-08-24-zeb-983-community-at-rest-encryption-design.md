# ZEB-983: Device-sealed at-rest encryption for community persistence

**Status:** approved 2026-08-24 (design review with Jake; all three decision
points accepted as recommended)
**Prior art:** ZEB-981 (`fleet_dataset_file`, sealed v2 under the fleet
KeyTree), ZEB-982 (`device_dataset_file`, sealed v3 under the seed-derived
device key — spec `docs/specs/2026-08-23-zeb-982-device-at-rest-encryption-design.md`)

## Problem

Everything under `identity_dir/communities/{cid_hex}/` is plaintext:

- `crdt.cbor` — the signed membership log: roster, power structure,
  kick/unban reasons, channel topology.
- `replay.cbor` — per-device root-HLC tracker.
- `segments.cbor` — the ZEB-814 publisher segment index. The state-root
  segment *blobs* live in CAS already encrypted; this sidecar holds each
  segment's 32-byte `k_s` key — sealing it is what protects those keys.
- `voting.cbor` — every `SignedVotingEvent` + policy + `PollRestore` overlay.
- `addrbook.cbor` — the co-member network graph (up to 4096 rows).
- `backfill_state.cbor` (community-level and per-channel) — resync stamps.
- `pre_fork_snapshot.bin` — full membership event set + channel-log snapshot
  + fork_reason; the largest single file in the tree.
- `channels/{ch_hex}/manifest.cbor` + `tail.cbor` + `segments/{N:08x}.cbor`
  — **the entire community chat history with `Post.body` in cleartext.**
  The largest at-rest exposure in the tree. (Channel events are AEAD-sealed
  on the wire under epoch channel keys; the at-rest form is the decrypted,
  verified event.)

Threat model is ZEB-982's: protection floor equals the seed's own
keychain/passphrase protection; the adversary is offline disk access
(backups, stolen disk images, other-user reads), not a live same-user
process.

**A second problem this ticket must fix, discovered in design
exploration:** the channel log already has a dead-channel cliff *without*
encryption. `ChannelLogEngine::new` walks every manifest segment with a
hard `?` (`community_channel_log_engine.rs:588-590`) to rebuild the replay
tracker — one unreadable segment file fails engine construction. The spawn
site warns and continues (`reconcile_from_state`), `lib.rs` warns once,
and nothing retries in-session: no engine ⇒ no backfill driver ⇒ the
channel is dead every session until the file is fixed by hand. Sealing
widens the entrance to this cliff (an undecryptable file joins the class),
so the recovery contract is designed first and the envelope drops in
beneath it.

## Decisions (approved 2026-08-24)

1. **Channel-log recovery = finest-granularity quarantine, Crypto-only.**
   Corrupt segment → quarantine that segment + drop it from the manifest
   (RBSR refills the hole). Corrupt tail → quarantine `tail.cbor` alone.
   Corrupt manifest → quarantine the whole channel log dir (fresh log →
   automatic full-history backfill). I/O errors stay hard everywhere —
   transient ≠ corrupt (ZEB-460). Rejected: coarse whole-log quarantine
   (throws away readable history); keep-hard-error (the ticket exists to
   retire that contract).
2. **Write-atomicity unifies on `save_atomically`.** The three duplicate
   `write_atomic` implementations (community_state_persist, voting's
   byte-identical copy, addrbook's inline copy) are deleted; every
   community write routes through `device_dataset_file::write_image` →
   `owner_state_persist::save_atomically` (randomized temp + file fsync +
   dir fsync; `write_image` supplies `create_dir_all`). The documented
   no-fsync rationale ("peer-recoverable") is retired: writes are
   debounced, so the fsync cost is bounded by debounce cadence. Voting's
   hold-mutex-across-write survives harmlessly (its fixed-`.tmp` collision
   rationale disappears with randomized temp names).
3. **Channel segments migrate eagerly at engine spawn.** Spawn already
   reads every sealed segment during index/tracker rebuilds; migration
   adds one write per legacy segment, once ever, inside the existing
   `spawn_blocking` reload. Rejected: background task (plaintext lingers,
   needs its own lifecycle); lazy (immutable segments would never migrate
   — historical chat bodies stay in the clear indefinitely).

## Key, envelope, AAD

- **Key & format inherited unchanged from ZEB-982:** `DeviceCipher`
  (HKDF-SHA256 from the node identity master seed), sentinel
  `SEALED_DEVICE_SCHEMA_V3 = 3`, envelope
  `[0x03]‖nonce(12)‖ChaCha20-Poly1305(complete legacy image)‖tag(16)`,
  256 MiB size cap, no plaintext fallback mode. Available on every boot
  mode — the registry is constructed outside the fleet band and
  `device_cipher` is already derived and memoized before the community
  band (`lib.rs:5973`), including ZEB-905 keyless local-only boots.
- **AAD label = the stable path relative to `identity_dir`:**
  - `communities/{cid_hex}/crdt.cbor` (likewise replay/segments/voting/
    addrbook/backfill_state/pre_fork_snapshot.bin)
  - `communities/{cid_hex}/channels/{ch_hex}/manifest.cbor` (likewise
    tail.cbor, backfill_state.cbor)
  - `communities/{cid_hex}/channels/{ch_hex}/segments/{N:08x}.cbor` —
    the label for a segment is built from the descriptor's validated
    `rel_path` (already checked against absolute/`..` escape in
    `read_segment_at`).

  Binding community id + channel id + role + segment index into the AAD
  makes cross-community, cross-channel, and cross-index ciphertext swaps
  fail the tag. This CLOSES a real gap: stored events are not re-verified
  against their channel binding on reload ("the per-stored-event binding
  isn't re-checked — only the manifest is"), and legacy segment files
  carry no internal binding at all.
- Label construction is centralized in one helper per module family (a
  `fn seal_label(...) -> String` beside the path builder), never inlined
  at call sites — the AAD string and the path must be derived from the
  same inputs or a rename refactor could silently split them.

## Cipher threading

- `CommunityRegistryConfig` gains `device_cipher: DeviceCipher`
  (`community_state_sync.rs`); the registry threads it into
  `spawn_engine_inner_now` loads, `persist_both`/`persist_crdt_only`/
  `persist_replay_only`, and the `encode_root_packet` segment-index
  load/save.
- `ChannelLogRegistryConfig` gains `device_cipher: DeviceCipher`
  (`community_channel_log_engine.rs`); threaded into `ChannelLog::reload`,
  `flush_tail`, `seal_and_persist`, `read_segment_at`,
  `ChannelBackfillState::{load, save}`, and the engine spawn walk.
- Free functions gain a `cipher: &DeviceCipher` parameter:
  `community_voting_persist::{write_snapshot, save_voting_log,
  save_policy_only, load_voting_log}`,
  `community_address_book::{save_addrbook, load_addrbook}`, the
  `pre_fork_snapshot.bin` writers (community_fork.rs + the redeem-side
  copy in lib.rs) and its three readers.
- Production wiring: the existing `device_cipher` binding at
  `lib.rs:5973` is cloned into both registry configs and the voting/
  addrbook/fork call sites. No new derives; `get_or_derive` stays a
  boot-time-only concern.
- Tests: every registry/engine test construction site (~40) threads
  `device_dataset_file::test_cipher()`. Consistency rule from ZEB-982:
  within one test, the cipher that seals fixtures must be the cipher the
  code under test is threaded — always `test_cipher()` here, since
  production threads rather than re-derives.

## Per-family integration (envelope beneath the contract)

Each family keeps its exact load/save signature shape plus the cipher
parameter; internally raw `fs::read`/`write_atomic` becomes
`read_image`/`write_image`, and the existing schema/CBOR handling runs
against `image.bytes`. Envelope `Crypto` maps onto each family's existing
*corruption* branch; envelope `Io` onto its existing *transient* branch:

| File | Crypto → | Io → | Preserved asymmetries |
|---|---|---|---|
| `crdt.cbor` | quarantine `.corrupt.<unix_ms>` + default | hard `PersistError::Io` | inner `community_id` mismatch stays HARD and deliberately un-quarantined |
| `replay.cbor` | quarantine + default | hard | |
| `segments.cbor` | quarantine + default | hard | costs one full segment re-upload on next publish (unchanged) |
| `voting.cbor` | quarantine + empty | hard → caller disarms persistence for the session (engine still runs; RBSR recovers in memory) | id-mismatch IS quarantined here (unlike crdt) — preserved |
| `addrbook.cbor` | `Vec::new()` (swallow) | `Vec::new()` | no Result type; rows re-verified by signature on ingest |
| `backfill_state.cbor` ×2 | `None` | `None` | pure hint; see migration note |
| `pre_fork_snapshot.bin` | warn + degrade (invite path also clears `forked_from` to keep the paired invariant) | per-reader stance kept (invite: warn+None; metadata getters: hard) | the two hand-rolled `.bin.tmp` writers (fork + redeem) unify onto `write_image` |

Quarantine filename dialect is preserved per family: community families
keep `.corrupt.<unix_ms>` (dot); nothing here adopts the fleet hyphen
form. Quarantined bytes are ciphertext going forward — strictly better
than today's plaintext asides.

## Channel-log recovery contract (new)

The channel log gains its first quarantine semantics, at the finest
granularity that machinery already proven in production can heal:

- **Segment (Crypto) — during the engine-spawn tracker walk:** new
  `ChannelLog::quarantine_segment(&mut self, descriptor)` renames the
  segment file to `<name>.corrupt.<unix_ms>`, removes its descriptor from
  the in-memory manifest, and rewrites `manifest.cbor` sealed. The walk
  warns and continues. Soundness: the replay tracker rebuild is a max-fold
  (`record`), so a skipped segment only lowers lane watermarks; the
  ZEB-969 append-side ReconcileKey guard is the authoritative duplicate
  gate, and the reconcile index already skips unreadable segments — RBSR
  set-reconciliation then refills the mid-history hole from peers (holes
  are refilled by set difference, not by `since` watermarks). The three
  derived-view rebuild loops in `reload` keep their existing warn-and-skip.
- **Tail (Crypto) — during `reload`:** quarantine `tail.cbor` alone
  (`tail.cbor.corrupt.<unix_ms>`), continue with an empty tail and the
  intact manifest + segments. Backfill refills the tail window (the
  watermark comes from segment ranges).
- **Manifest (Crypto) — during `reload`:** quarantine the WHOLE channel
  log dir (rename `channels/{ch_hex}` → `channels/{ch_hex}.corrupt.<unix_ms>`,
  recreate empty). Rationale: segments are unreachable without the
  manifest, and a fresh log starts sealing at index 0 — leaving old
  segment files in place would let the next seal OVERWRITE
  `segments/00000000.cbor` (the orphan-index hazard). Fresh log ⇒
  `max_hlc() == None` ⇒ the backfill driver requests full history
  automatically (retry forever, 30 s → 600 s backoff). `backfill_state.cbor`
  moves aside with the dir; the fresh driver falls back to
  interval-from-spawn — correct, since the log is empty.
- **Legacy manifest id-mismatch stays HARD** (`ChannelLogPersistError::Manifest`)
  — wrong-data-at-path is a bug signal, not corruption. (A *sealed*
  swapped manifest never reaches that check: the AAD path binding fails
  it first, landing in the Crypto→dir-quarantine arm.)
- **Io stays hard everywhere** in reload and the spawn walk — a transient
  read failure must not relocate state (ZEB-460). The spawn site's
  existing warn-and-continue keeps the rest of the community's channels
  spawning.
- **Loss window, stated honestly:** locally-authored events that no peer
  ever received (publish is store-first) are unrecoverable when their
  file is quarantined — but under today's hard-error contract a corrupt
  file loses them identically (the bytes are unreadable either way);
  quarantine additionally recovers everything any peer holds, and keeps
  the ciphertext aside for forensics.
- **Documented, not relitigated:** a quarantined `crdt.cbor` in an
  invite-only community does not self-heal from reboot alone — bootstrap
  admission deliberately refuses a Joined publisher's root
  (`community_membership.rs:5481-5495`); recovery needs a fresh invite
  redemption. Pre-existing contract, unchanged by sealing.

## Migration & rollback

- **Community-level files** (`crdt`, `replay`, `segments`, `voting`,
  `addrbook`, `pre_fork_snapshot.bin`): eager byte-lossless reseal on
  first successful load (ZEB-982 pattern: reseal only AFTER the family's
  own parse succeeds; reseal write failure warns and leaves plaintext for
  the next boot).
- **Channel logs**: eager at engine spawn, inside the existing
  `spawn_blocking` reload — legacy manifest and tail rewritten sealed;
  each legacy segment read + rewritten sealed (the exact
  `[0x01]‖CBOR` inner image preserved byte-for-byte). Every file is
  independently legacy-detectable (first-byte sniff: anything ≠ 3 is a
  legacy image), so a crash mid-migration is safe and resumes next boot;
  no inter-file ordering constraint.
- **`backfill_state.cbor` (both levels): lazy** — sealed on next save
  (rewritten roughly hourly by the resync floor), legacy accepted on
  read indefinitely. It carries a single timestamp; an eager pass buys
  nothing.
- **Rollback (pre-983 binary + sealed files):** every community family
  degrades exactly as its corruption contract dictates — quarantine +
  default + peer-recovery for crdt/replay/segments/voting, empty
  addrbook, `None` hints, degraded fork reads, and for channel logs the
  OLD binary's hard-error (dead channel until re-upgrade — accepted; the
  old binary predates the recovery contract by definition). No silent
  misreads anywhere: sentinel 3 parses as neither valid CBOR nor schema
  byte 1.
- The whole-dir detach on community delete
  (`{cid_hex}.deleting.<nanos>.<seq>`) and the ZEB-436 dir-level
  freshness probe are unaffected (both operate on names/dirs, not
  contents).

## Out of scope

- CAS-stored state-root segment blobs — already encrypted under
  per-segment `k_s`; sealing `segments.cbor` protects the keys.
- Wire formats, epoch channel keys, CRDT logic — untouched; this is the
  at-rest layer only.
- Invite-only bootstrap admission (above).
- Mail (ZEB-984), `ledger.db` (ZEB-985), JSON sweep (ZEB-986).

## Testing

- **Per-family contract pins:** for each community-level family — sealed
  round-trip; Crypto → its exact recovery branch (quarantine dialect
  `.corrupt.<unix_ms>` asserted); Io → its exact transient branch
  (dir-at-path trick for deterministic non-NotFound reads); the crdt
  id-mismatch-hard vs voting id-mismatch-quarantined asymmetry.
- **Channel recovery matrix:** segment-Crypto → quarantined + descriptor
  dropped + manifest rewritten sealed + engine spawns + reconcile index
  consistent; tail-Crypto → tail quarantined, history intact, engine
  spawns; manifest-Crypto → dir quarantined, fresh log, `max_hlc() == None`;
  segment-Io and tail/manifest-Io → hard error (contract pinned);
  regression: the pre-983 dead-channel repro (unreadable plaintext
  segment) now spawns.
- **Migration:** byte-lossless reseal per family (inner image compared);
  mixed-dir resume (some files sealed, some legacy → full migration
  completes on next spawn); `.hrmr`-style fixture: a pre-983 community
  dir boots and fully seals.
- **AAD swaps:** same-name file moved across communities fails; channel
  tail swapped across channels fails; segment ciphertext copied to a
  different index fails.
- **Keyless boot:** registry + channel logs sealed-functional on a
  ZEB-905 local-only boot (test threads `test_cipher()`; production path
  asserted by the existing boot wiring).
- Full gates: fmt, clippy `--all-targets -D warnings`, full workspace
  nextest sweep, tsc + vitest (no frontend change expected).
