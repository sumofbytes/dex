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

# fetch without following redirects and without a body: reads the
# /releases/latest Location without downloading anything.
fetch_redirect() {
    curl -s --connect-timeout 15 --retry 3 --retry-delay 2 -o /dev/null "$@"
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
linux)
    # Release binaries are glibc (the GUI needs dlopen); a static/musl distro cannot run them.
    if (ldd --version 2>&1 || true) | grep -qi musl; then
        err "musl-based Linux (e.g. Alpine) is not supported by the release binaries. Build from source: cargo install --git https://github.com/$REPO"
    fi
    TARGET="${arch}-unknown-linux-gnu" ;;
darwin) TARGET="${arch}-apple-darwin" ;;
esac

# --- resolve version -------------------------------------------------------
if [ "$VERSION" = "latest" ]; then
    # Read the Location of the /releases/latest redirect (no -L, no API call,
    # so no rate limits). Fall back to the API if the redirect is not a tag URL
    # (proxies/mirrors can answer 200 or rewrite it).
    # Read the Location of the /releases/latest redirect (no -L, no API call,
    # so no rate limits). Fall back to the API if the redirect is not a tag URL
    # (proxies/mirrors can answer 200 or rewrite it; the API call can hit
    # rate limits, unlike the redirect).
    loc="$(fetch_redirect -w '%{redirect_url}' \
        "https://github.com/$REPO/releases/latest" 2>/dev/null || true)"
    case "$loc" in
    */releases/tag/*)
        VERSION="${loc##*/tag/}"
        # A rewriting proxy can append a query or fragment after the tag.
        VERSION="${VERSION%%\?*}"
        VERSION="${VERSION%%\#*}"
        ;;
    *)
        VERSION="$(fetch "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null |
            grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n1 | cut -d'"' -f4)" || true
        [ -n "$VERSION" ] || err "cannot determine latest release (none published yet?); pass a version, e.g. sh -s -- 0.30.0"
        ;;
    esac
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
