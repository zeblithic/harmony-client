# CI compilation cache (sccache → Cloudflare R2)

harmony-client CI caches Rust compilation with [sccache](https://github.com/mozilla/sccache),
backed by the shared Cloudflare R2 bucket `harmony-sccache`. This caches
*every* crate's `rustc` output — including the workspace crate and the
vendored `zenoh-link` fork — which `Swatinem/rust-cache` cannot (it only
caches third-party dependency artifacts). See ZEB-499.

## How it's wired

- The three Rust jobs (`rust-check`, `rust-test`, `msrv`) install sccache via
  the pinned `taiki-e/install-action` and set `RUSTC_WRAPPER=sccache`.
- Backend config is workflow-level env: `SCCACHE_BUCKET=harmony-sccache`,
  `SCCACHE_ENDPOINT` (R2 S3 endpoint), `SCCACHE_REGION=auto`, and
  `SCCACHE_S3_KEY_PREFIX=harmony-client` (namespaces our entries vs the
  harmony repo, which shares the bucket).
- `CARGO_INCREMENTAL=0` is required — incremental artifacts are not cacheable
  by sccache.
- `Swatinem/rust-cache` is kept with `cache-targets: false` to warm `~/.cargo`
  (the crates.io index/downloads), which sccache does not cache. We no longer
  cache `target/` — R2 owns compiled artifacts, which also removed the 10 GB
  Actions-cache churn (ZEB-440).

## Credentials

Two repo secrets hold a single R2 API token scoped to the `harmony-sccache`
bucket with Object Read & Write:

- `SCCACHE_R2_ACCESS_KEY_ID`
- `SCCACHE_R2_SECRET_ACCESS_KEY`

They are exported as `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` at job
scope on the Rust jobs only. `RUSTC_WRAPPER=sccache` is set **only when the
secret is present**, so fork PRs (which GitHub denies secrets) compile
normally without sccache instead of failing.

**Rotating the token:** create a new R2 API token in the Cloudflare dashboard
(R2 → API Tokens, Object Read & Write on `harmony-sccache`), update both repo
secrets (`gh secret set SCCACHE_R2_ACCESS_KEY_ID` / `..._SECRET_ACCESS_KEY`),
then revoke the old token.

## Verifying it works

Each Rust job ends with `sccache --show-stats`. On a PR that doesn't change
Rust source, expect a high cache-hit rate; the first `main` run after a
dependency or source change is mostly misses (it uploads to R2 for next time).
