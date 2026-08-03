#!/usr/bin/env bash
# Fetch the prebuilt swss-common Debian packages (libswsscommon + python3-swsscommon
# and friends) from the public SONiC Azure DevOps pipeline (Azure.sonic-swss-common,
# definition id 9), for use with the "real swsscommon" image variant.
#
#   ./fetch-swsscommon.sh [distro] [arch]
#     distro: bookworm (default) | trixie
#     arch:   amd64 | arm64   (default: this machine's docker architecture)
#
# Debs land in dev/vendor/debs/<distro>-<arch>/. This is dev tooling only;
# nothing is written into the cloned repos.
set -euo pipefail

DISTRO="${1:-bookworm}"
# Default arch to the running Docker engine's architecture so the debs match
# the base image we will build on.
DEFAULT_ARCH="$(docker info --format '{{.Architecture}}' 2>/dev/null || echo amd64)"
case "$DEFAULT_ARCH" in
    aarch64|arm64) DEFAULT_ARCH=arm64 ;;
    x86_64|amd64)  DEFAULT_ARCH=amd64 ;;
esac
ARCH="${2:-$DEFAULT_ARCH}"

# Azure artifact naming: amd64 has no suffix, other arches append ".<arch>".
if [ "$ARCH" = "amd64" ]; then
    ARTIFACT="sonic-swss-common-$DISTRO"
else
    ARTIFACT="sonic-swss-common-$DISTRO.$ARCH"
fi

API="https://dev.azure.com/mssonic/build/_apis/build"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$SCRIPT_DIR/vendor/debs/$DISTRO-$ARCH"

echo ">> distro=$DISTRO arch=$ARCH artifact=$ARTIFACT"
echo ">> Finding latest succeeded master build of Azure.sonic-swss-common ..."
BID=$(curl -fsSL "$API/builds?definitions=9&branchName=refs/heads/master&statusFilter=completed&resultFilter=succeeded&\$top=1&api-version=7.0" \
  | python -c 'import sys,json;print(json.load(sys.stdin)["value"][0]["id"])')
echo "   build id: $BID"

echo ">> Resolving artifact download URL for '$ARTIFACT' ..."
URL=$(curl -fsSL "$API/builds/$BID/artifacts?artifactName=$ARTIFACT&api-version=7.0" \
  | python -c 'import sys,json;print(json.load(sys.stdin)["resource"]["downloadUrl"])')

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
echo ">> Downloading artifact zip ..."
curl -fsSL -o "$TMP/artifact.zip" "$URL"

echo ">> Extracting *.deb ..."
mkdir -p "$OUT"
rm -f "$OUT"/*.deb 2>/dev/null || true
( cd "$TMP" && unzip -q artifact.zip )
# Copy the runtime debs (skip debug symbols and the -dev package).
find "$TMP/$ARTIFACT" -name '*.deb' \
     ! -name '*dbgsym*' ! -name '*-dev_*' \
     -exec cp {} "$OUT/" \;

echo ">> Done. Debs in $OUT:"
ls -1 "$OUT"
