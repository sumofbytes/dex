#!/bin/sh
# dex installer — detects your platform and installs the latest release.
#
#   curl -fsSL https://raw.githubusercontent.com/arpitsr/dex/develop/install.sh | sh
#
# Supported: linux (musl, static) and macOS, x86_64 + aarch64.
# Windows: download the .zip from https://github.com/arpitsr/dex/releases/latest
#
# Optional env overrides:
#   DEX_REPO         github repo (default: arpitsr/dex)
#   DEX_INSTALL_DIR  install directory (default: ~/.local/bin)
set -eu

REPO="${DEX_REPO:-arpitsr/dex}"
INSTALL_DIR="${DEX_INSTALL_DIR:-$HOME/.local/bin}"

msg() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- dependencies ------------------------------------------------------------
command -v curl >/dev/null 2>&1 || die "curl is required (https://curl.se)"
command -v tar  >/dev/null 2>&1 || die "tar is required"

# --- platform detection ------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin) triple_os="apple-darwin" ;;
  Linux)  triple_os="unknown-linux-musl" ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    msg "no sh-based installer for windows yet — grab the zip directly:"
    msg "https://github.com/$REPO/releases/latest"
    exit 1
    ;;
  *) die "unsupported os: $os (supported: linux, darwin)" ;;
esac

case "$arch" in
  x86_64|amd64)  triple_arch="x86_64" ;;
  aarch64|arm64) triple_arch="aarch64" ;;
  *) die "unsupported architecture: $arch (supported: x86_64, aarch64)" ;;
esac

triple="${triple_arch}-${triple_os}"

# --- find latest release ------------------------------------------------------
msg "→ resolving latest release of $REPO ..."
tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)"
[ -n "$tag" ] || die "could not resolve latest release (is $REPO public, with releases?)"

asset="dex-${tag}-${triple}.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"
msg "→ found $tag — target: $triple"

# --- download -----------------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

msg "→ downloading $asset ..."
curl -fSL --retry 3 -o "$tmp/$asset" "$base/$asset" \
  || die "download failed: $base/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" \
  || die "could not download SHA256SUMS"

# --- verify checksum ----------------------------------------------------------
if   command -v sha256sum >/dev/null 2>&1; then hash_cmd="sha256sum"
elif command -v shasum    >/dev/null 2>&1; then hash_cmd="shasum -a 256"
else die "need sha256sum (or shasum) to verify the download"
fi

expected="$(awk -v a="$asset" '$2 == a { print $1 }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "no checksum entry for $asset in SHA256SUMS"
actual="$($hash_cmd "$tmp/$asset" | awk '{print $1}')"
[ "$actual" = "$expected" ] \
  || die "checksum mismatch for $asset (want $expected, got $actual)"
msg "→ checksum verified"

# --- install ------------------------------------------------------------------
tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/dex" ] || die "unexpected archive layout — no 'dex' binary at archive root"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/dex" "$INSTALL_DIR/dex"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    msg ""
    msg "note: $INSTALL_DIR is not on your PATH. add it to your shell profile:"
    msg "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

msg ""
msg "✓ installed dex $tag → $INSTALL_DIR/dex"
msg "  try: dex --version"
