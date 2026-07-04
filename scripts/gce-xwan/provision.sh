#!/usr/bin/env bash
# Provision the GCE cross-WAN test VM (ZEB-635 / plan §2 steps 2-6). Idempotent —
# safe to re-run after a source change (rsync + incremental rebuild).
#
# Credential posture (plan §2 step 4): the private zeblithic/harmony.git cargo
# deps are fetched under ssh agent forwarding; no long-lived repo credential
# ever rests on the VM. Requires Koya's ssh agent to hold a GitHub-authorized key.
# shellcheck disable=SC2016  # single-quoted remote commands are deliberate:
#   $HOME etc. must expand on the VM, not locally.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=common.sh
source ./common.sh

REPO_ROOT="$(git rev-parse --show-toplevel)"

[ "$(vm_status)" = "RUNNING" ] || die "$VM_NAME is not RUNNING — run up.sh first."
ssh-add -l >/dev/null 2>&1 || die "ssh agent has no keys loaded — the agent-forwarded cargo build needs one (ssh-add)."

log "1/6 System deps (apt; CI's tauri Linux list + build essentials)…"
gssh "sudo DEBIAN_FRONTEND=noninteractive apt-get -qq update && \
  sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y \
  build-essential curl pkg-config rsync jq git time \
  libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
  librsvg2-dev libssl-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev"

log "2/6 Rust toolchain (rustup; repo pin 1.94.1 self-installs at first cargo run)…"
gssh 'command -v "$HOME/.cargo/bin/cargo" >/dev/null 2>&1 || \
  (curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none)'
gssh 'mkdir -p ~/.cargo && printf "[net]\ngit-fetch-with-cli = true\n" > ~/.cargo/config.toml'
gssh 'grep -q github.com ~/.ssh/known_hosts 2>/dev/null || ssh-keyscan github.com >> ~/.ssh/known_hosts 2>/dev/null'

log "3/6 Source sync (rsync via gcloud config-ssh; excludes target/, node_modules/, .git/)…"
gcloud compute config-ssh --project "$GCP_PROJECT" --quiet >/dev/null
SSH_HOST="${VM_NAME}.${GCP_ZONE}.${GCP_PROJECT}"
rsync -az --delete \
  --exclude 'target/' --exclude 'node_modules/' --exclude '.git/' \
  -e "ssh -o BatchMode=yes" \
  "$REPO_ROOT/" "${SSH_HOST}:harmony-client/"

log "4/6 Build (agent-forwarded; --locked release; timing recorded)…"
gssh_agent "source \$HOME/.cargo/env && cd ${REMOTE_SRC}/src-tauri && \
  /usr/bin/time -v cargo build --locked --release --bin harmony-app 2>&1 | tail -25"

log "5/6 Profile + vault (named profile => file vault; passphrase mandatory, ZEB-449)…"
gssh 'test -f ~/.harmony-xwan-pass || (umask 177 && openssl rand -hex 32 > ~/.harmony-xwan-pass)'
gssh "cat > ${REMOTE_ENV} << 'EOF'
export HARMONY_PROFILE=${REMOTE_PROFILE}
export HARMONY_PASSPHRASE_FILE=\$HOME/.harmony-xwan-pass
export HARMONY_DISABLE_KEYCHAIN=1
EOF"

log "6/6 Sanity: binary runs and reports a version/help…"
gssh "source ${REMOTE_ENV} && ${REMOTE_BIN} --help >/dev/null && echo 'harmony-app binary OK'"

log "Provision complete. Next: scripts/gce-xwan/run-tests.sh"
