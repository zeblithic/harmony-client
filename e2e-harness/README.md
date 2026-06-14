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
| `s2_friend_graph_and_dm_send` | friend-token iroh handshake → friendship `active` both ways (ZEB-431 DM-picker graph) + `send_dm` accepted. (1:1 DM byte-**delivery** is characterized, not asserted — co-located gap ZEB-461.) | ✅ pass |
| `s3_offline_channel_reconnect_catchup` | channel created while peer offline → reconnect catch-up (ZEB-434) | ✅ pass |
| `s4_restart_durability` | single-node community survives a restart (ZEB-393) | ✅ pass |

The single-machine harness validates **first-contact + join/handshake-time state**
(S1, S2), **single-node restart durability** (S4), and **co-located ongoing
community-state sync + offline→restart→catch-up** (S3). S3/S4 were previously
`#[ignore]`'d "blocked by ZEB-462", but that was a harness bug — the assertions
checked `c.get("id")` while the DTOs are camelCase (`channelId` / `spaceId`), so
they always timed out and *looked* like a sync failure. With the keys corrected
and the ZEB-462 (B) membership-CRDT durability fix on main (#253), both pass
reliably (S3 proven 3/3). The one remaining co-located gap is **1:1 DM
byte-delivery** (ZEB-461, characterized not asserted). **Cross-WAN** reachability
(two real machines / LANs) is still only exercised by the cross-machine playbook
(`docs/playbooks/e2e-two-agent-suite.md`, ZEB-444) — a co-located pass does not
prove cross-WAN re-peering.

## CI

This suite is its own deliberately-invoked CI job, **not** run on every push (it
spawns real binaries + touches the network, and is excluded from the per-task
`--lib` gate and from harmony-app's `--all-targets`). The job: (1) builds
`harmony-app`, (2) runs `cargo nextest run --features e2e --test-threads 1` from
`e2e-harness/`, (3) uploads `target/e2e-runs/` on failure.
