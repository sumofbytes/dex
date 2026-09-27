# Releasing

How `dex` releases are cut and shipped. Part of the
[dex README](../README.md) — for contributing changes, see
[CONTRIBUTING.md](../CONTRIBUTING.md).

Creating the `v*` tag is the only manual step; CI does the rest:

```sh
scripts/release.sh patch    # tag the next version — also major, minor, or 1.2.3
# or by hand:
git tag v0.1.1 && git push origin v0.1.1
```

`.github/workflows/release.yml` then: bumps `Cargo.toml`/`Cargo.lock` on the
default branch (`develop`) to the tag's version (github-actions bot commit) →
builds all targets from that commit → smoke tests → attaches tarballs +
`SHA256SUMS` to the GitHub release. The install script resolves the latest
release and picks the right asset for the running machine. `git pull` afterwards
to pick up the version bump.

If the default branch is protected, let GitHub Actions push to it (Settings →
Branches → add the Actions bot as bypass), since the bump commit is written by
CI.
