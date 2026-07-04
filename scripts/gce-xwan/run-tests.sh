#!/usr/bin/env bash
# Cross-WAN validation choreography (ZEB-635 / plan §4): {local Koya node} ↔
# {GCE node}, driven entirely through ssh-exec'd headless `api` verbs.
#
#   run-tests.sh --mode open     --test all   # t1 + t2 (t3 via --test t3)
#   run-tests.sh --mode filtered --test t2
#
# --mode ASSERTS the current firewall state (toggle with mode.sh first) so a
# result can never be attributed to the wrong mode. Tests:
#   t1  invite → connectivity_redeem_invite_iroh → roster convergence
#       (ZEB-330's missing distinct-WAN evidence)
#   t2  friend-token → DM both directions → assert connectionMode "direct"
#       on BOTH sides (the headline direct-path/hole-punch check; note the
#       serde camelCase value is "direct", lowercase)
#   t3  remote restart → cross-WAN channel backfill catch-up
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
    *) die "unknown arg: $1 (usage: run-tests.sh --mode open|filtered [--test t1|t2|t3|all])" ;;
  esac
done

[ -n "$MODE_WANT" ] || die "--mode open|filtered is required (results must be attributable)"
command -v jq >/dev/null || die "jq is required locally"
[ -x "$LOCAL_BIN" ] || die "local release binary missing — cd src-tauri && cargo build --locked --release --bin harmony-app"
[ "$(vm_status)" = "RUNNING" ] || die "$VM_NAME is not RUNNING — run up.sh"
ACTUAL_MODE="$(firewall_mode)"
[ "$ACTUAL_MODE" = "$MODE_WANT" ] || die "firewall mode is '$ACTUAL_MODE' but --mode $MODE_WANT was requested — run mode.sh $MODE_WANT first"

mkdir -p "$ARTIFACTS"
log "mode=$MODE_WANT tests=$TESTS artifacts=$ARTIFACTS"

# ---- node control ----------------------------------------------------------

local_env() {
  HARMONY_PROFILE="$LOCAL_PROFILE" \
  HARMONY_PASSPHRASE="xwan-local-test-passphrase" \
  HARMONY_DISABLE_KEYCHAIN=1 \
  "$@"
}

local_api() { # verb [json]
  local_env "$LOCAL_BIN" api "$1" "${2:-{\}}"
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
# port file / conn refused). "Server up" == rc != 2.
wait_api() { # local|remote
  local side="$1" rc
  for _ in $(seq 1 30); do
    rc=0
    if [ "$side" = local ]; then
      local_api get_owner_state > /dev/null 2>&1 || rc=$?
    else
      remote_api get_owner_state > /dev/null 2>&1 || rc=$?
    fi
    if [ "$rc" -ne 2 ] && { [ "$side" = local ] || [ "$rc" -ne 255 ]; }; then
      log "$side api is up"
      return 0
    fi
    sleep 2
  done
  die "$side api did not come up within 60s"
}

cleanup() {
  if [ -n "$LOCAL_PID" ] && kill -0 "$LOCAL_PID" 2>/dev/null; then
    kill "$LOCAL_PID" 2>/dev/null || true
    wait "$LOCAL_PID" 2>/dev/null || true
  fi
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

api_of() { # local|remote verb [json]
  local side="$1"; shift
  if [ "$side" = local ]; then local_api "$@"; else remote_api "$@"; fi
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
  log "identity_discoverable=true on both; pkarr propagation needs ~75-90s (polls below absorb it)"
}

# ---- T1: first-contact + community ----------------------------------------

t1_first_contact() {
  log "T1: create community + invite (local) → redeem over iroh (remote) → roster convergence"
  local cname cid url out status
  cname="xwan-t1-$(date +%s)"
  cid="$(local_api create_community "{\"name\": \"$cname\", \"isInviteOnly\": true}" | jq -r '.communityId // .id // empty')"
  [ -n "$cid" ] || die "T1: create_community returned no id"
  url="$(local_api generate_invite "{\"communityId\": \"$cid\"}" | jq -r 'if type == "string" then . else (.url // empty) end')"
  [ -n "$url" ] || die "T1: generate_invite returned no url"
  log "T1: communityId=$cid — redeeming from GCE node (retries absorb pkarr warm-up)…"

  local deadline=$(( $(date +%s) + 300 ))
  status=""
  while [ "$(date +%s)" -lt "$deadline" ]; do
    out="$(remote_api connectivity_redeem_invite_iroh "{\"url\": \"$url\"}" 2>&1 || true)"
    status="$(echo "$out" | jq -r '.status // empty' 2>/dev/null || true)"
    [ "$status" = "joined" ] && break
    sleep 10
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
  url="$(remote_api generate_friend_token | jq -r 'if type == "string" then . else (.url // empty) end')"
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

  space="$(local_api add_space "{\"kind\": \"dm\", \"name\": \"xwan-dm\", \"members\": [\"$REMOTE_OWNER\"]}" | jq -r '.spaceId // .id // empty')"
  [ -n "$space" ] || die "T2: add_space returned no spaceId"
  msg_a="ping-from-koya-$(date +%s)"
  local_api send_dm "{\"spaceId\": \"$space\", \"content\": \"$msg_a\", \"mimeType\": \"text/plain\"}" > /dev/null

  remote_sees() { remote_api read_dm_thread "{\"spaceId\": \"$space\", \"limit\": 100}" | grep -q "$msg_a"; }
  poll 240 10 "T2 DM local→remote" remote_sees || { echo "FAIL T2 (mode=$MODE_WANT): DM local→remote never arrived"; return 1; }
  log "T2: DM local→remote delivered"

  msg_b="pong-from-gce-$(date +%s)"
  remote_api send_dm "{\"spaceId\": \"$space\", \"content\": \"$msg_b\", \"mimeType\": \"text/plain\"}" > /dev/null
  local_sees() { local_api read_dm_thread "{\"spaceId\": \"$space\", \"limit\": 100}" | grep -q "$msg_b"; }
  poll 240 10 "T2 DM remote→local" local_sees || { echo "FAIL T2 (mode=$MODE_WANT): DM remote→local never arrived"; return 1; }
  log "T2: DM remote→local delivered"

  # The headline assert: a live peer at connectionMode "direct" (camelCase
  # serde value is lowercase) on BOTH sides. Relay-only = traversal failed
  # for this mode — delivery above still passing is expected and reported.
  direct_on() { api_of "$1" network_health_snapshot | jq -e '.peers[] | select(.connectionMode == "direct")' > /dev/null; }
  local direct_ok=1
  poll 180 10 "T2 direct path (local view)"  direct_on local  || direct_ok=0
  poll 60  10 "T2 direct path (remote view)" direct_on remote || direct_ok=0
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
  chan="$(local_api create_channel "{\"communityId\": \"$cid\", \"name\": \"xwan-t3\", \"writePower\": 0}" | jq -r '.channelId // .id // empty')"
  [ -n "$chan" ] || die "T3: create_channel returned no channelId"

  stop_remote
  m1="offline-msg-1-$(date +%s)"; m2="offline-msg-2-$(date +%s)"
  local_api post_channel_message "{\"communityId\": \"$cid\", \"channelId\": \"$chan\", \"body\": \"$m1\"}" > /dev/null
  local_api post_channel_message "{\"communityId\": \"$cid\", \"channelId\": \"$chan\", \"body\": \"$m2\"}" > /dev/null
  start_remote

  caught_up() {
    remote_api list_channel_messages "{\"communityId\": \"$cid\", \"channelId\": \"$chan\", \"limit\": 100}" | grep -q "$m2"
  }
  poll 300 10 "T3 backfill catch-up" caught_up || { echo "FAIL T3 (mode=$MODE_WANT): remote never caught up"; return 1; }
  echo "PASS T3 (mode=$MODE_WANT): remote caught up on offline channel messages"
}

# ---- main -------------------------------------------------------------------

setup_nodes
overall=0
case "$TESTS" in
  t1)  t1_first_contact || overall=1 ;;
  t2)  t2_friend_dm_direct || overall=1 ;;
  t3)  t1_first_contact && t3_restart_backfill || overall=1 ;;
  all) { t1_first_contact && t2_friend_dm_direct; } || overall=1 ;;
  *)   die "unknown --test $TESTS" ;;
esac

log "done (mode=$MODE_WANT, overall=$([ $overall = 0 ] && echo PASS || echo FAIL)); artifacts: $ARTIFACTS"
exit "$overall"
