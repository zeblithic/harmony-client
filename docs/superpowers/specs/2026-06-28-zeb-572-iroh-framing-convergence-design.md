# ZEB-572 — iroh length-prefixed framing convergence (design)

**Status:** approved (design + full-convergence scope, 2026-06-28)
**Ticket:** ZEB-572 (surfaced by the ZEB-571 platform/app seam audit, Tier-3 item 10)
**Repo:** harmony-client only (the `harmony-node` BE twin is a deferred follow-up)

## Problem

The "length-prefixed framing over an iroh bi-stream" codec — a `u32` byte-length
prefix + a cap-before-alloc DoS guard + a read-exact body — is open-coded across
**10 sites on current `main`**, in two endiannesses. The ticket cited 4; six more
drifted in since it was pinned at `9c1bf201`.

| Site | Endian | Shape | Zero-len | Cap constant |
|---|---|---|---|---|
| `butler_deposit` (`*_length_prefixed*` helpers) | LE | factored, plain I/O | reject | `DEPOSIT_MAX_FRAME_BYTES` (256 KiB) / `RELAY_PULL_MAX_FRAME_BYTES` (16 MiB) |
| `tunnel_task` (`*_length_prefixed` helpers) | **BE** | factored, plain I/O | **allow** | `HANDSHAKE_MAX_MESSAGE` (8 KiB) / `DATA_MAX_MESSAGE` (2 MiB) |
| `iroh_pex_acceptor` | LE | inline, per-step `tokio::time::timeout`, `String` err | reject | `PEX_MAX_PACKET_LEN` |
| `iroh_friend_acceptor` | LE | inline (read) + `write_friend_response` helper, per-step timeout, `FriendAcceptError` | reject | `FRIEND_MAX_PACKET_LEN` |
| `iroh_invite_acceptor` | LE | inline, per-step timeout, `HandshakeAcceptError` | reject | `HANDSHAKE_MAX_PACKET_LEN` |
| `open_join_dial` | LE | inline, timeout + `conn.close` + structured outcome | reject | `HANDSHAKE_MAX_PACKET_LEN` |
| `lib.rs` ×4 (invite-redeem dial + 3 siblings) | LE | inline, timeout + `conn.close` + structured outcome | reject | `HANDSHAKE_MAX_PACKET_LEN` |

Two distinct hazards (both latent, neither live):
1. **Interop trap** — a future "share one helper" refactor that assumes a single
   endianness silently breaks whichever protocol's peer still expects the old order.
2. **Duplicated security boundary** — the cap-before-`Vec::with_capacity`/`read_exact`
   guard is re-implemented per copy. One copy with a wrong bound is a real DoS vuln,
   and there is no single audited implementation.

## Key insight: two orthogonal axes of duplication

8 of the 10 sites bury the codec inside per-step `tokio::time::timeout(...)` +
structured-error + (for dial paths) `conn.close()` + structured-outcome
scaffolding. A single `read_len_prefixed(reader, max)` call **cannot** preserve the
per-step timeout attribution (`step: "read length-prefix"` vs `"read body"`) or each
protocol's typed error. So the **security boundary** (the cap guard + prefix
encode/decode) and the **async I/O loop** are different axes — only the first is
duplicated at all 10 sites. Splitting them is what makes converging all 10 safe.

## Design — two-layer `src-tauri/src/iroh_framing.rs`

### Layer 1 — pure, sync, audited core (the single security boundary)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian { Be, Le }

/// A length prefix that is zero (when empty is disallowed) or exceeds the cap.
/// Raised BEFORE any body byte is read or allocated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("frame length {len} out of bounds (min {}, max {max})", if *.allow_empty { 0 } else { 1 })]
pub struct FrameLenError { pub len: usize, pub max: usize, pub allow_empty: bool }

/// Encode a body length into a 4-byte prefix. Rejects empty (unless `allow_empty`)
/// or `> max` before producing any bytes.
pub fn encode_len_prefix(body_len: usize, max: usize, endian: Endian, allow_empty: bool)
    -> Result<[u8; 4], FrameLenError>;

/// Decode a 4-byte length prefix into the body length to read next. Rejects 0
/// (unless `allow_empty`) or `> max` — an attacker-supplied prefix never drives an
/// allocation past `max`.
pub fn decode_len_prefix(buf: [u8; 4], max: usize, endian: Endian, allow_empty: bool)
    -> Result<usize, FrameLenError>;
```

All 10 sites use these for the length check. An entangled site changes from

```rust
let len = u32::from_le_bytes(len_buf) as usize;
if len == 0 || len > MAX { return Err(SomeError::PrefixOutOfBounds { len, max: MAX }); }
```

to

```rust
let len = decode_len_prefix(len_buf, MAX, Endian::Le, false)
    .map_err(|e| SomeError::PrefixOutOfBounds { len: e.len, max: e.max })?;
```

— a ~2-line swap that keeps every surrounding `tokio::time::timeout`, typed error,
and `conn.close()`/outcome branch byte-for-byte. The write side likewise computes
the prefix via `encode_len_prefix(body.len(), MAX, Endian::Le, false)?` and writes it
under the site's existing timeout.

### Layer 2 — async convenience wrappers (built on Layer 1)

```rust
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    #[error("frame length out of bounds: {0}")]
    OutOfBounds(#[from] FrameLenError),
    #[error("frame I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Write `[u32 prefix][body]`; rejects out-of-bounds bodies before writing anything.
pub async fn write_len_prefixed<W: AsyncWrite + Unpin>(
    w: &mut W, body: &[u8], max: usize, endian: Endian, allow_empty: bool,
) -> Result<(), FramingError>;

/// Read `[u32 prefix][body]`; rejects an out-of-bounds prefix before allocating.
pub async fn read_len_prefixed<R: AsyncRead + Unpin>(
    r: &mut R, max: usize, endian: Endian, allow_empty: bool,
) -> Result<Vec<u8>, FramingError>;
```

Used by the 2 plain factored sites only:
- `butler_deposit::{write,read}_length_prefixed_with_max` keep their public
  signatures + `DepositWireError` (other modules call them, e.g.
  `community_relay_pull_driver`). Bodies become a delegation to
  `iroh_framing::{write,read}_len_prefixed(.., Endian::Le, /*allow_empty*/ false)`,
  mapping `FramingError::OutOfBounds → DepositWireError::FrameOutOfBounds`,
  `FramingError::Io → DepositWireError::Io`. Every existing butler test stays green.
- `tunnel_task::{write,read}_length_prefixed` delegate with `Endian::Be,
  allow_empty: true`, mapping `FramingError → Box<dyn Error + Send + Sync>`.

### `allow_empty` is load-bearing

`tunnel_task::read_length_prefixed` rejects only `len > max` — it **accepts**
zero-length BE frames. Every LE site rejects `len == 0`. Without the flag,
convergence would silently change `tunnel_task`'s accept behavior. The flag defaults
conceptually to "reject empty"; only `tunnel_task` passes `true`.

### Single-buffer write

`tunnel_task` deliberately writes prefix+body from one buffer ("a partial write
can't leave the peer's `LengthDelimitedCodec` mid-frame"); `butler_deposit` does two
`write_all`s. Both emit identical bytes. Layer 2's `write_len_prefixed` uses the
single-buffer form (strictly safer under cancellation; one extra `Vec` alloc, on a
control-plane handshake path — negligible). This does not change any wire byte.

## Wire-compatibility guarantee

Option (a) from the ticket: **endianness is a parameter; every shipped wire format is
preserved.** Each site passes the endianness it uses today. New framing should use
`Endian::Be` (network order) by convention, documented in the module header.

Pinned by a **round-trip test per endianness/empty combination** plus the migrated
sites' existing wire/round-trip tests. The convergence changes no constant and no
byte order at any site.

## Out of scope (deferred)

- `harmony/crates/harmony-node/src/tunnel_task.rs` BE twin — different repo,
  transport frozen/unused, one-PR-per-repo. Tracked as a harmony-repo follow-up
  alongside ZEB-571 item 12.
- Option (b) (migrate the LE protocols to BE behind a coordinated wire bump) — defer
  to whenever those protocols next bump their wire version.

## Acceptance

- One framing module (`iroh_framing.rs`); zero open-coded `u32` length-prefix
  encode/decode + bound-check in the 10 cited sites (all route through Layer 1 or 2).
- A single audited cap-before-alloc guard, unit-tested for oversize-reject,
  zero-reject (and zero-accept under `allow_empty`), at-cap-accept, and BE/LE
  round-trip (mirrors `butler_deposit.rs` oversize tests).
- Every existing protocol's wire bytes unchanged — full Rust suite green
  (`cargo nextest run --locked --workspace --all-targets --features test-fixtures`),
  clippy `-D warnings`, fmt.
