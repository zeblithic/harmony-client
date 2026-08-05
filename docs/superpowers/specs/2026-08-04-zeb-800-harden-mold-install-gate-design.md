# ZEB-800 — Harden the CI mold-install gate against transient network failures

**Status:** design of record
**Ticket:** ZEB-800 (Medium, `harmony-client`)
**Author:** Koya (koya-zeblith.lan)
**Date:** 2026-08-04

## Problem

The `Install mold linker (ZEB-498)` step runs the pinned composite action
`rui314/setup-mold@9c9c13bf4c3f1adef0cc596abc155580bcb04444` (v1). mold is a
**pure speed optimization** — `make-default: true` symlinks `/usr/bin/ld` →
mold, so nothing in the build references it and a job that never installs mold
produces a **byte-identical** result, only slower. Yet the step can **red a
required gate** on a transient network blip: on PR #554's head `4df11152`
(run 30186565183, 2026-07-26) the runner failed to resolve `github.com`, the
action's `wget` failed, and the job exited non-zero — while every job that
actually exercises the code (`rust-check`, `frontend`, all three nextest
shards) passed on the same head. The identical tree then merged and the same
job passed post-merge, confirming it as infrastructure, not code.

A speed optimization is wired **fail-closed**: a step that contributes only
wall-clock can take down the whole run.

### Blast radius

Four steps pin the same action across two workflows:

| File | Line | Job |
| -- | -- | -- |
| `ci.yml` | 87 | `rust-check` — Rust — fmt + clippy |
| `ci.yml` | 256 | `rust-test` — nextest, all 3 shards |
| `ci.yml` | 407 | `msrv` — cargo check on declared rust-version |
| `release.yml` | 148 | Linux release build |

Six job instances per CI run (rust-test fans out to 3 shards). On `release.yml`
the same blip discards a full compile — the ZEB-764 §1 shape.

### Why existing retries don't cover it

The `wget` lives *inside* the pinned action, not in our YAML. Its flags are
`--timeout=10 --waitretry=3 --retry-connrefused`; `--retry-connrefused` retries
a refused *connection*, not a failure to *resolve* the host. A DNS timeout gets
exactly one attempt, then the pipeline collapses into `tar`.

## Verified safety analysis (the crux)

The fix is `continue-on-error: true`, which lets the job proceed when the step
fails. That is only safe if a **failed install can never leave `/usr/bin/ld`
pointing at a broken mold**. This was verified against the action source at the
pinned SHA, not assumed.

The action's `runs:` is `using: composite` with `shell: bash`. GitHub Actions
expands `shell: bash` to `bash --noprofile --norc -eo pipefail {0}` — so
**`errexit` and `pipefail` are both active**. The script is:

```bash
set -x
echo "mold <version>"
if [ "$(whoami)" = root ]; then SUDO=; else SUDO=sudo; fi
wget -O- --timeout=10 --waitretry=3 --retry-connrefused --progress=dot:mega \
  https://github.com/rui314/mold/releases/download/v<version>/mold-<version>-$(uname -m)-linux.tar.gz \
  | $SUDO tar -C /usr/local --strip-components=1 --no-overwrite-dir -xzf -
test <make-default> = true -a "$(realpath /usr/bin/ld)" != /usr/local/bin/mold \
  && $SUDO ln -sf /usr/local/bin/mold "$(realpath /usr/bin/ld)"; true
```

The download-and-extract pipeline and the symlink are on **separate lines**.
Under `pipefail`, a failed `wget | tar` returns non-zero; under `errexit`, the
script aborts **immediately, before the symlink line**. So on any download or
extract failure, `/usr/bin/ld` is never touched — it stays the distro default,
the job compiles correctly (slower), and `continue-on-error` simply spares the
required gate. The observed failure exiting with **code 2 from `tar`** is direct
evidence the script died at the pipeline and never reached the `ln`.

The ticket's caveat — "could a partial extract leave a truncated
`/usr/local/bin/mold` that then gets symlinked?" — is **discharged**:

1. The install is a streaming `wget -O- | tar -xzf -`. A truncated stream makes
   `tar` exit non-zero (`gzip: stdin: unexpected end of file`, as logged), so
   `tar` can only exit 0 on a complete, valid archive — i.e. a fully-extracted
   mold, not a truncated one.
2. Even if some exotic path left a bad binary, `errexit` means any non-zero
   step aborts before the symlink line. The `ln` is only reached when the
   pipeline **succeeded**, i.e. mold is fully installed.

Therefore bare `continue-on-error: true` is provably safe for the observed
failure and all realistic streaming-pipe failures. The restore-`ld` wrapper the
ticket floats as a fallback guards a case the pipe makes unreachable.

## Chosen approach — Option A

Add `continue-on-error: true` to the four mold-install steps, each with a
one-line comment stating the safety invariant (this is the **first**
`continue-on-error` in the repo, so the rationale must travel with the code).

`continue-on-error` governs only **failure** handling; the success path is
byte-for-byte unchanged. When the network is healthy, mold installs and the
symlink is created exactly as today.

### Rejected alternatives

- **B — restore-`ld` wrapper step.** Reimplement/guard the install and, on
  failure, explicitly reset `/usr/bin/ld`. Guards a case `errexit` + the
  streaming pipe already make unreachable; adds a bespoke step and drifts from
  the upstream action. YAGNI.
- **C — drop mold.** Out of scope. ZEB-498 measured mold's benefit and the
  rust-test sharding depends on the compile/link time it buys.

## Known cosmetic residual

`continue-on-error: true` renders the step as a red ✗ *annotation* on a blip
while the **job** and the required check stay green — the idiomatic GitHub
Actions signal for a best-effort step. Suppressing the annotation would require
reimplementing the action inline (a `run:` step with `|| true`), losing the
upstream SHA pin. We keep the pin and accept the annotation. Documented so a
red mold-step annotation on an otherwise-green run reads as "network blip,
handled," not "something broke."

## Scope

- **Touches:** `.github/workflows/ci.yml` (3 steps), `.github/workflows/release.yml`
  (1 step). YAML only.
- **No change** to: the pinned action SHA, `mold-version`, `make-default`, job
  dependencies, required-check configuration, or any Rust/frontend/build logic.
- One PR.

## Verification

- `actionlint` (1.7.12, available locally) on both workflow files — clean.
- Semantic reasoning above (errexit ⇒ symlink unreachable on failure).
- Real-world proof is deferred to CI: the next run must stay green with mold
  still installed and active on the network-OK path (the step is not skipped —
  it still runs and still symlinks when the download succeeds).

There is no unit-testable surface; a CI-config change is validated by the
linter, the semantic argument, and the subsequent live run.

## Out of scope

- The `release.yml` §1 class of "transient tool failure discards a long compile"
  beyond the mold step (ZEB-764).
- Any change to mold's version, the SHA pin, or the decision to use mold.
- Broader CI retry/resilience work (ZEB-499 family).
