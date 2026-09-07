#!/bin/sh
# Cut a release: the only manual step. Tags the current commit and pushes the
# tag; .github/workflows/release.yml then bumps Cargo.toml/Cargo.lock on the
# default branch (develop) to the tag's version (bot commit), builds every
# target from that commit, and publishes the GitHub release that
# scripts/install.sh installs from.
#
# usage: scripts/release.sh [major|minor|patch|X.Y.Z]
#         The next version is computed from the version in Cargo.toml.

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
    echo "warning: tagging from '$branch', not $base. The CI bump commit is pushed" >&2
    echo "to $base, so the tag should point at its tip (otherwise the push fails)." >&2
elif [ "$(git rev-list --count "HEAD..origin/$base")" != "0" ]; then
    echo "error: local $base is behind origin/$base — git pull first" >&2
    exit 1
fi

CUR="$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)"
case "$1" in
major | minor | patch)
    a=${CUR%%.*}
    rest=${CUR#*.}
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

if git rev-parse -q --verify "refs/tags/v$NEW" >/dev/null; then
    echo "error: tag v$NEW already exists" >&2
    exit 1
fi

git tag "v$NEW"
git push origin "v$NEW"

echo "tagged v$NEW (Cargo.toml still says $CUR locally — CI bumps it on $base)."
echo "follow the build: https://github.com/arpitsr/dex/actions"
echo "after it finishes: git pull"
