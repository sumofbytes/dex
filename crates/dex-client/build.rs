//! Single source of version truth: the workspace root `[package]` version
//! (the CLI release), not this crate's own `CARGO_PKG_VERSION`. Emitted as
//! `DEX_VERSION` so every crate sharing `dex-runtime` reports the release
//! version in `dex doctor`, `dex update`, User-Agents, and MCP handshakes.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest_dir).join("../../Cargo.toml");
    let text = fs::read_to_string(&root).unwrap_or_else(|e| panic!("read {}: {e}", root.display()));
    // First `version =` in the file is the root `[package]` version.
    let version = text
        .lines()
        .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
        .unwrap_or_else(|| panic!("no [package] version in {}", root.display()));
    println!("cargo:rustc-env=DEX_VERSION={version}");
}
