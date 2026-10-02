#!/bin/sh
# Installs InBand: gets the inband binary, then runs `inband install`, which sets up the daemon and
# connects Claude Code, Codex and OpenCode. Safe to run again; it migrates a v1 install.
#
#   ./install.sh               from a checkout: builds with cargo when it is installed
#   ./install.sh --client      this machine runs agents only; the daemon runs elsewhere
#   ./install.sh --no-service  no systemd user service
#
# Without cargo, it downloads the static binary of the latest release and checks its SHA-256.
set -eu

REPO="ruipedro-pinheiro/InBand"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

case "${1:-}" in
  -h|--help)
    sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
    exit 0 ;;
esac

if [ -f "$ROOT/Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
  echo "== Building inband with cargo"
  (cd "$ROOT" && cargo build --release --locked)
  BIN="$ROOT/target/release/inband"
else
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) ARCH=x86_64 ;;
    Linux-aarch64|Linux-arm64) ARCH=aarch64 ;;
    *) echo "no release binary for $(uname -s) $(uname -m): install Rust (https://rustup.rs) and run this from a checkout" >&2
       exit 1 ;;
  esac
  command -v curl >/dev/null 2>&1 || { echo "curl is required" >&2; exit 1; }
  command -v sha256sum >/dev/null 2>&1 || { echo "sha256sum is required" >&2; exit 1; }
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  BASE="https://github.com/$REPO/releases/latest/download"
  ARCHIVE="inband-$ARCH-linux.tar.gz"
  echo "== Downloading $ARCHIVE"
  curl -fsSL -o "$TMP/$ARCHIVE" "$BASE/$ARCHIVE"
  curl -fsSL -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS"
  (cd "$TMP" && grep " $ARCHIVE\$" SHA256SUMS | sha256sum -c -) || { echo "checksum mismatch" >&2; exit 1; }
  tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
  BIN="$TMP/inband"
fi

"$BIN" install "$@"
