# ZEB-674 — Per-viewer encrypted-file sharing (honest `sharedWith` viewer ACL)

**Status:** design approved 2026-07-20 (Jake). **Repo:** `harmony-client` (client-only; no `harmony` change → no rev-bump → single PR).
**Origin:** deferred surface from ZEB-669 (storage-buddies) spec §6 honesty ledger — "Old ShareList / `sharedWith` viewer ACL → deferred; it is an encrypted-content key-sharing problem."

## Problem

The Files detail panel used to show a "Shared with (can view)" list. ZEB-612 S3 (PR #441, commit `cd2ed6c8`) **removed** it rather than render it dishonestly: it was backed by two hardcoded `mockPeers`, with zero cryptographic meaning. ZEB-669 restored the other Files surfaces (origin rows, backup flag, contribution meter) against real backends but deliberately left `sharedWith` unimplemented because there was no real sharing model behind it.

This is **not a UI ticket.** In a content-addressed P2P store, "shared with" for non-public content means *who holds a decryption key*, not who is on a list — and the machinery to make that real does not exist yet.

### Verified constraints (confirmed against HEAD `00429e5e`)

Three code-level facts shape every decision below:

1. **Personal Files are 100% public today.** Every `ingest_content` CID is `PublicDurable`/unencrypted (`lib.rs` `ingest_content` → `streaming_ingest` with `IngestOptions::default()`, `flags.encrypted=false`). The "Private" `SensitivityBadge` is a decoupled cosmetic label — no production code mutates it and it does not imply encryption. Per-file keys do not exist; the only encrypt-at-ingest path is community channel artifacts under a *space epoch key* (`lib.rs:~28795`).

2. **Content class is immutable.** The `encrypted` flag is a hash input to the CID (`ContentId::for_book(&chunk, opts.flags)`, `lib.rs:594`). A public CID cannot be "flipped" to encrypted — that mints a different CID. So an ACL is only coherent for a file that was **encrypted at ingest**; you cannot retrofit encryption onto an existing public CID.

3. **The serve layer is CID-keyed, not identity-keyed (open-once-allowlisted).** The content-serve gate is `!cid.flags().encrypted || serve_allowlist.contains(cid)` over a *peerless* `HashSet<ContentId>` (`event_loop.rs:10484-10489`); the serve queryable replies to whoever sent the query with no ZID/peer/cert check (`event_loop.rs:10549-10567`). **There is no per-requester authorization.** Once an encrypted CID is allowlisted, anyone who can name it fetches the ciphertext. Therefore the *only* real access control on a shared blob is **possession of the decryption key**, and per-viewer revocation cannot be enforced at the serve layer.

## Product decisions (approved)

- **Scope — end-to-end MVP:** per-file encryption (C1) · grant record + wrap (C2) · deposit distribution (C3) · grantee read path (C4) · honest ShareList UI (C5) · **lazy** revoke. Deferred to follow-ups: encrypted-content **backup** unblock (the 3-gate public-durable relaxation), **rotate-on-revoke**, automatic **new-device re-seal** fan-out, PQ-hybrid seal, grantee-side "shared with me" browsing surface.
- **Grant privacy — confidential, deposit-only:** the content key is sealed end-to-end to the grantee and delivered *only* via butler deposit. The sharing graph `(cid, grantee)` is **never published**. The owner's ShareList renders from the owner's *own local* grant records.
- **Encryption model — private-at-ingest:** a file is born **public** *or* **private-encrypted** (fresh per-file key). The Share surface appears **only on encrypted files**; public files honestly show "Public — anyone with the link can view (no viewer list)." Retro-ACLing a public file means re-ingesting it as a new encrypted CID.

## Architecture (client-only)

Everything lands in `harmony-client/src-tauri` + `src/`. The `harmony` crates (`harmony-identity` X25519, `harmony-content` CID, `harmony-crypto`) are consumed unchanged.

### Reused primitives (build nothing new)

- **Seal to a recipient:** `dm_signing::seal_to_owner_with_info(x25519_pub, plaintext, info)` / `open_from_owner_with_info` — ephemeral-ECDH → HKDF(info) → ChaCha20-Poly1305, low-order-point-safe (`dm_signing.rs:66-159`). Use a **fresh domain string** `b"harmony-file-grant-v1"` so a grant-seal can never be opened as a DM/epoch seal.
- **Whole-blob symmetric encryption:** `encrypt_blob(&key, &plaintext)` (as used by the channel-artifact path, `lib.rs:28795`).
- **Recipient key resolution:** `owner_id_from_master_ed25519(friend_master_ed25519)` (`friend_graph.rs:57`) → `state.owner_device_cache.devices.get(&owner_addr)` → per device X25519 = `device_identity_pubs[i][0..32]` (`owner_state_types.rs:661-662`).
- **Offline delivery:** `build_deposit_frame` + `IrohButlerDepositClient`, extensible `DepositPayload` (`butler_deposit.rs:152-249, 463-495, 544-571`).
- **Encrypted-CID member serve:** `content_store` allowlist / `allow_serve_subtree` (ZEB-395/535/539).
- **Durable owner-local persistence:** `OwnerState` fields → `save_owner_state_cbor_only` / `write_atomic_0600` (`owner_state.rs:715-722`), replicated across the owner's own devices; secret values KeyTree-sealed before entering the CRDT field, exactly like `FriendEntry.sealed_secret` (`owner_state_crypto::encrypt_friend_secret`, `lib.rs:55317`).

### New pieces

| # | Component | Location |
|---|-----------|----------|
| C1 | **Encrypt-on-ingest**: fresh 32-byte DEK → `encrypt_blob(&dek, plaintext)` → `streaming_ingest_with_options(ciphertext, IngestOptions{ flags.encrypted=true, serveable })`. FE "private/encrypted" toggle on ingest. | `lib.rs` (mirror `:28795`); `src/lib/file-manager-service.ts` |
| — | **DEK store**: `file_deks: BTreeMap<ContentId, SealedFileDek>` on `OwnerState`; each DEK KeyTree-sealed like `FriendEntry.sealed_secret`; flushed via `write_atomic_0600`; auto-replicates to the owner's devices. | `owner_state_types.rs` |
| C2 | **Grant records** (owner-local, unpublished): `file_grants: BTreeMap<ContentId, Vec<GrantEntry>>`, `GrantEntry{ grantee_owner: [u8;16], granted_at: u64 }`. Sealing is done at *send* time from the DEK — **the sealed key is not stored**. | `owner_state_types.rs` |
| C3 | **Distribution**: add `grant_push: Option<Vec<u8>>` to `DepositPayload`; grant payload = CBOR `{ cid, file_meta, sealed_dek }` sealed per grantee device; deliver via `IrohButlerDepositClient`. Inbound routing extends the deposit-payload demux. | `butler_deposit.rs`, deposit-ingest path |
| C4 | **Grantee read**: `received_file_grants: BTreeMap<ContentId, ReceivedGrant{ granter_owner, sealed_dek, file_meta, received_at }>` on `OwnerState`; open = `open_from_owner_with_info` → DEK → fetch CID → `decrypt_blob` → plaintext. | `owner_state_types.rs`, `lib.rs` |
| C5 | **UI**: restore `ShareList.svelte` from `cd2ed6c8^`, re-typed onto a real `grants` DTO + a real friend picker (reuse `friendContacts`); mount in `FileDetailPanel.svelte` between the backup section (`:199`) and `FileActions` (`:207`); render **only when the file is encrypted**; self-hide until `list_grants` proves the set. | `src/lib/components/`, `src/App.svelte` |
| — | **IPC** (storage-buddy command pattern — thin `#[tauri::command]` → `_impl` → `emit_ser("grants-updated")`): `list_grants(cid)`, `grant_read(cid, grantee)`, `revoke_read(cid, grantee)`. Reject grant-of-public-CID / non-friend with the stable `ineligible:` prefix (`FileDetailPanel.svelte:89` idiom). | `lib.rs` (register at `:63197`), `src/lib/file-manager-service.ts` |

## Data flows

1. **Ingest private file.** FE "private" toggle → backend generates a fresh 32-byte DEK → `encrypt_blob(&dek, bytes)` → chunk the ciphertext with `flags.encrypted=true` → store the (sealed) DEK in `file_deks[cid]` → return an `EncryptedDurable` CID.
2. **Share F with friend Y.** Resolve Y's device X25519 set → for each device, `seal_to_owner_with_info(dev_x25519, dek, b"harmony-file-grant-v1")` → `content_store` allowlist F's CID for member serve → append `GrantEntry{ Y, now }` to `file_grants[cid]` → build a `grant_push` per device and deposit to Y's butler inbox.
3. **Y reads F.** Butler-inbox ingest lands the `grant_push` → Y records `received_file_grants[cid]{ granter, sealed_dek, meta }` → on open: `open_from_owner_with_info` → DEK → fetch the CID over the shared friend/community content transport (the owner, having allowlisted it, serves the ciphertext on `harmony/content/{shard}/{cid}`) → `decrypt_blob(&dek, ct)` → plaintext. Grant delivery requires friend resolution, so owner and grantee already share a content transport; if no serving node is reachable, the fetch surfaces a transient "content unavailable" error (not a crypto failure).
4. **Owner revokes Y** — see below.

## Security model (state plainly)

Because serve is open-once-allowlisted (constraint 3), **a shared file's ciphertext is fetchable by anyone who learns the CID** — the allowlist is not an access boundary and the CID is not a secret. Confidentiality rests *entirely* on DEK secrecy, i.e. the per-device sealed-key distribution. This is standard envelope encryption; the design does not assume the CID or the allowlist restrict who can fetch the ciphertext.

## Revocation semantics — **lazy** (honest limits)

Constraint 3 means an already-shared CID's access **cannot** be withdrawn: the grantee already holds the DEK, and the CID stays served (removing it from the allowlist would break the remaining, still-authorized viewers). So the MVP `revoke_read(cid, Y)`:

- removes `Y` from `file_grants[cid]` (Y disappears from the owner's ShareList), and
- stops the owner from *re-*delivering the grant to Y in future.

It does **not** and cannot revoke Y's existing access to that CID version. Honest UI copy:

> *"Removed from the list. Anyone previously shared with keeps access to this version — to fully cut them off, re-share a new private copy."*

Real cutoff = **rotate-to-new-CID** (re-encrypt under a fresh DEK, re-share to the remaining grantees, de-allowlist the old CID). That is the deferred **C6-full** follow-up, out of MVP scope.

## Multi-device reality

- **Grantee side:** a grant is sealed **per device** (constraint 2). At grant time the owner seals to *all currently-known* devices of Y (`owner_device_cache.devices[Y].device_identity_pubs`). A device whose X25519 pub is not yet learned (`None`, or enrolled *after* the grant) is unreachable until a re-share — the honest edge of "new-device re-seal deferred."
- **Owner side:** solved for free — `file_deks` and `file_grants` live on `OwnerState`, which already replicates across the owner's own devices, so any of the owner's devices can render the ShareList and re-share/decrypt.

## Testing (TDD)

Rust unit/integration (`--features test-fixtures`):

- DEK round-trip: encrypt-ingest → fetch ciphertext → decrypt → byte-identical plaintext.
- Per-device seal fan-out: a grantee with N known devices gets N sealed copies; each device opens with its own X25519; a foreign device fails to open.
- Grant record: append/remove, whole-record persistence + reload (`write_atomic_0600` round-trip), DEK stays sealed at rest.
- `grant_push`: `DepositPayload` encode/decode with the new field (backward-compatible `Option`); the inner seal is opaque to the butler (butler cannot open the DEK).
- Revoke: `revoke_read` drops the `GrantEntry`; a subsequent `list_grants` omits Y; no key material is destroyed (lazy).
- Honesty gates: Share surface hidden on public CIDs; `grant_read` on a public CID / non-friend returns `ineligible:`.

e2e-harness (stretch, `--features e2e`): two nodes — owner ingests a private file, shares with the grantee, grantee fetches + decrypts; revoke removes it from the owner's list.

## Scope boundary — explicit deferrals (each a clean follow-up ticket)

- **Encrypted-content backup unblock** — relax the 3 public-durable gates (`lib.rs:17923`, `:17253`, `storage_records.rs:671`) for *granted* CIDs, so buddy backup can cover encrypted files. Its own capability; own ticket.
- **Rotate-on-revoke (C6-full)** — real access cutoff via CID rotation + re-wrap.
- **New-device re-seal** — automatically re-deliver existing grants when a grantee enrolls a new device.
- **Grantee "shared with me" browser** — a first-class surface listing files others shared with you (the MVP delivers the *owner-side* ShareList + the grantee *read* path, not a grantee browse UI).
- **PQ-hybrid grant seal** — `hybrid_kem` instead of X25519-only, matching the DM PQ posture.

## Rollout

Single `harmony-client` PR (no `harmony` change). Standard gates: `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`; frontend build. Do **not** auto-merge.
