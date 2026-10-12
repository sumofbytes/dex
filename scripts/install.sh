#!/bin/sh
# dex installer (Linux / macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/sumofbytes/dex/HEAD/scripts/install.sh | sh
#   curl -fsSL ... | sh -s -- 0.2.0                    # pin a version
#   DEX_INSTALL_DIR=/usr/local/bin curl -fsSL ... | sh # custom directory
#
# Env:
#   DEX_VERSION       version to install ("latest" or e.g. "0.2.0" / "v0.2.0")
#   DEX_INSTALL_DIR   install directory (default: $HOME/.local/bin)

set -eu

REPO="${DEX_REPO:-sumofbytes/dex}"
INSTALL_DIR="${DEX_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${1:-${DEX_VERSION:-latest}}"

err() {
    echo "dex-install: $1" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || err "curl not found"
command -v tar >/dev/null 2>&1 || err "tar not found"

# github.com occasionally answers a redirect with a transient 5xx/timeout;
# retry the handful of small fetches instead of failing the install.
fetch() {
    curl -fsSL --connect-timeout 15 --retry 3 --retry-delay 2 "$@"
}

# --- detect platform -------------------------------------------------------
case "$(uname -s)" in
Linux) os="linux" ;;
Darwin) os="darwin" ;;
*) err "unsupported OS ($(uname -s)). Build from source: cargo install --git https://github.com/$REPO" ;;
esac

case "$(uname -m)" in
x86_64 | amd64) arch="x86_64" ;;
aarch64 | arm64) arch="aarch64" ;;
*) err "unsupported architecture ($(uname -m))" ;;
esac

case "$os" in
linux) TARGET="${arch}-unknown-linux-musl" ;;
darwin) TARGET="${arch}-apple-darwin" ;;
esac

# --- resolve version -------------------------------------------------------
if [ "$VERSION" = "latest" ]; then
    # Follow the /releases/latest redirect; no API call, so no rate limits.
    # Use a temp file instead of a pipeline: POSIX sh reports a pipeline's
    # status from its last command, so curl's exit status would be swallowed —
    # and `-w '%{url_effective}'` echoes the *original* URL on failure, which
    # used to leak into the asset name.
    tag_file="$(mktemp)"
    if ! fetch -o /dev/null -w '%{url_effective}' \
            "https://github.com/$REPO/releases/latest" >"$tag_file"; then
        rm -f "$tag_file"
        err "cannot determine latest release (none published yet?)"
    fi
    VERSION="$(sed 's|.*/tag/||' "$tag_file")"
    rm -f "$tag_file"
fi
case "$VERSION" in
v[0-9]*) ;;
[0-9]*) VERSION="v$VERSION" ;;
*) err "invalid version '$VERSION' (expected e.g. 0.2.0 or v0.2.0)" ;;
esac

BASE="https://github.com/$REPO/releases/download/$VERSION"
ASSET="dex-$VERSION-$TARGET.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Downloading dex $VERSION for $TARGET ..."
fetch -o "$TMP/$ASSET" "$BASE/$ASSET" ||
    err "download failed ($BASE/$ASSET) — see https://github.com/$REPO/releases"
fetch -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS" ||
    err "download failed ($BASE/SHA256SUMS)"

# --- verify checksum -------------------------------------------------------
expected="$(awk -v f="$ASSET" '$2 == f { print $1 }' "$TMP/SHA256SUMS")"
[ -n "$expected" ] || err "no checksum entry for $ASSET"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
else
    actual="$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')"
fi
[ "$actual" = "$expected" ] ||
    err "checksum mismatch for $ASSET (got $actual, want $expected)"

# --- install ---------------------------------------------------------------
tar -xzf "$TMP/$ASSET" -C "$TMP"
[ -f "$TMP/dex" ] || err "archive did not contain a dex binary"

mkdir -p "$INSTALL_DIR"
mv "$TMP/dex" "$INSTALL_DIR/dex"
chmod 755 "$INSTALL_DIR/dex"

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*)
    echo "note: $INSTALL_DIR is not in your PATH."
    echo "      add:  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

echo "Installed: $("$INSTALL_DIR/dex" --version)"
echo "Location:  $INSTALL_DIR/dex"
echo
echo "Next: run \`dex doctor\` to see what's resolved, then set a provider key"
echo "(e.g. export ANTHROPIC_API_KEY=...) or edit \$XDG_CONFIG_HOME/dex/config.yaml."
