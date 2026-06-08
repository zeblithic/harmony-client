# ZEB-397 — Eliminate the large-test CI long pole by shrinking the depth-2 fixture

**Status:** Approved 2026-06-08
**Issue:** [ZEB-397](https://linear.app/zeblith/issue/ZEB-397) (High, harmony-client)
**Context:** The `Rust — large tests (HARMONY_LARGE_TESTS=1)` job is the per-PR
CI long pole (~45 min). Jake's call (2026-06-08): "either they finish within 20
minutes or they're useless and a waste of resources/electricity."

## Problem

The `rust-test-large` CI job exists for **exactly one test**:
`folder_ingest_walker_integration::nested_bundle_tree_round_trip`. That test
validates the depth-2+ nested-bundle tree-build path in `streaming_ingest` — the
path taken when a stream produces more than `MAX_BUNDLE_ENTRIES` (32,767) leaf
chunks and the tree-builder must add a second bundle level.

To reach >32,767 leaves it creates a **36 GiB + 1 byte sparse tempfile** and
streams it through `streaming_ingest` with `ChunkerConfig::DEFAULT`. On a
sparse-zero stream the FastCDC gear hash never fires a content boundary, so every
cut is forced at `max_chunk` (~1 MiB) → 36 GiB ÷ 1 MiB ≈ 36,864 leaves. Hashing
36 GiB of zeros through FastCDC takes **~17 min** of wall-clock; with the cold
compile of the test binary on top, the job runs ~37–45 min. Because GitHub
re-runs all checks on every push, even a doc-only commit restarts it — so in
practice nobody waits for it and PRs merge with it pending.

## Key insight

Depth-2 is driven by **leaf COUNT**, not byte volume. Leaf count on a sparse-zero
stream is `ceil(size / max_chunk)`, and `streaming_ingest` **already accepts a
`ChunkerConfig`** (the test just passes `DEFAULT`). `ChunkerConfig` is a plain
public struct `{ min_chunk, avg_chunk, max_chunk }` whose only validation is
`min < avg < max ≤ MAX_PAYLOAD_SIZE` (avg a power of two) — **no floor on
`max_chunk`**. So a tiny `max_chunk` forces the identical depth-2 tree-build with
a far smaller input:

| `max_chunk` | input for >32,767 leaves | FastCDC hash time |
|---|---|---|
| ~1 MiB (DEFAULT) | ~36 GiB | ~17 min |
| **4 KiB (this change)** | **~140 MiB** | **< 1 s** |

The tree-build path exercised is byte-for-byte identical — only the chunk size
differs. The 17-minute hash was never testing anything the logic needs; it was an
artifact of the default chunk size.

## Why all-zeros always cut at `max_chunk` (for any config)

FastCDC's rolling Gear hash over a constant byte converges to a fixed point:
`hash_n = G0·(2ⁿ−1) mod 2⁶⁴`, whose low `m` bits settle to `−G0 mod 2ᵐ` — a
fixed, pseudo-random, non-zero residue. The cut-on-`(hash & mask) == 0` test
therefore essentially never fires on zeros, regardless of which avg/max-derived
mask is used. So the "cut at `max_chunk`" behaviour the 36 GiB fixture relied on
holds for the 4 KiB config too. (Verified empirically — see Testing.)

## Change

**`src-tauri/tests/folder_ingest_walker_integration.rs`** — `nested_bundle_tree_round_trip`:

- Replace `ChunkerConfig::DEFAULT` with a const `TINY_CHUNKER`
  `{ min_chunk: 1024, avg_chunk: 2048, max_chunk: 4096 }`.
- Shrink the sparse fixture from `36 GiB + 1` to `36_000 * 4096 + 1`
  (~140 MiB), yielding ~36,001 leaves — same `(32_767, 50_000)` assertion
  window, same `Bundle(depth >= 2)` assertion, same inline-metadata round-trip.
- **Remove the `HARMONY_LARGE_TESTS` gate** — at ~140 MiB the test runs in the
  normal `cargo nextest run` flow on every PR.
- Update the (now stale) 36-GiB commentary to describe the leaf-count rationale.

**`.github/workflows/ci.yml`:**

- **Delete the `rust-test-large` job** entirely. The test now runs un-gated in
  the existing `rust-test` job under `--workspace --all-targets`.
- Update the `rust-test` job comment to record the ZEB-397 history.

No production code changes; no harmony-repo change (the `ChunkerConfig` seam
already exists). CI drops from 5 jobs to 4, and the per-PR long pole disappears —
depth-2 coverage now runs in seconds on every PR.

## Testing

- Run `nested_bundle_tree_round_trip` locally (now fast) and confirm:
  `root.cid_type() == Bundle(depth >= 2)`, `leaf_count ∈ (32_767, 50_000)`, and
  the inline-metadata `total_size`/`chunk_count` round-trip — i.e. the **same
  assertions** as the 36 GiB version, just reached cheaply.
- Full gates: `cargo fmt --all -- --check`,
  `cargo clippy --locked --all-targets --features test-fixtures --no-deps -- -D warnings`,
  `cargo nextest run --locked --workspace --all-targets --features test-fixtures`.

## Out of scope

- **Nightly real-scale soak.** Considered (run the 36 GiB fixture nightly for
  at-scale memory/I-O behaviour) and declined: the test validates tree-build
  *logic*, which the tiny fixture covers identically; the streaming path's
  bounded-memory behaviour is independently covered by the drain pattern and is
  not what the 36 GiB run was asserting. Revisit only if an at-scale regression
  is ever observed.
- **Other large tests.** There are none — `nested_bundle_tree_round_trip` was the
  sole `HARMONY_LARGE_TESTS`-gated test.
