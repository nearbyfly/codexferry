#!/usr/bin/env bash
# Cut a release of codexferry.
#
# What it does:
#   1. Validates the requested version (vX.Y.Z), working tree state, and branch.
#   2. Bumps Cargo.toml version if --bump-cargo is given (default: skip).
#   3. Cuts release/vX.Y from main, strips docs/ (internal design artifacts -
#      not published to GitHub), commits the bump + strip as one commit.
#   4. Pushes release/vX.Y + tag vX.Y to BOTH `origin` (gitea) and `github`.
#   5. When GITHUB_TOKEN is set, also creates a GitHub Release via the API
#      with the changelog body auto-generated from `git log v<prev>..vX.Y`.
#
# Usage:
#   scripts/release.sh v0.1.1                 # dry-run-ish: bump skipped, tag + push
#   scripts/release.sh v0.1.1 --bump-cargo     # also edit Cargo.toml
#   scripts/release.sh v0.1.1 --dry-run        # do everything except the network pushes
#   scripts/release.sh v0.1.1 --no-gh-release  # skip the GitHub Release API call
#
# Required env:
#   GITHUB_TOKEN (only when creating the GitHub Release; push still works)
# Required git remotes:
#   origin  -> gitea (already configured)
#   github  -> git@github.com:nearbyfly/codexferry.git  (add via `git remote add github ...`)
#
# Cadence: see RELEASE.md (this script is the how, not the when).

set -euo pipefail

# ---------- helpers (match the style of scripts/e2e*.sh) ----------

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
log()  { printf '[release] %s\n' "$*"; }
fail() { printf '[FAIL] %s\n' "$*" >&2; exit 1; }

usage() {
  sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
}

# ---------- arg parsing ----------

VERSION="${1:-}"
[[ -n "$VERSION" && "$VERSION" != "--help" && "$VERSION" != "-h" ]] || usage
# Accept both `v0.1.1` and `0.1.1`; normalize to `v0.1.1`.
[[ "$VERSION" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$ ]] || fail "bad version: '$VERSION' (expected vX.Y.Z or X.Y.Z)"
[[ "$VERSION" == v* ]] || VERSION="v$VERSION"
TAG="$VERSION"

BUMP_CARGO=0
DRY_RUN=0
GH_RELEASE=1
shift
while [[ $# -gt 0 ]]; do
  case "$1" in
    --bump-cargo) BUMP_CARGO=1 ;;
    --dry-run)    DRY_RUN=1 ;;
    --no-gh-release) GH_RELEASE=0 ;;
    -h|--help) usage ;;
    *) fail "unknown arg: $1" ;;
  esac
  shift
done

# ---------- preflight ----------

cd "$REPO_ROOT"

log "preflight: version=$TAG bump=$BUMP_CARGO dry_run=$DRY_RUN gh_release=$GH_RELEASE"

if [[ -n "$(git status --porcelain)" ]]; then
  fail "working tree is not clean - commit/stash first"
fi

CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$CURRENT_BRANCH" != "main" ]]; then
  fail "must be on main (currently on $CURRENT_BRANCH)"
fi

# Remote check: `origin` (gitea) required; `github` required for the push step.
git remote get-url origin  >/dev/null 2>&1 || fail "remote 'origin' not configured"
git remote get-url github >/dev/null 2>&1 || fail "remote 'github' not configured (add via: git remote add github git@github.com:nearbyfly/codexferry.git)"

if git tag --list "$TAG" | grep -q .; then
  fail "tag $TAG already exists locally - bump the version or delete the tag"
fi

RELEASE_BRANCH="release/${TAG#v}"
if git branch --list "$RELEASE_BRANCH" | grep -q .; then
  fail "branch $RELEASE_BRANCH already exists - delete it or bump the version"
fi

# Cargo.toml version sync check (warning, not fatal, when --bump-cargo is off).
CARGO_VER="$(grep '^version = ' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')"
log "preflight: Cargo.toml version is $CARGO_VER"
if [[ "$BUMP_CARGO" -eq 0 && "${TAG#v}" != "$CARGO_VER" ]]; then
  log "WARNING: tag $TAG != Cargo.toml version $CARGO_VER - pass --bump-cargo to sync"
fi

# ---------- cut release branch ----------

log "cutting $RELEASE_BRANCH from main"
git checkout -b "$RELEASE_BRANCH" main

# Strip internal design artifacts from the public mirror.
# `docs/superpowers/{specs,plans}/` is internal workflow, not user-facing.
if [[ -d docs ]]; then
  git rm -rf docs >/dev/null
  STRIPPED_DOCS=1
else
  STRIPPED_DOCS=0
fi

# Cargo.toml version bump + Cargo.lock (lockfile changes with version).
# Combined into the same commit as the docs strip below so we never emit
# an empty "nothing to commit" under `set -euo pipefail`.
if [[ "$BUMP_CARGO" -eq 1 ]]; then
  log "bumping Cargo.toml: $CARGO_VER -> ${TAG#v}"
  sed -i "0,/^version = /s/^version = \".*\"/version = \"${TAG#v}\"/" Cargo.toml
  git add Cargo.toml Cargo.lock
fi

# One combined commit covering both the bump and the docs strip. We stage
# before deciding to commit (so the commit message reflects both intents when
# both apply) and skip the commit entirely if neither side changed (e.g. no
# docs/ to strip on this repo and no --bump-cargo).
if ! git diff --cached --quiet; then
  if [[ "$BUMP_CARGO" -eq 1 ]]; then
    msg="chore(release): bump version to ${TAG#v} and strip internal docs/"
  else
    msg="chore(release): strip internal docs/ for github mirror"
  fi
  git commit -m "$msg"
fi

# ---------- tag + push ----------

log "tagging $TAG"
git tag -a "$TAG" -m "$TAG"

if [[ "$DRY_RUN" -eq 1 ]]; then
  log "DRY RUN - not pushing to origin or github"
  log "release branch: $RELEASE_BRANCH, tag: $TAG"
  log "to complete: git push origin $RELEASE_BRANCH $TAG && git push github $RELEASE_BRANCH $TAG && git push github $RELEASE_BRANCH:refs/heads/main"
  log "and re-run without --dry-run (with GITHUB_TOKEN set) for the GitHub Release"
  exit 0
fi

log "pushing release branch + tag to gitea (origin)"
git push origin "$RELEASE_BRANCH" "$TAG"

log "pushing release branch + tag to github"
git push github "$RELEASE_BRANCH" "$TAG"

# Fast-forward github's `main` to the release commit. This makes
# `git clone github.com:nearbyfly/codexferry` checkout v0.X.Y by default
# AND keeps the per-release branch around for inspection. We only push
# `main` on the github remote - never on origin (gitea) - because
# gitea already tracks the full main including docs/.
#
# We use `--force-with-lease=<refname>:<expect>` — note the ORDER: refname
# FIRST, expected SHA second. git silently ignores the lease when the two
# are swapped, and the push degrades to a plain non-FF reject (exactly what
# happened during the v0.1.3 release). We force rather than plain
# fast-forward because github's main can diverge from local main between
# releases (e.g. when someone resets it via web UI, or after a force-push
# in a previous release). --force-with-lease refuses to clobber unexpected
# SHAs, so a typo in this script or a stray commit can't silently destroy
# history.
EXPECTED_SHA="$(git rev-parse github/main 2>/dev/null || echo 0000000000000000000000000000000000000000)"
log "fast-forwarding github main to release (expected current tip: ${EXPECTED_SHA:0:12})"
git push github "$RELEASE_BRANCH:refs/heads/main" --force-with-lease="refs/heads/main:$EXPECTED_SHA"
log "verify on github: https://github.com/nearbyfly/codexferry/releases/tag/$TAG"

# ---------- github release ----------

if [[ "$GH_RELEASE" -eq 0 ]]; then
  log "skipping GitHub Release API call (--no-gh-release)"
elif [[ -z "${GITHUB_TOKEN:-}" ]]; then
  log "GITHUB_TOKEN unset - tag+branch are pushed, draft the GitHub Release manually"
else
  log "creating GitHub Release via API"
  PREV_TAG="$(git tag --sort=-version:refname | grep -v "^$TAG\$" | head -1 || true)"
  if [[ -n "$PREV_TAG" ]]; then
    CHANGELOG="$(git log --no-decorate --no-merges "$PREV_TAG..$TAG" --pretty=format:'- %s')"
  else
    CHANGELOG="Initial release $TAG"
  fi
  BODY="$(printf '## What'\''s in this release\n\n%s\n\nGenerated by `scripts/release.sh`.' "$CHANGELOG")"
  payload="$(python3 -c "
import json, sys
print(json.dumps({
  'tag_name': '$TAG',
  'name': '$TAG',
  'body': '''$BODY''',
  'draft': False,
  'prerelease': False,
}))")"
  curl -sf -X POST -H "Authorization: token $GITHUB_TOKEN" -H "Content-Type: application/json" \
    -d "$payload" \
    "https://api.github.com/repos/nearbyfly/codexferry/releases" >/dev/null \
    && log "GitHub Release created: https://github.com/nearbyfly/codexferry/releases/tag/$TAG" \
    || log "WARNING: GitHub Release API call failed - tag+branch are pushed; create the release manually"
fi

log "switch back to main"
git checkout main

log "done - $TAG published"
