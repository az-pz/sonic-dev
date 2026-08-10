#!/bin/bash
# lib_remote.sh -- transport shim so the tools/*.sh wrappers can stage + execute
# either on a REMOTE sonic-dev host (over ssh, the default) or DIRECTLY on the
# local box when the pipeline is being run ON sonic-dev itself.
#
# Sourced by the wrapper scripts. It exposes four primitives that replace the raw
# ssh/tar/scp calls:
#     r_run "<cmd>"                 run a shell command on the target
#     r_put_dir <src> <dest>        stage a directory tree (excludes target/)
#     r_put_files <dest> <file>...  copy files into a target directory
#     r_get <src> <dest>            fetch a file from the target
#
# Mode selection (RECODE_RUN_MODE):
#   remote (default) -- ssh/scp to $RECODE_SSH_HOST (default "sonic-dev"); today's
#                       behavior, unchanged.
#   local            -- no ssh; operate on the local filesystem. Auto-selected when
#                       RECODE_SSH_HOST is localhost/127.0.0.1, else set it yourself
#                       (e.g. `RECODE_RUN_MODE=local bash tools/validate_on_dut.sh M1`).
# The inner DUT hops (tools/dut/*.sh: mgmt -> admin@10.250.0.101 -> pmon) are
# unaffected -- they always run on the sonic-dev host regardless of this mode.

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

# r_run "<shell command>" -- execute on the target. In local mode the command runs
# in a plain bash -c (the same `~`/$HOME as the remote sonic user, since local mode
# means we ARE that user on sonic-dev).
r_run() {
  if [ "$RECODE_RUN_MODE" = local ]; then bash -c "$1"; else ssh "$RECODE_SSH_HOST" "$1"; fi
}

# r_put_dir <local_src> <target_dir> -- stage a directory tree, excluding target/.
# Uses a tar stream in both modes so the exclude semantics (and sonic-dev's cargo
# target/ cache) are identical.
r_put_dir() {
  local src="$1" dest="$2"
  if [ "$RECODE_RUN_MODE" = local ]; then
    dest="$(_r_lpath "$dest")"
    mkdir -p "$dest"
    tar -C "$src" --exclude target -cf - . | tar -C "$dest" -xf -
  else
    ssh "$RECODE_SSH_HOST" "mkdir -p $dest"
    tar -C "$src" --exclude target -cf - . | ssh "$RECODE_SSH_HOST" "tar -C $dest -xf -"
  fi
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
    scp -q "$@" "$RECODE_SSH_HOST:$dest"
  fi
}

# r_get <target_src_file> <local_dest> -- fetch a file from the target.
r_get() {
  if [ "$RECODE_RUN_MODE" = local ]; then cp "$(_r_lpath "$1")" "$(_r_lpath "$2")"; else scp -q "$RECODE_SSH_HOST:$1" "$2"; fi
}
