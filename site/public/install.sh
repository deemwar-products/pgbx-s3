#!/bin/sh
# pgbx installer: the pgbx CLI, the pgbx Postgres extension (Linux, PostgreSQL 13-18) and the agent skill.
#
#   curl -fsSL https://pgbx.deemwar.com/install.sh | sh
#   curl -fsSL https://pgbx.deemwar.com/install.sh | sh -s -- --pg-version 16 --no-skill
#
# Options:
#   --version vX.Y.Z   release to install (default: latest)
#   --pg-version N     install the extension only for PostgreSQL N (default: every local 13-18 found)
#   --prefix DIR       where pgbx goes (default: /usr/local/bin, else ~/.local/bin)
#   --skill            install the agent skill without asking
#   --no-skill         do not install the agent skill
#   --no-extension     CLI only
#   --base-url URL     download from URL/<file> instead of GitHub Releases (testing, mirrors)
#
# Default source: https://github.com/deemwar-products/pgbx/releases/latest/download/<asset> (no GitHub API call);
# --version vX uses .../releases/download/vX/<asset>. Downloads are verified against the release's SHA256SUMS; any mismatch aborts.
# It never edits postgresql.conf and never restarts anything: `sudo pgbx setup` does the configuration.
set -eu

REPO=deemwar-products/pgbx
DOCS=https://pgbx.deemwar.com
VERSION=""
PG_VERSION=""
PREFIX=""
SKILL=ask
EXTENSION=1
BASE_URL=""

say() { printf '%s\n' "$*"; }
warn() { printf 'pgbx install: %s\n' "$*" >&2; }
die() { warn "$*"; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION=$2; shift 2 ;;
    --version=*) VERSION=${1#*=}; shift ;;
    --pg-version) [ $# -ge 2 ] || die "--pg-version needs a value"; PG_VERSION=$2; shift 2 ;;
    --pg-version=*) PG_VERSION=${1#*=}; shift ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a value"; PREFIX=$2; shift 2 ;;
    --prefix=*) PREFIX=${1#*=}; shift ;;
    --base-url) [ $# -ge 2 ] || die "--base-url needs a value"; BASE_URL=$2; shift 2 ;;
    --base-url=*) BASE_URL=${1#*=}; shift ;;
    --skill) SKILL=yes; shift ;;
    --no-skill) SKILL=no; shift ;;
    --no-extension) EXTENSION=0; shift ;;
    -h|--help) sed -n '2,18p' "$0" 2>/dev/null || say "see $DOCS/getting-started/install/"; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done

case "$PG_VERSION" in ''|*[!0-9]*) [ -z "$PG_VERSION" ] || die "--pg-version must be a number, got $PG_VERSION" ;; esac

# ---------------------------------------------------------------- platform
case "$(uname -s)" in
  Linux) OS=linux ;;
  Darwin) OS=darwin ;;
  *) die "unsupported OS $(uname -s); on Windows use: powershell -c \"irm $DOCS/install.ps1 | iex\"" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ARCH=amd64 ;;
  aarch64|arm64) ARCH=arm64 ;;
  *) die "unsupported CPU $(uname -m) (amd64 and arm64 only)" ;;
esac
[ "$OS" = darwin ] && [ "$ARCH" != arm64 ] && die "no prebuilt pgbx for Intel Macs; build it from source: git clone https://github.com/$REPO && cargo install --locked --path pgbx/cli"

IS_ROOT=0
[ "$(id -u)" = 0 ] && IS_ROOT=1

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "need curl or wget"
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "need sha256sum or shasum to verify downloads"
fi

TMP=$(mktemp -d 2>/dev/null || mktemp -d -t pgbx)
trap 'rm -rf "$TMP"' EXIT INT TERM

# run a command as root when the target is not writable by us
as_root() {
  if [ "$IS_ROOT" = 1 ]; then "$@"
  elif command -v sudo >/dev/null 2>&1; then sudo "$@"
  else die "need root to run: $* (no sudo found)"
  fi
}
writable() { # dir (or its nearest existing parent) is writable by us
  d=$1
  while [ ! -d "$d" ]; do d=$(dirname "$d"); done
  [ -w "$d" ]
}
put() { # mode src dest-dir
  if writable "$3"; then mkdir -p "$3" && install -m "$1" "$2" "$3/"
  else as_root mkdir -p "$3" && as_root install -m "$1" "$2" "$3/"
  fi
}

# ---------------------------------------------------------------- release
# version-less asset names: the "latest" redirect needs no API call (no rate limit)
if [ -z "$BASE_URL" ]; then
  if [ -n "$VERSION" ]; then
    case "$VERSION" in v*) ;; *) VERSION=v$VERSION ;; esac
    BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
  else
    BASE_URL="https://github.com/$REPO/releases/latest/download"
  fi
fi
BASE_URL=${BASE_URL%/}
fetch "$BASE_URL/SHA256SUMS" "$TMP/SHA256SUMS" || die "cannot download $BASE_URL/SHA256SUMS"
say "pgbx ${VERSION:-latest} ($OS/$ARCH) from $BASE_URL"

# download + verify one release file into $TMP; aborts on any mismatch
get() {
  f=$1
  want=$(awk -v f="$f" '{ n=$2; sub(/^\*/, "", n); if (n == f) print $1 }' "$TMP/SHA256SUMS")
  [ -n "$want" ] || die "$f is not listed in SHA256SUMS"
  fetch "$BASE_URL/$f" "$TMP/$f" || die "cannot download $BASE_URL/$f"
  got=$(sha256 "$TMP/$f")
  [ "$got" = "$want" ] || die "checksum mismatch for $f (expected $want, got $got); nothing was installed from it"
}

# ---------------------------------------------------------------- pgbx CLI
get "pgbx-$OS-$ARCH.tar.gz"
mkdir -p "$TMP/cli" && tar -xzf "$TMP/pgbx-$OS-$ARCH.tar.gz" -C "$TMP/cli"
BIN_SRC=$(find "$TMP/cli" -type f -name pgbx | head -1)
[ -n "$BIN_SRC" ] || die "pgbx binary missing from the tarball"
if [ -z "$PREFIX" ]; then
  if writable /usr/local/bin || [ "$IS_ROOT" = 1 ] || command -v sudo >/dev/null 2>&1; then PREFIX=/usr/local/bin
  else PREFIX=$HOME/.local/bin
  fi
fi
put 755 "$BIN_SRC" "$PREFIX"
PGBX=$PREFIX/pgbx
say "installed $PGBX ($("$PGBX" --version))"
case ":$PATH:" in *":$PREFIX:"*) ;; *) say "note: $PREFIX is not on your PATH; add it: export PATH=\"$PREFIX:\$PATH\"" ;; esac

# example connection adapters (ssh, aws, gcp, azure; Node scripts) into the user's pgbx config dir, where
# `pgbx profile add --adapter ssh ...` finds them. Copies of the examples: edit a copy elsewhere, not these.
AD_SRC=$(find "$TMP/cli" -type d -name adapters | head -1)
if [ -n "$AD_SRC" ] && [ "$IS_ROOT" != 1 ]; then
  AD_DIR=${PGBX_ADAPTERS_DIR:-${PGBX_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/pgbx}/adapters}
  mkdir -p "$AD_DIR" && cp -R "$AD_SRC"/. "$AD_DIR"/ && say "example adapters: $AD_DIR (need Node 18+ only if you use one)"
elif [ -n "$AD_SRC" ]; then
  say "example adapters: skipped as root (they belong in a user's config dir); see $DOCS/guides/adapters/"
fi

# ---------------------------------------------------------------- extension (Linux)
# prints "major pkglibdir sharedir" for every local PostgreSQL
find_postgres() {
  for pc in "$(command -v pg_config 2>/dev/null || true)" /usr/lib/postgresql/*/bin/pg_config /usr/pgsql-*/bin/pg_config; do
    if [ -z "$pc" ] || [ ! -x "$pc" ]; then continue; fi
    m=$("$pc" --version 2>/dev/null | sed -n 's/^PostgreSQL \([0-9]*\).*/\1/p')
    [ -n "$m" ] && printf '%s %s %s\n' "$m" "$("$pc" --pkglibdir)" "$("$pc" --sharedir)"
  done
  # server installed without pg_config (Debian postgresql-N without -dev): use the packaging layout
  for d in /usr/lib/postgresql/*/bin/postgres; do
    [ -x "$d" ] || continue
    m=$(basename "$(dirname "$(dirname "$d")")")
    printf '%s %s %s\n' "$m" "/usr/lib/postgresql/$m/lib" "/usr/share/postgresql/$m"
  done
  for d in /usr/pgsql-*/bin/postgres; do
    [ -x "$d" ] || continue
    r=$(dirname "$(dirname "$d")")
    printf '%s %s %s\n' "${r#/usr/pgsql-}" "$r/lib" "$r/share"
  done
}

EXT_DONE=""
if [ "$OS" = darwin ]; then
  say "macOS: installed the pgbx CLI only. The pgbx extension runs on Linux Postgres servers (PostgreSQL 13-18)."
elif [ "$EXTENSION" = 1 ]; then
  find_postgres | sort -n -k1,1 -u > "$TMP/pg"
  [ -z "$PG_VERSION" ] || awk -v v="$PG_VERSION" '$1 == v' "$TMP/pg" > "$TMP/pg.sel"
  if [ -n "$PG_VERSION" ]; then
    [ -s "$TMP/pg.sel" ] || die "PostgreSQL $PG_VERSION not found here (found: $(cut -d' ' -f1 "$TMP/pg" | tr '\n' ' '))"
    mv "$TMP/pg.sel" "$TMP/pg"
  fi
  if [ ! -s "$TMP/pg" ]; then
    say "no local PostgreSQL server found: installed the pgbx CLI only (it works against remote servers)."
    say "pgbx's extension supports PostgreSQL 13–18; install it on the database server."
  fi
  while read -r m libdir sharedir; do
    if [ "$m" -lt 13 ] || [ "$m" -gt 18 ]; then
      say "pgbx's extension supports PostgreSQL 13–18; found $m (CLI installed, extension skipped)"
      continue
    fi
    f="pgbx-ext-pg$m-linux-$ARCH.tar.gz"
    get "$f"
    mkdir -p "$TMP/ext$m" && tar -xzf "$TMP/$f" -C "$TMP/ext$m"
    src=$(dirname "$(find "$TMP/ext$m" -type d -name lib | head -1)")
    for so in "$src"/lib/*.so; do put 755 "$so" "$libdir"; done
    for x in "$src"/extension/*; do put 644 "$x" "$sharedir/extension"; done
    say "installed the extension for PostgreSQL $m: $libdir, $sharedir/extension"
    EXT_DONE="$EXT_DONE $m"
  done < "$TMP/pg"
fi

# ---------------------------------------------------------------- agent skill
tty_ok() { (exec </dev/tty >/dev/tty) 2>/dev/null; }
if [ "$SKILL" = ask ] && [ "$IS_ROOT" = 1 ]; then
  SKILL=no
  say "skill: skipped as root (it belongs in a user's home); as your user run: pgbx skill install"
elif [ "$SKILL" = ask ]; then
  if tty_ok; then
    printf 'Install the pgbx agent skill for Claude Code / Codex (~/.claude/skills/pgbx-skill)? [Y/n] ' >/dev/tty
    read -r ans </dev/tty || ans=""
    case "$ans" in [Nn]*) SKILL=no ;; *) SKILL=yes ;; esac
  else
    SKILL=yes # no terminal (CI, piped): default yes; --no-skill to skip
  fi
fi
if [ "$SKILL" = yes ]; then
  if "$PGBX" skill install; then
    say "skill installed (remove with: pgbx skill uninstall)"
  else
    warn "skill install failed; retry later with: pgbx skill install"
  fi
fi

# ---------------------------------------------------------------- next
say ""
if [ -n "$EXT_DONE" ]; then
  say "Next (nothing was configured or restarted yet):"
  say "  sudo pgbx setup      # S3 settings + shared_preload_libraries in conf.d; prints the restart command"
  say "  <restart Postgres>   # the one command pgbx setup prints"
  say "  pgbx doctor"
else
  say "Next: pgbx help    (docs: $DOCS/)"
fi
