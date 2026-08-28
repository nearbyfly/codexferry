#!/usr/bin/env bash
# One-command release upgrade for the local install.
#
# Downloads a release tarball from GitHub (or takes a local one via
# --from-file, e.g. for offline installs or testing), installs the binary
# as a versioned copy in ~/bin, flips the codexferry symlink, restarts the
# systemd user service, and VERIFIES the running process actually switched
# (healthz + /proc/<pid>/exe pointing at the new file). On verification
# failure the previous version is restored automatically.
#
# Usage:
#   scripts/upgrade.sh v0.1.4                       # download + install + restart
#   scripts/upgrade.sh latest                       # newest release
#   scripts/upgrade.sh --from-file codexferry-vX.Y.Z-x86_64-....tar.gz
#   scripts/upgrade.sh --rollback v0.1.3            # flip back to an installed copy
#
# Requires: gh (authenticated) for downloads. The systemd user service is
# restarted only when currently active; override its name/port via
# CODEXFERRY_SERVICE / CODEXFERRY_HEALTHZ_URL, and the install dir via
# CODEXFERRY_BIN_DIR.
set -euo pipefail

REPO=nearbyfly/codexferry
BIN_DIR="${CODEXFERRY_BIN_DIR:-$HOME/bin}"
SERVICE="${CODEXFERRY_SERVICE:-codexferry}"
HEALTHZ="${CODEXFERRY_HEALTHZ_URL:-http://127.0.0.1:8787/healthz}"

# Logs go to stderr: download_artifact's output is command-substituted,
# and stdout must carry only the artifact path.
log()  { printf '[upgrade] %s\n' "$*" >&2; }
fail() { printf '[FAIL] %s\n' "$*" >&2; exit 1; }
usage() { sed -n '3,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0; }

MODE=install
FROM_FILE=""
TAG=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --rollback) MODE=rollback; shift; TAG="${1:-}"; [[ -n "$TAG" ]] || fail "--rollback needs a version (e.g. v0.1.3)"; shift ;;
    --from-file) shift; FROM_FILE="${1:-}"; [[ -n "$FROM_FILE" ]] || fail "--from-file needs a path"; shift ;;
    -h|--help) usage ;;
    v*|latest) [[ -z "$TAG" ]] || fail "multiple versions given: $TAG and $1"; TAG="$1"; shift ;;
    *) fail "unknown arg: $1 (see --help)" ;;
  esac
done

service_active() { systemctl --user is-active --quiet "$SERVICE" 2>/dev/null; }

# Restart the daemon and verify the running process is the expected
# versioned copy. Returns 1 (does not exit) so the caller can auto-revert.
restart_and_verify() { # $1 = version without leading v
  local ver="$1" pid
  if ! service_active; then
    log "service '$SERVICE' not active - skipped restart; start it manually"
    return 0
  fi
  systemctl --user restart "$SERVICE"
  local _
  for _ in $(seq 1 50); do
    curl -sf "$HEALTHZ" >/dev/null 2>&1 && break
    sleep 0.2
  done
  if ! curl -sf "$HEALTHZ" >/dev/null; then
    log "daemon did not become healthy at $HEALTHZ after restart"
    return 1
  fi
  pid="$(pgrep -x codexferry | head -1)"
  if [[ -z "$pid" ]]; then
    log "no codexferry process found after restart"
    return 1
  fi
  if ! readlink -f "/proc/$pid/exe" | grep -q "codexferry-$ver\$"; then
    log "running process is not codexferry-$ver: $(readlink -f /proc/$pid/exe)"
    return 1
  fi
  log "verified: daemon healthy on codexferry-$ver (pid $pid)"
}

# Download TAG's tarball + SHA256SUMS into a temp dir; echo the tarball path.
download_artifact() { # $1 = tag (with v)
  local tag="$1" dir tarball
  dir="$(mktemp -d /tmp/codexferry-upgrade.XXXXXX)"
  tarball="codexferry-${tag}-x86_64-unknown-linux-gnu.tar.gz"
  gh release download "$tag" -R "$REPO" -p "$tarball" -O "$dir/$tarball" \
    || fail "download failed for $tarball (releases carry built artifacts since the CI workflow landed)"
  if gh release download "$tag" -R "$REPO" -p SHA256SUMS -O "$dir/SHA256SUMS" 2>/dev/null; then
    (cd "$dir" && grep " ${tarball}\$" SHA256SUMS | sha256sum -c - >/dev/null) \
      || fail "sha256 mismatch for $tarball"
    log "sha256 verified"
  else
    log "WARNING: no SHA256SUMS asset on $tag - skipped checksum verification"
  fi
  echo "$dir/$tarball"
}

# Install a tarball's binary as $BIN_DIR/codexferry-<ver>, after the
# binary's own --version self-check matches. Echoes nothing.
install_from_tarball() { # $1 = tarball path, $2 = version without leading v
  local tarball="$1" ver="$2" extract_dir dest reported
  extract_dir="$(mktemp -d /tmp/codexferry-extract.XXXXXX)"
  tar xzf "$tarball" -C "$extract_dir"
  [[ -x "$extract_dir/codexferry" ]] || fail "tarball contains no ./codexferry executable"
  reported="$("$extract_dir/codexferry" --version | awk '{print $2}')"
  [[ "$reported" == "$ver" ]] \
    || fail "binary self-reports version '$reported', expected '$ver' - refusing to install"
  dest="$BIN_DIR/codexferry-$ver"
  # Install via temp file + rename: cp over a running executable fails with
  # ETXTBSY ("Text file busy"), while rename(2) atomically swaps the
  # directory entry and the running process keeps the old inode (found
  # during a same-version reinstall over the live daemon).
  cp "$extract_dir/codexferry" "$dest.new"
  mv "$dest.new" "$dest"
  log "installed $dest"
}

case "$MODE" in
  rollback)
    ver="${TAG#v}"
    [[ -x "$BIN_DIR/codexferry-$ver" ]] || fail "no installed copy at $BIN_DIR/codexferry-$ver"
    ln -sfn "codexferry-$ver" "$BIN_DIR/codexferry"
    restart_and_verify "$ver" || fail "rollback to $ver failed verification"
    log "rolled back to $ver"
    ;;
  install)
    if [[ -n "$FROM_FILE" ]]; then
      [[ -n "$TAG" ]] || TAG="$(basename "$FROM_FILE" | sed -n 's/^codexferry-\(.*\)-x86_64.*\.tar\.gz$/\1/p')"
      [[ -n "$TAG" ]] || fail "cannot derive version from filename: $(basename "$FROM_FILE")"
      log "installing from local file: $FROM_FILE"
      TARBALL="$FROM_FILE"
    else
      [[ -n "$TAG" ]] || usage
      [[ "$TAG" == "latest" ]] && TAG="$(gh release view -R "$REPO" --json tagName -q .tagName)"
      log "downloading $TAG from $REPO"
      TARBALL="$(download_artifact "$TAG")"
    fi
    ver="${TAG#v}"
    PREV_TARGET="$(readlink "$BIN_DIR/codexferry" 2>/dev/null || true)"
    install_from_tarball "$TARBALL" "$ver"
    ln -sfn "codexferry-$ver" "$BIN_DIR/codexferry"
    if ! restart_and_verify "$ver"; then
      log "verification failed - reverting symlink to ${PREV_TARGET:-<none>}"
      if [[ -n "$PREV_TARGET" ]]; then
        ln -sfn "$PREV_TARGET" "$BIN_DIR/codexferry"
        restart_and_verify "${PREV_TARGET#codexferry-}" || true
      fi
      fail "upgrade to $ver failed verification and was reverted"
    fi
    log "upgrade to $ver complete"
    ;;
esac
