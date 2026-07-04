#!/usr/bin/env bash
# Create-or-start the GCE cross-WAN test VM (ZEB-635 / plan §2 step 1).
# Idempotent: creates the VM if absent, starts it if stopped, no-ops if running.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=common.sh
source ./common.sh

status="$(vm_status)"
case "$status" in
  RUNNING)
    log "$VM_NAME already RUNNING."
    ;;
  TERMINATED)
    log "$VM_NAME exists (stopped) — starting. Note: ephemeral external IP will be fresh."
    gcloud compute instances start "$VM_NAME" --project "$GCP_PROJECT" --zone "$GCP_ZONE"
    ;;
  "")
    log "Creating $VM_NAME ($MACHINE_TYPE, ${BOOT_DISK_GB}GB, $IMAGE_FAMILY, $GCP_ZONE)…"
    gcloud compute instances create "$VM_NAME" \
      --project "$GCP_PROJECT" --zone "$GCP_ZONE" \
      --machine-type "$MACHINE_TYPE" \
      --image-family "$IMAGE_FAMILY" --image-project "$IMAGE_PROJECT" \
      --boot-disk-size "${BOOT_DISK_GB}GB" --boot-disk-type pd-balanced \
      --tags "$NETWORK_TAG"
    ;;
  *)
    die "$VM_NAME is in unexpected state '$status' — resolve manually."
    ;;
esac

log "Waiting for ssh…"
for _ in $(seq 1 30); do
  if gssh "true" >/dev/null 2>&1; then
    log "ssh is up. External IP: $(vm_external_ip)"
    log "Firewall mode: $(firewall_mode)"
    exit 0
  fi
  sleep 5
done
die "ssh did not come up within 150s"
