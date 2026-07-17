#!/usr/bin/env bash
# Cross-WAN validation choreography (ZEB-635 / plan §4): {local Koya node} ↔
# {GCE node}, driven entirely through ssh-exec'd headless `api` verbs.
#
#   run-tests.sh --mode open     --test all   # t1 + t2 (t3 via --test t3)
#   run-tests.sh --mode filtered --test t2
#   run-tests.sh --mode open     --test d3    # butler deposit→recover (ZEB-689)
#
# --mode ASSERTS the current firewall state (toggle with mode.sh first) so a
# result can never be attributed to the wrong mode. Tests:
#   t1  invite → connectivity_redeem_invite_iroh → roster convergence
#       (ZEB-330's missing distinct-WAN evidence)
#   t2  friend-token → DM both directions → assert connectionMode "direct"
#       on BOTH sides (the headline direct-path/hole-punch check; note the
#       serde camelCase value is "direct", lowercase)
#   t3  remote restart → cross-WAN channel backfill catch-up
#   d3  ZEB-477 Scenario D3 / ZEB-689: butler deposit→recover with the
#       deposit dial crossing a real WAN. Local P (primary) + B2 (butler,
#       SAS-paired into P's fleet on the LAN — pairing rides Zenoh
#       harmony/pairing/v2/lan/** and cannot cross WAN); remote GCE A is the
#       sender. P goes offline → A's deposit must dial B2 direct
#       (harmony/butler-deposit/v1) GCE→home-NAT. The co-located s7 sibling
#       (e2e_two_node.rs) can't prove this dial; this scenario is the
#       authoritative proof either way (works → co-located-harness-specific
#       confirmed; HELD timeout → deposit not RETAINED by the butler — from
#       the sender that is indistinguishable between a transport failure and
#       an acceptor reject (wire-silent by design); check the butler at
#       RUST_LOG=harmony_app=debug before classifying. 2026-07-17 verdict:
#       dial delivered, authorization rejected → ZEB-702).
#
# Local node: fresh binary, profile xwan-local, keychain disabled + passphrase
# (per CLAUDE.md keychain-isolation rules). The standing fleet-koya node (iroh
# 0.98 wire — flag-day pending) is never touched.
# shellcheck disable=SC2016  # single-quoted remote fragments expand on the VM.
# shellcheck disable=SC2329  # poll/trap invoke these functions indirectly.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=common.sh
source ./common.sh

REPO_ROOT="$(git rev-parse --show-toplevel)"
LOCAL_BIN="$REPO_ROOT/src-tauri/target/release/harmony-app"
ARTIFACTS="${XWAN_ARTIFACTS:-$HOME/.cache/gce-xwan-logs/$(date +%Y%m%d-%H%M%S)}"
MODE_WANT="" ; TESTS="all"

while [ $# -gt 0 ]; do
  case "$1" in
    --mode) MODE_WANT="$2"; shift 2 ;;
    --test) TESTS="$2"; shift 2 ;;
    *) die "unknown arg: $1 (usage: run-tests.sh --mode open|filtered [--test t1|t2|t3|d3|all])" ;;
  esac
done

[ -n "$MODE_WANT" ] || die "--mode open|filtered is required (results must be attributable)"
command -v jq >/dev/null || die "jq is required locally"
command -v xxd >/dev/null || die "xxd is required locally (to_hex for the DM assertions — a silent miss would masquerade as a poll timeout)"
[ -x "$LOCAL_BIN" ] || die "local release binary missing — cd src-tauri && cargo build --locked --release --bin harmony-app"

# Local vault passphrase: generated once, never hardcoded (Qodo PR #399 R1).
# NOTE: changing/removing this file orphans an existing xwan-local vault —
# the profile is disposable by design; wipe it and re-run.
LOCAL_PASS_FILE="${XWAN_LOCAL_PASS_FILE:-$HOME/.harmony-xwan-local-pass}"
[ -f "$LOCAL_PASS_FILE" ] || (umask 177 && openssl rand -hex 32 > "$LOCAL_PASS_FILE")

# D3 runs TWO extra local nodes (P primary + B2 butler). Their profiles are
# disposable BY DESIGN: d3_fresh_profiles wipes them at scenario start (fresh
# identities per run — the stale-ghost-key lesson, ZEB-477), so their pass
# files are mint-on-demand like the xwan-local one.
D3_P_PROFILE="xwan-d3-p"
D3_B2_PROFILE="xwan-d3-b2"
D3_P_PASS_FILE="$HOME/.harmony-xwan-d3-p-pass"
D3_B2_PASS_FILE="$HOME/.harmony-xwan-d3-b2-pass"
[ -f "$D3_P_PASS_FILE" ] || (umask 177 && openssl rand -hex 32 > "$D3_P_PASS_FILE")
[ -f "$D3_B2_PASS_FILE" ] || (umask 177 && openssl rand -hex 32 > "$D3_B2_PASS_FILE")
[ "$(vm_status)" = "RUNNING" ] || die "$VM_NAME is not RUNNING — run up.sh"
ACTUAL_MODE="$(firewall_mode)"
[ "$ACTUAL_MODE" = "$MODE_WANT" ] || die "firewall mode is '$ACTUAL_MODE' but --mode $MODE_WANT was requested — run mode.sh $MODE_WANT first"

mkdir -p "$ARTIFACTS"
log "mode=$MODE_WANT tests=$TESTS artifacts=$ARTIFACTS"

# ---- node control ----------------------------------------------------------

local_env() {
  HARMONY_PROFILE="$LOCAL_PROFILE" \
  HARMONY_PASSPHRASE_FILE="$LOCAL_PASS_FILE" \
  HARMONY_DISABLE_KEYCHAIN=1 \
  "$@"
}

local_api() { # verb [json]
  local_env "$LOCAL_BIN" api "$1" "${2:-{\}}"
}

d3_env() { # p|b2 cmd...
  local side="$1"; shift
  local profile pass
  if [ "$side" = p ]; then profile="$D3_P_PROFILE"; pass="$D3_P_PASS_FILE"
  else profile="$D3_B2_PROFILE"; pass="$D3_B2_PASS_FILE"; fi
  HARMONY_PROFILE="$profile" \
  HARMONY_PASSPHRASE_FILE="$pass" \
  HARMONY_DISABLE_KEYCHAIN=1 \
  "$@"
}

d3_api() { # p|b2 verb [json]
  local side="$1"; shift
  d3_env "$side" "$LOCAL_BIN" api "$1" "${2:-{\}}"
}

remote_api() { # verb [json] — JSON must not contain single quotes
  gssh "source ${REMOTE_ENV} && ${REMOTE_BIN} api $1 '${2:-{\}}'"
}

LOCAL_PID=""
start_local() {
  log "starting local node (profile $LOCAL_PROFILE)…"
  local_env "$LOCAL_BIN" serve --api-port 0 > "$ARTIFACTS/local-serve.log" 2>&1 &
  LOCAL_PID=$!
  wait_api local
}

D3_P_PID="" ; D3_B2_PID=""
start_d3() { # p|b2 [logsuffix]
  local side="$1" suffix="${2:-}"
  # NB: ${side} braced — bash 5.3 parses a bare `$side…` (multibyte char
  # adjacent to the name) as one variable name → "unbound variable" at runtime.
  log "starting d3 node ${side}…"
  d3_env "$side" "$LOCAL_BIN" serve --api-port 0 \
    >> "$ARTIFACTS/d3-$side-serve$suffix.log" 2>&1 &
  if [ "$side" = p ]; then D3_P_PID=$!; else D3_B2_PID=$!; fi
  wait_api "$side"
}

kill_d3() { # p|b2 — SIGKILL, mirroring the harness NodeHandle::kill (s7's
  # offline semantics are a real crash, not a graceful stop). Also removes the
  # profile's stale api port/token discovery files so the next start (and the
  # api CLI) can't latch onto the dead process's endpoint — the same trap the
  # harness's remove_stale_discovery_files exists for.
  local side="$1" pid profile
  if [ "$side" = p ]; then pid="$D3_P_PID"; profile="$D3_P_PROFILE"
  else pid="$D3_B2_PID"; profile="$D3_B2_PROFILE"; fi
  [ -n "$pid" ] || return 0
  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  local dir
  while IFS= read -r dir; do
    rm -f "$dir/api/port" "$dir/api/token"
  done < <(d3_data_dirs "$profile")
  if [ "$side" = p ]; then D3_P_PID=""; else D3_B2_PID=""; fi
}

# Kill + respawn from the same profile (the harness `relaunch()` analogue —
# which is also SIGKILL-based; on-disk identity/app-data rehydrate on boot).
relaunch_d3() { # p|b2
  kill_d3 "$1"
  start_d3 "$1" "-relaunch"
}

start_remote() {
  log "starting remote node (profile $REMOTE_PROFILE)…"
  # Both halves are load-bearing (found live, first open-mode sessions
  # 2026-07-04): the ';' keeps '&' bound to the nohup command alone — with
  # '&&' the whole list backgrounds as a subshell that runs serve in ITS
  # foreground while holding the ssh channel fds, and ssh blocks until the
  # node exits. The </dev/null keeps stdin off the channel for the same reason.
  gssh "source ${REMOTE_ENV}; nohup ${REMOTE_BIN} serve > ~/serve.log 2>&1 < /dev/null & disown; exit 0"
  wait_api remote
}

stop_remote() {
  # Graceful: POST /v1/shutdown with the node's own token (plan §4 / T3).
  log "stopping remote node (graceful /v1/shutdown)…"
  gssh 'API_DIR="$HOME/.local/share/net.zeblith.harmony/profiles/'"$REMOTE_PROFILE"'/api" && \
    curl -s -X POST -H "Authorization: Bearer $(cat "$API_DIR/token")" \
    "http://127.0.0.1:$(cat "$API_DIR/port")/v1/shutdown" >/dev/null || true'
  sleep 3
}

# api CLI exit codes: 0 ok, 1 server-reported error, 2 local failure (no
# port file / conn refused). "Server up" == rc != 2. (255 = ssh-level failure,
# possible on the remote side only.)
wait_api() { # local|remote|p|b2
  local side="$1" rc
  for _ in $(seq 1 30); do
    rc=0
    api_of "$side" get_owner_state > /dev/null 2>&1 || rc=$?
    if [ "$rc" -ne 2 ] && { [ "$side" != remote ] || [ "$rc" -ne 255 ]; }; then
      log "$side api is up"
      return 0
    fi
    sleep 2
  done
  die "$side api did not come up within 60s"
}

cleanup() {
  local pid
  for pid in "$LOCAL_PID" "$D3_P_PID" "$D3_B2_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT

poll() { # seconds interval description command...
  local deadline=$(( $(date +%s) + $1 )) interval="$2" desc="$3"; shift 3
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep "$interval"
  done
  echo "TIMEOUT: $desc" >&2
  return 1
}

# Verbs that mint things return a BARE JSON string (see e2e-harness driver.rs
# `as_str`); tolerate both shapes.
id_of() { jq -r 'if type == "string" then . else (.communityId // .channelId // .spaceId // .id // .url // empty) end'; }

# DM/channel payloads travel as BYTE ARRAYS (`Vec<u8>` DTOs) and read back as
# HEX-encoded `body` strings (driver.rs read_dm_plaintext) — a plain-string
# send is a 400, and a plain grep on the read is a silent timeout.
payload_json() { # key1 val1 msg → {"key1":"val1","content":bytes,"mimeType":"text/plain"}
  jq -nc --arg k "$1" --arg v "$2" --arg s "$3" '{($k): $v, "content": ($s | explode), "mimeType": "text/plain"}'
}
to_hex() { printf '%s' "$1" | xxd -p -c 9999; }

# ---- identity + discoverability -------------------------------------------

ensure_identity() { # local|remote → echoes ownerId
  local side="$1" owner
  owner="$(api_of "$side" get_owner_state | jq -r '.ownerId // empty')"
  if [ -z "$owner" ]; then
    api_of "$side" mint_owner_identity > /dev/null
    owner="$(api_of "$side" get_owner_state | jq -r '.ownerId // empty')"
  fi
  [ -n "$owner" ] || die "$side: could not mint/read owner identity"
  echo "$owner"
}

api_of() { # local|remote|p|b2 verb [json]
  local side="$1"; shift
  case "$side" in
    local)  local_api "$@" ;;
    remote) remote_api "$@" ;;
    p|b2)   d3_api "$side" "$@" ;;
    *)      die "api_of: unknown side '$side'" ;;
  esac
}

# GATE on a FRESH identity publish from THIS process of the INVITER side
# before anything mints an invite/token. Two live-measured traps (2026-07-04)
# make this load-bearing:
#   1. identityLastPublishMs PERSISTS across restarts — a stale value passes a
#      null-check gate ~5s after boot while the real boot publish lands ~70s in
#      (runtime-enable is worse: up to the ~7.5min publisher tick, 4m49s seen).
#   2. A redeem attempted BEFORE the current process's record is on the relay
#      poisons the redeeming side's retry loop for minutes (resolver-side
#      caching/cooldown) — whereas publish-first joined on the FIRST attempt.
identity_published_since() { # side floor_ms
  api_of "$1" network_health_snapshot | \
    grep -E '^\{' | tail -1 | \
    jq -e --argjson floor "$2" \
      '.pkarrStatus | select(.identityPublished == true and (.identityLastPublishMs // 0) >= $floor)' > /dev/null
}

setup_nodes() {
  start_remote
  start_local
  LOCAL_OWNER="$(ensure_identity local)";  log "local ownerId:  $LOCAL_OWNER"
  REMOTE_OWNER="$(ensure_identity remote)"; log "remote ownerId: $REMOTE_OWNER"
  # Both discoverable: T1's redeem resolves the INVITER (local) by identity
  # key over pkarr, so case-B publish must be on before the invite is minted.
  local_api  connectivity_set_identity_discoverable '{"enabled": true}' > /dev/null
  remote_api connectivity_set_identity_discoverable '{"enabled": true}' > /dev/null
  local gate_start_ms=$(( $(date +%s) * 1000 ))
  log "waiting for a fresh local identity pkarr publish (boot publish ≈70s; runtime-enable can wait for the ~7.5min tick)…"
  poll 600 10 "fresh local identity pkarr publish" identity_published_since local "$gate_start_ms" \
    || die "local identity record never published after script start"
  # Settle before ANY redeem attempt. Measured live (3 manual probes vs 3
  # scripted runs, 2026-07-04): a publish-gated + ~90s-settled FIRST attempt
  # joins instantly, while an attempt fired seconds after publish fails AND
  # poisons subsequent same-peer attempts for minutes (remote-side dial
  # backoff) — a 300s retry loop cannot recover from one early attempt.
  log "local identity record freshly published — settling 90s before first contact…"
  sleep 90
}

# ---- T1: first-contact + community ----------------------------------------

t1_first_contact() {
  log "T1: create community + invite (local) → redeem over iroh (remote) → roster convergence"
  local cname cid url out status attempt
  cname="xwan-t1-$(date +%s)"
  cid="$(local_api create_community "{\"name\": \"$cname\", \"isInviteOnly\": true}" | id_of)"
  [ -n "$cid" ] || die "T1: create_community returned no id"

  # A FRESH invite per attempt: hammering one URL after a failed attempt never
  # recovered in three scripted runs (remote-side backoff outlives the retry
  # budget), while a fresh mint against a settled inviter joined first-try in
  # all three manual probes.
  status=""
  for attempt in 1 2 3 4; do
    url="$(local_api generate_invite "{\"communityId\": \"$cid\"}" | id_of)"
    [ -n "$url" ] || die "T1: generate_invite returned no url"
    log "T1: attempt $attempt (fresh invite) — redeeming from GCE node…"
    out="$(remote_api connectivity_redeem_invite_iroh "{\"url\": \"$url\"}" 2>&1 || true)"
    # gcloud ssh interleaves its own stderr chatter ("Existing host keys…")
    # into 2>&1 — jq must only ever see the JSON line (found live: a JOINED
    # attempt was invisible to a bare jq parse and the loop kept going).
    status="$(echo "$out" | grep -E '^\{' | tail -1 | jq -r '.status // empty' 2>/dev/null || true)"
    [ "$status" = "joined" ] && break
    log "T1: attempt $attempt: ${out}"
    sleep 45
  done
  echo "$out" > "$ARTIFACTS/t1-redeem-final.json"
  [ "$status" = "joined" ] || { echo "FAIL T1 (mode=$MODE_WANT): redeem never reached joined — last: $out"; return 1; }

  members_converged() {
    local side="$1"
    [ "$(api_of "$side" list_community_members "{\"communityId\": \"$cid\"}" | jq 'length')" -ge 2 ]
  }
  poll 120 5 "T1 local roster"  members_converged local  || { echo "FAIL T1: local roster never showed 2 members"; return 1; }
  poll 120 5 "T1 remote roster" members_converged remote || { echo "FAIL T1: remote roster never showed 2 members"; return 1; }
  echo "PASS T1 (mode=$MODE_WANT): joined + roster converged (communityId=$cid)"
  T1_COMMUNITY_ID="$cid"
}

# ---- T2: friend + DM + direct path ----------------------------------------

t2_friend_dm_direct() {
  log "T2: friend-token (remote→local) → DM both directions → direct-path assert"
  local url space msg_a msg_b
  url="$(remote_api generate_friend_token | id_of)"
  [ -n "$url" ] || die "T2: generate_friend_token returned no url"
  local_api redeem_friend_token "{\"url\": \"$url\"}" > "$ARTIFACTS/t2-redeem-token.json"

  accept_pending() { # side expected-peer-owner
    local side="$1" peer="$2" pending
    pending="$(api_of "$side" list_pending_friend_requests | jq -r '.[]?.ownerIdHex // empty' 2>/dev/null || true)"
    if echo "$pending" | grep -q "$peer"; then
      api_of "$side" accept_friend_request "{\"ownerIdHex\": \"$peer\"}" > /dev/null || true
    fi
  }
  friends_active() { # side peer
    api_of "$1" list_friends | jq -e --arg p "$2" '.[] | select((.ownerIdHex // .ownerId // "") == $p) | select((.status // "") == "active")' > /dev/null
  }
  accept_pending remote "$LOCAL_OWNER"; accept_pending local "$REMOTE_OWNER"
  poll 120 5 "T2 friendship active (local view)" friends_active local "$REMOTE_OWNER" || {
    accept_pending remote "$LOCAL_OWNER"
    poll 60 5 "T2 friendship active retry" friends_active local "$REMOTE_OWNER" || { echo "FAIL T2: friendship never active"; return 1; }
  }
  log "T2: friendship active"

  space="$(local_api add_space "{\"kind\": \"dm\", \"name\": \"xwan-dm\", \"members\": [\"$REMOTE_OWNER\"]}" | id_of)"
  [ -n "$space" ] || die "T2: add_space returned no spaceId"
  msg_a="ping-from-koya-$(date +%s)"
  local_api send_dm "$(payload_json spaceId "$space" "$msg_a")" > /dev/null

  remote_sees() { remote_api read_dm_thread "{\"spaceId\": \"$space\", \"limit\": 100}" | grep -q "$(to_hex "$msg_a")"; }
  poll 240 10 "T2 DM local→remote" remote_sees || { echo "FAIL T2 (mode=$MODE_WANT): DM local→remote never arrived"; return 1; }
  log "T2: DM local→remote delivered"

  msg_b="pong-from-gce-$(date +%s)"
  remote_api send_dm "$(payload_json spaceId "$space" "$msg_b")" > /dev/null
  local_sees() { local_api read_dm_thread "{\"spaceId\": \"$space\", \"limit\": 100}" | grep -q "$(to_hex "$msg_b")"; }
  poll 240 10 "T2 DM remote→local" local_sees || { echo "FAIL T2 (mode=$MODE_WANT): DM remote→local never arrived"; return 1; }
  log "T2: DM remote→local delivered"

  # The headline assert: THE peer under test at connectionMode "direct"
  # (camelCase serde value is lowercase) on BOTH sides. Peer-scoped (Qodo
  # PR #399 R1): the self ownerAddr row is filtered as of ZEB-637;
  # peer-scoping retained as belt-and-braces against future extra rows.
  # PeerHealth.ownerAddr == the 32-hex ownerId (verified against the
  # 2026-07-04 session snapshots). Relay-only = traversal failed for this
  # mode — delivery above still passing is expected and reported.
  is_direct_with() { # side peer_owner
    api_of "$1" network_health_snapshot \
      | jq -e --arg owner "$2" '.peers[] | select(.ownerAddr == $owner) | select(.connectionMode == "direct")' > /dev/null
  }
  local direct_ok=1
  poll 180 10 "T2 direct path (local view of remote peer)"  is_direct_with local  "$REMOTE_OWNER" || direct_ok=0
  poll 60  10 "T2 direct path (remote view of local peer)"  is_direct_with remote "$LOCAL_OWNER"  || direct_ok=0
  local_api  network_health_snapshot > "$ARTIFACTS/t2-snapshot-local-$MODE_WANT.json"  || true
  remote_api network_health_snapshot > "$ARTIFACTS/t2-snapshot-remote-$MODE_WANT.json" || true
  if [ "$direct_ok" = 1 ]; then
    echo "PASS T2 (mode=$MODE_WANT): DMs both directions + connectionMode=direct on both sides"
  else
    echo "FAIL T2 (mode=$MODE_WANT): DMs delivered but NO direct path (relay-only) — snapshots in $ARTIFACTS"
    return 1
  fi
}

# ---- T3: restart + backfill -------------------------------------------------

t3_restart_backfill() {
  [ -n "${T1_COMMUNITY_ID:-}" ] || die "T3 needs T1's community — run --test all"
  log "T3: channel + offline messages → remote restart → catch-up"
  local cid="$T1_COMMUNITY_ID" chan m1 m2
  chan="$(local_api create_channel "{\"communityId\": \"$cid\", \"name\": \"xwan-t3\", \"writePower\": 0}" | id_of)"
  [ -n "$chan" ] || die "T3: create_channel returned no channelId"

  post_body() { # msg
    jq -nc --arg cid "$cid" --arg ch "$chan" --arg s "$1" \
      '{"communityId": $cid, "channelId": $ch, "body": ($s | explode)}'
  }
  stop_remote
  m1="offline-msg-1-$(date +%s)"; m2="offline-msg-2-$(date +%s)"
  local_api post_channel_message "$(post_body "$m1")" > /dev/null
  local_api post_channel_message "$(post_body "$m2")" > /dev/null
  start_remote

  caught_up() {
    # ChannelMessageDto.body is a JSON ARRAY of byte numbers — NOT hex like the
    # DM read (e2e_two_node.rs channel_msg_body_bytes documents exactly this
    # trap: "matching it as hex would silently never match"). Decode via jq
    # implode (bytes == codepoints for our ASCII probe messages).
    remote_api list_channel_messages "{\"communityId\": \"$cid\", \"channelId\": \"$chan\", \"limit\": 100}" \
      | grep -E '^\[' | tail -1 | jq -r '.[].body | implode' 2>/dev/null | grep -q "$m2"
  }
  poll 300 10 "T3 backfill catch-up" caught_up || { echo "FAIL T3 (mode=$MODE_WANT): remote never caught up"; return 1; }
  echo "PASS T3 (mode=$MODE_WANT): remote caught up on offline channel messages"
}

# ---- D3: butler deposit→recover across a real WAN (ZEB-477 D3 / ZEB-689) ----
#
# Topology: A (sender) = GCE remote. P (recipient primary) + B2 (butler) =
# two LOCAL nodes — SAS pairing rides Zenoh harmony/pairing/v2/lan/** and
# cannot cross WAN, so the fleet half is co-located by necessity; the dial
# under test (A → B2, harmony/butler-deposit/v1) crosses the real WAN, which
# is exactly what the co-located s7 sibling cannot prove (ZEB-689).
#
# Every boundary is a HARD FAIL here (unlike s7's characterize fallbacks):
# this scenario IS the authoritative proof, so a silent skip would waste the
# session. A HELD timeout = the deposit was NOT RETAINED by B2 — from the
# sender that is indistinguishable between a transport failure and an
# acceptor reject (the acceptor is wire-silent by design, no oracle); rerun
# B2 with RUST_LOG=harmony_app=debug to classify before escalating
# (2026-07-17: dial delivered, authorization rejected → ZEB-702).
# RECV/CLEARED timeouts = the recover half broke cross-WAN (a real finding;
# ZEB-509's co-located topology gap does not apply here).

# Harmony's profile data dir follows dirs::data_dir(): macOS
# `~/Library/Application Support`, Linux XDG `~/.local/share` (the path
# stop_remote already uses on the VM). Emit BOTH candidates — literal
# per-profile paths, safe no-ops where absent — so wipe/cleanup work
# whichever OS the operator machine runs (Qodo PR #480).
d3_data_dirs() { # profile → per-profile data-dir candidates, one per line
  echo "$HOME/Library/Application Support/net.zeblith.harmony/profiles/$1"
  echo "$HOME/.local/share/net.zeblith.harmony/profiles/$1"
}

d3_fresh_profiles() {
  # Disposable by design (stale-ghost-key lesson): wipe BOTH the profile data
  # dir and the ZEB-449 file vault so every run mints fresh identities.
  # Literal profile names only — never glob near the standing fleet-koya node.
  local name dir
  for name in "$D3_P_PROFILE" "$D3_B2_PROFILE"; do
    while IFS= read -r dir; do
      rm -rf "$dir"
    done < <(d3_data_dirs "$name")
    rm -rf "$HOME/.harmony/profiles/$name"
  done
}

# Poll BOTH pairing sides until `kind` matches, echoing the matching state
# JSON from the side named first. Fails the scenario on a `failed` state.
d3_pairing_state() { # p|b2
  d3_api "$1" get_pairing_state
}

d3_butler_deposit() {
  log "D3: butler deposit→recover — A(GCE) → B2(local butler) while P offline"
  local ts sas_p sas_b2 p_owner
  ts="$(date +%s)"

  # -- Phase 0: fresh local pair; remote A is already up (d3_setup).
  d3_fresh_profiles
  start_d3 p
  start_d3 b2
  d3_api p mint_owner_identity > /dev/null || { echo "FAIL D3: mint P owner identity"; return 1; }
  p_owner="$(d3_api p get_owner_state | jq -r '.ownerId // empty')"
  [ -n "$p_owner" ] || die "D3: P mint/read owner identity failed"
  log "D3: P ownerId: $p_owner (B2 stays unminted — it enrolls via pairing)"

  # -- Phase 1: SAS-pair B2 into P's fleet (mirror of driver.rs
  #    pair_into_fleet: discover → mutual select → SAS match → confirm →
  #    complete). LAN-local by necessity.
  d3_api p  start_inviter_pairing '{"displayName": "d3-P"}'  > /dev/null
  d3_api b2 start_joiner_pairing  '{"displayName": "d3-B2"}' > /dev/null

  local joiner_session joiner_vk inviter_session st
  discovered_joiner() {
    st="$(d3_pairing_state p)"
    [ "$(echo "$st" | jq -r '.kind // empty')" = discovered ] || return 1
    joiner_session="$(echo "$st" | jq -r '[.peers[]? | select(.role == "joiner")] | if length == 1 then .[0].sessionId else empty end')"
    joiner_vk="$(echo "$st" | jq -r '[.peers[]? | select(.role == "joiner")] | if length == 1 then .[0].joinerEd25519VerifyHex else empty end')"
    [ -n "$joiner_session" ] && [ -n "$joiner_vk" ]
  }
  discovered_inviter() {
    st="$(d3_pairing_state b2)"
    [ "$(echo "$st" | jq -r '.kind // empty')" = discovered ] || return 1
    inviter_session="$(echo "$st" | jq -r '[.peers[]? | select(.role == "inviter")] | if length == 1 then .[0].sessionId else empty end')"
    [ -n "$inviter_session" ]
  }
  poll 180 3 "D3 pairing: P discovers B2"  discovered_joiner  || { echo "FAIL D3 (mode=$MODE_WANT): P never discovered joiner B2"; return 1; }
  poll 60  3 "D3 pairing: B2 discovers P"  discovered_inviter || { echo "FAIL D3 (mode=$MODE_WANT): B2 never discovered inviter P"; return 1; }
  d3_api p  select_pairing_peer "{\"peerSessionId\": \"$joiner_session\"}"  > /dev/null || { echo "FAIL D3: P select_pairing_peer"; return 1; }
  d3_api b2 select_pairing_peer "{\"peerSessionId\": \"$inviter_session\"}" > /dev/null || { echo "FAIL D3: B2 select_pairing_peer"; return 1; }

  sas_of() { # p|b2 → echoes sasDigits when handshaking
    d3_pairing_state "$1" | jq -re 'select(.kind == "handshaking") | .sasDigits'
  }
  handshaking() { sas_of "$1" > /dev/null; }
  poll 120 3 "D3 pairing: P handshaking"  handshaking p  || { echo "FAIL D3: P never reached handshaking"; return 1; }
  poll 60  3 "D3 pairing: B2 handshaking" handshaking b2 || { echo "FAIL D3: B2 never reached handshaking"; return 1; }
  sas_p="$(sas_of p)"; sas_b2="$(sas_of b2)"
  [ -n "$sas_p" ] && [ "$sas_p" = "$sas_b2" ] || { echo "FAIL D3: SAS mismatch (P=$sas_p B2=$sas_b2) — MITM-check property violated"; return 1; }
  log "D3: SAS match ($sas_p) — confirming both sides"
  d3_api p  confirm_pairing_sas > /dev/null || { echo "FAIL D3: P confirm_pairing_sas"; return 1; }
  d3_api b2 confirm_pairing_sas > /dev/null || { echo "FAIL D3: B2 confirm_pairing_sas"; return 1; }
  pairing_complete() { [ "$(d3_pairing_state "$1" | jq -r '.kind // empty')" = complete ]; }
  poll 120 3 "D3 pairing: P complete"  pairing_complete p  || { echo "FAIL D3: P pairing never completed"; return 1; }
  poll 60  3 "D3 pairing: B2 complete" pairing_complete b2 || { echo "FAIL D3: B2 pairing never completed"; return 1; }
  log "D3: B2 enrolled into P's fleet (joiner vk $joiner_vk)"

  # -- Phase 2: friend A<->P while both online (fresh token per attempt — the
  #    T1 lesson; the redeem is P's genuine cross-WAN first contact with A).
  # Local exit codes are trustworthy (no ssh layer): 0 = redeem accepted.
  local url attempt friended=0
  for attempt in 1 2 3 4; do
    url="$(remote_api generate_friend_token | id_of)"
    [ -n "$url" ] || die "D3: generate_friend_token returned no url"
    log "D3: friend attempt $attempt (fresh token) — P redeeming…"
    if d3_api p redeem_friend_token "{\"url\": \"$url\"}" \
        > "$ARTIFACTS/d3-redeem-token.json" 2>&1; then
      friended=1; break
    fi
    log "D3: friend attempt $attempt failed: $(tail -c 300 "$ARTIFACTS/d3-redeem-token.json")"
    sleep 45
  done
  [ "$friended" = 1 ] || { echo "FAIL D3 (mode=$MODE_WANT): P never redeemed A's friend token"; return 1; }

  accept_pending_d3() { # side expected-peer-owner
    local pending
    pending="$(api_of "$1" list_pending_friend_requests | grep -E '^\[' | tail -1 | jq -r '.[]?.ownerIdHex // empty' 2>/dev/null || true)"
    if echo "$pending" | grep -q "$2"; then
      api_of "$1" accept_friend_request "{\"ownerIdHex\": \"$2\"}" > /dev/null || true
    fi
  }
  friends_active_d3() { # side peer
    api_of "$1" list_friends | grep -E '^\[' | tail -1 | \
      jq -e --arg p "$2" '.[] | select((.ownerIdHex // .ownerId // "") == $p) | select((.status // "") == "active")' > /dev/null
  }
  friend_dance() {
    accept_pending_d3 remote "$p_owner"
    accept_pending_d3 p "$REMOTE_OWNER"
    friends_active_d3 p "$REMOTE_OWNER"
  }
  poll 180 5 "D3 friendship active (P view)" friend_dance || { echo "FAIL D3 (mode=$MODE_WANT): A<->P friendship never active"; return 1; }
  log "D3: A<->P friendship active"

  # -- Phase 3: shared community (A creates, P joins over iroh). REQUIRED:
  #    P's ReachabilityAnnounce (carrying the durable B2 butler-set) replicates
  #    to A only via community-CRDT co-membership (ZEB-488).
  local cid cstatus cattempt cout curl
  cid="$(remote_api create_community "{\"name\": \"xwan-d3-$ts\", \"isInviteOnly\": true}" | id_of)"
  [ -n "$cid" ] || die "D3: create_community returned no id"
  cstatus=""
  for cattempt in 1 2 3 4; do
    curl="$(remote_api generate_invite "{\"communityId\": \"$cid\"}" | id_of)"
    [ -n "$curl" ] || die "D3: generate_invite returned no url"
    log "D3: community attempt $cattempt (fresh invite) — P redeeming over iroh…"
    cout="$(d3_api p connectivity_redeem_invite_iroh "{\"url\": \"$curl\"}" 2>&1 || true)"
    cstatus="$(echo "$cout" | grep -E '^\{' | tail -1 | jq -r '.status // empty' 2>/dev/null || true)"
    [ "$cstatus" = "joined" ] && break
    log "D3: community attempt $cattempt: $cout"
    sleep 45
  done
  echo "$cout" > "$ARTIFACTS/d3-community-redeem-final.json"
  [ "$cstatus" = "joined" ] || { echo "FAIL D3 (mode=$MODE_WANT): P never joined A's community"; return 1; }
  log "D3: A<->P co-members of $cid"

  # -- Phase 3.5: P must observe A's durable announce BEFORE the relaunch.
  #    Found live (first D3 run, 2026-07-17): P relaunched seconds after the
  #    join boots with an EMPTY reachability resolver — nothing persisted to
  #    replay, so nothing seeds a cross-WAN re-dial of A, and P sits isolated
  #    forever (co-located, Zenoh LAN multicast masks this by rediscovering
  #    automatically). Observing A's announce over the still-live join session
  #    persists it, so the post-relaunch boot replay re-seeds the dial policy —
  #    the exact restart machinery T3 proves cross-WAN.
  p_observes_a() {
    d3_api p connectivity_list_peer_reachability | grep -E '^\[' | tail -1 | \
      jq -e --arg owner "$REMOTE_OWNER" \
        '.[] | select(.ownerAddress == $owner) | select(.source == "durableCrdt")' > /dev/null
  }
  poll 180 5 "D3: P observes A's durable announce (pre-relaunch persist)" p_observes_a \
    || { echo "FAIL D3 (mode=$MODE_WANT): P never observed A's durable announce over the live join session"; return 1; }
  log "D3: P observed A's durable announce — persisted; relaunch replay can re-seed the dial"

  # -- Phase 4: relaunch P (start_node rebuilds the boot-time enrolled-set
  #    snapshot from PERSISTED enrollments — the set_butler_pin precondition),
  #    then read B2's deviceVkHex from P's own persisted view. The relaunch
  #    startup also republishes P's announce into the community CRDT
  #    (publisher trigger 1) — P is a member at THIS boot, unlike its first.
  #    Capture A's newest announcedAtMs for P BEFORE the relaunch: A retains
  #    P's pre-relaunch durable record, so Phase 4.5 must demand a STRICTLY
  #    newer announce or it can pass on the stale record without proving the
  #    re-dial (CodeRabbit PR #480).
  local pre_relaunch_announced_ms
  pre_relaunch_announced_ms="$(
    remote_api connectivity_list_peer_reachability | grep -E '^\[' | tail -1 | \
      jq -r --arg owner "$p_owner" \
        '[.[] | select(.ownerAddress == $owner) | (.record.announcedAtMs // 0)] | max // 0'
  )"
  relaunch_d3 p
  local devices peer_vk
  devices="$(d3_api p get_owner_state | jq '[.devices[]? | select(.isThisDevice == false)]')"
  case "$(echo "$devices" | jq 'length')" in
    1) peer_vk="$(echo "$devices" | jq -r '.[0].deviceVkHex // empty')" ;;
    0) echo "FAIL D3: after pairing + relaunch P's persisted enrolled set has no peer device (inviter enrollment persist gap — ZEB-491 class)"; return 1 ;;
    *) echo "FAIL D3: P's fleet has multiple peer devices (expected exactly B2)"; return 1 ;;
  esac
  [ -n "$peer_vk" ] || { echo "FAIL D3: peer device row missing deviceVkHex"; return 1; }
  [ "$peer_vk" = "$joiner_vk" ] || { echo "FAIL D3: persisted peer vk ($peer_vk) != pairing-captured joiner vk ($joiner_vk)"; return 1; }

  # -- Phase 4.5: prove the relaunched P re-established the cross-WAN session —
  #    A observes a durable announce from P authored STRICTLY AFTER the
  #    pre-relaunch max (the stale pre-relaunch record would otherwise satisfy
  #    an existence check instantly and prove nothing). Isolates a re-dial
  #    failure from a pin-propagation failure. The replayed A-announce
  #    (Phase 3.5) seeds P's dial policy; P's relaunch-startup publish
  #    provides the fresh announce to sync.
  a_observes_p() {
    remote_api connectivity_list_peer_reachability | grep -E '^\[' | tail -1 | \
      jq -e --arg owner "$p_owner" --argjson before "$pre_relaunch_announced_ms" \
        '.[] | select(.ownerAddress == $owner) | select(.source == "durableCrdt")
             | select((.record.announcedAtMs // 0) > $before)' > /dev/null
  }
  poll 300 10 "D3: A observes relaunched P (post-relaunch announce)" a_observes_p \
    || { echo "FAIL D3 (mode=$MODE_WANT): A never observed a POST-relaunch announce from P — cross-WAN re-dial after restart did not re-establish"; return 1; }
  log "D3: A observed relaunched P's fresh announce — cross-WAN session re-established"

  # -- Phase 5: relaunch B2 (ZEB-492: boots its dm-inbox/butler engines from
  #    the persisted fleet KeyTree), then hard-assert butler readiness.
  relaunch_d3 b2
  local held0
  held0="$(d3_api b2 get_butler_held | jq -r '.held | length' 2>/dev/null || echo ERR)"
  [ "$held0" = 0 ] || { echo "FAIL D3: B2 get_butler_held not a clean empty array post-relaunch (got: $held0) — ZEB-492 KeyTree/engine regression"; return 1; }
  log "D3: B2 butler engine live (held=[])"

  # -- Phase 6: pin B2 as P's butler + readback. Capture the pin floor FIRST:
  #    set_butler_pin_impl fires force_reachability_republish, so the very
  #    next announce P authors carries the B2 butler-set and an
  #    announced_at_ms >= this floor (authored on the same local clock).
  local pin_floor_ms
  pin_floor_ms=$(( $(date +%s) * 1000 ))
  d3_api p set_butler_pin "{\"deviceId\": \"$peer_vk\"}" > /dev/null || { echo "FAIL D3: set_butler_pin"; return 1; }
  local pin
  pin="$(d3_api p get_butler_pin | jq -r '.pinnedDeviceId // empty')"
  [ "$pin" = "$peer_vk" ] || { echo "FAIL D3: get_butler_pin ($pin) does not reflect pinned B2 ($peer_vk)"; return 1; }
  log "D3: P pinned B2 as butler (pin floor $pin_floor_ms)"

  # -- Phase 7: post-pin reachability barrier — A (the SENDER, cross-WAN)
  #    must hold P's DURABLE-CRDT ReachabilityAnnounce authored AT/AFTER the
  #    pin, i.e. the one whose butler-set carries B2 (ZEB-488; the DTO does
  #    not expose the set itself, but announcedAtMs >= pin floor identifies
  #    the post-pin announce exactly — no staleness proxy, no settle sleeps;
  #    source == "durableCrdt" because a pkarrLive cache-back is not
  #    replication and does not count).
  post_pin_observed() {
    remote_api connectivity_list_peer_reachability | grep -E '^\[' | tail -1 | \
      jq -e --arg owner "$p_owner" --argjson floor "$pin_floor_ms" \
        '.[] | select(.ownerAddress == $owner) | select(.source == "durableCrdt")
             | select((.record.announcedAtMs // 0) >= $floor)' > /dev/null
  }
  poll 300 10 "D3 reachability: A holds P's POST-PIN announce" post_pin_observed \
    || { echo "FAIL D3 (mode=$MODE_WANT): A never received P's post-pin ReachabilityAnnounce (B2 butler-set never replicated cross-WAN)"; return 1; }
  log "D3: A holds P's post-pin announce — B2-bearing butler-set replicated cross-WAN"

  # -- Phase 7.5 (ZEB-705): B2's roster must converge BEFORE P departs — the
  #    acceptor admits by OwnerState.friend_graph, which reaches B2 only from
  #    P (same-owner sync; A cannot serve another owner's state). Without
  #    this barrier HELD conflates "roster sync lost a boot-window race"
  #    with "deposit dial/authorization failed" — the 2026-07-17 lesson:
  #    both roster publishes arrived ~2 s after B2's relaunch boot, died on
  #    a content-fetch race, and P was SIGKILLed 3 s later, making the loss
  #    unrecoverable. The barrier keeps the two claims separable: this poll
  #    proves bounded-time roster convergence while P is alive; HELD then
  #    proves the deposit dial + authorization alone.
  if ! poll 120 5 "D3 ROSTER: B2 converged P's friend graph" friends_active_d3 b2 "$REMOTE_OWNER"; then
    d3_api b2 network_health_snapshot > "$ARTIFACTS/d3-roster-timeout-b2-snapshot-$MODE_WANT.json" 2>&1 || true
    echo "FAIL D3 (mode=$MODE_WANT): B2 never converged P's friend graph while P was alive (same-owner sync to the butler failed — ZEB-705 class; check butlerDeposits + fleet_sync warns in the b2 log/snapshot artifacts)"
    return 1
  fi
  log "D3: B2 roster carries A — owner-state synced to the butler"

  # -- Phase 8: P goes OFFLINE (real SIGKILL — mirror s7's crash semantics).
  kill_d3 p
  log "D3: P is offline (SIGKILL)"

  # -- Phase 9: A creates the DM space + sends while P is unreachable.
  local space msg
  space="$(remote_api add_space "{\"kind\": \"dm\", \"name\": \"xwan-d3-dm\", \"members\": [\"$p_owner\"]}" | id_of)"
  [ -n "$space" ] || die "D3: add_space returned no spaceId"
  msg="d3-butler-durable-$ts"
  remote_api send_dm "$(payload_json spaceId "$space" "$msg")" > /dev/null || { echo "FAIL D3: A send_dm"; return 1; }
  log "D3: A sent DM to offline P (space $space) — deposit fans out after DEPOSIT_NOACK_WINDOWS=2"

  # -- Phase 10: HELD — THE HEADLINE. B2 must receive A's deposit over the
  #    harmony/butler-deposit/v1 dial that crosses the real WAN (GCE →
  #    home-NAT B2). This is the exact dial the co-located harness cannot
  #    establish (ZEB-689); its outcome here is the ticket's verdict signal.
  #    NB a timeout proves only "not retained" — transport vs authorization
  #    reject is decided by B2's debug log, not by this poll.
  held_for_a() {
    d3_api b2 get_butler_held | \
      jq -e --arg a "$REMOTE_OWNER" '.held[] | select(.senderOwnerHex == $a)' > /dev/null
  }
  if ! poll 300 10 "D3 HELD: B2 holds A's deposit" held_for_a; then
    d3_api b2 get_butler_held > "$ARTIFACTS/d3-held-timeout-$MODE_WANT.json" 2>&1 || true
    # ZEB-705: counter-first triage — butlerDeposits.rejectedUnauthorized
    # climbing = roster/authorization; zero rejects = transport-side (the
    # debug log remains the classifier of record).
    d3_api b2 network_health_snapshot > "$ARTIFACTS/d3-held-timeout-b2-snapshot-$MODE_WANT.json" 2>&1 || true
    echo "FAIL D3 (mode=$MODE_WANT): B2 never RETAINED A's deposit — check butlerDeposits counters in the snapshot artifact first (ZEB-702 observability), then B2's debug log to classify"
    return 1
  fi
  d3_api b2 get_butler_held > "$ARTIFACTS/d3-held-$MODE_WANT.json" 2>&1 || true
  echo "PASS D3-HELD (mode=$MODE_WANT): cross-WAN butler-deposit dial landed on B2"

  # -- Phase 11: RECV — P relaunches, fleet-merges with B2, recovers the
  #    deposited invite + message.
  start_d3 p "-recover"
  p_sees() { d3_api p read_dm_thread "{\"spaceId\": \"$space\", \"limit\": 100}" | grep -q "$(to_hex "$msg")"; }
  poll 300 10 "D3 RECV: P recovers the deposited DM" p_sees \
    || { echo "FAIL D3 (mode=$MODE_WANT): HELD passed but P never recovered the deposited DM after relaunch"; return 1; }
  echo "PASS D3-RECV (mode=$MODE_WANT): P recovered the butler-deposited DM"

  # -- Phase 12: CLEARED — B2's held entry records P's ingest (grow-only
  #    ingestedByDevices) or is GC'd away; accept either.
  cleared() {
    local entries entry
    entries="$(d3_api b2 get_butler_held)" || return 1
    entry="$(echo "$entries" | jq --arg a "$REMOTE_OWNER" '[.held[] | select(.senderOwnerHex == $a)]')"
    if [ "$(echo "$entry" | jq 'length')" = 0 ]; then return 0; fi
    echo "$entry" | jq -e '.[0].ingestedByDevices | length > 0' > /dev/null
  }
  poll 180 10 "D3 CLEARED: B2 records P's ingest" cleared \
    || { echo "FAIL D3 (mode=$MODE_WANT): RECV passed but B2 never recorded P's recovery (ingest/GC handshake)"; return 1; }
  echo "PASS D3-CLEARED (mode=$MODE_WANT): B2 recorded P's recovery"

  # Snapshots for the session record.
  remote_api network_health_snapshot > "$ARTIFACTS/d3-snapshot-remote-$MODE_WANT.json" || true
  d3_api b2 network_health_snapshot  > "$ARTIFACTS/d3-snapshot-b2-$MODE_WANT.json"     || true
  echo "PASS D3 (mode=$MODE_WANT): butler deposit→recover proven across a real WAN"
}

d3_setup() {
  # Remote A only (the d3 local pair is managed inside the scenario). A is
  # the inviter for both the friend token and the community invite, so the
  # fresh-publish gate applies to the REMOTE side here (t1/t2 gate local).
  start_remote
  REMOTE_OWNER="$(ensure_identity remote)"; log "remote ownerId: $REMOTE_OWNER"
  remote_api connectivity_set_identity_discoverable '{"enabled": true}' > /dev/null
  local gate_start_ms=$(( $(date +%s) * 1000 ))
  log "waiting for a fresh REMOTE identity pkarr publish (boot publish ≈70s)…"
  poll 600 10 "fresh remote identity pkarr publish" identity_published_since remote "$gate_start_ms" \
    || die "remote identity record never published after script start"
  log "remote identity freshly published — settling 90s before first contact…"
  sleep 90
}

# ---- main -------------------------------------------------------------------

overall=0
case "$TESTS" in
  t1)  setup_nodes; t1_first_contact || overall=1 ;;
  t2)  setup_nodes; t2_friend_dm_direct || overall=1 ;;
  t3)  setup_nodes; t1_first_contact && t3_restart_backfill || overall=1 ;;
  all) setup_nodes; { t1_first_contact && t2_friend_dm_direct; } || overall=1 ;;
  d3)  d3_setup; d3_butler_deposit || overall=1 ;;
  *)   die "unknown --test $TESTS" ;;
esac

log "done (mode=$MODE_WANT, overall=$([ $overall = 0 ] && echo PASS || echo FAIL)); artifacts: $ARTIFACTS"
exit "$overall"
