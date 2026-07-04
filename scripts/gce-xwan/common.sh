# shellcheck shell=bash
# Shared config for the GCE cross-WAN test node (ZEB-635, plan: ZEB-522 /
# docs/plans/2026-07-04-gce-cross-wan-test-node.md). Sourced by the sibling
# scripts; not executable on its own.
#
# shellcheck disable=SC2034  # variables are consumed by the sourcing scripts.
# shellcheck disable=SC2088  # tilde-in-quotes is deliberate: REMOTE_* paths
#   must expand on the VM's shell (via gssh), never on the local machine.

GCP_PROJECT="${GCP_PROJECT:-zeblith-minecraft}"
GCP_ZONE="${GCP_ZONE:-us-west1-b}"
VM_NAME="${VM_NAME:-harmony-xwan-1}"          # NEVER harmony-relay — that VM is production pkarr rendezvous.
MACHINE_TYPE="${MACHINE_TYPE:-e2-standard-4}" # builds AND runs; micro/small cannot build Rust.
BOOT_DISK_GB="${BOOT_DISK_GB:-50}"
IMAGE_FAMILY="${IMAGE_FAMILY:-ubuntu-2404-lts-amd64}"
IMAGE_PROJECT="${IMAGE_PROJECT:-ubuntu-os-cloud}"
NETWORK_TAG="harmony-xwan"
FIREWALL_RULE="harmony-xwan-udp"              # present = open mode; absent = filtered mode.

REMOTE_SRC='~/harmony-client'                 # tilde expands on the VM, not here.
REMOTE_BIN="${REMOTE_SRC}/src-tauri/target/release/harmony-app"
REMOTE_ENV='~/xwan-env.sh'                    # sourced before every remote harmony-app invocation.
REMOTE_PROFILE="xwan"
LOCAL_PROFILE="xwan-local"

# All unattended ssh carries BatchMode=yes: automation must fail fast, never
# hang on an interactive prompt (Qodo rule 305197 / PR #398 round 1).
GSSH_FLAGS=(--project "$GCP_PROJECT" --zone "$GCP_ZONE" --ssh-flag="-o BatchMode=yes")

gssh() {
  gcloud compute ssh "$VM_NAME" "${GSSH_FLAGS[@]}" --command "$*"
}

# Agent-forwarded variant — ONLY for the cargo build step, which must fetch
# the private zeblithic/harmony.git deps. Keys never rest on the VM; the
# forwarded socket lives only as long as this session (plan §2 step 4).
gssh_agent() {
  gcloud compute ssh "$VM_NAME" --project "$GCP_PROJECT" --zone "$GCP_ZONE" \
    --ssh-flag="-A -o BatchMode=yes" --command "$*"
}

vm_status() {
  gcloud compute instances describe "$VM_NAME" \
    --project "$GCP_PROJECT" --zone "$GCP_ZONE" \
    --format='get(status)' 2>/dev/null || true
}

vm_external_ip() {
  gcloud compute instances describe "$VM_NAME" \
    --project "$GCP_PROJECT" --zone "$GCP_ZONE" \
    --format='get(networkInterfaces[0].accessConfigs[0].natIP)'
}

firewall_mode() {
  if gcloud compute firewall-rules describe "$FIREWALL_RULE" \
    --project "$GCP_PROJECT" --format='get(name)' >/dev/null 2>&1; then
    echo open
  else
    echo filtered
  fi
}

die() {
  echo "ERROR: $*" >&2
  exit 1
}

log() {
  echo "[gce-xwan $(date -u +%H:%M:%S)] $*"
}
