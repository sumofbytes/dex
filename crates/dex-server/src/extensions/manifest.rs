//! `manifest.yaml` parse + validation for Lua extensions.
//!
//! The manifest is parsed with `serde_yaml` and never executed. It declares
//! the extension identity, the capabilities the code may use (the engine
//! withholds `dex.*` subtrees that are not declared), and the tool schemas.

use serde::Deserialize;

/// Manifest-format version. Bumped only when the shape below changes; the
/// extension's own `version:` is independent.
pub const MANIFEST_VERSION: u32 = 1;

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
    /// Fail-closed hooks: a `tool.before` error denies the call instead of
    /// being skipped (default fail-open, plan §8). Declared here so the
    /// enforcement posture is install-time consent, not runtime surprise.
    #[serde(default)]
    pub strict: bool,
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
    Ok(manifest)
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
}
