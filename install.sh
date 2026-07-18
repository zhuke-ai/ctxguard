#!/usr/bin/env bash
# install.sh — one-line installer for ctxguard
#
# Usage: curl -fsSL https://raw.githubusercontent.com/zhuke-ai/ctxguard/master/install.sh | sh
#
# Detects OS + arch, downloads the matching release binary from GitHub Releases,
# and installs to ~/.cargo/bin (or /usr/local/bin if writable).

set -e

REPO="zhuke-ai/ctxguard"
BIN="ctxguard"
INSTALL_DIR="${CTXGUARD_INSTALL_DIR:-$HOME/.cargo/bin}"

# Detect target triple
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  linux*)  TARGET="ctxguard-linux-x86_64" ;;
  darwin)
    if [ "$ARCH" = "arm64" ]; then
      TARGET="ctxguard-macos-aarch64"
    else
      TARGET="ctxguard-macos-x86_64"
    fi
    ;;
  mingw*|msys*|cygwin*) TARGET="ctxguard-windows-x86_64.exe" ;;
  *)
    echo "Unsupported OS: $OS" >&2
    exit 1
    ;;
esac

# Fetch latest release tag
LATEST=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep -o '"tag_name": *"[^"]*"' | cut -d'"' -f4)

if [ -z "$LATEST" ]; then
    echo "Could not determine latest release. Falling back to cargo install."
    cargo install ctxguard
    exit 0
fi

# Determine archive format
case "$OS" in
  mingw*|msys*|cygwin*) ARCHIVE_FMT="zip" ;;
  *)                    ARCHIVE_FMT="tar.gz" ;;
esac

URL="https://github.com/$REPO/releases/download/$LATEST/${TARGET}.${ARCHIVE_FMT}"
echo "Downloading $URL"

TMP=$(mktemp -d)
trap "rm -rf $TMP" EXIT

if [ "$ARCHIVE_FMT" = "zip" ]; then
    curl -fsSL "$URL" -o "$TMP/archive.zip"
    (cd "$TMP" && unzip -q archive.zip)
else
    curl -fsSL "$URL" | tar -xzf - -C "$TMP"
fi

mkdir -p "$INSTALL_DIR"
cp "$TMP/$TARGET" "$INSTALL_DIR/$BIN"
chmod +x "$INSTALL_DIR/$BIN"

echo
echo "✓ ctxguard installed to $INSTALL_DIR/$BIN"
echo
echo "Try:  $BIN --version"
echo "      $BIN profile --days 7 --by day"
echo "      $BIN run --budget 80000 --on-full warn -- claude 'fix the bug'"
