#!/usr/bin/env bash
# Linux release assets for one architecture, built INSIDE a throwaway Ubuntu 22.04 VM (glibc 2.35, as release.yml's
# runners: the .so then loads on Debian 12 / Ubuntu 22.04+). Called by release.conf's build() through OrbStack; the VM
# sees the repo at the same path. Writes into dist/:
#   pgbx-ext-pg<13..18>-linux-<arch>.tar.gz   extension: lib/*.so + extension/* + sql upgrades + VERSION
#   pgbx-linux-<arch>.tar.gz                  CLI, static musl
#   pgbx-windows-amd64.zip                    CLI, cross-built with cargo-xwin (amd64 VM only)
# Usage: scripts/release-linux.sh vX.Y.Z amd64|arm64
set -euo pipefail
tag=${1:?usage: release-linux.sh vX.Y.Z amd64|arm64}; arch=${2:?}; ver=${tag#v}
cd "$(dirname "$0")/.."
export DEBIAN_FRONTEND=noninteractive CARGO_TARGET_DIR=$HOME/target   # build on the VM's disk, not the shared folder
work=$(mktemp -d); mkdir -p dist

sudo apt-get update -qq
sudo apt-get install -y -qq curl ca-certificates gnupg lsb-release build-essential libclang-dev clang pkg-config \
  musl-tools lld llvm zip >/dev/null
sudo install -d /usr/share/postgresql-common/pgdg
sudo curl -fsSL -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc https://www.postgresql.org/media/keys/ACCC4CF8.asc
echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" |
  sudo tee /etc/apt/sources.list.d/pgdg.list >/dev/null
sudo apt-get update -qq
for pg in 13 14 15 16 17 18; do sudo apt-get install -y -qq "postgresql-$pg" "postgresql-server-dev-$pg" >/dev/null; done
[ -x "$HOME/.cargo/bin/cargo" ] || curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal
. "$HOME/.cargo/env"

# extension, one tarball per Postgres major (cargo-pgrx version pinned in Cargo.toml)
pgrx=$(sed -n 's/^pgrx = "=\{0,1\}\([0-9.]*\)".*/\1/p' Cargo.toml | head -1)
cargo install --locked cargo-pgrx --version "$pgrx"
for pg in 13 14 15 16 17 18; do
  pgc=/usr/lib/postgresql/$pg/bin/pg_config
  cargo pgrx init --pg$pg "$pgc"
  cargo pgrx package --pg-config "$pgc" --no-default-features --features pg$pg
  name=pgbx-ext-pg$pg-linux-$arch; out=$work/$name; mkdir -p "$out/lib" "$out/extension"
  pkg=$(ls -d "$CARGO_TARGET_DIR"/release/*-pg$pg)
  cp "$pkg"/usr/lib/postgresql/$pg/lib/*.so "$out/lib/"
  cp "$pkg"/usr/share/postgresql/$pg/extension/* "$out/extension/"
  [ -d sql ] && cp sql/*.sql "$out/extension/" || true   # upgrade scripts (pgrx only generates the base install)
  echo "$ver" > "$out/VERSION"
  tar -C "$work" -czf "dist/$name.tar.gz" "$name"
done

# CLI, static musl (TLS to Postgres and S3 is rustls, no OpenSSL)
case $arch in amd64) target=x86_64-unknown-linux-musl ;; arm64) target=aarch64-unknown-linux-musl ;; *) exit 1 ;; esac
rustup target add "$target"
cargo build --release --locked --manifest-path cli/Cargo.toml --target "$target"
name=pgbx-linux-$arch; out=$work/$name; mkdir -p "$out/adapters"
cp "$CARGO_TARGET_DIR/$target/release/pgbx" "$out/"; cp LICENSE "$out/" 2>/dev/null || true; echo "$ver" > "$out/VERSION"
cp -R adapters/README.md adapters/lib adapters/ssh adapters/aws adapters/gcp adapters/azure "$out/adapters/"
"$out/pgbx" --version
tar -C "$work" -czf "dist/$name.tar.gz" "$name"

# Windows CLI from the amd64 VM (Windows on ARM runs the x64 exe through its emulation, as release.yml decided)
if [ "$arch" = amd64 ]; then
  target=x86_64-pc-windows-msvc; rustup target add $target
  cargo install --locked cargo-xwin
  command -v clang-cl >/dev/null || sudo ln -sf "$(command -v clang)" /usr/local/bin/clang-cl
  CC_x86_64_pc_windows_msvc=clang-cl cargo xwin build --release --locked --manifest-path cli/Cargo.toml --target $target
  name=pgbx-windows-amd64; out=$work/$name; mkdir -p "$out/adapters"
  cp "$CARGO_TARGET_DIR/$target/release/pgbx.exe" "$out/"; cp LICENSE "$out/" 2>/dev/null || true; printf '%s' "$ver" > "$out/VERSION"
  cp -R adapters/README.md adapters/lib adapters/ssh adapters/aws adapters/gcp adapters/azure "$out/adapters/"
  (cd "$out" && zip -q -r "$OLDPWD/dist/$name.zip" .)
fi
rm -rf "$work"; ls -l dist
