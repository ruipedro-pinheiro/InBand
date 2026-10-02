#!/bin/sh
## @file install.sh
## @brief Installs InBand on this machine.
##
## @details The script gets the `inband` binary, then runs `inband install`.
## That command installs the daemon and connects Claude Code, Codex and OpenCode.
## The script is safe to run again, and it migrates a v1 install.
##
## From a checkout with cargo, the script builds the binary.
## Without cargo, it downloads the static binary of the latest release and checks its SHA-256.
set -eu

REPO="ruipedro-pinheiro/InBand"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

## @brief Prints the usage.
usage() {
  cat <<'EOF'
usage: ./install.sh [--client] [--no-service]

Installs InBand: gets the inband binary, then runs `inband install`, which sets up the
daemon and connects Claude Code, Codex and OpenCode. Safe to run again; it migrates a v1 install.

  --client      this machine runs agents only; the daemon runs on another machine
  --no-service  do not install the systemd user service

From a checkout with cargo, it builds inband. Without cargo, it downloads the static binary
of the latest release and checks its SHA-256.
EOF
}

## @brief Gives the release name of this machine: x86_64 or aarch64.
release_arch() {
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) echo x86_64 ;;
    Linux-aarch64|Linux-arm64) echo aarch64 ;;
    *) echo "no release binary for $(uname -s) $(uname -m): install Rust (https://rustup.rs) and run this from a checkout" >&2
       exit 1 ;;
  esac
}

## @brief Downloads the binary of the latest release into a directory, and checks its SHA-256.
## @param $1 The directory.
download() {
  dir=$1
  arch=$(release_arch)
  command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
  command -v sha256sum >/dev/null 2>&1 || { echo "sha256sum is required" >&2; exit 1; }
  base="https://github.com/$REPO/releases/latest/download"
  archive="inband-$arch-linux.tar.gz"
  echo "== Downloading $archive"
  curl -fsSL -o "$dir/$archive" "$base/$archive"
  curl -fsSL -o "$dir/SHA256SUMS" "$base/SHA256SUMS"
  (cd "$dir" && grep " $archive\$" SHA256SUMS | sha256sum -c -) || { echo "checksum mismatch" >&2; exit 1; }
  tar -xzf "$dir/$archive" -C "$dir"
}

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
esac

if [ -f "$ROOT/Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
  echo "== Building inband with cargo"
  (cd "$ROOT" && cargo build --release --locked)
  BIN="$ROOT/target/release/inband"
else
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  download "$TMP"
  BIN="$TMP/inband"
fi

"$BIN" install "$@"
