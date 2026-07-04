# Fleet flag-day — iroh 0.98 → 1.0 coordinated rebuild (ZEB-636)

One coordinated window that moves every standing fleet node (Koya, Ildwyn,
AVALON) from the pre-ZEB-619 iroh 0.98 wire onto current `main` (iroh 1.0).
Prep (Phase 0) is safe to run any time, per machine, with **no restarts**.
The window (Phase 1) runs only on Jake's explicit go.

**Why this is a flag-day:** iroh 1.0 cannot interoperate with 0.98 on the wire
(ZEB-619 / PR #391, merged 2026-07-02). The `harmony/tunnel/v2` → v1 ALPN
fallback (PR #395) is app-layer *on top of* iroh and does not bridge the gap.
A mixed fleet is a partitioned fleet — so everyone moves in one window.

**Why now:** `main` has been 1.0 since 2026-07-02 and ZEB-635 (PR #399) proved
the 1.0 wire cross-WAN end-to-end (first-contact, direct-path NAT traversal in
both firewall modes, restart backfill) against the GCE node. The fleet is the
last thing running the old wire, and it is one accidental restart away from an
*uncoordinated* flag-day (see below).

## Current fleet state (Koya verified live 2026-07-04; others self-report in Phase 0)

| Machine | Profile | Owner ID | Node today |
|---|---|---|---|
| Koya (lead, macOS) | `fleet-koya` | `8fb9c58adb2d638d0c5aef07ae93b695` | `target/debug/harmony-app --profile fleet-koya serve --api-port 7421`, running since 2026-07-01 ~18:00 (pre-#391 image ⇒ 0.98 wire; snapshot `schemaVersion: 3`) |
| Ildwyn (Windows) | `fleet-ildwyn` | `4dc40fa5a7a3b3e2e25318ad4fc4218e` | self-report at preflight |
| AVALON (Windows) | self-report | `a379fc70b688d09eba5c4c7b7e7476b6` (per Koya's peer table; confirm) | self-report at preflight |

Koya's node is fragile in three ways the flag-day permanently fixes:

1. **It runs a debug binary out of the build tree** — every `cargo build` since
   July 1 has overwritten `target/debug/harmony-app` on disk, so the running
   image and the on-disk file diverged long ago, and an unplanned restart
   (crash, reboot) would boot whatever the tree last built: a unilateral,
   uncoordinated flag-day.
2. **Its logs write into a dead Claude session's `/private/tmp` scratchpad**
   (subject to OS tmp cleanup).
3. **It was started ad-hoc** (nohup from a long-gone session) with no recorded
   restart recipe.

Fix: every fleet node moves to a **staged release binary outside any build
tree**, logging to a stable path, with its exact start command recorded here.

## Phase 0 — preflight (per machine, BEFORE the window, no restarts)

1. Repo on `main` at or past `1fb35b72`; `git pull` first.
2. Build and stage the binary **outside the build tree** so future dev work
   cannot mutate the fleet node underneath itself:

   ```bash
   cd <repo>/src-tauri
   REV=$(git rev-parse --short HEAD)        # must be at/past 1fb35b72
   cargo build --locked --release --bin harmony-app
   mkdir -p ~/work/fleet-bin ~/work/fleet-logs
   cp -f target/release/harmony-app ~/work/fleet-bin/harmony-app-"$REV"
   shasum -a 256 ~/work/fleet-bin/harmony-app-"$REV"   # record REV + sha256 in ZEB-636
   ~/work/fleet-bin/harmony-app-"$REV" --help > /dev/null && echo staged-OK
   ```

   (Windows: same shape under `/c/zeblith/work/fleet-bin/`, `certutil -hashfile
   … SHA256` or `sha256sum` in Git Bash.)
3. Record the **current** node's full command line and its env/secret source
   (`ps aux | grep [h]armony` / `Get-Process`), and post it to ZEB-636. Koya's
   keys are macOS-keychain-backed (no file vault in the profile dir ⇒ no
   passphrase env needed at restart; the GUI session must be logged in).
   Windows nodes: confirm whether the profile uses keychain or the ZEB-449
   file vault — if file vault, the restart command MUST carry the same
   `HARMONY_PASSPHRASE_FILE`.
4. Read-only sanity against the running node and record the owner ID — this is
   the value that must SURVIVE the restart:

   ```bash
   <current-binary> --profile <fleet-profile> api get_owner_state | jq -r .ownerId
   ```

5. Do **not** stop anything. Preflight done = post "Phase 0 complete: <machine>,
   staged sha256 <…>, owner <…>" to ZEB-636.

## Phase 1 — the window (Jake's go; target < 30 min of bus downtime)

**Coordination is out-of-band for the whole window** — the fleet bus rides the
nodes being restarted. Use ZEB-636 comments (all three agents can reach Linear)
plus pushover to Jake at window open/close. During the mixed window, cross-wire
connection failures between restarted and not-yet-restarted nodes are
**expected — do not debug them.**

Order: **Koya → (optional GCE derisk) → Ildwyn → AVALON.** Lead node first;
each later node comes up with a live 1.0 peer already waiting to converge
against.

Per-node recipe (Koya's concrete version; Windows nodes substitute paths):

```bash
# 1. Stop gracefully: POST /v1/shutdown with the node's own bearer token
#    (port + token live in the profile's api dir — same recipe the GCE
#    suite uses). Fallback: SIGTERM the PID and wait for exit. Never
#    SIGKILL (shutdown flush guard).
API_DIR="$HOME/Library/Application Support/net.zeblith.harmony/profiles/fleet-koya/api"
PID=$(cat "$API_DIR/serve.lock")   # serve mode writes its PID lockfile here
curl -fsS -X POST -H "Authorization: Bearer $(cat "$API_DIR/token")" \
  "http://127.0.0.1:$(cat "$API_DIR/port")/v1/shutdown" || kill "$PID"
while kill -0 "$PID" 2>/dev/null; do sleep 1; done

# 2. Start the staged binary — same profile, same port, stable log path.
#    BIN = the exact staged file you recorded in ZEB-636.
BIN=~/work/fleet-bin/harmony-app-1fb35b72
nohup "$BIN" --profile fleet-koya serve --api-port 7421 \
  > ~/work/fleet-logs/fleet-koya-serve.log 2>&1 < /dev/null & disown

# 3. Verify, in order:
$BIN --profile fleet-koya api get_owner_state | jq -r .ownerId
#    → MUST equal the Phase 0 value. If it changed: STOP THE WINDOW, post to
#      ZEB-636, do NOT mint_owner_identity (ZEB-477 stale-ghost-key lesson).
$BIN --profile fleet-koya api network_health_snapshot \
  | jq '{schemaVersion, publish: .pkarrStatus.identityLastPublishMs}'
#    → schemaVersion 4 (proves the new build);
#    → poll until publish >= your restart timestamp. The field
#      PERSISTS across restarts (ZEB-635 trap) — a non-null stale value is NOT
#      a fresh publish. Boot publish lands in ≈15–90 s; don't panic-restart
#      inside that window.
```

Post "up: <machine>, owner unchanged, publish fresh" to ZEB-636 after each
node; then the next machine goes.

Relay note: 0.98 presets pinned the n0 *canary* relay cluster; 1.0 defaults to
the *stable* cluster (mirrored in `iroh_endpoint.rs`,
`iroh::endpoint::default_relay_mode()`), with `pkarr.q8.fyi` still prepended
for pkarr (PR #304). Peers reconnect from **fresh** post-restart records, not
cached pre-window ones — another reason convergence needs the publish gate
above, and why the window order matters less than everyone finishing.

## Phase 2 — revalidation (window closes when all three pass)

1. **Pairwise convergence:** on each pair, `list_community_members` for the
   fleet community shows both owners; DM both directions delivers. Assert
   *connected*; do not over-assert `connectionMode: "direct"` on the fleet LAN
   — all fleet hosts share one egress IP (hairpin NAT), where direct-path
   validation historically churns. The genuine direct-path proof already
   exists (ZEB-635) and can be re-run below.
2. **Bus check:** post one message into the Zeblithic Fleet channel from each
   machine; all three receive all three (api `--events` wake or poll).
3. **Off-LAN check (recommended, ≈$0.40, Koya-only):** run the GCE suite
   against the exact staged generation —
   `scripts/gce-xwan/up.sh && scripts/gce-xwan/provision.sh` (incremental) then
   `run-tests.sh --mode open --test all` per
   [`gce-cross-wan-runbook.md`](gce-cross-wan-runbook.md). This is the
   cross-WAN proof for the fleet's actual binary, not just `main`-in-CI.
4. Pushover Jake "flag-day complete"; post the closing summary (per-machine
   sha256, owner IDs, validation results) to ZEB-636.

## Failure handling — there is no rollback

Crossing the wire boundary is one-way in practice: nothing rebuilds a 0.98
binary from current `main`, and the old images live only in running processes.
If a node fails to come up healthy, it stays down and gets **fixed forward**
(its identity and community state are untouched on disk/keychain; the fleet is
merely N-1 until fixed). That is why Phase 0 stages and verifies binaries
*before* anything stops, and why the owner-ID check halts the window rather
than improvising identity recovery mid-flight.

## Standing rules that survive the flag-day

- Fleet nodes run **staged release binaries** from `~/work/fleet-bin/`
  (`/c/zeblith/work/fleet-bin/` on Windows), never from a repo `target/` dir.
- Logs under `~/work/fleet-logs/`, never `/tmp`.
- The start command for each node is recorded in this file and in ZEB-636;
  update both when it changes.
- `harmony-relay` (production pkarr rendezvous VM) and `harmony-xwan-1` (GCE
  test node) are not part of the fleet restart set.
