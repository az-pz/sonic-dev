#!/bin/bash
# install_agents.sh -- install the recodeAgent custom-agent profiles where the
# GitHub Copilot CLI discovers them.
#
# The canonical profiles live (version-controlled) in dev/recodeAgent/agents/, but
# the CLI only discovers custom agents from ~/.copilot/agents/ (user level) or a
# repo's .github/agents/. We install to the user-level dir so `copilot --agent
# analyzer|planner|translator|validator` resolves from any working directory.
#
# Idempotent; re-run after editing a profile. Honors COPILOT_HOME.
# Usage: bash tools/install_agents.sh
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$(cd "$HERE/.." && pwd)/agents"
DEST="${COPILOT_HOME:-$HOME/.copilot}/agents"

mkdir -p "$DEST"
n=0
for f in "$SRC"/*.agent.md; do
  [ -e "$f" ] || { echo "[agents] no profiles in $SRC" >&2; exit 1; }
  cp -f "$f" "$DEST/"
  n=$((n+1))
done
echo "[agents] installed $n profile(s) into $DEST:"
ls -1 "$DEST"/*.agent.md
