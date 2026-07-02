#!/usr/bin/env bash
###############################################################################
# setup-sonic-testbed.sh
#
# One-shot, idempotent setup of a SONiC KVM virtual testbed following the
# sonic-mgmt VsSetup doc:
#   https://github.com/sonic-net/sonic-mgmt/blob/master/docs/testbed/README.testbed.VsSetup.md
#
# It brings up: docker-sonic-mgmt container + a sonic-vs DUT (vlab-01) + N SONiC
# neighbor VMs (vsonic, so NO Arista cEOS/vEOS download is needed) + PTF, deploys
# a T0 topology, and runs a BGP smoke test.
#
# RUN THIS *ON THE TESTBED HOST* (the Ubuntu 22.04/24.04 VM with nested-virt/KVM),
# as a normal user that has passwordless sudo and is (or will be) in the docker
# group. Example provisioning: Azure Standard_D8ads_v7 / D8s_v5 (8 vCPU, 32 GB),
# Ubuntu 24.04, 64 GB+ OS disk OR a large local/temp disk (auto-used below).
#
# USAGE:
#   ./setup-sonic-testbed.sh              # run every phase in order
#   ./setup-sonic-testbed.sh <phase>      # run a single phase (re-runnable)
#   ./setup-sonic-testbed.sh smoke_test   # just run the BGP verification test
#   ./setup-sonic-testbed.sh transceiver_tests      # xcvrd/SFP tests (vs-green subset)
#   ./setup-sonic-testbed.sh transceiver_tests_all  # full xcvrd/SFP set (needs emulator)
#   VERBOSE=1 ./setup-sonic-testbed.sh transceiver_tests_all   # full tracebacks for errors
#   ./setup-sonic-testbed.sh transceiver_tests_all -v          # same, via -v flag
#   ./setup-sonic-testbed.sh remove_topo  # tear down the topology + VMs
#
# To push+run from a Windows/git-bash workstation:
#   scp -i ~/Downloads/myVm_key.pem setup-sonic-testbed.sh azureuser@<IP>:~/
#   ssh -i ~/Downloads/myVm_key.pem azureuser@<IP> 'bash ~/setup-sonic-testbed.sh'
###############################################################################
set -uo pipefail

# ---------------------------------------------------------------------------
# Config (override via environment, e.g. TESTBED_NAME=vms-kvm-t0-64 ./script)
# ---------------------------------------------------------------------------
HOST_USER="$(whoami)"
MGMT_CONTAINER="${MGMT_CONTAINER:-mgmt}"
REPO_DIR="${REPO_DIR:-$HOME/sonic-mgmt}"
DATA="${DATA:-/mnt/data}"                     # big-disk mount point
TESTBED_NAME="${TESTBED_NAME:-vms-kvm-t0}"    # conf-name in vtestbed.yaml
DUT="${DUT:-vlab-01}"
VM_TYPE="${VM_TYPE:-vsonic}"                   # vsonic | ceos | csonic | veos
NUM_VMS="${NUM_VMS:-4}"                         # neighbor VM count for T0
SERVER="${SERVER:-server_1}"
INV="${INV:-veos_vtb}"
TB_FILE="${TB_FILE:-vtestbed.yaml}"
VAULT_FILE="${VAULT_FILE:-password.txt}"
SONIC_VS_URL="${SONIC_VS_URL:-https://sonic-build.azurewebsites.net/api/sonic/artifacts?branchName=master&platform=vs&target=target/sonic-vs.img.gz}"

# Env passed into the mgmt container for testbed-cli / pytest (host creds + paths).
# Host auth to the vm_host is key-based (see setup_ssh); passwords are placeholders.
CONTAINER_ENV=(
  -e ANSIBLE_HOST_KEY_CHECKING=False
  -e SONIC_MGMT_VM_HOST_USER="$HOST_USER"
  -e SONIC_MGMT_VM_HOST_PASSWORD=dummy
  -e SONIC_MGMT_VM_HOST_BECOME_PASSWORD=dummy
)
# Extra env needed to run pytest directly (mirrors what run_tests.sh sets).
PYTEST_ENV=(
  -e ANSIBLE_CONFIG=/data/sonic-mgmt/ansible
  -e ANSIBLE_LIBRARY=/data/sonic-mgmt/ansible/library/
  -e ANSIBLE_CONNECTION_PLUGINS=/data/sonic-mgmt/ansible/plugins/connection
  -e ANSIBLE_CLICONF_PLUGINS=/data/sonic-mgmt/ansible/cliconf_plugins
  -e ANSIBLE_TERMINAL_PLUGINS=/data/sonic-mgmt/ansible/terminal_plugins
)

log()  { echo -e "\n\033[1;36m==== $* ====\033[0m"; }
ok()   { echo -e "\033[1;32m[ok]\033[0m $*"; }
warn() { echo -e "\033[1;33m[warn]\033[0m $*"; }
die()  { echo -e "\033[1;31m[fail]\033[0m $*"; exit 1; }

dexec() { docker exec --user "$HOST_USER" "${CONTAINER_ENV[@]}" "$@"; }

# ---------------------------------------------------------------------------
# Phase 0: preflight — verify KVM/nested-virt, OS, sudo
# ---------------------------------------------------------------------------
preflight() {
  log "Phase 0: preflight (KVM / OS / sudo)"
  . /etc/os-release; echo "OS: ${PRETTY_NAME:-unknown}"
  [ -e /dev/kvm ] || die "/dev/kvm missing — this host has no nested virtualization. Use a KVM-capable size (e.g. Azure D8s_v5 / D8ads_v7)."
  [ "$(grep -Ec '(vmx|svm)' /proc/cpuinfo)" -gt 0 ] || die "No vmx/svm CPU flags — nested virt unavailable."
  sudo -n true 2>/dev/null || die "Passwordless sudo required for $HOST_USER."
  ok "KVM present, virt flags present, passwordless sudo OK"
}

# ---------------------------------------------------------------------------
# Phase 1: base prerequisites + Docker + Open vSwitch
# ---------------------------------------------------------------------------
install_prereqs() {
  log "Phase 1: prerequisites + Docker + Open vSwitch"
  export DEBIAN_FRONTEND=noninteractive
  sudo apt-get update -y
  sudo apt-get install -y python3 python3-pip openssh-server git make curl jq \
                          bridge-utils sshpass openvswitch-switch
  sudo systemctl enable --now openvswitch-switch
  if ! command -v docker >/dev/null 2>&1; then
    curl -fsSL https://get.docker.com -o /tmp/get-docker.sh && sudo sh /tmp/get-docker.sh
  fi
  sudo usermod -aG docker "$HOST_USER" || true
  ok "Docker $(sudo docker --version | awk '{print $3}' | tr -d ,), OVS $(sudo ovs-vsctl --version | head -1 | awk '{print $NF}')"
}

# ---------------------------------------------------------------------------
# Phase 2: storage — use the largest spare disk for docker + testbed images
#   (Azure D*ads/D*ds sizes expose a big local NVMe. NOTE: local/temp disks are
#    wiped on VM DEALLOCATE. Keep the VM running.)
# ---------------------------------------------------------------------------
find_data_disk() {
  lsblk -bdno NAME,SIZE,TYPE | awk '$3=="disk"{print $2, $1}' | sort -rn | while read -r size name; do
    [ "$(lsblk -no MOUNTPOINT "/dev/$name" | grep -c '/')" -eq 0 ] && { echo "/dev/$name"; break; }
  done
}
setup_storage() {
  log "Phase 2: storage (relocate docker + images to a big disk)"
  if mountpoint -q "$DATA"; then
    ok "$DATA already mounted"
  else
    local dev; dev="$(find_data_disk)"
    if [ -z "$dev" ]; then
      warn "No spare disk found — using OS disk (make sure it is >= 64 GB)."
      return 0
    fi
    echo "Using spare disk: $dev"
    sudo mkfs.ext4 -F -q "$dev"
    sudo mkdir -p "$DATA" && sudo mount "$dev" "$DATA"
    echo "$dev $DATA ext4 defaults,nofail 0 2" | sudo tee -a /etc/fstab >/dev/null
  fi
  sudo chown "$HOST_USER:$HOST_USER" "$DATA"

  # Move Docker's data-root onto the big disk.
  if command -v docker >/dev/null 2>&1 && [ ! -d "$DATA/docker" ]; then
    sudo systemctl stop docker docker.socket 2>/dev/null || true
    [ -d /var/lib/docker ] && sudo mv /var/lib/docker "$DATA/docker"
    echo "{ \"data-root\": \"$DATA/docker\" }" | sudo tee /etc/docker/daemon.json >/dev/null
    sudo systemctl start docker
  fi
  # Relocate testbed image dirs onto the big disk (symlink back to $HOME).
  for d in veos-vm sonic-vm; do
    if [ -d "$HOME/$d" ] && [ ! -L "$HOME/$d" ]; then mv "$HOME/$d" "$DATA/$d"; ln -s "$DATA/$d" "$HOME/$d"; fi
    [ -e "$HOME/$d" ] || { mkdir -p "$DATA/$d"; ln -s "$DATA/$d" "$HOME/$d"; }
  done
  ok "Docker root: $(sudo docker info --format '{{.DockerRootDir}}' 2>/dev/null); images under $DATA"
}

# ---------------------------------------------------------------------------
# Phase 3: clone sonic-mgmt
# ---------------------------------------------------------------------------
clone_repo() {
  log "Phase 3: clone sonic-mgmt"
  [ -d "$REPO_DIR" ] || git clone https://github.com/sonic-net/sonic-mgmt.git "$REPO_DIR"
  ok "sonic-mgmt @ $(git -C "$REPO_DIR" rev-parse --short HEAD)"
}

# ---------------------------------------------------------------------------
# Phase 4: management bridge network (br1)
# ---------------------------------------------------------------------------
setup_mgmt_network() {
  log "Phase 4: management bridge network"
  ( cd "$REPO_DIR/ansible" && sudo -H ./setup-management-network.sh )
  ip -br addr show br1 | sed 's/^/  br1: /' || true
  ok "management bridge br1 configured"
}

# ---------------------------------------------------------------------------
# Phase 5: download sonic-vs image into the image dirs
# ---------------------------------------------------------------------------
download_image() {
  log "Phase 5: download sonic-vs image"
  mkdir -p "$HOME/sonic-vm/images" "$HOME/veos-vm/images"
  if [ ! -f "$HOME/veos-vm/images/sonic-vs.img" ]; then
    local tmp="$HOME/sonic-vs.img"
    wget -q --show-progress -O "$tmp.gz" "$SONIC_VS_URL"
    gzip -df "$tmp.gz"
    cp -f "$tmp" "$HOME/sonic-vm/images/"
    mv -f "$tmp" "$HOME/veos-vm/images/"
  fi
  ok "sonic-vs.img present ($(du -h "$HOME/veos-vm/images/sonic-vs.img" | awk '{print $1}'))"
}

# ---------------------------------------------------------------------------
# Phase 6: create the sonic-mgmt container
# ---------------------------------------------------------------------------
setup_container() {
  log "Phase 6: sonic-mgmt container"
  if docker ps --format '{{.Names}}' | grep -qx "$MGMT_CONTAINER"; then
    ok "container '$MGMT_CONTAINER' already running"
  else
    ( cd "$REPO_DIR" && ./setup-container.sh -n "$MGMT_CONTAINER" -d /data )
  fi
  dexec "$MGMT_CONTAINER" bash -lc 'ls -d /data/sonic-mgmt >/dev/null && echo repo-mounted'
  ok "container '$MGMT_CONTAINER' ready (repo at /data/sonic-mgmt)"
}

# ---------------------------------------------------------------------------
# Phase 7: container->host passwordless SSH + ansible vault password file
# ---------------------------------------------------------------------------
setup_ssh() {
  log "Phase 7: container->host SSH + creds"
  dexec "$MGMT_CONTAINER" bash -lc 'test -f ~/.ssh/id_rsa || ssh-keygen -t rsa -b 2048 -N "" -f ~/.ssh/id_rsa -q; cat ~/.ssh/id_rsa.pub' > /tmp/mgmt_pub
  mkdir -p ~/.ssh && chmod 700 ~/.ssh
  grep -qFf /tmp/mgmt_pub ~/.ssh/authorized_keys 2>/dev/null || cat /tmp/mgmt_pub >> ~/.ssh/authorized_keys
  chmod 600 ~/.ssh/authorized_keys
  echo abc > "$REPO_DIR/ansible/$VAULT_FILE"
  dexec "$MGMT_CONTAINER" bash -lc "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null $HOST_USER@172.17.0.1 'echo CONTAINER_TO_HOST_OK'" \
    | grep -q CONTAINER_TO_HOST_OK && ok "container->host SSH works" || die "container->host SSH failed"
}

# convenience: run a testbed-cli command inside the container
tbcli() {
  dexec "$MGMT_CONTAINER" bash -lc "cd /data/sonic-mgmt/ansible && ./testbed-cli.sh $*"
}

# ---------------------------------------------------------------------------
# Phase 8: start neighbor VMs
# ---------------------------------------------------------------------------
start_vms() {
  log "Phase 8: start $NUM_VMS $VM_TYPE neighbor VMs"
  tbcli "-t $TB_FILE -m $INV -n $NUM_VMS -k $VM_TYPE start-vms $SERVER $VAULT_FILE"
  timeout 20 sudo virsh list --all 2>/dev/null || true
  ok "neighbor VMs started"
}

# ---------------------------------------------------------------------------
# Phase 9: deploy T0 topology (creates DUT + PTF + wiring)
# ---------------------------------------------------------------------------
add_topo() {
  log "Phase 9: add-topo $TESTBED_NAME"
  tbcli "-t $TB_FILE -m $INV -k $VM_TYPE add-topo $TESTBED_NAME $VAULT_FILE"
  ok "topology $TESTBED_NAME deployed (DUT + neighbors + PTF)"
}

# ---------------------------------------------------------------------------
# Phase 10: deploy minigraph (configure DUT as T0)
# ---------------------------------------------------------------------------
deploy_mg() {
  log "Phase 10: deploy-mg $TESTBED_NAME"
  tbcli "-t $TB_FILE -m $INV deploy-mg $TESTBED_NAME $INV $VAULT_FILE"
  ok "minigraph deployed — DUT configured as T0"
}

# ---------------------------------------------------------------------------
# Phase 11: verify DUT health
# ---------------------------------------------------------------------------
verify() {
  log "Phase 11: verify DUT health"
  dexec "$MGMT_CONTAINER" bash -lc "cd /data/sonic-mgmt/ansible && \
     ansible -m shell -a 'show version | head -12' -i $INV $DUT -b 2>/dev/null | sed -n '2,14p'; \
     echo '--- BGP summary ---'; \
     ansible -m shell -a 'show ip bgp summary' -i $INV $DUT -b 2>/dev/null | sed -n '/Neighbhor\\|Neighbor/,+8p'"
}

# ---------------------------------------------------------------------------
# smoke_test / transceiver_tests: run sonic-mgmt pytest against the DUT.
#
# Verbose mode: set VERBOSE=1 (env) or pass -v/--verbose as the FIRST arg to any
# of the test phases to get full tracebacks for FAILED *and* ERROR/skipped tests:
#   VERBOSE=1 ./setup-sonic-testbed.sh transceiver_tests_all
#   ./setup-sonic-testbed.sh transceiver_tests_all -v
# Verbose adds: --tb=long (full tracebacks), --showlocals (local vars in frames),
#   -rA (report reason for every outcome incl. errors/skips) and -s (no capture).
# ---------------------------------------------------------------------------
run_pytest() {
  # $* = pytest test paths/args
  local extra="-ra --tb=short"
  if [ "${VERBOSE:-0}" = "1" ]; then
    extra="-rA --tb=long --showlocals -s"
  fi
  docker exec --user "$HOST_USER" "${CONTAINER_ENV[@]}" "${PYTEST_ENV[@]}" "$MGMT_CONTAINER" bash -lc \
    "cd /data/sonic-mgmt/tests && python3 -m pytest $* \
        --inventory ../ansible/$INV --host-pattern $DUT \
        --testbed $TESTBED_NAME --testbed_file ../ansible/$TB_FILE \
        --skip_sanity --disable_loganalyzer $extra -v"
}

# Consume a leading -v/--verbose arg (sets VERBOSE=1) so `<phase> -v` works.
parse_verbose() {
  case "${1:-}" in
    -v|--verbose) VERBOSE=1; return 0 ;;
  esac
  return 1
}

smoke_test() {
  parse_verbose "${1:-}" && shift || true
  local tp="${1:-bgp/test_bgp_fact.py}"
  log "Smoke test: pytest $tp  (verbose=${VERBOSE:-0})"
  run_pytest "$tp"
}

# ---------------------------------------------------------------------------
# transceiver_tests: run the xcvrd/transceiver tests that PASS on a vs DUT
#   (sfpshow + sfputil presence/eeprom/reset — the sonic-vs platform provides
#    enough for these). This is the green transceiver smoke set.
# ---------------------------------------------------------------------------
transceiver_tests() {
  parse_verbose "${1:-}" && shift || true
  log "Transceiver tests (vs-compatible subset)  (verbose=${VERBOSE:-0})"
  run_pytest \
    "platform_tests/sfp/test_sfpshow.py" \
    "platform_tests/sfp/test_sfputil.py::test_check_sfputil_presence" \
    "platform_tests/sfp/test_sfputil.py::test_check_sfputil_eeprom" \
    "platform_tests/sfp/test_sfputil.py::test_check_sfputil_reset"
}

# ---------------------------------------------------------------------------
# transceiver_tests_all: the FULL transceiver/xcvrd test set. On a stock vs DUT
#   many ERROR/skip (api/test_sfp needs the platform-API service;
#   test_xcvr_info_in_db needs a lab connection graph; some are 'physical'-only).
#   These are the cases the xcvr-emu emulator is meant to light up later.
#   Run with VERBOSE=1 (or -v) to see WHY each one errors/skips.
# ---------------------------------------------------------------------------
transceiver_tests_all() {
  parse_verbose "${1:-}" && shift || true
  log "Transceiver tests (FULL set — expect errors on stock vs; needs emulator/real optics)  (verbose=${VERBOSE:-0})"
  run_pytest \
    "platform_tests/test_xcvr_info_in_db.py" \
    "platform_tests/test_sfp_thermal_state_db.py" \
    "platform_tests/sfp/" \
    "platform_tests/api/test_sfp.py"
}

# ---------------------------------------------------------------------------
# teardown helpers
# ---------------------------------------------------------------------------
remove_topo() {
  log "Teardown: remove-topo + stop-vms"
  tbcli "-t $TB_FILE -m $INV -k $VM_TYPE remove-topo $TESTBED_NAME $VAULT_FILE" || true
  tbcli "-t $TB_FILE -m $INV stop-vms $SERVER $VAULT_FILE" || true
  ok "topology removed"
}

# ---------------------------------------------------------------------------
# main: run all phases, or a single named phase
# ---------------------------------------------------------------------------
all() {
  preflight
  install_prereqs
  setup_storage
  clone_repo
  setup_mgmt_network
  download_image
  setup_container
  setup_ssh
  start_vms
  add_topo
  deploy_mg
  verify
  smoke_test
  transceiver_tests
  log "DONE — SONiC KVM testbed is up. DUT=$DUT  testbed=$TESTBED_NAME"
}

"${1:-all}" "${@:2}"
