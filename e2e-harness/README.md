# e2e-harness (ZEB-447)

Standalone harness that spawns two real `harmony-app serve` nodes under named
profiles and drives them over the live HTTP/WS API.

## Run

```bash
# 1. Build the binary the harness drives:
cd src-tauri && cargo build --bin harmony-app && cd ..

# 2. Run the scenario suite (slow, real transport):
cd e2e-harness && cargo nextest run --features e2e
```

Set `HARMONY_APP_BIN=/path/to/harmony-app` to override binary discovery.
Set `HARMONY_E2E_KEEP=1` to retain run artifacts on success
(`e2e-harness/target/e2e-runs/<scenario>-<runid>/`).

Run the real-transport scenarios serially (avoid two scenarios contending on
transport/discovery at once):

```bash
cargo nextest run --features e2e --test-threads 1
# or, with plain cargo test:
cargo test --features e2e -- --test-threads 1
```

First contact is racy + relay-dependent (~75–90s pkarr propagation; relays
warm up ~1–2 min after a node boots), so scenarios poll/retry — allow a few
minutes of wall-clock.

## Scenarios

Two-sided scenarios live in `tests/e2e_two_node.rs`. Status on a **single
machine** (two co-located nodes):

| Scenario | What it proves | Status |
|---|---|---|
| `s1_invite_join_roster_convergence` | invite → iroh first-contact join → roster converges both ways | ✅ pass |
| `s2_friend_dm_exchange` | friend-token iroh handshake → friendship `active` both ways (ZEB-431 DM-picker graph) + `send_dm` accepted | ✅ pass |
| `s3_offline_channel_reconnect_catchup` | channel created while peer offline → reconnect catch-up (ZEB-434) | ⏸ `#[ignore]` — ZEB-462 |
| `s4_restart_durability` | single-node community survives a restart (ZEB-393) | ⏸ `#[ignore]` — ZEB-462 (B) |

The single-machine harness reliably validates **first-contact + join/handshake-
time state** (S1, S2). It cannot validate **ongoing community-state sync**, **1:1
DM byte-delivery**, or **restart catch-up** between two co-located nodes — those
ride the cross-machine playbook (`docs/playbooks/e2e-two-agent-suite.md`). The
`#[ignore]`'d tests carry FINDING blocks; the gaps are filed as **ZEB-461** (DM
delivery) and **ZEB-462** (co-located ongoing sync + restart membership
rehydration). Run an ignored scenario explicitly with
`cargo test --features e2e -- --ignored s3_offline`.

## CI

This suite is its own deliberately-invoked CI job, **not** run on every push (it
spawns real binaries + touches the network, and is excluded from the per-task
`--lib` gate and from harmony-app's `--all-targets`). The job: (1) builds
`harmony-app`, (2) runs `cargo nextest run --features e2e --test-threads 1` from
`e2e-harness/`, (3) uploads `target/e2e-runs/` on failure.
