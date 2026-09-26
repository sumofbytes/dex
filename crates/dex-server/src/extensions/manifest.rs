//! `manifest.yaml` parse + validation for Lua extensions.
//!
//! The manifest is parsed with `serde_yaml` and never executed. It declares
//! the extension identity, the capabilities the code may use (the engine
//! withholds `dex.*` subtrees that are not declared), and the tool schemas.

use serde::Deserialize;
use std::collections::BTreeMap;

/// Manifest-format version. Bumped only when the shape below changes; the
/// extension's own `version:` is independent.
pub const MANIFEST_VERSION: u32 = 1;

/// Slots a manifest may declare under `components:` (spec §31): the
/// middleware-wrappable harness slots plus `agent_loop` (which the Lua side
/// registers via `dex.replace`, not `dex.use`). Kept here so manifest
/// validation needs no engine types; a test pins this list against the
/// engine's `WRAPPABLE_SLOTS` so the two cannot drift.
pub const COMPONENT_SLOTS: &[&str] = &[
    "model_selector",
    "harness.summarize",
    "harness.compact",
    "harness.overflow",
    "harness.conflict",
    "tool_catalog",
    "agent_loop",
];

/// Hard cap for per-tool `timeout:` (plan §6.3).
pub const MAX_TOOL_TIMEOUT_SECS: u64 = 120;

/// Default tool timeout when the manifest omits it (plan §6.3).
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 30;

/// Capabilities the engine understands. Unknown entries are rejected so a
/// typo fails loudly at load instead of silently granting nothing. `model`
/// exposes the current model + its credentials (`dex.model`); `net` allows
/// HTTP confined to that model's own endpoint (`dex.net.fetch`) and
/// requires `model` (confinement needs the endpoint); `net.providers`
/// additionally widens the confinement to the configured provider
/// endpoints (each with its own key), gates `dex.model.auth("<provider>")`
/// (cross-provider keys ride the same capability), and requires `net`.
const KNOWN_CAPABILITIES: &[&str] = &[
    "tools",
    "tools.override",
    "workspace.read",
    "model",
    "net",
    "net.providers",
    "harness",
    // agent_loop: replaces the turn loop via `dex.replace("agent_loop", …)`
    // (spec §9) — the whole orchestration, so it is its own capability.
    "agent_loop",
];

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestTool {
    pub name: String,
    pub description: String,
    #[serde(default = "default_parameters")]
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub timeout: Option<u64>,
}

fn default_parameters() -> serde_json::Value {
    serde_json::json!({"type": "object"})
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
    /// Harness compatibility: `dex = ">=0.15"` pins the minimum harness
    /// that can serve this extension (new events/capabilities). Checked at
    /// load against the harness crate version — a mismatch skips the whole
    /// extension loudly instead of running it against events it never saw.
    #[serde(default)]
    pub dex: Option<String>,
    /// Fail-closed hooks: a `tool.before` error denies the call instead of
    /// being skipped (default fail-open, plan §8). Declared here so the
    /// enforcement posture is install-time consent, not runtime surprise.
    #[serde(default)]
    pub strict: bool,
    /// Declarative component files (spec §31): slot name → `.lua` file
    /// relative to the extension dir. Each file is loaded after
    /// `extension.lua` with the same `return function(dex) … end` contract
    /// and registers itself (`dex.use` / `dex.replace` / `dex.wrap`).
    /// Validated at parse time: known slot, confined path, `harness`
    /// capability required.
    #[serde(default)]
    pub components: BTreeMap<String, String>,
}

pub fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.contains("__")
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Parse and validate a manifest. `Err(String)` keeps serde errors out of
/// the signature; callers prefix with the extension dir.
pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let manifest: Manifest =
        serde_yaml::from_str(text).map_err(|e| format!("invalid manifest.yaml: {e}"))?;
    if manifest.manifest_version != MANIFEST_VERSION {
        return Err(format!(
            "unsupported manifest_version {} (want {MANIFEST_VERSION})",
            manifest.manifest_version
        ));
    }
    if !valid_segment(&manifest.id) {
        return Err(format!(
            "invalid id '{}': use [a-z0-9_-]+, max 64 chars, no `__`",
            manifest.id
        ));
    }
    if manifest.version.is_empty() || manifest.version.len() > 32 {
        return Err("version must be 1-32 chars".to_string());
    }
    for cap in &manifest.capabilities {
        if !KNOWN_CAPABILITIES.contains(&cap.as_str()) {
            return Err(format!(
                "unknown capability '{cap}' (known: {})",
                KNOWN_CAPABILITIES.join(", ")
            ));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for tool in &manifest.tools {
        if !valid_segment(&tool.name) {
            return Err(format!(
                "invalid tool name '{}': use [a-z0-9_-]+, max 64 chars, no `__`",
                tool.name
            ));
        }
        if !seen.insert(tool.name.clone()) {
            return Err(format!("duplicate tool '{}'", tool.name));
        }
        if tool.description.is_empty() || tool.description.len() > 1024 {
            return Err(format!(
                "tool '{}': description must be 1-1024 chars",
                tool.name
            ));
        }
        if !tool.parameters.is_object() {
            return Err(format!(
                "tool '{}': parameters must be a JSON object schema",
                tool.name
            ));
        }
        if let Some(timeout) = tool.timeout {
            if timeout == 0 || timeout > MAX_TOOL_TIMEOUT_SECS {
                return Err(format!(
                    "tool '{}': timeout must be 1-{MAX_TOOL_TIMEOUT_SECS}s",
                    tool.name
                ));
            }
        }
    }
    if !manifest.capabilities.contains(&"tools".to_string()) && !manifest.tools.is_empty() {
        return Err("manifest declares tools without the 'tools' capability".to_string());
    }
    if manifest.has_capability("net") && !manifest.has_capability("model") {
        return Err("capability 'net' requires the 'model' capability (fetch is confined to the model's endpoint)".to_string());
    }
    if manifest.has_capability("net.providers") && !manifest.has_capability("net") {
        return Err("capability 'net.providers' requires the 'net' capability".to_string());
    }
    if !manifest.components.is_empty() && !manifest.has_capability("harness") {
        return Err("manifest declares components without the 'harness' capability".to_string());
    }
    let mut seen_files = std::collections::BTreeSet::new();
    for (slot, file) in &manifest.components {
        if !COMPONENT_SLOTS.contains(&slot.as_str()) {
            return Err(format!(
                "unknown component slot '{slot}' (known: {})",
                COMPONENT_SLOTS.join(", ")
            ));
        }
        // Confined relative path: the loader joins it onto the extension
        // dir, so `..`, absolute paths, and empties are rejected here.
        if file.is_empty()
            || file.starts_with('/')
            || file.split(['/', '\\']).any(|seg| seg == "..")
        {
            return Err(format!(
                "component '{slot}': path {file:?} must be relative to the extension dir"
            ));
        }
        if !file.ends_with(".lua") {
            return Err(format!("component '{slot}': {file:?} must be a .lua file"));
        }
        // The same file declared twice would load twice and double-register.
        if !seen_files.insert(file.as_str()) {
            return Err(format!("component '{slot}': {file:?} declared twice"));
        }
    }
    if let Some(req) = manifest.dex.as_deref() {
        check_dex_compat(req)?;
    }
    Ok(manifest)
}

/// The harness version serving this extension (the `dex-server` crate
/// version — the code that owns the event surface, not the CLI release).
pub fn harness_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Check a manifest `dex = ">=a.b[.c]"` pin against the running harness.
/// Only `>=` pins are accepted (an extension names the minimum it needs);
/// anything else — or a newer harness requirement — is a loud load-time
/// skip, never a silent run against unknown events.
fn check_dex_compat(req: &str) -> Result<(), String> {
    let min = req
        .trim()
        .strip_prefix(">=")
        .ok_or_else(|| format!("unsupported dex pin {req:?}: use '>=<major>.<minor>[.<patch>]'"))?;
    let mut need = min.split('.');
    let parse_part = |part: Option<&str>| -> Result<u64, String> {
        part.unwrap_or("0")
            .parse::<u64>()
            .map_err(|_| format!("unsupported dex pin {req:?}: use '>=<major>.<minor>[.<patch>]'"))
    };
    let need = (
        parse_part(need.next())?,
        parse_part(need.next())?,
        parse_part(need.next())?,
    );
    let mut have = harness_version().split('.');
    let have = (
        parse_part(have.next())?,
        parse_part(have.next())?,
        parse_part(have.next())?,
    );
    if have < need {
        return Err(format!(
            "extension needs dex harness >={}.{}.{} (running {})",
            need.0,
            need.1,
            need.2,
            harness_version()
        ));
    }
    Ok(())
}

impl Manifest {
    /// Stable display form for errors (never empty: `id` is validated).
    pub fn id_for_error(&self) -> &str {
        &self.id
    }

    pub fn has_capability(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }

    pub fn declares_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }

    pub fn timeout_secs(&self, tool: &str) -> u64 {
        self.tools
            .iter()
            .find(|t| t.name == tool)
            .and_then(|t| t.timeout)
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> String {
        r#"
manifest_version: 1
id: my-ext
version: 0.1.0
capabilities: [tools, workspace.read]
tools:
  - name: create_issue
    description: File an issue.
    parameters: {"type": "object"}
"#
        .to_string()
    }

    #[test]
    fn accepts_valid_manifest() {
        let m = parse_manifest(&base()).unwrap();
        assert_eq!(m.id, "my-ext");
        assert_eq!(m.timeout_secs("create_issue"), DEFAULT_TOOL_TIMEOUT_SECS);
    }

    #[test]
    fn rejects_bad_version_and_id() {
        assert!(
            parse_manifest(&base().replace("manifest_version: 1", "manifest_version: 2")).is_err()
        );
        assert!(parse_manifest(&base().replace("id: my-ext", "id: Bad Name!")).is_err());
    }

    #[test]
    fn rejects_double_underscore_segments() {
        // `__` is the `ext__<ext>__<tool>` delimiter: allowing it would mint
        // names `split_ext_name` cannot dispatch.
        assert!(!valid_segment("my__ext"));
        assert!(!valid_segment("my__tool"));
        assert!(parse_manifest(&base().replace("id: my-ext", "id: my__ext")).is_err());
        assert!(parse_manifest(&base().replace("name: create_issue", "name: bad__tool")).is_err());
    }

    #[test]
    fn rejects_unknown_capability() {
        assert!(
            parse_manifest(&base().replace("workspace.read]", "workspace.read, bogus]")).is_err()
        );
    }

    #[test]
    fn tools_require_tools_capability() {
        let no_cap = base().replace("capabilities: [tools, workspace.read]", "capabilities: []");
        assert!(parse_manifest(&no_cap).is_err());
    }

    #[test]
    fn net_requires_model_capability() {
        let net_only = base().replace(
            "capabilities: [tools, workspace.read]",
            "capabilities: [tools, net]",
        );
        let err = parse_manifest(&net_only).unwrap_err();
        assert!(err.contains("'net' requires the 'model'"), "got: {err}");
        let both = base().replace(
            "capabilities: [tools, workspace.read]",
            "capabilities: [tools, model, net]",
        );
        assert!(parse_manifest(&both).is_ok());
    }

    #[test]
    fn harness_capability_is_known() {
        let m = parse_manifest(
            "manifest_version: 1\nid: hook-ext\nversion: 0.1.0\ncapabilities: [harness]\n",
        )
        .unwrap();
        assert!(m.has_capability("harness"));
    }

    #[test]
    fn dex_pin_accepts_compatible_and_rejects_newer_or_garbage() {
        let base = "manifest_version: 1\nid: pin-ext\nversion: 0.1.0\ncapabilities: []\n";
        // Current or older floor: loads.
        assert!(parse_manifest(&format!("{base}dex: \">={}\"\n", harness_version())).is_ok());
        assert!(parse_manifest(&format!("{base}dex: \">=0.0\"\n")).is_ok());
        // Newer floor than the running harness: loud skip.
        let err = parse_manifest(&format!("{base}dex: \">=999.0\"\n")).unwrap_err();
        assert!(err.contains("needs dex harness"), "got: {err}");
        // Only `>=` pins exist.
        assert!(parse_manifest(&format!("{base}dex: \"==1.0\"\n")).is_err());
        assert!(parse_manifest(&format!("{base}dex: \"hello\"\n")).is_err());
    }

    #[test]
    fn components_parse_and_validate() {
        let base = "manifest_version: 1\nid: comp-ext\nversion: 0.1.0\ncapabilities: [harness]\n";
        let with = |comp: &str| format!("{base}components:\n{comp}");
        let ok = parse_manifest(&with("  model_selector: router.lua\n")).unwrap();
        assert_eq!(ok.components.get("model_selector").unwrap(), "router.lua");
        // Subdirectories are fine.
        assert!(parse_manifest(&with("  harness.summarize: comp/sum.lua\n")).is_ok());
        // Unknown slot.
        let err = parse_manifest(&with("  summarizer: s.lua\n")).unwrap_err();
        assert!(err.contains("unknown component slot"), "got: {err}");
        // Escaping / absolute / non-lua paths.
        for bad in [
            "  model_selector: ../evil.lua\n",
            "  model_selector: /etc/x.lua\n",
            "  model_selector: router.txt\n",
        ] {
            assert!(parse_manifest(&with(bad)).is_err(), "{bad}");
        }
        // Same file twice: would load twice.
        let err =
            parse_manifest(&with("  model_selector: r.lua\n  tool_catalog: r.lua\n")).unwrap_err();
        assert!(err.contains("declared twice"), "got: {err}");
        // Components are harness surface: capability required.
        let no_cap = base.replace("capabilities: [harness]", "capabilities: []");
        assert!(
            parse_manifest(&format!("{no_cap}components:\n  model_selector: r.lua\n")).is_err()
        );
    }
}
