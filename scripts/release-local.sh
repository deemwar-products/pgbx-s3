#!/usr/bin/env bash
# release-local.sh: a release with NO GitHub Actions (owner rule 3, 2026-10-15; CEO-approved recipe, mac:r-1792005872043-dd52).
# Shared template: copy this file to <repo>/scripts/release-local.sh unchanged and put the per-repo part in <repo>/release.conf.
# Run it on the Mac, never on deemwar-dev-agent (the clinic prod box: no heavy builds there).
#
#   scripts/release-local.sh patch|minor|major|vX.Y.Z     DRY_RUN=1 runs every check + the build, changes nothing remote
#
# Steps: clean, in-sync main -> test (local CI) -> build dist/ + checksums -> pre_tag hook (changelog commit) -> tag ->
# push main + tag -> gh release create with dist/* -> npm publish (if NPM_DIR) -> site deploy (if SITE_DIR) -> verify.
# The last line is the ship line to paste into shiplog/STATE: "released vX.Y.Z (CI: local <TEST_CMD> pass)".
#
# release.conf (bash, sourced from the repo root). Variables:
#   REPO=owner/name          required: gh -R target
#   REMOTE=git@github.com:owner/name.git   default: origin
#   TEST_CMD="task test"     local CI; default: 'task test' if a Taskfile exists, else 'go test ./...' (go.mod) or 'cargo test'
#   GO_MAIN=./cmd/x GO_BIN=x GO_TARGETS="darwin/arm64 linux/amd64 ..." GO_VERSION_VAR=main.version   for the default build()
#   MIN_ASSETS=7             verify: the release must have at least this many assets (default: files in dist/)
#   NPM_DIR=npm NPM_PKG=@scope/name   publish from the Mac: needs 'npm login' once (CI's OIDC trusted publishing and
#                            --provenance only exist inside Actions; if the npm package is set to "require trusted
#                            publishing", turn that off on npmjs.com first or the publish is refused)
#   SITE_DIR=site SITE_URL=https://...   build SITE_DIR and push its output to the gh-pages branch (see site_deploy)
# Functions it may override: build (fill dist/), pre_tag (runs before the tag; may commit), site_build (fill site_out),
# site_deploy, verify_extra.
#
# GitHub Pages without a workflow: Settings -> Pages -> Source "Deploy from a branch", branch gh-pages, folder / (one-time
# switch: gh api -X PUT repos/$REPO/pages -f build_type=legacy -f 'source[branch]=gh-pages' -f 'source[path]=/').
# GitHub still serves a branch site through its own built-in pages job; no workflow file is added or enabled.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
say() { printf '\033[1m== %s\033[0m\n' "$*"; }
die() { printf 'release-local: %s\n' "$*" >&2; exit 1; }
[ -f release.conf ] || die "no release.conf in the repo root (see the header of this script)"

REMOTE=origin; TEST_CMD=""; GO_TARGETS="darwin/arm64 darwin/amd64 linux/arm64 linux/amd64 windows/amd64 windows/arm64"
GO_VERSION_VAR=main.version; MIN_ASSETS=""; NPM_DIR=""; NPM_PKG=""; SITE_DIR=""; SITE_URL=""; site_out=""
build() { # default: Go cross-compile, same flags as the old release.yml
  [ -n "${GO_MAIN:-}" ] && [ -n "${GO_BIN:-}" ] || die "set GO_MAIN and GO_BIN in release.conf, or define build()"
  local t os arch ext
  for t in $GO_TARGETS; do
    os=${t%/*}; arch=${t#*/}; ext=; [ "$os" = windows ] && ext=.exe
    CGO_ENABLED=0 GOOS=$os GOARCH=$arch go build -trimpath -ldflags "-s -w -X $GO_VERSION_VAR=$next" \
      -o "dist/${GO_BIN}_${os}_${arch}${ext}" "$GO_MAIN"
  done
}
pre_tag() { :; }
site_build() { (cd "$SITE_DIR" && { [ -d node_modules ] || npm ci --silent; } && npm run build --silent >/dev/null); site_out=$SITE_DIR/dist; }
site_deploy() { # push site_out as the whole gh-pages branch (a new single commit each release; history is not needed)
  local w; w=$(mktemp -d)
  cp -R "$site_out"/. "$w"/; touch "$w/.nojekyll"
  git -C "$w" init -q -b gh-pages && git -C "$w" add -A && git -C "$w" commit -qm "site $next" && \
    git -C "$w" push -qf "$(git remote get-url "$REMOTE" 2>/dev/null || echo "$REMOTE")" gh-pages
  rm -rf "$w"
}
verify_extra() { :; }
# shellcheck source=/dev/null
. ./release.conf
[ -n "${REPO:-}" ] || die "REPO is not set in release.conf"
if [ -z "$TEST_CMD" ]; then
  if [ -f Taskfile.yml ]; then TEST_CMD="task test"; elif [ -f go.mod ]; then TEST_CMD="go test ./..."; elif [ -f Cargo.toml ]; then TEST_CMD="cargo test"; else die "set TEST_CMD"; fi
fi

bump=${1:-}; [ -n "$bump" ] || die "usage: scripts/release-local.sh patch|minor|major|vX.Y.Z"
last=$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || echo v0.0.0)
IFS=. read -r ma mi pa <<<"${last#v}"
case $bump in
  patch) next=v$ma.$mi.$((pa+1)) ;;
  minor) next=v$ma.$((mi+1)).0 ;;
  major) next=v$((ma+1)).0.0 ;;
  v[0-9]*.[0-9]*.[0-9]*) next=$bump ;;
  *) die "bump must be patch, minor, major or vX.Y.Z (got $bump)" ;;
esac
case $next in v0.*) ;; *) die "$next: versions stay 0.x (owner rule 10-11); pass vX.Y.Z only after the CEO says so" ;; esac
git rev-parse -q --verify "refs/tags/$next" >/dev/null && die "$next already exists"
command -v gh >/dev/null || die "gh is not installed"

say "checks for $last -> $next"
[ "$(git branch --show-current)" = main ] || die "not on main"
[ -z "$(git status --porcelain)" ] || die "working tree not clean"
git fetch -q "$REMOTE" main
git merge-base --is-ancestor FETCH_HEAD HEAD || die "main is behind $REMOTE: pull first"
[ -z "$(ls .github/workflows 2>/dev/null | grep -v '\.disabled$' || true)" ] || \
  echo "  note: .github/workflows/ still has files; they stay disabled on GitHub (never re-enable, owner rule 3)"

say "local CI: $TEST_CMD"
bash -c "$TEST_CMD" || die "local CI failed: $TEST_CMD (ship line would read 'CI: local $TEST_CMD fail')"

say "build dist/"
rm -rf dist && mkdir dist
build
[ -n "$(ls dist)" ] || die "build() put nothing in dist/"
(cd dist && { command -v sha256sum >/dev/null && sha256sum * || shasum -a 256 *; } > checksums.txt)
n_local=$(ls dist | wc -l | tr -d ' '); echo "  $n_local files"
# the site is built again after the tag (a header version from 'git describe' must show $next); this pass only proves it builds
[ -z "$SITE_DIR" ] || { say "site build (check)"; site_build; [ -d "$site_out" ] || die "site_build left no $site_out"; }
[ -n "${DRY_RUN:-}" ] && { say "dry run ok: would release $next on $(git rev-parse --short HEAD) (CI: local $TEST_CMD pass)"; exit 0; }

say "pre-tag ($next)"
pre_tag
say "tag and push $next"
git tag -a "$next" -m "$(basename "$REPO") $next"
git push -q "$REMOTE" main "$next"

say "GitHub release"
gh release create "$next" -R "$REPO" dist/* --generate-notes --verify-tag >/dev/null
if [ -n "$NPM_DIR" ]; then
  say "npm publish ($NPM_DIR)"
  copied=""; for f in README.md LICENSE; do # npm shows the repo README; copy only what the package dir doesn't track
    [ -f "$f" ] && ! git ls-files --error-unmatch "$NPM_DIR/$f" >/dev/null 2>&1 && cp "$f" "$NPM_DIR/" && copied="$copied $NPM_DIR/$f"
  done
  (cd "$NPM_DIR" && npm version "${next#v}" --no-git-tag-version --allow-same-version >/dev/null && npm publish --access public)
  git checkout -q -- "$NPM_DIR" 2>/dev/null || true; rm -f $copied # the version bump and the copies stay out of git
fi
[ -z "$SITE_DIR" ] || { say "site build + deploy"; site_build; site_deploy; }

say "verify"
n=$(gh release view "$next" -R "$REPO" --json assets -q '.assets | length')
[ "$n" -ge "${MIN_ASSETS:-$n_local}" ] || die "release $next has $n assets (want ${MIN_ASSETS:-$n_local})"
echo "  release: $n assets"
if [ -n "$NPM_PKG" ]; then
  for _ in $(seq 1 30); do [ "$(npm view "$NPM_PKG" version 2>/dev/null)" = "${next#v}" ] && break; sleep 10; done
  [ "$(npm view "$NPM_PKG" version 2>/dev/null)" = "${next#v}" ] || die "npm does not show ${next#v} yet"
  echo "  npm: ${next#v}"
fi
verify_extra
say "released $next (CI: local $TEST_CMD pass)"
