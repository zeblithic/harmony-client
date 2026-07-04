#!/usr/bin/env bash
# Toggle the cross-WAN firewall experiment mode (ZEB-635 / plan §3).
#
#   mode.sh open      — create the harmony-xwan-udp ingress rule (udp:1024-65535
#                       from anywhere, scoped to the VM's tag). The VM behaves
#                       like a public-IP host: isolates the pure cross-WAN path.
#   mode.sh filtered  — delete the rule. GCE's stateful firewall then only
#                       admits UDP on 5-tuples the VM has probed outbound —
#                       hole-punch must genuinely work (≈ address/port-dependent
#                       filtering).
#   mode.sh status    — print the current mode.
#
# The wide port range is forced by iroh's ephemeral (unpinnable) UDP bind; the
# rule is meant to exist only while a session runs (down.sh removes it).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=common.sh
source ./common.sh

case "${1:-status}" in
  open)
    if [ "$(firewall_mode)" = "open" ]; then
      log "already open"
    else
      gcloud compute firewall-rules create "$FIREWALL_RULE" \
        --project "$GCP_PROJECT" \
        --direction INGRESS --action ALLOW --rules udp:1024-65535 \
        --source-ranges 0.0.0.0/0 --target-tags "$NETWORK_TAG"
      log "mode: open (rule $FIREWALL_RULE created)"
    fi
    ;;
  filtered)
    if [ "$(firewall_mode)" = "filtered" ]; then
      log "already filtered"
    else
      gcloud compute firewall-rules delete "$FIREWALL_RULE" --project "$GCP_PROJECT" --quiet
      log "mode: filtered (rule $FIREWALL_RULE deleted)"
    fi
    ;;
  status)
    log "mode: $(firewall_mode)"
    ;;
  *)
    die "usage: mode.sh {open|filtered|status}"
    ;;
esac
