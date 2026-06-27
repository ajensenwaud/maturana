#!/usr/bin/env bash
# Maturana release helper — a guarded, one-command "cut a release from main".
# Releasing is otherwise four manual steps you have to remember in order; this is
# the thin adapter around them. It does NOT own release policy (the build/publish
# lives in .github/workflows/release.yml, triggered by the tag push) — it just
# does the safety checks, picks the next version, and pushes the tag.
#
# What it does:
#   1. Sanity-checks: you're on `main`, the tree is clean, and local main matches
#      origin/main (so you tag exactly what's on GitHub).
#   2. Picks the version: next patch after the latest `v*` tag, or an explicit arg.
#   3. Runs the workspace tests (the same gate CI runs) unless --skip-tests.
#   4. Tags + pushes the tag → release.yml builds + publishes Linux/Windows
#      binaries (signed if the repo secrets are set, else unsigned + SHA256SUMS).
#   5. Optionally watches the release run to completion (needs `gh`).
#
# Merge first, then release: this tags whatever is on main, so land your PR
# (`gh pr merge <N> --merge`) before running it.
#
#   ./scripts/release.sh                 # auto-bump patch, confirm, release + watch
#   ./scripts/release.sh v0.2.0          # explicit version
#   ./scripts/release.sh --skip-tests    # skip local cargo test (CI still runs it)
#   ./scripts/release.sh --yes           # no interactive confirmation
#   ./scripts/release.sh --no-watch      # don't wait for the build to finish
#
set -euo pipefail

MAIN_BRANCH="${MATURANA_MAIN_BRANCH:-main}"
VERSION=""
SKIP_TESTS=0
ASSUME_YES=0
WATCH=1

die() { echo "release: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-tests) SKIP_TESTS=1 ;;
    --yes|-y) ASSUME_YES=1 ;;
    --no-watch) WATCH=0 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    v[0-9]*) VERSION="$1" ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

command -v git >/dev/null 2>&1 || die "git not found"
[ "$SKIP_TESTS" -eq 1 ] || command -v cargo >/dev/null 2>&1 || die "cargo not found (use --skip-tests to skip the local test run)"

# --- 1. sanity checks ----------------------------------------------------------
branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "$MAIN_BRANCH" ] || die "not on $MAIN_BRANCH (on '$branch'); releases are cut from $MAIN_BRANCH"
[ -z "$(git status --porcelain)" ] || die "working tree is dirty; commit or stash first"

echo "release: fetching origin…"
git fetch --quiet --tags origin
local_head="$(git rev-parse @)"
remote_head="$(git rev-parse "origin/$MAIN_BRANCH")"
[ "$local_head" = "$remote_head" ] || die "local $MAIN_BRANCH ($local_head) != origin/$MAIN_BRANCH ($remote_head); pull/push first"

# --- 2. choose the version -----------------------------------------------------
if [ -z "$VERSION" ]; then
  latest="$(git tag --list 'v*' --sort=-v:refname | head -n1)"
  [ -n "$latest" ] || die "no existing v* tag to bump from; pass an explicit version (e.g. v0.1.0)"
  ver="${latest#v}"
  IFS=. read -r major minor patch <<EOF
$ver
EOF
  case "$major$minor$patch" in
    *[!0-9]*|"") die "latest tag '$latest' is not vMAJOR.MINOR.PATCH; pass an explicit version" ;;
  esac
  VERSION="v${major}.${minor}.$((patch + 1))"
  echo "release: latest tag is $latest → next is $VERSION"
fi

case "$VERSION" in
  v[0-9]*.[0-9]*.[0-9]*) : ;;
  *) die "version '$VERSION' must look like vMAJOR.MINOR.PATCH" ;;
esac
if git rev-parse "$VERSION" >/dev/null 2>&1; then
  die "tag $VERSION already exists"
fi

# --- 3. tests (the same gate CI runs) -----------------------------------------
if [ "$SKIP_TESTS" -eq 1 ]; then
  echo "release: skipping local tests (--skip-tests); CI still runs them"
else
  echo "release: running cargo test --workspace --locked …"
  cargo test --workspace --locked
fi

# --- 4. confirm + tag + push ---------------------------------------------------
echo
echo "  Release plan:"
echo "    version : $VERSION"
echo "    commit  : $local_head"
echo "    trigger : push tag → .github/workflows/release.yml builds + publishes"
echo
if [ "$ASSUME_YES" -ne 1 ]; then
  printf "  Proceed? [y/N] "
  read -r answer
  case "$answer" in y|Y|yes|YES) : ;; *) die "aborted" ;; esac
fi

git tag -a "$VERSION" -m "$VERSION"
git push origin "$VERSION"
echo "release: pushed tag $VERSION"

# --- 5. watch the build (best-effort) -----------------------------------------
if [ "$WATCH" -ne 1 ] || ! command -v gh >/dev/null 2>&1; then
  [ "$WATCH" -eq 1 ] && echo "release: gh not found; skipping build watch"
  echo "release: track the build at the repo's Actions tab; published under Releases when green"
  exit 0
fi

echo "release: waiting for the release run to register…"
run_id=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
  run_id="$(gh run list --workflow release.yml --limit 1 --json databaseId,headBranch \
    --jq ".[] | select(.headBranch==\"$VERSION\") | .databaseId" 2>/dev/null || true)"
  [ -n "$run_id" ] && break
  sleep 3
done

if [ -z "$run_id" ]; then
  echo "release: couldn't find the run yet — check the Actions tab; the tag is pushed."
  exit 0
fi

echo "release: watching run $run_id …"
if gh run watch "$run_id" --exit-status; then
  echo "release: $VERSION published 🎉"
  gh release view "$VERSION" --web >/dev/null 2>&1 || true
else
  die "release run $run_id failed — see the Actions tab (the tag is pushed; re-run the workflow after a fix)"
fi
