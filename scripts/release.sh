#!/bin/sh
# Cut a release: tag the current commit and push the tag;
# .github/workflows/release.yml then builds every target from it and
# publishes the GitHub release that scripts/install.sh installs from.
#
# develop is protected (PR reviews + checks, no direct pushes — not even
# for CI), so the version bump must land via a normal PR first: bump
# `version` in Cargo.toml, run `cargo update -p dex`, merge, then tag
# its tip here.
#
# usage: scripts/release.sh [major|minor|patch|X.Y.Z]
#         A bump word is applied to the LAST TAG, not to Cargo.toml: by tag
#         time the version bump has already landed via PR, so Cargo.toml
#         carries the new version. The computed target must match
#         Cargo.toml — otherwise the script aborts and tells you which
#         bump PR to land first.

set -eu
cd "$(dirname "$0")/.."

case "${1:-}" in
major | minor | patch | v[0-9]* | [0-9]*.[0-9]*.[0-9]*) ;;
*)
    echo "usage: scripts/release.sh [major|minor|patch|X.Y.Z]" >&2
    exit 1
    ;;
esac

if ! git diff --quiet -- || ! git diff --cached --quiet; then
    echo "error: working tree not clean — commit or stash first" >&2
    exit 1
fi

git fetch origin --quiet
branch="$(git branch --show-current)"
base="$(git symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null | sed 's|^origin/||')"
base="${base:-develop}"
if [ "$branch" != "$base" ]; then
    echo "warning: tagging from '$branch', not $base. Release tags should" >&2
    echo "point at the $base tip — merge/land your change there first." >&2
elif [ "$(git rev-list --count "HEAD..origin/$base")" != "0" ]; then
    echo "error: local $base is behind origin/$base — git pull first" >&2
    exit 1
fi

CUR="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
case "$1" in
major | minor | patch)
    # The bump word applies to the last tag (the previous release), not to
    # Cargo.toml: the bump PR for THIS release has already landed.
    last="$(git describe --tags --abbrev=0 2>/dev/null || true)"
    last="${last#v}"
    case "$last" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) last="0.0.0" ;;
    esac
    a=${last%%.*}
    rest=${last#*.}
    b=${rest%%.*}
    c=${rest#*.}
    case "$1" in
    major) a=$((a + 1)); b=0; c=0 ;;
    minor) b=$((b + 1)); c=0 ;;
    patch) c=$((c + 1)) ;;
    esac
    NEW="$a.$b.$c"
    ;;
*) NEW="${1#v}" ;;
esac

if [ "$NEW" != "$CUR" ]; then
    echo "error: Cargo.toml says $CUR but the release would be v$NEW." >&2
    echo "develop is protected: land the bump via PR first" >&2
    echo "(version = \"$NEW\" in Cargo.toml + cargo update -p dex), merge it," >&2
    echo "pull, then re-run this script." >&2
    exit 1
fi

if git rev-parse -q --verify "refs/tags/v$NEW" >/dev/null; then
    echo "error: tag v$NEW already exists" >&2
    exit 1
fi

git tag "v$NEW"
git push origin "v$NEW"

echo "tagged v$NEW."
echo "follow the build: https://github.com/arpitsr/dex/actions"
echo "after it finishes: git pull"
