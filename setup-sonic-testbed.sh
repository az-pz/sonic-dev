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
#   ./setup-sonic-testbed.sh              # run every phase in order (now INCLUDES the emulator)
#   ./setup-sonic-testbed.sh <phase>      # run a single phase (re-runnable)
#   ./setup-sonic-testbed.sh smoke_test   # just run the BGP verification test
#   ./setup-sonic-testbed.sh transceiver_tests      # xcvrd/SFP tests (now pass with the emulator)
#   ./setup-sonic-testbed.sh transceiver_tests_all  # full validated xcvrd/SFP set
#   VERBOSE=1 ./setup-sonic-testbed.sh transceiver_tests_all   # full tracebacks for errors
#   ./setup-sonic-testbed.sh transceiver_tests_all -v          # same, via -v flag
#   RESET_TESTS=0 ./setup-sonic-testbed.sh transceiver_tests_all  # skip the SLOW module-reset tests
#   ./setup-sonic-testbed.sh transceiver_eeprom_tests  # declarative transceiver/eeprom suite
#                                              #   (injects inventory + temporarily flips asic_type)
#   ./setup-sonic-testbed.sh emulator          # native emulator deploy: host sonic_platform:=bridge,
#                                              #   skip_xcvrd=false, pmon inject, + xcvr-emu container
#   ./setup-sonic-testbed.sh emulator_revert   # undo the native emulator deploy (restore stock platform)
#   ./setup-sonic-testbed.sh transceiver_emu_test  # run test_xcvr_info_in_db (needs emulator)
#   ./setup-sonic-testbed.sh emulator_e2e      # emulator + test_xcvr_info_in_db in one go
#   ./setup-sonic-testbed.sh remove_topo  # tear down the topology + VMs
#   ./setup-sonic-testbed.sh rebuild      # recover after a /mnt/data wipe (also redeploys the emulator)
#
# The `emulator` phase needs this script's sibling assets (platform/ and
# emu-deploy/), so run it from a full sonic-develop checkout on the VM:
#   git clone git@github.com:t-fhabibi_microsoft/sonic-develop.git
#   cd sonic-develop/dev && ./setup-sonic-testbed.sh emulator_e2e
#
# IMPORTANT: /mnt/data is an Azure EPHEMERAL "Direct" disk — it is WIPED when the
# VM is deallocated/stopped. Either keep the VM running, or attach a PERSISTENT
# Azure managed data disk and mount it at /mnt/data. After any wipe, run `rebuild`.
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

# --- emulator (xcvr-emu) config, used by the `emulator` phase ----------------
# Where this script lives, so we can find its sibling assets (platform/ bridge +
# emu-deploy/ toolkit) that ship in the same sonic-develop checkout.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BRIDGE_DIR="${BRIDGE_DIR:-$SCRIPT_DIR/platform/sonic_platform}"   # the gRPC bridge
EMU_DEPLOY_DIR="${EMU_DEPLOY_DIR:-$SCRIPT_DIR/emu-deploy}"        # build/ship/deploy scripts
XCVR_EMU_URL="${XCVR_EMU_URL:-git@github.com:gsoosk/xcvr-emu.git}" # gsoosk fork (fixes on sonic-dev)
XCVR_EMU_URL_HTTPS="${XCVR_EMU_URL_HTTPS:-https://github.com/gsoosk/xcvr-emu.git}"  # read-only fallback
XCVR_EMU_BRANCH="${XCVR_EMU_BRANCH:-sonic-dev}"                    # branch to build the emulator from
XCVR_EMU_DIR="${XCVR_EMU_DIR:-$HOME/xcvr-emu}"                     # cloned on the VM on demand
EMU_MODULES="${EMU_MODULES:-33}"                                  # present CMIS modules (0..N-1)
EMU_BUNDLE="${EMU_BUNDLE:-$EMU_DEPLOY_DIR/emu-bundle.tar.gz}"
EMU_IMAGE_TAR="${EMU_IMAGE_TAR:-$EMU_DEPLOY_DIR/xcvr-emu-image.tar.gz}"  # emulator image tarball (docker save|gzip)
EMU_REBUILD_IMAGE="${EMU_REBUILD_IMAGE:-0}"                        # 1 = force rebuild the emulator image
DUT_IP="${DUT_IP:-10.250.0.101}"                                  # DUT mgmt IPv4 (from mgmt ctr)
DUT_PASS="${DUT_PASS:-password}"                                  # DUT admin password

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
  # Install base tools resiliently — one package at a time so a single
  # unavailable/conflicting package does not abort the WHOLE set (apt-get install
  # is transactional: one broken package rolls back every other in the same call).
  for pkg in python3 python3-pip openssh-server git make curl jq bridge-utils sshpass; do
    dpkg -s "$pkg" >/dev/null 2>&1 && continue
    sudo apt-get install -y "$pkg" || warn "apt: could not install '$pkg' (continuing)"
  done
  # Open vSwitch: only install the upstream package if NO OVS is present. Some
  # hosts (e.g. NVIDIA BlueField / DOCA) ship doca-openvswitch-* which *conflicts*
  # with openvswitch-common, so adding openvswitch-switch would break apt. In that
  # case the DOCA-provided OVS is already usable, so we just use it.
  if command -v ovs-vsctl >/dev/null 2>&1; then
    ok "Open vSwitch already present ($(sudo ovs-vsctl --version | head -1 | awk '{print $NF}')) — skipping openvswitch-switch"
  else
    sudo apt-get install -y openvswitch-switch \
      && sudo systemctl enable --now openvswitch-switch \
      || warn "could not install/enable openvswitch-switch"
  fi
  if ! command -v docker >/dev/null 2>&1; then
    curl -fsSL https://get.docker.com -o /tmp/get-docker.sh && sudo sh /tmp/get-docker.sh
  fi
  sudo usermod -aG docker "$HOST_USER" || true
  # j2cli: sonic-mgmt's setup-management-network.sh installs it via `pip3 install`,
  # which fails when pip3 is absent OR when the distro is PEP-668 "externally
  # managed" (Ubuntu 24.04+). Provide it here so that step is a no-op. Try apt
  # first, then pip with the managed-env override.
  if ! command -v j2 >/dev/null 2>&1; then
    sudo apt-get install -y python3-j2cli 2>/dev/null \
      || sudo pip3 install --break-system-packages j2cli 2>/dev/null \
      || pip3 install --user --break-system-packages j2cli 2>/dev/null \
      || warn "could not install j2cli (setup-management-network.sh may warn)"
  fi
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
    # Idempotent fstab entry (avoid duplicates across re-runs).
    if ! grep -qs "[[:space:]]$DATA[[:space:]]" /etc/fstab; then
      echo "$dev $DATA ext4 defaults,nofail 0 2" | sudo tee -a /etc/fstab >/dev/null
    fi
  fi
  sudo chown "$HOST_USER:$HOST_USER" "$DATA"

  # Move Docker's data-root onto the big disk.
  if command -v docker >/dev/null 2>&1 && [ ! -d "$DATA/docker" ]; then
    sudo systemctl stop docker docker.socket 2>/dev/null || true
    [ -d /var/lib/docker ] && sudo mv /var/lib/docker "$DATA/docker"
    echo "{ \"data-root\": \"$DATA/docker\" }" | sudo tee /etc/docker/daemon.json >/dev/null
    sudo systemctl start docker
  fi
  # Relocate containerd storage onto the big disk too. Docker (v25+) uses the
  # containerd image store under /var/lib/containerd, which is NOT covered by the
  # docker data-root above — image layers there can fill the small OS disk (seen
  # as "no space left on device" while pulling docker-ptf). Bind-mount it.
  if ! findmnt /var/lib/containerd >/dev/null 2>&1; then
    sudo systemctl stop docker docker.socket containerd 2>/dev/null || true
    sleep 2
    if [ ! -d "$DATA/containerd" ]; then
      [ -d /var/lib/containerd ] && sudo mv /var/lib/containerd "$DATA/containerd" || sudo mkdir -p "$DATA/containerd"
    fi
    sudo mkdir -p /var/lib/containerd
    sudo mount --bind "$DATA/containerd" /var/lib/containerd
    if ! grep -qs '/var/lib/containerd' /etc/fstab; then
      echo "$DATA/containerd /var/lib/containerd none bind,nofail 0 0" | sudo tee -a /etc/fstab >/dev/null
    fi
    sudo systemctl start containerd docker
    sleep 3
  fi
  # Relocate testbed image dirs onto the big disk (symlink back to $HOME).
  for d in veos-vm sonic-vm; do
    if [ -d "$HOME/$d" ] && [ ! -L "$HOME/$d" ]; then mv "$HOME/$d" "$DATA/$d"; ln -s "$DATA/$d" "$HOME/$d"; fi
    [ -e "$HOME/$d" ] || { mkdir -p "$DATA/$d"; ln -s "$DATA/$d" "$HOME/$d"; }
  done
  ok "Docker root: $(sudo docker info --format '{{.DockerRootDir}}' 2>/dev/null); containerd+images under $DATA"
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
  # Download + decompress DIRECTLY into the (symlinked) veos image dir, which
  # lives on the big data disk. Doing this in $HOME risks filling the small OS
  # disk (the 7GB decompressed image won't fit) -> "No space left on device".
  local dst="$HOME/veos-vm/images/sonic-vs.img"
  if [ ! -f "$dst" ]; then
    # clean any stale partials on the OS disk from a previous failed run
    rm -f "$HOME/sonic-vs.img" "$HOME/sonic-vs.img.gz" 2>/dev/null || true
    rm -f "$dst.gz" 2>/dev/null || true
    wget -q --show-progress -O "$dst.gz" "$SONIC_VS_URL"
    gzip -df "$dst.gz"
  fi
  # second copy for the sonic-vm image dir (also on the data disk via symlink)
  cp -f "$dst" "$HOME/sonic-vm/images/sonic-vs.img"
  ok "sonic-vs.img present ($(du -h "$dst" | awk '{print $1}'))"
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
# On hosts that already provide Open vSwitch through a *conflicting* package set
# — notably NVIDIA DOCA / BlueField, whose doca-openvswitch-common declares
# "Conflicts: openvswitch-common" — sonic-mgmt's vm_set host_setup task cannot
# apt-install openvswitch-switch, and because that apt call is transactional the
# ENTIRE 'start-vms' play fails (taking libvirt/qemu with it). OVS is already
# present on such hosts, so drop just that one package from the cloned harness
# task. Idempotent; leaves the kernel tuning and libvirt/qemu installs intact.
patch_vm_host_setup_for_doca() {
  command -v ovs-vsctl >/dev/null 2>&1 || return 0   # only when OVS already present
  local f="$REPO_DIR/ansible/roles/vm_set/tasks/host_setup.yml"
  [ -f "$f" ] || return 0
  if grep -qE '^[[:space:]]*-[[:space:]]*openvswitch-switch[[:space:]]*$' "$f"; then
    warn "OVS already present (likely DOCA) — removing openvswitch-switch from vm_set host_setup to avoid an apt conflict"
    sed -i -E '/^[[:space:]]*-[[:space:]]*openvswitch-switch[[:space:]]*$/d' "$f"
    ok "patched $(basename "$f") (dropped openvswitch-switch; DOCA OVS is used instead)"
  fi
}

start_vms() {
  log "Phase 8: start $NUM_VMS $VM_TYPE neighbor VMs"
  patch_vm_host_setup_for_doca
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
  # Force IPv4 (-e ansible_host=$DUT_IP) — the KVM mgmt network has no IPv6 route
  # from the container, so let ansible use the DUT's IPv4 mgmt address.
  local DUT_IP="${DUT_IP:-10.250.0.101}"
  dexec "$MGMT_CONTAINER" bash -lc "cd /data/sonic-mgmt/ansible && \
     ansible -m shell -a 'show version | head -12' -i $INV $DUT -b -e ansible_host=$DUT_IP 2>/dev/null | sed -n '2,14p'; \
     echo '--- BGP summary ---'; \
     ansible -m shell -a 'show ip bgp summary' -i $INV $DUT -b -e ansible_host=$DUT_IP 2>/dev/null | sed -n '/Neighbhor\\|Neighbor/,+8p'"
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
# transceiver_tests_all: the FULL validated xcvrd/SFP test set — every test we
#   have confirmed passing against the xcvr-emu emulator (plus the one known
#   fail, error_status[--fetch-from-hardware], which needs ALL 32 ports OK).
#   Requires the emulator to be deployed first (run `emulator`), which installs
#   platform.json, injects the sonic_platform bridge, and activates datapaths.
#
#   Suites (from SQL baseline xcvrd_tests):
#     - platform_tests/test_xcvr_info_in_db.py   (TRANSCEIVER_INFO/DOM populated)
#     - platform_tests/sfp/test_sfpshow.py       (presence, eeprom)
#     - platform_tests/sfp/test_sfputil.py       (presence, eeprom(_hexdump),
#                                                 error_status, low_power_mode, reset)
#     - platform_tests/api/test_sfp.py           (23 SFP platform-API methods,
#                                                 incl. lpmode + error_description)
#
#   The declarative transceiver/eeprom suite runs in its own phase
#   (`transceiver_eeprom_tests`) since it lives under a different pytest path.
#   The emulator deploy stamps a non-vs asic_type into the DUT platform.json, so
#   all of these suites actually RUN (real PASS/FAIL) rather than being skipped/
#   xfailed by sonic-mgmt's `asic_type in ['vs']` conditional marks.
#
#   RESET TESTS TOGGLE: the module-reset tests (sfputil `reset` + api `test_reset`)
#   are SLOW (they reset all 32 emulated modules and wait for recovery). Skip them
#   with RESET_TESTS=0:
#       RESET_TESTS=0 ./setup-sonic-testbed.sh transceiver_tests_all
#   Default (RESET_TESTS=1) runs them. test_get_reset_status (a quick bool read)
#   is always kept.
# ---------------------------------------------------------------------------
transceiver_tests_all() {
  parse_verbose "${1:-}" && shift || true
  log "Transceiver tests (full validated set)  (verbose=${VERBOSE:-0}, reset_tests=${RESET_TESTS:-1})"
  inject_conn_graph
  local reset_deselect=()
  if [ "${RESET_TESTS:-1}" = "0" ]; then
    log "  RESET_TESTS=0 -> skipping the slow module-reset tests (sfputil reset + api test_reset)"
    reset_deselect=(
      "--deselect" "platform_tests/sfp/test_sfputil.py::test_check_sfputil_reset"
      "--deselect" "platform_tests/api/test_sfp.py::TestSfpApi::test_reset"
    )
  fi
  run_pytest \
    "platform_tests/test_xcvr_info_in_db.py" \
    "platform_tests/sfp/test_sfpshow.py" \
    "platform_tests/sfp/test_sfputil.py" \
    "platform_tests/api/test_sfp.py" \
    "${reset_deselect[@]}"
}

# ---------------------------------------------------------------------------
# transceiver_eeprom_tests: the declarative tests/transceiver/eeprom/ suite
#   (presence + eeprom-content). tests/transceiver/conftest.py hard-skips the
#   whole suite when duthost.facts["asic_type"] == "vs"; the emulator deploy
#   (ship_and_deploy.sh) now stamps a non-vs asic_type into the DUT platform.json
#   as part of the emulator setup, so this suite just runs — no per-phase flip.
#
#   The transceiver inventory this suite reads is installed into the mgmt
#   container by the emulator deploy too (mirrors gen_emu_config.py: vendor
#   xcvr-emu, PN EMU-40G-LR4). Requires the emulator deployed first.
# ---------------------------------------------------------------------------
transceiver_eeprom_tests() {
  parse_verbose "${1:-}" && shift || true
  log "Transceiver eeprom suite (declarative)  (verbose=${VERBOSE:-0})"
  inject_conn_graph
  run_pytest "transceiver/eeprom/"
}

# ---------------------------------------------------------------------------
# inject_conn_graph: provide a lab connection graph for the KVM DUT (vlab-01).
#   The stock KVM (vms-kvm-t0) testbed ships NO lab connection graph, so the
#   pytest `conn_graph_facts` fixture returns an empty dict and every
#   transceiver test that reads conn_graph_facts["device_conn"][dut] dies at
#   setup with KeyError('vlab-01') (blocks api/test_sfp + test_xcvr_info_in_db).
#
#   We synthesize a graph group "vlab" from the DUT's real front-panel ports and
#   DEVICE_NEIGHBOR wiring and drop it into the mgmt container's ansible/files/.
#   This is lab-provisioning DATA (exactly what a real lab supplies), injected at
#   runtime into the *container copy* only — nothing is committed into the
#   sonic-mgmt repo. It is idempotent and rebuilt on every testbed rebuild.
#
#   The port<->neighbor map below is the standard vms-kvm-t0 wiring (32x40G
#   Force10-S6000: 24 server downlinks + 4 ARISTA T1 uplinks; Ethernet0/100/104/
#   108 are unused and intentionally omitted, matching DEVICE_NEIGHBOR).
# ---------------------------------------------------------------------------
inject_conn_graph() {
  log "Inject lab connection graph for $DUT (fixes conn_graph_facts KeyError)"
  local files_dir="/data/sonic-mgmt/ansible/files"
  local tmp; tmp="$(mktemp -d)"

  cat > "$tmp/sonic_vlab_devices.csv" <<'CSV'
Hostname,ManagementIp,HwSku,Type,Protocol,Os,AuthType
vlab-01,10.250.0.101/24,Force10-S6000,DevSonic,,sonic,
ARISTA01T1,10.64.1.1/24,Arista-VM,DevSonic,,eos,
ARISTA02T1,10.64.1.2/24,Arista-VM,DevSonic,,eos,
ARISTA03T1,10.64.1.3/24,Arista-VM,DevSonic,,eos,
ARISTA04T1,10.64.1.4/24,Arista-VM,DevSonic,,eos,
Servers0,10.64.0.1/24,TestServ,Server,,ubuntu,
Servers1,10.64.0.2/24,TestServ,Server,,ubuntu,
Servers2,10.64.0.3/24,TestServ,Server,,ubuntu,
Servers3,10.64.0.4/24,TestServ,Server,,ubuntu,
Servers4,10.64.0.5/24,TestServ,Server,,ubuntu,
Servers5,10.64.0.6/24,TestServ,Server,,ubuntu,
Servers6,10.64.0.7/24,TestServ,Server,,ubuntu,
Servers7,10.64.0.8/24,TestServ,Server,,ubuntu,
Servers8,10.64.0.9/24,TestServ,Server,,ubuntu,
Servers9,10.64.0.10/24,TestServ,Server,,ubuntu,
Servers10,10.64.0.11/24,TestServ,Server,,ubuntu,
Servers11,10.64.0.12/24,TestServ,Server,,ubuntu,
Servers12,10.64.0.13/24,TestServ,Server,,ubuntu,
Servers13,10.64.0.14/24,TestServ,Server,,ubuntu,
Servers14,10.64.0.15/24,TestServ,Server,,ubuntu,
Servers15,10.64.0.16/24,TestServ,Server,,ubuntu,
Servers16,10.64.0.17/24,TestServ,Server,,ubuntu,
Servers17,10.64.0.18/24,TestServ,Server,,ubuntu,
Servers18,10.64.0.19/24,TestServ,Server,,ubuntu,
Servers19,10.64.0.20/24,TestServ,Server,,ubuntu,
Servers20,10.64.0.21/24,TestServ,Server,,ubuntu,
Servers21,10.64.0.22/24,TestServ,Server,,ubuntu,
Servers22,10.64.0.23/24,TestServ,Server,,ubuntu,
Servers23,10.64.0.24/24,TestServ,Server,,ubuntu,
CSV

  cat > "$tmp/sonic_vlab_links.csv" <<'CSV'
StartDevice,StartPort,EndDevice,EndPort,BandWidth,VlanID,VlanMode,AutoNeg
vlab-01,Ethernet4,Servers0,eth0,40000,,,
vlab-01,Ethernet8,Servers1,eth0,40000,,,
vlab-01,Ethernet12,Servers2,eth0,40000,,,
vlab-01,Ethernet16,Servers3,eth0,40000,,,
vlab-01,Ethernet20,Servers4,eth0,40000,,,
vlab-01,Ethernet24,Servers5,eth0,40000,,,
vlab-01,Ethernet28,Servers6,eth0,40000,,,
vlab-01,Ethernet32,Servers7,eth0,40000,,,
vlab-01,Ethernet36,Servers8,eth0,40000,,,
vlab-01,Ethernet40,Servers9,eth0,40000,,,
vlab-01,Ethernet44,Servers10,eth0,40000,,,
vlab-01,Ethernet48,Servers11,eth0,40000,,,
vlab-01,Ethernet52,Servers12,eth0,40000,,,
vlab-01,Ethernet56,Servers13,eth0,40000,,,
vlab-01,Ethernet60,Servers14,eth0,40000,,,
vlab-01,Ethernet64,Servers15,eth0,40000,,,
vlab-01,Ethernet68,Servers16,eth0,40000,,,
vlab-01,Ethernet72,Servers17,eth0,40000,,,
vlab-01,Ethernet76,Servers18,eth0,40000,,,
vlab-01,Ethernet80,Servers19,eth0,40000,,,
vlab-01,Ethernet84,Servers20,eth0,40000,,,
vlab-01,Ethernet88,Servers21,eth0,40000,,,
vlab-01,Ethernet92,Servers22,eth0,40000,,,
vlab-01,Ethernet96,Servers23,eth0,40000,,,
vlab-01,Ethernet112,ARISTA01T1,Ethernet1,40000,,,
vlab-01,Ethernet116,ARISTA02T1,Ethernet1,40000,,,
vlab-01,Ethernet120,ARISTA03T1,Ethernet1,40000,,,
vlab-01,Ethernet124,ARISTA04T1,Ethernet1,40000,,,
CSV

  docker cp "$tmp/sonic_vlab_devices.csv" "$MGMT_CONTAINER:$files_dir/sonic_vlab_devices.csv"
  docker cp "$tmp/sonic_vlab_links.csv"   "$MGMT_CONTAINER:$files_dir/sonic_vlab_links.csv"
  rm -rf "$tmp"

  # Register the "vlab" graph group so find_graph() considers it (idempotent).
  docker exec --user root "$MGMT_CONTAINER" bash -c '
    GG=/data/sonic-mgmt/ansible/files/graph_groups.yml
    grep -qE "^[[:space:]]*-[[:space:]]*vlab[[:space:]]*$" "$GG" || echo "  - vlab" >> "$GG"
  '

  # Verify resolution via the same code path the pytest fixture uses.
  if docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc '
        cd /data/sonic-mgmt/ansible
        python3 - <<PY
import sys
sys.path.insert(0, "module_utils"); sys.path.insert(0, "library")
import conn_graph_facts as c
c.LAB_GRAPHFILE_PATH = "files/"
g = c.find_graph(["vlab-01"])
assert g is not None
ok, res = g.build_results(["vlab-01"], False)
assert ok and res["device_conn"]["vlab-01"], "empty device_conn"
print(len(res["device_conn"]["vlab-01"]))
PY'; then
    ok "connection graph injected — vlab-01 resolves in conn_graph_facts"
  else
    die "connection graph injection failed to resolve for $DUT"
  fi
}

# ---------------------------------------------------------------------------
# emulator: run the xcvr-emu CMIS emulator as a standalone Docker container on
#   the DUT and install the sonic_platform gRPC bridge into pmon, so xcvrd
#   populates TRANSCEIVER_INFO + TRANSCEIVER_DOM_SENSOR in STATE_DB (what
#   test_xcvr_info_in_db needs).
#
#   * the emulator runs as its own `docker run --network host --restart
#     unless-stopped` container — NOT inside pmon — so it survives the SONiC
#     `config reload` that sonic-mgmt tests trigger.
#   * only the bridge lives inside pmon, loaded via PYTHONPATH from a side dir
#     (/opt/xcvr-emu-bridge) so pmon's dist-packages is never modified; xcvrd
#     stays supervised inside pmon, unchanged.
#
#   Uses this checkout's sibling assets:
#     $BRIDGE_DIR      = platform/sonic_platform   (the gRPC bridge)
#     $EMU_DEPLOY_DIR  = emu-deploy/               (build_emu_image.sh, build_bundle.sh,
#                                                   ship_and_deploy.sh, deploy_on_dut.sh,
#                                                   gen_emu_config.py)
#   and clones the xcvr-emu emulator ($XCVR_EMU_URL) to $XCVR_EMU_DIR on demand.
#   Nothing is written into the cloned SONiC repos; the emulator lives only in
#   its own (disposable) container + the pmon writable layer.
# ---------------------------------------------------------------------------
ensure_emu_assets() {
  [ -d "$BRIDGE_DIR" ] || die "bridge not found at $BRIDGE_DIR — run this from a full sonic-develop checkout (git clone) so platform/ and emu-deploy/ sit next to the script, not a lone scp'd copy."
  [ -d "$EMU_DEPLOY_DIR" ] || die "emu-deploy toolkit not found at $EMU_DEPLOY_DIR — run from a full sonic-develop checkout."
  # Obtain the xcvr-emu source (gsoosk fork) on the $XCVR_EMU_BRANCH branch, which
  # carries the emulator fixes. Prefer SSH; fall back to read-only HTTPS when SSH
  # auth is not configured on this host (a read-only clone is enough to build).
  local url="$XCVR_EMU_URL"
  if ! GIT_SSH_COMMAND='ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new' git ls-remote "$url" >/dev/null 2>&1; then
    log "SSH remote $url not reachable — using HTTPS read-only ($XCVR_EMU_URL_HTTPS)"
    url="$XCVR_EMU_URL_HTTPS"
  fi
  if [ ! -d "$XCVR_EMU_DIR/.git" ]; then
    log "Cloning xcvr-emu ($XCVR_EMU_BRANCH) to $XCVR_EMU_DIR"
    git clone --branch "$XCVR_EMU_BRANCH" "$url" "$XCVR_EMU_DIR" || die "git clone $url failed"
  else
    # An existing checkout may be a stale upstream clone: repoint to the fork and
    # move onto the sonic-dev branch so build_bundle.sh also sees the fixed source.
    [ "$(git -C "$XCVR_EMU_DIR" remote get-url origin 2>/dev/null)" = "$url" ] || \
      git -C "$XCVR_EMU_DIR" remote set-url origin "$url"
    # widen the fetch refspec in case this was a shallow/single-branch clone;
    # -f discards any stale in-tree changes (e.g. the old build-time sed patches)
    git -C "$XCVR_EMU_DIR" config remote.origin.fetch '+refs/heads/*:refs/remotes/origin/*'
    git -C "$XCVR_EMU_DIR" fetch origin "$XCVR_EMU_BRANCH" || die "git fetch $url ($XCVR_EMU_BRANCH) failed"
    git -C "$XCVR_EMU_DIR" checkout -f -B "$XCVR_EMU_BRANCH" "origin/$XCVR_EMU_BRANCH" || die "git checkout $XCVR_EMU_BRANCH failed"
  fi
  ok "emulator assets present (bridge + emu-deploy + xcvr-emu @ $XCVR_EMU_BRANCH)"
}

emulator() {
  log "Deploy xcvr-emu (NATIVE) on $DUT: host sonic_platform:=bridge + skip_xcvrd=false + pmon inject (MGMT_CONTAINER=$MGMT_CONTAINER)"
  ensure_emu_assets

  log "Building emulator image (cached; EMU_REBUILD_IMAGE=$EMU_REBUILD_IMAGE)"
  EMU_REBUILD_IMAGE="$EMU_REBUILD_IMAGE" \
    bash "$EMU_DEPLOY_DIR/build_emu_image.sh" "$XCVR_EMU_DIR" "xcvr-emu:local" "$EMU_IMAGE_TAR" \
    || die "build_emu_image.sh failed"

  log "Building emulator bundle ($EMU_MODULES modules)"
  bash "$EMU_DEPLOY_DIR/build_bundle.sh" "$XCVR_EMU_DIR" "$EMU_MODULES" \
    || die "build_bundle.sh failed"

  log "Shipping image + bundle to $DUT and running the native deploy"
  MGMT_CONTAINER="$MGMT_CONTAINER" DUT_IP="$DUT_IP" DUT_PASS="$DUT_PASS" \
    bash "$EMU_DEPLOY_DIR/ship_and_deploy.sh" "$EMU_BUNDLE" "$EMU_IMAGE_TAR" \
    || die "ship_and_deploy.sh failed"
  ok "emulator deployed (native) — host sfputil + pmon xcvrd both use the emulator; STATE_DB populated; transceiver inventory installed in mgmt"
}

# ---------------------------------------------------------------------------
# emulator_revert: undo the native emulator deploy — restore the stock host
#   sonic_platform, restore skip_xcvrd, remove the pmon injection, restart pmon.
#   The xcvr-emu container is left running (harmless); remove it by hand if you
#   want (`docker rm -f xcvr-emu` on the DUT).
# ---------------------------------------------------------------------------
emulator_revert() {
  log "Reverting native emulator deploy on $DUT (restore stock platform + skip_xcvrd)"
  [ -d "$EMU_DEPLOY_DIR" ] || die "emu-deploy toolkit not found at $EMU_DEPLOY_DIR"
  MGMT_CONTAINER="$MGMT_CONTAINER" DUT_IP="$DUT_IP" DUT_PASS="$DUT_PASS" \
    bash "$EMU_DEPLOY_DIR/ship_and_revert.sh" \
    || die "ship_and_revert.sh failed"
  ok "emulator reverted — DUT back to stock sonic-vs platform"
}

# ---------------------------------------------------------------------------
# transceiver_emu_test: the end-to-end payoff. Ensures the connection graph is
#   injected (Option A) and runs test_xcvr_info_in_db, which now PASSES because
#   the emulator+xcvrd have populated TRANSCEIVER_INFO + DOM. Run `emulator`
#   first (or use the combined `emulator_e2e`).
# ---------------------------------------------------------------------------
transceiver_emu_test() {
  parse_verbose "${1:-}" && shift || true
  inject_conn_graph
  log "Running test_xcvr_info_in_db against the emulator-backed DUT"
  run_pytest "platform_tests/test_xcvr_info_in_db.py"
}

# emulator_e2e: one-shot — deploy the emulator then run the target test.
emulator_e2e() {
  emulator
  transceiver_emu_test "$@"
}

# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
remove_topo() {
  log "Teardown: remove-topo + stop-vms"
  tbcli "-t $TB_FILE -m $INV -k $VM_TYPE remove-topo $TESTBED_NAME $VAULT_FILE" || true
  tbcli "-t $TB_FILE -m $INV stop-vms $SERVER $VAULT_FILE" || true
  ok "topology removed"
}

# ---------------------------------------------------------------------------
# rebuild: recover a testbed after the ephemeral /mnt/data disk was wiped
#   (Azure Direct/temp disk is erased on VM deallocate/stop). Re-lays storage,
#   re-downloads the image if missing, and recreates VMs + topology + config.
#   Also cleans stale (diskless) libvirt domains so start-vms recreates cleanly.
# ---------------------------------------------------------------------------
rebuild() {
  log "Rebuild: recovering testbed after a disk wipe / stop"
  setup_storage
  # undefine any leftover diskless domains from a previous (wiped) run
  for d in $(sudo virsh list --all --name 2>/dev/null); do
    [ -n "$d" ] && { sudo virsh destroy "$d" 2>/dev/null; sudo virsh undefine "$d" 2>/dev/null; }
  done
  download_image
  start_vms
  add_topo
  deploy_mg
  verify
  inject_conn_graph
  emulator
  ok "rebuild complete — DUT=$DUT testbed=$TESTBED_NAME (emulator redeployed)"
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
  emulator
  inject_conn_graph
  transceiver_tests
  log "DONE — SONiC KVM testbed is up (emulator-backed transceivers). DUT=$DUT  testbed=$TESTBED_NAME"
}

"${1:-all}" "${@:2}"
