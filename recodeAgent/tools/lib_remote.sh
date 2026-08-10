#!/bin/bash
# lib_remote.sh -- transport shim so the tools/*.sh wrappers can stage + execute
# either on a REMOTE sonic-dev host (over ssh, the default) or DIRECTLY on the
# local box when the pipeline is being run ON sonic-dev itself.
#
# Sourced by the wrapper scripts. It exposes four primitives that replace the raw
# ssh/tar/scp calls:
#     r_run "<cmd>"                 run a shell command on the target
#     r_put_dir <src> <dest> [ex...]  stage a directory tree (excludes target/)
#     r_put_files <dest> <file>...  copy files into a target directory
#     r_get <src> <dest>            fetch a file from the target
#     r_resolve_crate <dir>         echo the buildable cargo workspace for <dir>
#
# Mode selection (RECODE_RUN_MODE):
#   remote (default) -- ssh/scp to $RECODE_SSH_HOST (default "sonic-dev"); today's
#                       behavior, unchanged.
#   local            -- no ssh; operate on the local filesystem. Auto-selected when
#                       RECODE_SSH_HOST is localhost/127.0.0.1, else set it yourself
#                       (e.g. `RECODE_RUN_MODE=local bash tools/validate_on_dut.sh M1`).
# The inner DUT hops (tools/dut/*.sh: mgmt -> admin@10.250.0.101 -> pmon) are
# unaffected -- they always run on the sonic-dev host regardless of this mode.
#
# Staging root: every wrapper stages into "~/recode/..." on the TARGET (never an
# absolute /home/<user> path), so the pipeline works for any account on any host --
# the sonic-dev "sonic" user over ssh, or e.g. "azureuser" in local mode. The
# tools/dut/*.sh scripts resolve the same root as "$HOME/recode".

: "${RECODE_SSH_HOST:=sonic-dev}"

if [ -z "${RECODE_RUN_MODE:-}" ]; then
  case "$RECODE_SSH_HOST" in
    localhost|127.0.0.1|::1) RECODE_RUN_MODE=local ;;
    *)                       RECODE_RUN_MODE=remote ;;
  esac
fi

# A short human label for log lines, e.g. "sonic-dev (remote)" or "local".
r_where() {
  if [ "$RECODE_RUN_MODE" = local ]; then printf 'local'; else printf '%s (remote)' "$RECODE_SSH_HOST"; fi
}

# Expand a leading "~/" to "$HOME/" for local-mode filesystem ops (a quoted "~/x"
# does NOT tilde-expand). Remote mode leaves paths untouched so "~" expands on the
# far side inside the ssh command string.
_r_lpath() { case "$1" in "~/"*) printf '%s' "$HOME/${1#\~/}" ;; *) printf '%s' "$1" ;; esac; }

# Turn a target-side path into one scp can use. scp/sftp does not reliably expand a
# leading "~/" in "host:~/path", so drop it -- a relative scp path is already taken
# relative to the remote user's home, which is exactly what "~/" meant.
_r_rpath() { case "$1" in "~/"*) printf '%s' "${1#\~/}" ;; *) printf '%s' "$1" ;; esac; }

# r_run "<shell command>" -- execute on the target. In local mode the command runs
# in a plain bash -c (the same `~`/$HOME as the remote sonic user, since local mode
# means we ARE that user on sonic-dev).
r_run() {
  if [ "$RECODE_RUN_MODE" = local ]; then bash -c "$1"; else ssh "$RECODE_SSH_HOST" "$1"; fi
}

# r_put_dir <local_src> <target_dir> [extra_exclude...] -- stage a directory tree.
# target/ (the cargo build cache) is always excluded; any extra arguments are
# passed to tar as additional --exclude patterns, e.g.
#     r_put_dir "$TESTS_DIR" "~/recode/xcvrd-tests" .pydeps results.xml __pycache__
# Uses a tar stream in both modes so the exclude semantics (and sonic-dev's cargo
# target/ cache) are identical.
#
# The destination is CLEANED first (everything except the excluded names is
# removed) so staging is a mirror, not a union. Without this, pointing
# RECODE_CRATE_DIR at the wrong directory quietly blends the new tree into
# whatever was staged before -- e.g. a pipeline result folder extracted over a
# previous crate leaves the OLD Cargo.toml/xcvrd-rs at the top level, so
# build_crate.sh happily builds the stale sources and the run grades the wrong
# binary. The excluded names are preserved precisely so the cargo target/ cache
# (and .pydeps) survive re-staging.
r_put_dir() {
  local src="$1" dest="$2"; shift 2
  local ex=(--exclude target) keep=(target) p
  for p in "$@"; do ex+=(--exclude "$p"); keep+=("$p"); done

  # Guard: never clean a path that isn't under the staging root we own.
  case "$dest" in
    "~/recode/"*|"$HOME/recode/"*) : ;;
    *) echo "r_put_dir: refusing to clean '$dest' (must be under ~/recode/)" >&2; return 1 ;;
  esac

  # find(1) expression that removes every top-level entry except the preserved
  # names. Paths are quoted per mode: local gets an absolute $HOME path, remote
  # gets a home-relative path (a single-quoted "~/" would NOT tilde-expand over
  # ssh, and ssh's cwd is the remote home anyway).
  local prune="" q
  for q in "${keep[@]}"; do prune="$prune ! -name '$q'"; done

  if [ "$RECODE_RUN_MODE" = local ]; then
    dest="$(_r_lpath "$dest")"
    bash -c "mkdir -p '$dest' && find '$dest' -mindepth 1 -maxdepth 1 $prune -exec rm -rf {} +" || return 1
    tar -C "$src" "${ex[@]}" -cf - . | tar -C "$dest" -xf -
  else
    local rdest; rdest="$(_r_rpath "$dest")"
    ssh "$RECODE_SSH_HOST" "mkdir -p '$rdest' && find '$rdest' -mindepth 1 -maxdepth 1 $prune -exec rm -rf {} +" || return 1
    tar -C "$src" "${ex[@]}" -cf - . | ssh "$RECODE_SSH_HOST" "tar -C '$rdest' -xf -"
  fi
}

# r_resolve_crate <dir> -- echo the buildable cargo workspace for <dir>.
# A crate dir must have Cargo.toml + xcvrd-rs/. recodeAgent pipeline RESULT
# folders (recodeAgent/results/result_N) hold the workspace one level down in
# crate/ alongside report.json, logs/ etc., and pointing RECODE_CRATE_DIR at the
# result folder is an easy mistake -- so transparently descend into crate/ when
# that is what was given. Anything else is a hard error: silently staging a
# non-crate directory is what let a stale binary get graded.
r_resolve_crate() {
  local d="$1"
  if [ -f "$d/Cargo.toml" ] && [ -d "$d/xcvrd-rs" ]; then
    printf '%s' "$d"; return 0
  fi
  if [ -f "$d/crate/Cargo.toml" ] && [ -d "$d/crate/xcvrd-rs" ]; then
    echo "[recode] '$d' is a pipeline result folder; using its crate/ subdir" >&2
    printf '%s' "$d/crate"; return 0
  fi
  echo "[recode] '$d' is not a buildable crate (need Cargo.toml + xcvrd-rs/, or a crate/ subdir with them)" >&2
  return 1
}

# r_put_files <target_dir> <file>... -- copy one or more local files into a dir on
# the target (the dir is created if missing).
r_put_files() {
  local dest="$1"; shift
  if [ "$RECODE_RUN_MODE" = local ]; then
    dest="$(_r_lpath "$dest")"
    mkdir -p "$dest"; cp "$@" "$dest"
  else
    ssh "$RECODE_SSH_HOST" "mkdir -p $dest"
    scp -q "$@" "$RECODE_SSH_HOST:$(_r_rpath "$dest")"
  fi
}

# r_get <target_src_file> <local_dest> -- fetch a file from the target.
r_get() {
  if [ "$RECODE_RUN_MODE" = local ]; then cp "$(_r_lpath "$1")" "$(_r_lpath "$2")"; else scp -q "$RECODE_SSH_HOST:$(_r_rpath "$1")" "$2"; fi
}
