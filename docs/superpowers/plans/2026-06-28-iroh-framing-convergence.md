# iroh Framing Convergence — Implementation Plan

> **For agentic workers:** TDD per task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Converge the 10 open-coded iroh length-prefixed framing sites onto one audited module, preserving every wire byte.

**Architecture:** Two-layer `src-tauri/src/iroh_framing.rs` — a pure sync core (`encode_len_prefix`/`decode_len_prefix` = the single cap-before-alloc guard, used by all 10 sites) and async wrappers (`read_len_prefixed`/`write_len_prefixed`, used by the 2 plain factored sites). Endianness + empty-allowed are parameters; no wire format changes.

**Tech Stack:** Rust, tokio `AsyncRead`/`AsyncWrite`, `thiserror`.

## Global Constraints

- Every wire byte at every site is preserved — endianness per-site is whatever it is today.
- Gates: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`, `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.
- `--all-targets` is load-bearing (catches integration-test compile breaks).
- No `KeychainStore::new()` in test-reachable code; not relevant here (no identity paths touched).
- One PR (harmony-client only). The `harmony-node` BE twin is a deferred follow-up.

---

### Task 1: Layer 1 — pure core (`encode_len_prefix` / `decode_len_prefix`)

**Files:** Create `src-tauri/src/iroh_framing.rs`; Modify `src-tauri/src/lib.rs` (add `pub mod iroh_framing;` by the other `iroh_*` mods, ~line 190).

**Produces:** `Endian{Be,Le}`, `FrameLenError{len,max,allow_empty}`, `encode_len_prefix(body_len,max,endian,allow_empty)->Result<[u8;4],FrameLenError>`, `decode_len_prefix(buf:[u8;4],max,endian,allow_empty)->Result<usize,FrameLenError>`.

- [ ] Tests (in-module `#[cfg(test)]`): encode rejects `0` when `!allow_empty`; encode accepts `0` when `allow_empty`; encode rejects `> max`; encode at `max` ok. decode mirror (zero-reject / zero-accept / oversize-reject / at-cap-accept). `Endian::Be` vs `Le` produce different bytes for the same len; encode→decode round-trips for both. Error carries `{len,max}`.
- [ ] Implement (simple `if`-guards + `u32::to_be_bytes`/`to_le_bytes` / `from_*`). Keep `FrameLenError`'s `Display` simple: `"frame length {len} out of bounds (max {max})"`.
- [ ] `cargo nextest run -p harmony-app --features test-fixtures -E 'test(iroh_framing)'` → green; `cargo clippy --all-targets`; commit.

### Task 2: Layer 2 — async wrappers (`read_len_prefixed` / `write_len_prefixed`)

**Files:** Modify `src-tauri/src/iroh_framing.rs`.

**Produces:** `FramingError{OutOfBounds(FrameLenError),Io(io::Error)}`, `write_len_prefixed<W:AsyncWrite+Unpin>(w,body,max,endian,allow_empty)`, `read_len_prefixed<R:AsyncRead+Unpin>(r,max,endian,allow_empty)->Result<Vec<u8>,FramingError>`.

- [ ] Tests: round-trip a body through `Vec`/`&[u8]` cursor for `(Le,false)` and `(Be,true)`; oversize prefix on a prefix-ONLY reader returns `OutOfBounds` (not `Io`/EOF) — proves rejection precedes body read; empty body rejected under `!allow_empty`, written/read under `allow_empty`. Write uses single-buffer (prefix+body in one `write_all`).
- [ ] Implement (write: `encode_len_prefix?` → build `[prefix ‖ body]` → one `write_all`; read: `read_exact(4)` → `decode_len_prefix?` → `vec![0;len]` → `read_exact`).
- [ ] tests green; clippy; commit.

### Task 3: Migrate `butler_deposit` → Layer 2

**Files:** Modify `src-tauri/src/butler_deposit.rs:255-301`.

- [ ] Keep public sigs of `write_length_prefixed_with_max` / `read_length_prefixed_with_max` (+ the no-`_with_max` wrappers). Reimplement bodies to delegate to `iroh_framing::{write,read}_len_prefixed(.., Endian::Le, false)`, mapping `FramingError::OutOfBounds(e) → DepositWireError::FrameOutOfBounds{len:e.len,max:e.max}`, `Io(e) → DepositWireError::Io(e)`.
- [ ] Existing butler tests (`frame_read_rejects_oversize_before_body`, the relay-cap tests) MUST stay green untouched. Run `-E 'test(butler_deposit)'`; clippy; commit.

### Task 4: Migrate `tunnel_task` → Layer 2 (BE, allow_empty)

**Files:** Modify `src-tauri/src/tunnel_task.rs:506-535`.

- [ ] Reimplement `write_length_prefixed`/`read_length_prefixed` to delegate with `Endian::Be, allow_empty: true`, mapping `FramingError → Box<dyn Error+Send+Sync>` (`.into()` on a formatted string, preserving the existing "message too large" semantics). Zero-length still accepted.
- [ ] Run `-E 'test(tunnel)'` (+ any handshake tests); clippy; commit.

### Task 5: Migrate `iroh_pex_acceptor` → Layer 1

**Files:** Modify `src-tauri/src/iroh_pex_acceptor.rs:160-231`.

- [ ] Read: replace `from_le_bytes`+bound-check with `decode_len_prefix(len_buf, PEX_MAX_PACKET_LEN, Endian::Le, false).map_err(|e| format!("length-prefix out of bounds: len={} max={}", e.len, e.max))?`. Write: replace `resp.len()>MAX` check + `resp_len.to_le_bytes()` with `encode_len_prefix(resp.len(), PEX_MAX_PACKET_LEN, Endian::Le, false).map_err(...)?` then write the `[u8;4]`. Keep all `tokio::time::timeout` wrapping.
- [ ] Run `-E 'test(pex)'`; clippy; commit.

### Task 6: Migrate `iroh_friend_acceptor` → Layer 1

**Files:** Modify `src-tauri/src/iroh_friend_acceptor.rs` (read ~1546-1559; `write_friend_response` ~1441-1456).

- [ ] Same swaps, `FRIEND_MAX_PACKET_LEN`, mapping to `FriendAcceptError::PrefixOutOfBounds{len,max}` (read) / `FriendAcceptError::ResponseTooLarge{len,max}` (write). Keep timeouts + `FriendAcceptError::IoTimeout{step}`.
- [ ] Run `-E 'test(friend)'`; clippy; commit.

### Task 7: Migrate `iroh_invite_acceptor` → Layer 1

**Files:** Modify `src-tauri/src/iroh_invite_acceptor.rs` (read ~310-324; write ~460-470).

- [ ] Same swaps, `HANDSHAKE_MAX_PACKET_LEN`, mapping to `HandshakeAcceptError::PrefixOutOfBounds{len,max}` / `ResponseTooLarge{len,max}`. Keep timeouts + structured errors.
- [ ] Run `-E 'test(invite)'`; clippy; commit.

### Task 8: Migrate `open_join_dial` → Layer 1

**Files:** Modify `src-tauri/src/open_join_dial.rs` (write ~259-264; read ~310-321).

- [ ] Same swaps, `HANDSHAKE_MAX_PACKET_LEN`. Read maps `decode_len_prefix` err → the existing `format!("response length out of bounds: ...")` String. Write computes prefix via `encode_len_prefix` (the inline write currently has no explicit oversize check — adding the guard is a net safety gain and preserves bytes). Keep `conn.close`/`OpenJoinOutcome` branches.
- [ ] Run `-E 'test(open_join)'`; clippy; commit.

### Task 9: Migrate `lib.rs` ×4 dial copies → Layer 1

**Files:** Modify `src-tauri/src/lib.rs` at the 4 dial sites (write `wire_len.to_le_bytes()` ~44375/45021/46718/47437; read `from_le_bytes(len_buf)` ~44469/45064/46760/47479 — re-grep for exact lines after prior edits).

- [ ] Per copy: read swaps to `decode_len_prefix(len_buf, HANDSHAKE_MAX_PACKET_LEN, Endian::Le, false)` → existing `format!("response length out of bounds: ...")`; write computes prefix via `encode_len_prefix(wire.len(), HANDSHAKE_MAX_PACKET_LEN, Endian::Le, false)`. Keep every `tokio::time::timeout` + `conn.close` + `RedemptionOutcome`/outcome branch.
- [ ] Run the relevant dial/redemption tests; clippy; commit.

### Task 10: Final sweep + full gates

- [ ] Grep the 10 files: zero remaining `from_le_bytes(len_buf)` / `from_be_bytes(len_buf)` / `len() as u32).to_*e_bytes()` framing (state_snapshot/dm_envelope/hashing uses are NOT framing — leave them).
- [ ] Module header documents the BE-for-new-framing convention.
- [ ] Full gates: `cargo fmt --all`; `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`; `cargo nextest run --locked --workspace --all-targets --features test-fixtures`. All green.
- [ ] Commit.
