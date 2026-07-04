#!/usr/bin/env bash
# Teardown for the GCE cross-WAN test node (ZEB-635 / plan §1 discipline).
#
#   down.sh           — STOP the VM (disk survives ≈$5/mo; restart is ~40s).
#                       Use between sessions within an active validation wave.
#   down.sh --delete  — DELETE the VM + boot disk. Use when the wave ends.
#
# Both variants remove the open-mode firewall rule (it must not outlive a
# session) and any fallback PAT, then end with the nothing-left-running check.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=common.sh
source ./common.sh

DELETE=0
[ "${1:-}" = "--delete" ] && DELETE=1

if [ "$(firewall_mode)" = "open" ]; then
  log "removing open-mode firewall rule $FIREWALL_RULE…"
  gcloud compute firewall-rules delete "$FIREWALL_RULE" --project "$GCP_PROJECT" --quiet
fi

status="$(vm_status)"
if [ "$status" = "RUNNING" ]; then
  # Best-effort credential hygiene before power-off: the PAT fallback (plan §2
  # step 4) must not survive the session. Revoking the token server-side is a
  # GitHub UI/API step — the runbook owns that; here we remove it from disk.
  gssh 'rm -f ~/.gce-xwan-github-pat 2>/dev/null; true' || true
fi

case "$status" in
  "")
    log "$VM_NAME does not exist — nothing to stop/delete."
    ;;
  *)
    if [ "$DELETE" = 1 ]; then
      log "DELETING $VM_NAME + boot disk…"
      gcloud compute instances delete "$VM_NAME" \
        --project "$GCP_PROJECT" --zone "$GCP_ZONE" --delete-disks all --quiet
    elif [ "$status" = "RUNNING" ]; then
      log "stopping $VM_NAME (disk survives; --delete to remove everything)…"
      gcloud compute instances stop "$VM_NAME" --project "$GCP_PROJECT" --zone "$GCP_ZONE"
    else
      log "$VM_NAME already stopped ($status)."
    fi
    ;;
esac

log "final check — instances still running in $GCP_PROJECT:"
gcloud compute instances list --project "$GCP_PROJECT" --filter='status=RUNNING' \
  --format='table(name, zone, machineType.basename(), status)'
log "(harmony-relay RUNNING is expected — it is production pkarr rendezvous.)"
