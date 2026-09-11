//! `dex update` — self-update from GitHub releases.
//!
//! Bare `dex update` downloads the latest released build for this platform
//! and replaces the running binary in place. `--models` keeps the old
//! catalog-refresh behavior; `--all` does both.
//!
//! The flow mirrors `scripts/install.sh`: resolve latest via the
//! `/releases/latest` redirect (no API call, so no rate limits), fetch
//! `dex-vX.Y.Z-<target>.tar.gz` + `SHA256SUMS`, verify sha256, then atomically
//! rename the verified binary over the running exe (POSIX allows renaming
//! over a running image; the old inode stays alive for in-flight processes).

use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub(crate) const DEFAULT_REPO: &str = "arpitsr/dex";

/// Sync entry point for `dex update`: blocks on the shared runtime.
pub(crate) fn self_update() -> Result<String, String> {
    crate::client::http::block_on(self_update_async())
}

pub(crate) async fn self_update_async() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate the running binary: {e}"))?
        .canonicalize()
        .map_err(|e| format!("cannot locate the running binary: {e}"))?;
    check_install_source(&exe)?;
    let target = release_target()?;
    let repo = env::var("DEX_REPO")
        .ok()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string());

    let client = reqwest::Client::builder()
        .user_agent(crate::client::http::USER_AGENT)
        // Generous total: a release tarball is tens of MB on slow links.
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    // DEX_VERSION (same env as install.sh) pins a version: it installs that
    // release even when it is older, i.e. it can downgrade/force-reinstall.
    let pinned = env::var("DEX_VERSION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v != "latest");

    let (tag, latest) = match &pinned {
        Some(v) => {
            let tag = if v.starts_with('v') {
                v.clone()
            } else {
                format!("v{v}")
            };
            let version = parse_version(&tag)
                .ok_or_else(|| format!("DEX_VERSION '{v}' is not a vX.Y.Z version"))?;
            (tag, version)
        }
        None => {
            // Follow the /releases/latest redirect; the tag is the last path
            // segment of the URL we landed on.
            let resp = client
                .get(format!("https://github.com/{repo}/releases/latest"))
                .send()
                .await
                .map_err(|e| format!("cannot check the latest release: {e}"))?;
            let resp = resp.error_for_status().map_err(|e| {
                format!("cannot check the latest release (none published yet?): {e}")
            })?;
            let tag = resp
                .url()
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .unwrap_or_default()
                .to_string();
            let version = parse_version(&tag)
                .ok_or_else(|| format!("no valid vX.Y.Z release found for {repo} (got '{tag}')"))?;
            (tag, version)
        }
    };

    let current = parse_version(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is semver");
    if pinned.is_none() && latest <= current {
        return Ok(format!(
            "dex {} is up to date (latest release {tag})",
            env!("CARGO_PKG_VERSION")
        ));
    }

    // Download, verify, extract, install. The temp dir is removed on every
    // path; the staged copy in the install dir is removed on rename failure.
    let dir = temp_download_dir()?;
    let outcome = async {
        let base = format!("https://github.com/{repo}/releases/download/{tag}");
        let asset = asset_name(&tag, target);
        let archive = dir.join(&asset);

        let bytes = fetch_bytes(&client, &format!("{base}/{asset}")).await?;
        fs::write(&archive, &bytes)
            .map_err(|e| format!("cannot write {}: {e}", archive.display()))?;
        let sums = fetch_bytes(&client, &format!("{base}/SHA256SUMS")).await?;
        let expected = checksum_for(&String::from_utf8_lossy(&sums), &asset)
            .ok_or_else(|| format!("no SHA256SUMS entry for {asset}"))?;
        let actual = sha256_hex(&bytes);
        if actual != expected {
            return Err(format!(
                "checksum mismatch for {asset} (got {actual}, want {expected})"
            ));
        }

        // System tar, same assumption install.sh already makes; avoids adding
        // flate2 + tar crates just for this.
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&dir)
            .status()
            .map_err(|e| format!("cannot run tar: {e}"))?;
        if !status.success() {
            return Err("tar extraction failed".to_string());
        }
        let new_bin = dir.join("dex");
        if !new_bin.is_file() {
            return Err(format!("archive did not contain a dex binary ({asset})"));
        }
        install(&new_bin, &exe)
    }
    .await;
    let _ = fs::remove_dir_all(&dir);
    outcome.map(|installed| {
        format!(
            "dex {} -> {tag} — updated at {installed}",
            env!("CARGO_PKG_VERSION")
        )
    })
}

/// Download a URL into bytes; the message names the URL like install.sh does.
async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download failed ({url}): {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("download failed ({url}): {e}"))?;
    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("download failed ({url}): {e}"))?;
    Ok(body.to_vec())
}

/// Move the verified binary into place: copy to a uniquely named staged file
/// next to the exe, chmod 755, rename over the running binary.
fn install(new_bin: &Path, exe: &Path) -> Result<String, String> {
    let dir = exe
        .parent()
        .ok_or_else(|| "binary has no parent directory".to_string())?;
    let staged = dir.join(format!(".dex.new-{}", std::process::id()));
    let _ = fs::remove_file(&staged);
    fs::copy(new_bin, &staged).map_err(|e| {
        format!(
            "cannot stage the new binary in {} (is the install directory writable?): {e}",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&staged)
            .map_err(|e| format!("cannot stat {}: {e}", staged.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&staged, perms)
            .map_err(|e| format!("cannot chmod {}: {e}", staged.display()))?;
    }
    fs::rename(&staged, exe).map_err(|e| {
        let _ = fs::remove_file(&staged);
        format!("cannot replace {}: {e}", exe.display())
    })?;
    Ok(exe.display().to_string())
}

/// Refuse to touch binaries we do not manage: replacing a dev build from a
/// release would silently lie about its version, and replacing a
/// cargo-installed binary would leave `~/.cargo/.crates.toml` stale.
fn check_install_source(exe: &Path) -> Result<(), String> {
    if cfg!(target_os = "windows") {
        return Err(
            "self-update is not supported on Windows — re-run the installer or download the .zip \
             from https://github.com/arpitsr/dex/releases"
                .to_string(),
        );
    }
    if exe.components().any(|c| c.as_os_str() == "target") {
        return Err(
            "this is a dev build (running from target/) — update via git pull + cargo build"
                .to_string(),
        );
    }
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    if let Some(cargo_home) = cargo_home {
        if exe.starts_with(cargo_home.join("bin")) {
            if let Ok(text) = fs::read_to_string(cargo_home.join(".crates.toml")) {
                if crates_toml_tracks_dex(&text) {
                    return Err("installed via cargo — update with `cargo install`".to_string());
                }
            }
        }
    }
    Ok(())
}

/// Does `~/.cargo/.crates.toml` (v1 or v2 format) track a package named dex?
fn crates_toml_tracks_dex(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line == "name = \"dex\"" || line.starts_with("\"dex ")
    })
}

/// Release target triple for this build, matching `release.yml`'s matrix.
fn release_target() -> Result<&'static str, String> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Ok("x86_64-unknown-linux-musl")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Ok("aarch64-unknown-linux-musl")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok("x86_64-apple-darwin")
    } else {
        Err(
            "self-update is not available for this platform — build from source or download a \
             release from https://github.com/arpitsr/dex/releases"
                .to_string(),
        )
    }
}

/// `dex-vX.Y.Z-<target>.tar.gz`, the asset name `release.yml` produces.
fn asset_name(tag: &str, target: &str) -> String {
    format!("dex-{tag}-{target}.tar.gz")
}

/// Parse `vX.Y.Z` (or bare `X.Y.Z`) into a comparable tuple; pre-release
/// tags do not parse (we only publish plain vX.Y.Z).
fn parse_version(tag: &str) -> Option<(u64, u64, u64)> {
    let version = tag.trim().trim_start_matches('v');
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Expected sha256 for `asset` in a `sha256sum`-format file (`<hash>  <name>`).
fn checksum_for(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        let name = fields.next()?;
        (name == asset).then(|| hash.to_string())
    })
}

fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Fresh unique temp dir for this run: a shared name would let two
/// concurrent `dex update` processes extract into each other's files.
fn temp_download_dir() -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = env::temp_dir().join(format!("dex-update-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create temp dir {}: {e}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_compares_versions() {
        assert_eq!(parse_version("v0.4.2"), Some((0, 4, 2)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v1.0.0-rc1"), None);
        assert_eq!(parse_version("nope"), None);
        assert!(parse_version("v0.4.2") > parse_version("v0.4.1"));
    }

    #[test]
    fn checksum_parsing_matches_sha256sum_format() {
        let sums = "abc123  dex-v0.4.2-x86_64-unknown-linux-musl.tar.gz\n\
                    def456  dex-v0.4.2-aarch64-apple-darwin.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "dex-v0.4.2-x86_64-unknown-linux-musl.tar.gz").as_deref(),
            Some("abc123")
        );
        assert_eq!(checksum_for(sums, "missing.tar.gz"), None);
    }

    #[test]
    fn asset_name_matches_release_yml() {
        let target = release_target().expect("this build's platform must be supported");
        assert_eq!(
            asset_name("v1.2.3", target),
            format!("dex-v1.2.3-{target}.tar.gz")
        );
    }

    #[test]
    fn crates_toml_v1_and_v2_are_recognized() {
        assert!(crates_toml_tracks_dex(
            "[[package]]\nname = \"dex\"\nversion = \"0.4.0\"\n"
        ));
        assert!(crates_toml_tracks_dex(
            "[v2]\n\"dex 0.4.0 (registry+https://github.com/rust-lang/crates.io-index)\" = [\"dex\"]\n"
        ));
        // "indexed" contains "dex" — substring matching must not fire.
        assert!(!crates_toml_tracks_dex("name = \"indexed\"\n"));
        assert!(!crates_toml_tracks_dex(""));
    }
}
