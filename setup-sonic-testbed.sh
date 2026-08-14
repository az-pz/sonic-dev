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
#   ./setup-sonic-testbed.sh              # run every phase in order (INCLUDES the emulator)
#   ./setup-sonic-testbed.sh <phase>      # run a single phase (re-runnable)
#   ./setup-sonic-testbed.sh --help       # full phase list, arguments and env vars
#
# The authoritative phase list lives in phase_registry() near the bottom of this
# file; `--help`, argument validation and shell completion are all generated from
# it, so they cannot drift out of sync. Do NOT maintain a duplicate list here.
#
# Tab completion (phase names, Rust pipeline folders, DUT ports):
#   eval "$(./setup-sonic-testbed.sh --completion bash)"     # current shell
#   ./setup-sonic-testbed.sh --completion bash | sudo tee \
#       /etc/bash_completion.d/setup-sonic-testbed >/dev/null   # persistent
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

# pytest's --neighbor_type must match the neighbor VM flavour we actually deploy.
# Upstream it defaults to "eos", and tests/conftest.py::converge_topo_if_needed
# treats "eos"/"ceos" as cEOS neighbors, so for a testbed with
# `use_converged_peers: True` (vms-kvm-t0 has had it since sonic-mgmt 1b3b173ac)
# it CONVERGES ansible/vars/topo_<topo>.yml in place -- every neighbor merged
# onto one multi-VRF VM. Our neighbors are vsonic/csonic, whose startup configs
# and the DUT minigraph are unconverged, and ansible/testbed-cli.sh skips the
# converge (and its restore-from-.bak) for non-ceos vm_type. A converged topo
# file left behind by pytest therefore makes a later `add-topo` wire every DUT
# link to VM0100, and only 1 of N BGP sessions ever comes up.
# Deriving --neighbor_type from VM_TYPE keeps conftest on the non-cEOS path.
case "$VM_TYPE" in
  veos) NEIGHBOR_TYPE="${NEIGHBOR_TYPE:-eos}" ;;
  *)    NEIGHBOR_TYPE="${NEIGHBOR_TYPE:-$VM_TYPE}" ;;
esac
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
# Special (non-uniform) modules: SFF-8636 / coherent 400G-ZR / flat-memory /
# multi-application. They exist for the xcvrd-tests suite, but the sonic-mgmt
# platform + transceiver suites assume every port is a uniform, fully-featured
# CMIS module and fail on them. Default to a UNIFORM testbed so those suites are
# green, and let xcvrd_tests re-deploy with the special modules it needs (see the
# prestep in xcvrd_tests). Exported so gen_emu_config.py sees it both when
# build_bundle.sh generates emu_config.yaml and when inject_conn_graph asks
# --list-special which ports to keep out of the connection graph -- that way the
# graph exclusions always match the modules actually deployed.
EMU_NO_SPECIAL="${EMU_NO_SPECIAL:-1}"                             # 1 = uniform CMIS only; 0 = provision the special modules
export EMU_NO_SPECIAL
EMU_TEST_HOOKS="${EMU_TEST_HOOKS:-1}"                             # 1 = enable the bridge error-injection hook for xcvrd_tests (this is a test/dev testbed); set 0 for a clean virtual platform
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
_run_pytest() {
  # $* = pytest test paths/args
  local extra="-ra --tb=short"
  if [ "${VERBOSE:-0}" = "1" ]; then
    extra="-rA --tb=long --showlocals -s"
  fi
  docker exec --user "$HOST_USER" "${CONTAINER_ENV[@]}" "${PYTEST_ENV[@]}" "$MGMT_CONTAINER" bash -lc \
    "cd /data/sonic-mgmt/tests && python3 -m pytest $* \
        --inventory ../ansible/$INV --host-pattern $DUT \
        --testbed $TESTBED_NAME --testbed_file ../ansible/$TB_FILE \
        --neighbor_type $NEIGHBOR_TYPE \
        --skip_sanity --disable_loganalyzer $extra -v"
}

# ---------------------------------------------------------------------------
# run_pytest: run ARBITRARY sonic-mgmt pytest targets against this testbed,
#   with the same wiring (inventory, testbed file, neighbor_type, --skip_sanity,
#   --disable_loganalyzer) the canned phases use. This is the escape hatch for
#   iterating on a single test instead of waiting out a whole suite.
#
#   Everything after the phase name is passed straight to pytest, so paths,
#   node IDs and pytest flags all work. A leading `--` is accepted (and dropped)
#   for symmetry with `xcvrd_tests`, and to disambiguate a leading pytest flag:
#     ./setup-sonic-testbed.sh run_pytest platform_tests/sfp/test_sfpshow.py
#     ./setup-sonic-testbed.sh run_pytest platform_tests/api/test_sfp.py -k lpmode
#     ./setup-sonic-testbed.sh run_pytest transceiver/eeprom/ --collect-only -q
#     ./setup-sonic-testbed.sh run_pytest -- -k "presence and not hexdump" transceiver/
#     VERBOSE=1 ./setup-sonic-testbed.sh run_pytest platform_tests/sfp/test_sfputil.py
#
#   --rust <folder>  builds the Rust xcvrd from a recodeAgent pipeline folder,
#   reversibly injects it into pmon (flushing STATE_DB so it must repopulate),
#   runs the target against it, then ALWAYS restores the Python xcvrd -- the
#   same crash-safe build/inject/restore the transceiver_tests_*_rust phases
#   use, so a single test can be graded against the Rust daemon without running
#   a whole suite. It is consumed by this phase and never forwarded to pytest;
#   `--rust=<folder>` works too, and it may appear before or after the target:
#     ./setup-sonic-testbed.sh run_pytest --rust ./recodeAgent/results/result_4 \
#         platform_tests/test_xcvr_info_in_db.py
#     ./setup-sonic-testbed.sh run_pytest transceiver/eeprom/ --rust ./recodeAgent/results/result_4
#
#   --dom_update_interval <secs>  (or DOM_UPDATE_INTERVAL=<secs>) is baked into
#   the Rust inject shim as the daemon's --dom_update_interval. xcvrd's DOM loop
#   defaults to 60 s, so after the inject's STATE_DB flush the DOM-backed tables
#   (TRANSCEIVER_DOM_SENSOR / _STATUS) stay empty for up to a minute while
#   TRANSCEIVER_INFO / _DOM_THRESHOLD appear immediately -- a smaller value makes
#   DOM-cadence tests finish sooner and shrinks that window. Opt-in: unset keeps
#   the upstream 60 s, so the Rust port is never silently graded under
#   non-default timing. Applies to --rust runs only (it is a property of the
#   injected daemon); it is ignored, with a warning, without --rust.
#     ./setup-sonic-testbed.sh run_pytest --rust recodeAgent/results/result_4 \
#         --dom_update_interval 5 platform_tests/test_xcvr_info_in_db.py
#     DOM_UPDATE_INTERVAL=5 ./setup-sonic-testbed.sh transceiver_tests_all_rust <folder>
#
#   NOTE: -v is NOT consumed as a verbosity flag here (unlike the canned test
#   phases) because it is also pytest's own flag -- it is forwarded to pytest
#   like everything else. Use VERBOSE=1 for full tracebacks/--showlocals/-s.
#
#   Paths are relative to /data/sonic-mgmt/tests inside the mgmt container.
#   The connection graph is injected first (same as the canned phases), so
#   conn_graph_facts resolves and the special-module ports stay excluded; set
#   SKIP_CONN_GRAPH=1 to skip that step when iterating rapidly.
# ---------------------------------------------------------------------------
run_pytest() {
  [ "${1:-}" = "--" ] && shift   # allow `run_pytest -- <pytest args>`

  # Pull --rust/--rust=<folder> out of the argv; everything else is pytest's.
  # Scanning (rather than only checking $1) lets the flag sit anywhere, which
  # matters because the natural thing to type is `<target> --rust <folder>`.
  local rust_folder="" ; local -a pyargs=()
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --rust)
        [ "$#" -ge 2 ] || die "--rust needs a recodeAgent pipeline folder, e.g. --rust ./recodeAgent/results/result_4"
        rust_folder="$2"; shift 2 ;;
      --rust=*)
        rust_folder="${1#--rust=}"
        [ -n "$rust_folder" ] || die "--rust= needs a folder, e.g. --rust=./recodeAgent/results/result_4"
        shift ;;
      --dom_update_interval)
        [ "$#" -ge 2 ] || die "--dom_update_interval needs a value in seconds, e.g. --dom_update_interval 5"
        DOM_UPDATE_INTERVAL="$2"; shift 2 ;;
      --dom_update_interval=*)
        DOM_UPDATE_INTERVAL="${1#--dom_update_interval=}"
        [ -n "$DOM_UPDATE_INTERVAL" ] || die "--dom_update_interval= needs a value, e.g. --dom_update_interval=5"
        shift ;;
      *) pyargs+=("$1"); shift ;;
    esac
  done
  set -- "${pyargs[@]+"${pyargs[@]}"}"

  if [ "$#" -eq 0 ]; then
    die "run_pytest needs at least one pytest target, e.g.
    ./setup-sonic-testbed.sh run_pytest platform_tests/sfp/test_sfpshow.py
    ./setup-sonic-testbed.sh run_pytest platform_tests/api/test_sfp.py -k lpmode
    ./setup-sonic-testbed.sh run_pytest --rust ./recodeAgent/results/result_4 platform_tests/test_xcvr_info_in_db.py
  (paths are relative to /data/sonic-mgmt/tests; run --help for more examples)"
  fi
  log "pytest: $*  (verbose=${VERBOSE:-0}${rust_folder:+, rust=$rust_folder}${DOM_UPDATE_INTERVAL:+, dom_update_interval=$DOM_UPDATE_INTERVAL})"
  # Validate BEFORE any DUT/docker work, so a typo'd path or interval fails
  # instantly instead of after the connection-graph injection.
  if [ -n "${DOM_UPDATE_INTERVAL:-}" ]; then
    case "$DOM_UPDATE_INTERVAL" in
      ''|*[!0-9]*) die "--dom_update_interval/DOM_UPDATE_INTERVAL must be a non-negative integer (got '$DOM_UPDATE_INTERVAL')" ;;
    esac
  fi
  if [ -n "$rust_folder" ]; then
    [ -d "$rust_folder" ] || die "rust pipeline folder not found: $rust_folder"
    [ -d "$rust_folder/crate" ] || die "no crate/ workspace under $rust_folder — is this a recodeAgent pipeline folder? (expected $rust_folder/crate/Cargo.toml)"
  elif [ -n "${DOM_UPDATE_INTERVAL:-}" ]; then
    # The interval is applied by baking it into the Rust inject shim, so without
    # --rust there is nothing to apply it to. Warn instead of silently ignoring
    # it -- a user who thinks they changed the DOM cadence would otherwise
    # misread the resulting timings.
    warn "--dom_update_interval/DOM_UPDATE_INTERVAL only applies to the injected Rust xcvrd (--rust); ignoring it for this run"
  fi
  if [ "${SKIP_CONN_GRAPH:-0}" = "1" ]; then
    log "  SKIP_CONN_GRAPH=1 -> not re-injecting the connection graph"
  else
    inject_conn_graph
  fi

  if [ -z "$rust_folder" ]; then
    _run_pytest "$@"
    return $?
  fi

  # Rust path: _rust_build_and_inject arms an EXIT/INT/TERM trap that restores
  # the Python xcvrd, so an interrupt or a die() mid-run cannot strand the DUT
  # with a Rust daemon injected.
  _rust_build_and_inject "$rust_folder"
  log "Running pytest against the injected Rust xcvrd: $*"
  _run_pytest "$@"
  local rc=$?
  _rust_restore
  trap - EXIT INT TERM
  if [ "$rc" -eq 0 ]; then ok "Rust xcvrd: pytest PASSED"; else warn "Rust xcvrd: pytest exited rc=$rc"; fi
  return "$rc"
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
  _run_pytest "$tp"
}

# ---------------------------------------------------------------------------
# transceiver_tests: run the xcvrd/transceiver tests that PASS on a vs DUT
#   (sfpshow + sfputil presence/eeprom/reset — the sonic-vs platform provides
#    enough for these). This is the green transceiver smoke set.
# ---------------------------------------------------------------------------
transceiver_tests() {
  parse_verbose "${1:-}" && shift || true
  log "Transceiver tests (vs-compatible subset)  (verbose=${VERBOSE:-0})"
  _run_pytest \
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
#     - transceiver/eeprom/                      (declarative presence, eeprom
#                                                 content, hexdump, breakout
#                                                 serial, VDM, error handling)
#
#   The declarative transceiver/eeprom suite is included here too, and also has
#   its own standalone phase (`transceiver_eeprom_tests`) for running it alone.
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
  _run_pytest \
    "platform_tests/test_xcvr_info_in_db.py" \
    "platform_tests/sfp/test_sfpshow.py" \
    "platform_tests/sfp/test_sfputil.py" \
    "platform_tests/api/test_sfp.py" \
    "transceiver/eeprom/" \
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
  _run_pytest "transceiver/eeprom/"
}

# ---------------------------------------------------------------------------
# Rust xcvrd variants: build a Rust xcvrd from a recodeAgent PIPELINE FOLDER,
#   reversibly inject it into pmon (crash-safe), run the SAME sonic-mgmt suite
#   against it, then ALWAYS restore the Python xcvrd. This lets you grade how
#   complete a translated Rust implementation is against the real transceiver
#   tests, without disturbing the stock testbed.
#
#   <folder> is a recodeAgent pipeline-run dir that contains a buildable crate/
#   workspace (crate/xcvrd-rs, crate/platform-bridge), e.g.
#   dev/recodeAgent/pipeline_run3. Build + inject reuse the proven recodeAgent
#   harness: tools/dut/build_crate.sh (Debian-13 build container matching pmon)
#   and tools/dut/rust_xcvrd_ctl.sh (the crash-safe pmon inject/restore).
#
#   Requires the emulator to be deployed first (run `emulator`), exactly like
#   transceiver_tests_all. Env RESET_TESTS / VERBOSE / -v behave as usual.
#     ./setup-sonic-testbed.sh transceiver_tests_rust      <folder> [-v]
#     ./setup-sonic-testbed.sh transceiver_tests_all_rust  <folder> [-v]
#     RESET_TESTS=0 ./setup-sonic-testbed.sh transceiver_tests_all_rust <folder>
# ---------------------------------------------------------------------------
# The bundled DUT helper scripts (rust_xcvrd_ctl.sh, build_crate.sh,
# ensure_swsslib.sh, Dockerfile.build) ship NEXT TO this script under
# recodeAgent/tools/dut/, and are copied to the host together with
# setup-sonic-testbed.sh. _dut_dir returns the first layout that actually holds
# them, so the ONLY argument you pass to the rust subcommands is the crate folder.
# RECODE_DUT_DIR still works as an explicit override but is not required.
_dut_dir() {
  local c
  for c in \
      "${RECODE_DUT_DIR:-}" \
      "$SCRIPT_DIR/recodeAgent/tools/dut" \
      "$SCRIPT_DIR/tools/dut" \
      "$SCRIPT_DIR/dut"; do
    [ -n "$c" ] && [ -f "$c/rust_xcvrd_ctl.sh" ] && [ -f "$c/build_crate.sh" ] && { echo "$c"; return 0; }
  done
  return 1
}

# Idempotent restore of the Python xcvrd on the DUT (explicit + EXIT-trap safety
# net). Vars are recomputed from globals so it works from the trap context.
_rust_restore() {
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  local dut="admin@$DUT_IP"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp ssh $sshopt $dut 'bash /home/admin/rust_xcvrd_ctl.sh restore'" 2>/dev/null || true

  # VERIFY, and complain loudly if the Python xcvrd is not actually back. The
  # restore above is best-effort (it runs from a trap, so it must never abort),
  # but staying silent means a later run can grade the WRONG daemon: a stranded
  # Rust xcvrd looks exactly like a normal testbed until you check. Non-fatal by
  # design -- we are often already unwinding from an error or an interrupt.
  local st
  st="$(docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
        "$sshp ssh $sshopt $dut 'bash /home/admin/rust_xcvrd_ctl.sh status'" 2>/dev/null)"
  case "$st" in
    *"PYTHON (stock)"*) : ;;   # restored as expected
    "")
      warn "could not verify xcvrd flavor after restore (DUT unreachable?) — run './setup-sonic-testbed.sh xcvrd_status' before trusting the next test run" ;;
    *)
      warn "RUST xcvrd may STILL be injected after restore — the next test run would grade the wrong daemon.
  Check with : ./setup-sonic-testbed.sh xcvrd_status
  Fix with   : ssh to the DUT and run 'bash /home/admin/rust_xcvrd_ctl.sh restore'"
      printf '%s\n' "$st" | sed 's/^/  /' ;;
  esac
}

_rust_build_and_inject() {
  local folder="$1"
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  local dut="admin@$DUT_IP"

  [ -n "$folder" ] || die "no rust pipeline folder given (usage: <cmd> <folder> [-v])"
  [ -d "$folder" ] || die "rust pipeline folder not found: $folder"
  folder="$(cd "$folder" && pwd)"          # absolute: docker -v needs it (relative => empty volume!)
  local crate="$folder/crate"
  local bin="$crate/target/release/xcvrd-rs"
  local dutdir; dutdir="$(_dut_dir)" \
    || die "DUT helper scripts not found — expected recodeAgent/tools/dut next to $SCRIPT_DIR/$(basename "$0") (copy recodeAgent/tools/dut alongside setup-sonic-testbed.sh, or set RECODE_DUT_DIR)"
  local ctl="$dutdir/rust_xcvrd_ctl.sh"
  [ -d "$crate" ] || die "no crate/ workspace under $folder (expected $crate with xcvrd-rs/, platform-bridge/)"
  [ -f "$crate/Cargo.toml" ] || die "no Cargo.toml at $crate — is this a recodeAgent pipeline folder?"

  # 0) ensure the Debian-13 build image exists (one-time; matches pmon runtime).
  if ! docker image inspect recode-rust-build >/dev/null 2>&1; then
    log "Building recode-rust-build image (one-time; trixie / py3.13 / glibc2.41)"
    docker build -t recode-rust-build -f "$dutdir/Dockerfile.build" "$dutdir" \
      || die "failed to build recode-rust-build image"
  fi

  # 1) build xcvrd-rs for pmon (glibc2.41, links libpython3.13 + libswsscommon).
  log "Building Rust xcvrd from $crate"
  bash "$dutdir/build_crate.sh" "$crate" || die "rust build FAILED for $crate"
  [ -x "$bin" ] || die "build produced no binary at $bin"
  ok "built $bin"

  # 2) ship binary + control script to the DUT (host -> mgmt container -> vlab).
  log "Shipping Rust xcvrd to $DUT"
  docker cp "$bin" "$MGMT_CONTAINER:/tmp/xcvrd-rs"            || die "docker cp binary -> mgmt failed"
  docker cp "$ctl" "$MGMT_CONTAINER:/tmp/rust_xcvrd_ctl.sh"   || die "docker cp ctl -> mgmt failed"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp scp $sshopt /tmp/xcvrd-rs $dut:/home/admin/xcvrd-rs && \
     $sshp scp $sshopt /tmp/rust_xcvrd_ctl.sh $dut:/home/admin/rust_xcvrd_ctl.sh" \
    || die "failed to copy Rust artifacts to DUT"

  # 3) inject (crash-safe) — arm the restore trap BEFORE touching pmon's xcvrd.
  #    DOM_UPDATE_INTERVAL (seconds, opt-in) is forwarded to the Rust daemon as
  #    --dom_update_interval. Unset => the daemon keeps its upstream 60 s default,
  #    so a Rust run is never silently graded under non-default DOM timing.
  local ival="${DOM_UPDATE_INTERVAL:-}"
  if [ -n "$ival" ]; then
    case "$ival" in
      ''|*[!0-9]*) die "DOM_UPDATE_INTERVAL must be a non-negative integer (got '$ival')" ;;
    esac
    log "Injecting Rust xcvrd into pmon (reversible, --dom_update_interval=$ival)"
  else
    log "Injecting Rust xcvrd into pmon (reversible)"
  fi
  trap '_rust_restore' EXIT INT TERM
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp ssh $sshopt $dut 'bash /home/admin/rust_xcvrd_ctl.sh inject /home/admin/xcvrd-rs $ival'" \
    || die "inject FAILED (Python xcvrd left intact)"
  ok "Rust xcvrd injected + RUNNING in pmon"
}

# _rust_run <folder> <test-fn> [test-args...] : build+inject, run the suite, restore.
_rust_run() {
  local folder="$1"; local testfn="$2"; shift 2
  _rust_build_and_inject "$folder"
  log "Running '$testfn' against the injected Rust xcvrd"
  "$testfn" "$@"
  local rc=$?
  _rust_restore
  trap - EXIT INT TERM
  if [ "$rc" -eq 0 ]; then ok "Rust xcvrd: $testfn PASSED"; else warn "Rust xcvrd: $testfn exited rc=$rc"; fi
  return "$rc"
}

transceiver_tests_rust()     { _rust_run "${1:-}" transceiver_tests     "${@:2}"; }
transceiver_tests_all_rust() { _rust_run "${1:-}" transceiver_tests_all "${@:2}"; }

# ---------------------------------------------------------------------------
# xcvrd_tests_rust <folder> [-- pytest args] : run the xcvrd-tests black-box
#   suite against an injected Rust xcvrd, then ALWAYS restore the Python one.
#
#   This is the highest-signal Rust gate available: every one of xcvrd-tests'
#   ~105 tests reads xcvrd's own STATE_DB output, whereas transceiver_tests_all
#   spends most of its run in sonic_platform code the Rust port does not replace.
#
#   The emulator prestep is hoisted OUT of xcvrd_tests and run BEFORE the inject
#   on purpose. xcvrd_tests re-deploys the emulator when the special modules are
#   missing, and that deploy restarts pmon ("restarting pmon so it regenerates
#   supervisord") -- doing it after the inject would throw away the clean STATE_DB
#   baseline and the post-start settle that _rust_build_and_inject just
#   established, right before the tests read STATE_DB. Ordering it first also
#   means the ~3 min deploy happens while the stock Python daemon is still in
#   place, so a failure there costs nothing to unwind.
#
#     ./setup-sonic-testbed.sh xcvrd_tests_rust recodeAgent/results/result_4
#     ./setup-sonic-testbed.sh xcvrd_tests_rust <folder> -- -m "not slow"
#     DOM_UPDATE_INTERVAL=5 ./setup-sonic-testbed.sh xcvrd_tests_rust <folder>
# ---------------------------------------------------------------------------
xcvrd_tests_rust() {
  local folder="${1:-}"
  [ -n "$folder" ] || die "xcvrd_tests_rust needs a recodeAgent pipeline folder, e.g.
    ./setup-sonic-testbed.sh xcvrd_tests_rust recodeAgent/results/result_4
    ./setup-sonic-testbed.sh xcvrd_tests_rust recodeAgent/results/result_4 -- -m \"not slow\""
  # Validate before any DUT work so a typo'd path fails instantly.
  [ -d "$folder" ] || die "rust pipeline folder not found: $folder"
  [ -d "$folder/crate" ] || die "no crate/ workspace under $folder — is this a recodeAgent pipeline folder? (expected $folder/crate/Cargo.toml)"

  # Emulator first (see above), while the Python xcvrd is still running.
  _xcvrd_tests_prestep
  # SKIP_EMU_PRESTEP stops xcvrd_tests from re-running the prestep post-inject.
  SKIP_EMU_PRESTEP=1 _rust_run "$folder" xcvrd_tests "${@:2}"
}

# _rust_ship_ctl: copy the DUT control script to the DUT (host -> mgmt -> vlab).
_rust_ship_ctl() {
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  local dut="admin@$DUT_IP" dutdir ctl
  dutdir="$(_dut_dir)" \
    || die "DUT helper scripts not found — expected recodeAgent/tools/dut next to $SCRIPT_DIR/$(basename "$0")"
  ctl="$dutdir/rust_xcvrd_ctl.sh"
  docker cp "$ctl" "$MGMT_CONTAINER:/tmp/rust_xcvrd_ctl.sh" || die "docker cp ctl -> mgmt failed"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp scp $sshopt /tmp/rust_xcvrd_ctl.sh $dut:/home/admin/rust_xcvrd_ctl.sh" \
    || die "failed to copy control script to DUT"
}

# _rust_run_noop <test-fn> [args...] : NEGATIVE CONTROL. Inject a no-op xcvrd with
#   the SAME clean-baseline flush used by the real runs, run the suite, restore.
#   The xcvrd-dependent tests (STATE_DB-backed: sfpshow, test_xcvr_info_in_db) MUST
#   fail here; the platform-API tests (sfputil, api/test_sfp) may still pass since
#   they bypass xcvrd. This proves which tests actually exercise the daemon, so a
#   PASS from the real Rust run is attributable to the Rust xcvrd, not stale data.
_rust_run_noop() {
  local testfn="$1"; shift
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  local dut="admin@$DUT_IP"
  log "NEGATIVE CONTROL: inject a NO-OP xcvrd (clean baseline), then run '$testfn'"
  _rust_ship_ctl
  trap '_rust_restore' EXIT INT TERM
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp ssh $sshopt $dut 'bash /home/admin/rust_xcvrd_ctl.sh inject-noop'" \
    || die "inject-noop FAILED (Python xcvrd left intact)"
  ok "no-op xcvrd injected + RUNNING (STATE_DB flushed; nothing is repopulating it)"
  log "Running '$testfn' against the NO-OP xcvrd (expect the STATE_DB-backed tests to FAIL)"
  "$testfn" "$@"
  local rc=$?
  _rust_restore
  trap - EXIT INT TERM
  if [ "$rc" -ne 0 ]; then
    ok "negative control OK: '$testfn' did NOT fully pass (rc=$rc) — the STATE_DB tests have teeth"
  else
    warn "negative control: '$testfn' PASSED with a no-op xcvrd (rc=0) — those tests do NOT depend on xcvrd!"
  fi
  return "$rc"
}

transceiver_tests_noop()     { _rust_run_noop transceiver_tests     "$@"; }
transceiver_tests_all_noop() { _rust_run_noop transceiver_tests_all "$@"; }

# ---------------------------------------------------------------------------
# xcvrd_status (alias: xcvrd_info): show which xcvrd is currently running in
#   pmon on the DUT -- stock PYTHON vs an injected RUST xcvrd-rs -- plus the
#   supervisor state (RUNNING/…, pid, uptime), the actually-running process
#   image, and the inject/backup markers. Read-only: changes nothing. Handy to
#   confirm an inject took effect, or that a run restored the Python xcvrd.
#     ./setup-sonic-testbed.sh xcvrd_status
# ---------------------------------------------------------------------------
xcvrd_status() {
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  local dut="admin@$DUT_IP"
  local dutdir ctl
  dutdir="$(_dut_dir)" \
    || die "DUT helper scripts not found — expected recodeAgent/tools/dut next to $SCRIPT_DIR/$(basename "$0")"
  ctl="$dutdir/rust_xcvrd_ctl.sh"
  log "xcvrd status on $DUT (pmon)"
  # Ship the (read-only) control script and run its status verb on the DUT --
  # same proven host->mgmt->ssh->pmon path used by inject/restore.
  docker cp "$ctl" "$MGMT_CONTAINER:/tmp/rust_xcvrd_ctl.sh" || die "docker cp ctl -> mgmt failed"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp scp $sshopt /tmp/rust_xcvrd_ctl.sh $dut:/home/admin/rust_xcvrd_ctl.sh" \
    || die "failed to copy control script to DUT"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp ssh $sshopt $dut 'bash /home/admin/rust_xcvrd_ctl.sh status'" \
    || die "failed to query xcvrd status on $DUT"
}
xcvrd_info() { xcvrd_status "$@"; }

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

  # ------------------------------------------------------------------------
  # Ports carrying a SPECIAL (non-uniform) emulator module are EXCLUDED from the
  # graph below. The sonic-mgmt platform suite derives the ports it tests from
  # this graph -- test_sfputil.py iterates conn_graph_facts' dev_conn, and
  # api/test_sfp.py builds sfp_test_port_indices from
  # conn_graph_facts["device_conn"] -- and it assumes every port is a uniform,
  # fully-featured CMIS module. The special modules deliberately are not:
  #
  #   idx10 SFF-8636   -> Sff8636Api has no reset() (sfputil reset / api test_reset)
  #   idx11 400G-ZR    -> coherent VDM thresholds the emulator does not serve
  #   idx13 flat mem   -> no upper pages: no laser-temp thresholds, no per-channel
  #                       tx disable, module reports ModuleLowPwr
  #   idx14 multi-app  -> app-selection module
  #
  # Leaving them in the graph made 6 sonic-mgmt tests fail for reasons that have
  # nothing to do with xcvrd. Excluding them at the GRAPH level (rather than
  # deselecting whole tests, or toggling the emulator config between suites)
  # keeps every assertion running on the remaining uniform ports, needs no DUT
  # or emulator mutation, and leaves no state to restore. xcvrd-tests is
  # unaffected: it talks to the emulator gRPC and STATE_DB directly and never
  # reads this graph, so the special modules stay fully covered there.
  #
  # The index list comes from the DEPLOYED-specials marker the emulator phase
  # writes, so the exclusions always describe the modules actually on the DUT.
  # Falling back to gen_emu_config.py --list-special (which only reflects the
  # current EMU_NO_SPECIAL) would be wrong whenever another phase re-deployed in
  # a different mode -- e.g. xcvrd_tests provisions the special modules, and a
  # later run inheriting EMU_NO_SPECIAL=1 would be told "no specials" and leave
  # them in the graph. Port<->index mapping is Ethernet(4*idx), verified on the
  # DUT: Ethernet40 is the SFF-8636 module and Ethernet44 advertises 400GBASE-ZR.
  # ------------------------------------------------------------------------
  local gen="$SCRIPT_DIR/emu-deploy/gen_emu_config.py"
  local special_idx="" special_ports=""
  if special_idx="$(_emu_deployed_specials)"; then
    log "  special modules deployed on $DUT: [${special_idx:-none}]"
  elif [ -f "$gen" ]; then
    special_idx="$(python3 "$gen" --list-special 2>/dev/null)" || special_idx=""
    warn "no deployed-specials marker on $DUT (emulator not deployed yet?) — falling back to EMU_NO_SPECIAL=${EMU_NO_SPECIAL:-unset} => [${special_idx:-none}]"
  else
    warn "gen_emu_config.py not found at $gen -- cannot exclude special-module ports"
  fi
  local i
  for i in $special_idx; do
    special_ports="$special_ports Ethernet$((i * 4))"
  done

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

  cat > "$tmp/links.raw" <<'CSV'
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

  # Strip the special-module links. Match ",EthernetNN," (with BOTH commas) so a
  # short port name can never match a longer one -- ",Ethernet4," must not delete
  # the Ethernet40/44/48 rows.
  if [ -n "$special_ports" ]; then
    local -a gv=()
    local p
    for p in $special_ports; do gv+=(-e ",${p},"); done
    grep -vF "${gv[@]}" "$tmp/links.raw" > "$tmp/sonic_vlab_links.csv"
    log "  excluding special-module ports from the graph:${special_ports}"
    log "  sonic-mgmt will test $(( $(grep -c '^vlab-01,' "$tmp/sonic_vlab_links.csv") )) of $(grep -c '^vlab-01,' "$tmp/links.raw") $DUT ports"
  else
    cp "$tmp/links.raw" "$tmp/sonic_vlab_links.csv"
    log "  no special modules reported -- graph keeps every port"
  fi

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
# ---------------------------------------------------------------------------
# Deployed-specials marker.
#
# The connection graph must exclude the special (non-uniform) module ports, but
# "which ports are special" is a property of what is ACTUALLY DEPLOYED, not of
# the EMU_NO_SPECIAL value that happens to be set when a test phase runs. Those
# two disagree as soon as one phase re-deploys in a different mode -- xcvrd_tests
# provisions the special modules, so a later transceiver_tests_all inheriting the
# default EMU_NO_SPECIAL=1 would ask gen_emu_config.py for the special list, be
# told "none", leave those ports in the graph, and resurrect exactly the failures
# the graph exclusion was added to remove.
#
# So the emulator deploy records what it deployed, on the DUT (the thing that
# actually holds the modules), and the graph reads that back. The file always has
# a `SPECIALS=` line so an empty list (a uniform testbed) is distinguishable from
# "no marker" (never deployed / unreachable), which must fall back rather than be
# read as "no specials".
# ---------------------------------------------------------------------------
EMU_SPECIALS_MARKER="${EMU_SPECIALS_MARKER:-/home/admin/.emu_specials}"

_emu_write_specials_marker() {
  local gen="$SCRIPT_DIR/emu-deploy/gen_emu_config.py" idx=""
  [ -f "$gen" ] && idx="$(python3 "$gen" --list-special 2>/dev/null)"
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  if docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
       "$sshp ssh $sshopt admin@$DUT_IP 'printf \"SPECIALS=%s\n\" \"$idx\" > $EMU_SPECIALS_MARKER'" >/dev/null 2>&1; then
    log "  recorded deployed special modules: [${idx:-none}]"
  else
    warn "could not record the deployed-specials marker on $DUT — the connection graph will fall back to EMU_NO_SPECIAL"
  fi
}

# Echo the deployed special indices; return non-zero when no marker is readable.
_emu_deployed_specials() {
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10'
  local out
  out="$(docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
        "$sshp ssh $sshopt admin@$DUT_IP 'cat $EMU_SPECIALS_MARKER 2>/dev/null'" 2>/dev/null \
        | tr -d '\r' | grep '^SPECIALS=')" || return 1
  [ -n "$out" ] || return 1
  printf '%s\n' "${out#SPECIALS=}"
}

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
  EMU_TEST_HOOKS="$EMU_TEST_HOOKS" \
    bash "$EMU_DEPLOY_DIR/ship_and_deploy.sh" "$EMU_BUNDLE" "$EMU_IMAGE_TAR" \
    || die "ship_and_deploy.sh failed"
  _emu_write_specials_marker
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
  _run_pytest "platform_tests/test_xcvr_info_in_db.py"
}

# emulator_e2e: one-shot — deploy the emulator then run the target test.
emulator_e2e() {
  emulator
  transceiver_emu_test "$@"
}

# ---------------------------------------------------------------------------
# hotplug_test: verify xcvrd reacts to a transceiver hot-unplug in the emulator.
#   Ships emu-deploy/xcvrd_hotplug_check.sh to the DUT (via the mgmt container)
#   and runs it: unplug a module -> assert TRANSCEIVER_INFO is cleared from
#   STATE_DB -> replug -> assert it is restored. Requires the emulator to be
#   deployed first (run `emulator`). Optional arg: the port to test.
#     ./setup-sonic-testbed.sh hotplug_test            # default Ethernet100
#     ./setup-sonic-testbed.sh hotplug_test Ethernet40
# ---------------------------------------------------------------------------
hotplug_test() {
  local port="${1:-Ethernet100}"
  local script="$EMU_DEPLOY_DIR/xcvrd_hotplug_check.sh"
  [ -f "$script" ] || die "hotplug check script not found at $script — run from a full sonic-develop checkout"
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  local dut="admin@$DUT_IP"
  log "Running xcvrd hotplug check on $DUT (port $port)"
  docker cp "$script" "$MGMT_CONTAINER:/tmp/xcvrd_hotplug_check.sh"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp scp $sshopt /tmp/xcvrd_hotplug_check.sh $dut:/home/admin/xcvrd_hotplug_check.sh" \
    || die "failed to copy hotplug check to DUT"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp ssh $sshopt $dut 'bash /home/admin/xcvrd_hotplug_check.sh $port'" \
    || die "hotplug check FAILED for $port"
  ok "hotplug check passed — xcvrd cleared+restored $port on unplug/replug"
}

# ---------------------------------------------------------------------------
# xcvrd_tests: ship dev/xcvrd-tests/ to the DUT and run the pytest black-box
#   suite there (the DUT has the emulator gRPC, sonic-db-cli and pmon locally).
#   Extra args after `--` are passed through to pytest, e.g.
#     ./setup-sonic-testbed.sh xcvrd_tests -- -m "not slow"
#   Requires the emulator deployed (run `emulator`); presence tests also need the
#   emulator image built from XCVR_EMU_BRANCH=fix/read-honor-presence.
#
#   PRESTEP: this suite NEEDS the special (non-uniform) modules --
#   tests/test_sff8636.py, test_pm.py, test_flat_memory.py and
#   test_app_select.py each depend on one -- but the testbed defaults to uniform
#   CMIS so the sonic-mgmt suites stay green (see EMU_NO_SPECIAL). So re-deploy
#   the emulator with EMU_NO_SPECIAL=0 first. It is skipped when the special
#   modules are already present, so back-to-back runs don't pay for a redeploy;
#   SKIP_EMU_PRESTEP=1 forces it off entirely.
# ---------------------------------------------------------------------------
_xcvrd_tests_prestep() {
  if [ "${SKIP_EMU_PRESTEP:-0}" = "1" ]; then
    log "SKIP_EMU_PRESTEP=1 -> not re-deploying the emulator (assuming special modules are present)"
    return 0
  fi
  # Cheap idempotency check: idx10 (Ethernet40) is the SFF-8636 module, so a
  # non-CMIS type there means the special modules are already deployed. Probing
  # STATE_DB is far cheaper than an unconditional redeploy (~minutes).
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  local t
  t="$(docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
        "$sshp ssh $sshopt admin@$DUT_IP 'redis-cli -n 6 hget \"TRANSCEIVER_INFO|Ethernet40\" type'" 2>/dev/null \
        | tr -d '\r')"
  case "$t" in
    *QSFP28*)
      log "Special modules already deployed (Ethernet40=$t) — skipping emulator re-deploy"
      return 0 ;;
  esac
  log "xcvrd-tests needs the special modules — re-deploying the emulator with EMU_NO_SPECIAL=0"
  EMU_NO_SPECIAL=0 emulator || die "emulator re-deploy (EMU_NO_SPECIAL=0) FAILED — xcvrd-tests would skip/fail the special-module tests"
}

xcvrd_tests() {
  local src="$SCRIPT_DIR/xcvrd-tests"
  [ -d "$src" ] || die "xcvrd-tests folder not found at $src — run from a full sonic-develop checkout"
  local sshp="sshpass -p $DUT_PASS"
  local sshopt='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=25'
  local dut="admin@$DUT_IP"

  _xcvrd_tests_prestep

  log "Packaging xcvrd-tests and shipping to $DUT"
  local tar=/tmp/xcvrd-tests.tar.gz
  # Exclude local build artifacts so we ship a clean tree.
  tar czf "$tar" -C "$src/.." --exclude='xcvrd-tests/.pydeps' \
      --exclude='xcvrd-tests/results.xml' --exclude='xcvrd-tests/**/__pycache__' \
      xcvrd-tests
  docker cp "$tar" "$MGMT_CONTAINER:/tmp/xcvrd-tests.tar.gz"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp scp $sshopt /tmp/xcvrd-tests.tar.gz $dut:/home/admin/xcvrd-tests.tar.gz" \
    || die "failed to copy xcvrd-tests to DUT"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp ssh $sshopt $dut 'rm -rf /home/admin/xcvrd-tests && tar xzf /home/admin/xcvrd-tests.tar.gz -C /home/admin && chmod +x /home/admin/xcvrd-tests/run.sh'" \
    || die "failed to unpack xcvrd-tests on DUT"

  log "Running pytest black-box suite on $DUT"
  [ "${1:-}" = "--" ] && shift   # allow `xcvrd_tests -- <pytest args>`
  # Encode pytest args (NUL-delimited) so quoted, space-containing args like
  # -m "not slow" survive the docker exec -> ssh -> run.sh nesting intact.
  local args_b64=""
  [ "$#" -gt 0 ] && args_b64="$(printf '%s\0' "$@" | base64 -w0)"
  docker exec --user "$HOST_USER" "$MGMT_CONTAINER" bash -lc \
    "$sshp ssh $sshopt $dut 'PYTEST_ARGS_B64=$args_b64 bash /home/admin/xcvrd-tests/run.sh'" \
    || die "xcvrd black-box tests FAILED"
  ok "xcvrd black-box tests passed"
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

# ---------------------------------------------------------------------------
# Phase registry — the SINGLE SOURCE OF TRUTH for `--help`, argument validation
# and shell completion. One record per line, pipe-delimited:
#
#     <phase>|<arg-hint>|<group>|<description>
#
# Add new user-facing phases HERE. Everything else (help text, the "unknown
# phase" check, `--list-phases`, tab completion) is generated from this table,
# so a new phase is documented and completable the moment it is registered.
# Internal helpers (log/die/tbcli/_rust_* ...) are deliberately absent, which is
# what stops them being invoked from the command line.
# ---------------------------------------------------------------------------
phase_registry() {
  cat <<'REG'
all||setup|Run every phase in order (default when no phase is given)
preflight||setup|Verify KVM/nested-virt, OS version and passwordless sudo
install_prereqs||setup|Install host packages, docker and python deps
setup_storage||setup|Lay out the big-disk storage under the DATA mount point
clone_repo||setup|Clone/refresh the sonic-mgmt repo
setup_mgmt_network||setup|Create the mgmt bridge network for the testbed
download_image||setup|Download the sonic-vs DUT image
setup_container||setup|Start the docker-sonic-mgmt container
setup_ssh||setup|Set up key-based SSH from the container to the vm_host
start_vms||setup|Start the neighbor VMs (see VM_TYPE / NUM_VMS below)
add_topo||setup|Deploy the topology (see TESTBED_NAME below)
deploy_mg||setup|Deploy the minigraph/config to the DUT
verify||setup|Verify the DUT is reachable and BGP sessions are up
inject_conn_graph||setup|Inject the connection graph used by the transceiver tests
smoke_test|[test] [-v]|test|Run the BGP verification test (default bgp/test_bgp_fact.py)
run_pytest|[--rust <folder>] <target>|test|Run ARBITRARY sonic-mgmt pytest targets/args (optionally vs an injected Rust xcvrd)
transceiver_tests|[-v]|test|xcvrd/SFP tests that pass on a vs DUT (green smoke set)
transceiver_tests_all|[-v]|test|Full validated xcvrd/SFP set + the transceiver/eeprom suite (RESET_TESTS=0 skips slow reset tests)
transceiver_eeprom_tests|[-v]|test|Declarative transceiver/eeprom suite (injects inventory, flips asic_type)
transceiver_emu_test||test|Run test_xcvr_info_in_db (needs the emulator deployed)
hotplug_test|[PORT]|test|Unplug/replug a module and assert xcvrd clears+restores it
xcvrd_tests|[-- pytest args]|test|Ship xcvrd-tests/ to the DUT and run the pytest black-box suite there
emulator||emu|Native emulator deploy (bridge sonic_platform, pmon inject, xcvr-emu container)
emulator_revert||emu|Undo the native emulator deploy and restore the stock platform
emulator_e2e||emu|emulator + transceiver_emu_test in one go
transceiver_tests_rust|<folder> [-v]|rust|Build+inject a Rust xcvrd from a recodeAgent folder, run the subset, always restore Python
transceiver_tests_all_rust|<folder> [-v]|rust|Same as transceiver_tests_rust but the FULL validated set
xcvrd_tests_rust|<folder> [-- pytest args]|rust|Run the xcvrd-tests black-box suite against an injected Rust xcvrd
transceiver_tests_noop|[-v]|rust|NEGATIVE CONTROL: inject a no-op xcvrd; STATE_DB tests SHOULD fail
transceiver_tests_all_noop|[-v]|rust|NEGATIVE CONTROL over the full set
xcvrd_status||rust|Report the xcvrd running in pmon: PYTHON vs injected RUST (read-only)
xcvrd_info||rust|Alias for xcvrd_status
remove_topo||teardown|Tear down the topology and stop the VMs
rebuild||teardown|Recover after a /mnt/data wipe (re-lays storage, VMs, topo, emulator)
REG
}

phase_names() { phase_registry | cut -d'|' -f1; }

# Exact-match lookup; used by the dispatcher to reject unknown/internal names.
phase_exists() { phase_names | grep -qxF -- "$1"; }

usage() {
  local b="" d="" n=""
  # Only colourise when stdout is a terminal, so `--help | less` stays clean.
  if [ -t 1 ]; then b=$'\033[1m'; d=$'\033[2m'; n=$'\033[0m'; fi

  cat <<EOF
${b}setup-sonic-testbed.sh${n} — one-shot, idempotent SONiC KVM virtual testbed

${b}USAGE${n}
  ./setup-sonic-testbed.sh [<phase>] [args...]
  ./setup-sonic-testbed.sh --help | --list-phases | --completion bash

  With no phase it runs ${b}all${n} (every setup phase in order). Every phase is
  re-runnable on its own.

EOF

  local group title
  for group in setup test emu rust teardown; do
    case "$group" in
      setup)    title="SETUP PHASES (in the order \`all\` runs them)" ;;
      test)     title="TESTS" ;;
      emu)      title="EMULATOR (xcvr-emu)" ;;
      rust)     title="RUST xcvrd / recodeAgent" ;;
      teardown) title="TEARDOWN & RECOVERY" ;;
    esac
    printf '%s%s%s\n' "$b" "$title" "$n"
    # Render "<phase> <arg-hint>" left-padded, then the description.
    phase_registry | awk -F'|' -v g="$group" -v d="$d" -v n="$n" '
      $3 == g {
        label = $1 (length($2) ? " " $2 : "")
        printf "  %-42s %s%s%s\n", label, d, $4, n
      }'
    printf '\n'
  done

  cat <<EOF
${b}COMMON ENV OVERRIDES${n} ${d}(prefix the command, e.g. VERBOSE=1 ./setup-sonic-testbed.sh ...)${n}
  ${b}VERBOSE${n}=1            Full tracebacks (-rA --tb=long --showlocals -s); same as the -v flag
  ${b}RESET_TESTS${n}=0        Skip the SLOW module-reset tests in transceiver_tests_all
  ${b}DOM_UPDATE_INTERVAL${n}= DOM poll seconds for an injected RUST xcvrd (unset = upstream 60s)
  ${b}TESTBED_NAME${n}=...     conf-name in vtestbed.yaml            (current: $TESTBED_NAME)
  ${b}DUT${n}=...              DUT hostname                          (current: $DUT)
  ${b}DUT_IP${n}=...           DUT mgmt IPv4 as seen from the mgmt ctr (current: $DUT_IP)
  ${b}VM_TYPE${n}=...          vsonic | ceos | csonic | veos         (current: $VM_TYPE)
  ${b}NUM_VMS${n}=...          Neighbor VM count                     (current: $NUM_VMS)
  ${b}EMU_MODULES${n}=...      Present CMIS modules (0..N-1)         (current: $EMU_MODULES)
  ${b}MGMT_CONTAINER${n}=...   sonic-mgmt container name             (current: $MGMT_CONTAINER)
  ${b}DATA${n}=...             Big-disk mount point                  (current: $DATA)

${b}EXAMPLES${n}
  ./setup-sonic-testbed.sh                                  ${d}# full setup, end to end${n}
  ./setup-sonic-testbed.sh emulator_e2e                     ${d}# emulator + its e2e test${n}
  ./setup-sonic-testbed.sh transceiver_tests_all -v         ${d}# full xcvrd set, verbose${n}
  ./setup-sonic-testbed.sh xcvrd_tests -- -m "not slow"     ${d}# skip the ~60s DOM tests${n}
  ./setup-sonic-testbed.sh xcvrd_tests -- -k test_dom       ${d}# one module's tests${n}
  ./setup-sonic-testbed.sh run_pytest platform_tests/sfp/test_sfpshow.py    ${d}# one file${n}
  ./setup-sonic-testbed.sh run_pytest platform_tests/api/test_sfp.py -k lpmode
  ./setup-sonic-testbed.sh run_pytest --rust recodeAgent/results/result_4 \\
      platform_tests/test_xcvr_info_in_db.py                ${d}# one test vs Rust xcvrd${n}
  ./setup-sonic-testbed.sh xcvrd_tests_rust recodeAgent/results/result_4
                                                            ${d}# best Rust gate: 105 xcvrd tests${n}
  DOM_UPDATE_INTERVAL=5 ./setup-sonic-testbed.sh transceiver_tests_all_rust \\
      recodeAgent/results/result_4                          ${d}# faster DOM cadence${n}
  ./setup-sonic-testbed.sh transceiver_tests_rust recodeAgent/results/result_4
  ./setup-sonic-testbed.sh hotplug_test Ethernet40
  ./setup-sonic-testbed.sh xcvrd_status                     ${d}# which xcvrd is live?${n}

${b}TAB COMPLETION${n}
  eval "\$(./setup-sonic-testbed.sh --completion bash)"       ${d}# this shell${n}
  ./setup-sonic-testbed.sh --completion bash | sudo tee \\
      /etc/bash_completion.d/setup-sonic-testbed >/dev/null  ${d}# persistent${n}
EOF
}

# ---------------------------------------------------------------------------
# print_completion: emit a bash completion script on stdout.
#   The completion asks THIS script for its phase list at runtime
#   (`--list-phases`), so newly registered phases complete without the user
#   re-installing anything. Falls back silently if the script is not executable.
# ---------------------------------------------------------------------------
print_completion() {
  local shell="${1:-bash}"
  case "$shell" in
    bash) ;;
    *) die "unsupported completion shell '$shell' (only 'bash' is supported)" ;;
  esac

  cat <<'COMPLETION'
# bash completion for setup-sonic-testbed.sh
# Install:  eval "$(./setup-sonic-testbed.sh --completion bash)"
_setup_sonic_testbed() {
  local cur prev script phase
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]:-}"
  # Guard index 0 so this is safe at the first word (and under `set -u`).
  if [ "$COMP_CWORD" -gt 0 ]; then prev="${COMP_WORDS[COMP_CWORD-1]:-}"; else prev=""; fi
  script="${COMP_WORDS[0]}"
  phase="${COMP_WORDS[1]:-}"

  # Position 1: the phase name (or a top-level flag).
  if [ "$COMP_CWORD" -eq 1 ]; then
    local phases
    phases="$("$script" --list-phases 2>/dev/null)"
    COMPREPLY=($(compgen -W "$phases --help -h --list-phases --completion" -- "$cur"))
    return 0
  fi

  # --completion <shell>
  if [ "$phase" = "--completion" ]; then
    COMPREPLY=($(compgen -W "bash" -- "$cur"))
    return 0
  fi

  case "$phase" in
    transceiver_tests_rust|transceiver_tests_all_rust|xcvrd_tests_rust)
      # Position 2 is the recodeAgent pipeline folder; after that, flags.
      if [ "$COMP_CWORD" -eq 2 ]; then
        local dirs
        dirs="$(compgen -d -- "$cur")"
        # Surface recodeAgent result/pipeline folders even before the user types
        # the path prefix, since those are what this phase actually expects.
        if [ -z "$cur" ]; then
          dirs="$dirs $(compgen -d -- recodeAgent/results/ 2>/dev/null)"
          dirs="$dirs $(compgen -d -- recodeAgent/pipeline_run 2>/dev/null)"
        fi
        COMPREPLY=($(compgen -W "$dirs" -- "$cur"))
      elif [ "$phase" = "xcvrd_tests_rust" ]; then
        # Everything after `--` is forwarded to pytest on the DUT.
        COMPREPLY=($(compgen -W "-- -k -m -x -q --collect-only" -- "$cur"))
      else
        COMPREPLY=($(compgen -W "-v --verbose" -- "$cur"))
      fi
      return 0
      ;;
    hotplug_test)
      # The vs testbed exposes Ethernet0..Ethernet124 in steps of 4.
      local ports="" i
      for ((i = 0; i <= 124; i += 4)); do ports="$ports Ethernet$i"; done
      COMPREPLY=($(compgen -W "$ports" -- "$cur"))
      return 0
      ;;
    xcvrd_tests)
      # Everything after `--` is forwarded to pytest on the DUT.
      COMPREPLY=($(compgen -W "-- -k -m -x -q --collect-only --capture-golden" -- "$cur"))
      return 0
      ;;
    run_pytest)
      # Complete sonic-mgmt test paths (relative to tests/) plus common pytest
      # flags. The suite lives in the mgmt container, so offer the well-known
      # roots rather than trying to stat a path that is not on this host.
      # After --rust, complete recodeAgent pipeline folders on THIS host.
      if [ "$prev" = "--rust" ]; then
        local rdirs
        rdirs="$(compgen -d -- "$cur")"
        if [ -z "$cur" ]; then
          rdirs="$rdirs $(compgen -d -- recodeAgent/results/ 2>/dev/null)"
        fi
        COMPREPLY=($(compgen -W "$rdirs" -- "$cur"))
      elif [[ "$cur" == -* ]]; then
        COMPREPLY=($(compgen -W "--rust --dom_update_interval -k -m -x -q -s --collect-only --durations=25 --tb=short --tb=long --lf --sw" -- "$cur"))
      else
        COMPREPLY=($(compgen -W "platform_tests/ platform_tests/test_xcvr_info_in_db.py \
          platform_tests/sfp/test_sfpshow.py platform_tests/sfp/test_sfputil.py \
          platform_tests/api/test_sfp.py transceiver/ transceiver/eeprom/ bgp/test_bgp_fact.py" -- "$cur"))
      fi
      return 0
      ;;
    smoke_test|transceiver_tests|transceiver_tests_all|transceiver_eeprom_tests| \
    transceiver_tests_noop|transceiver_tests_all_noop)
      COMPREPLY=($(compgen -W "-v --verbose" -- "$cur"))
      return 0
      ;;
  esac
  return 0
}
complete -o default -F _setup_sonic_testbed setup-sonic-testbed.sh ./setup-sonic-testbed.sh
COMPLETION
}

# Suggest close matches for a typo'd phase. Tries, in order: substring of a real
# phase, real phase contained in the input, then a shared-prefix match (which is
# what catches transposition typos like "transciever_tests").
suggest_phase() {
  local bad="$1" hits
  hits="$(phase_names | grep -iF -- "$bad" 2>/dev/null)"
  if [ -z "$hits" ]; then
    hits="$(phase_names | while read -r p; do
              case "$bad" in *"$p"*) echo "$p" ;; esac
            done)"
  fi
  if [ -z "$hits" ]; then
    local prefix="${bad:0:5}"
    [ -n "$prefix" ] && hits="$(phase_names | grep -i "^$(printf '%s' "$prefix" | sed 's/[^a-zA-Z0-9_]/./g')" 2>/dev/null)"
  fi
  [ -n "$hits" ] && { echo "Did you mean:" >&2; echo "$hits" | sed 's/^/  /' >&2; }
  return 0
}

# ---------------------------------------------------------------------------
# main: dispatch to a single named phase (default: all).
#   The phase name is validated against phase_registry() FIRST. Previously the
#   argument was executed directly, so a typo produced a bare "command not
#   found" and any shell command or internal helper was invocable.
# ---------------------------------------------------------------------------
main() {
  case "${1:-}" in
    -h|--help|help) usage; exit 0 ;;
    --list-phases)  phase_names; exit 0 ;;
    --completion)   print_completion "${2:-bash}"; exit 0 ;;
  esac

  local phase="${1:-all}"
  if ! phase_exists "$phase"; then
    echo -e "\033[1;31m[fail]\033[0m unknown phase: '$phase'" >&2
    suggest_phase "$phase"
    echo "Run './setup-sonic-testbed.sh --help' for the full list of phases." >&2
    exit 2
  fi

  "$phase" "${@:2}"
}

main "$@"

