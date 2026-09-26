//! Overwritable tool catalog.
//!
//! [`builtin_tools`] + [`merge_tool_schemas`] are the fixed catalog; this
//! registry wraps them so users can add, replace, or remove tools without
//! forking the crate:
//!
//! ```rust
//! use dex_ai::{FunctionDef, ToolDefinition};
//! use dex_coding_agent::ToolRegistry;
//!
//! let mut registry = ToolRegistry::with_builtins(false);
//! registry.remove("bash");
//! registry.register(
//!     ToolDefinition {
//!         tool_type: "function".into(),
//!         function: FunctionDef {
//!             name: "sql".into(),
//!             description: "Run a read-only query.".into(),
//!             parameters: serde_json::json!({"type": "object"}),
//!         },
//!     },
//!     None,
//! );
//! assert!(registry.contains("sql"));
//! assert!(!registry.contains("bash"));
//! ```

use std::collections::BTreeMap;

use dex_ai::ToolDefinition;

use crate::{native_tool_metadata, sort_tool_defs_by_name, ToolMetadata};

/// One catalog entry: schema plus optional native permission metadata.
/// `None` metadata = host-resolved (MCP / extension tools).
#[derive(Clone)]
pub struct ToolEntry {
    pub definition: ToolDefinition,
    pub metadata: Option<ToolMetadata>,
}

impl ToolEntry {
    pub fn new(definition: ToolDefinition, metadata: Option<ToolMetadata>) -> Self {
        Self {
            definition,
            metadata,
        }
    }

    pub fn name(&self) -> &str {
        &self.definition.function.name
    }
}

/// Ordered, overwritable catalog of tool schemas.
///
/// Native tools keep their fixed order; dynamic (host-provided) tools ride
/// in a name-sorted tail so provider request bytes stay deterministic.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    native: Vec<ToolEntry>,
    dynamic: BTreeMap<String, ToolEntry>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry preloaded with [`crate::builtin_tools`].
    pub fn with_builtins(delegation_enabled: bool) -> Self {
        let mut registry = Self::new();
        for definition in crate::builtin_tools(delegation_enabled) {
            let name = definition.function.name.clone();
            let metadata = native_tool_metadata(&name);
            registry.native.push(ToolEntry::new(definition, metadata));
        }
        registry
    }

    /// Insert or replace a dynamic tool by name.
    pub fn register(&mut self, definition: ToolDefinition, metadata: Option<ToolMetadata>) {
        let name = definition.function.name.clone();
        self.dynamic
            .insert(name, ToolEntry::new(definition, metadata));
    }

    /// Replace a native tool's schema in place. Returns false when the
    /// native tool does not exist (use [`register`](Self::register) then).
    pub fn override_native(&mut self, definition: ToolDefinition) -> bool {
        let name = definition.function.name.clone();
        let Some(entry) = self.native.iter_mut().find(|e| e.name() == name) else {
            return false;
        };
        let metadata = native_tool_metadata(&name).or(entry.metadata);
        *entry = ToolEntry::new(definition, metadata);
        true
    }

    /// Remove a tool by name from either section. Returns true when removed.
    pub fn remove(&mut self, name: &str) -> bool {
        if let Some(pos) = self.native.iter().position(|e| e.name() == name) {
            self.native.remove(pos);
            return true;
        }
        self.dynamic.remove(name).is_some()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.native.iter().any(|e| e.name() == name) || self.dynamic.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&ToolEntry> {
        self.native
            .iter()
            .find(|e| e.name() == name)
            .or_else(|| self.dynamic.get(name))
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.native.iter().map(|e| e.name()).collect();
        names.extend(self.dynamic.keys().map(String::as_str));
        names
    }

    pub fn metadata(&self, name: &str) -> Option<ToolMetadata> {
        self.get(name).and_then(|e| e.metadata)
    }

    /// Merged schemas: native order first, dynamic tail sorted by name.
    pub fn schemas(&self) -> Vec<ToolDefinition> {
        let mut tail: Vec<ToolDefinition> = self
            .dynamic
            .values()
            .map(|e| e.definition.clone())
            .collect();
        sort_tool_defs_by_name(&mut tail);
        let mut out: Vec<ToolDefinition> =
            self.native.iter().map(|e| e.definition.clone()).collect();
        out.extend(tail);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_ai::FunctionDef;

    fn def(name: &str) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
        }
    }

    #[test]
    fn register_override_remove_round_trip() {
        let mut registry = ToolRegistry::with_builtins(false);
        assert!(registry.contains("read"));

        assert!(registry.remove("bash"));
        assert!(!registry.contains("bash"));

        registry.register(def("sql"), None);
        assert!(registry.contains("sql"));
        assert!(registry.schemas().iter().any(|d| d.function.name == "sql"));

        let mut custom = def("read");
        custom.function.description = "custom reader".into();
        assert!(registry.override_native(custom));
        assert_eq!(
            registry
                .get("read")
                .unwrap()
                .definition
                .function
                .description,
            "custom reader"
        );
    }

    #[test]
    fn dynamic_tail_stays_sorted() {
        let mut registry = ToolRegistry::with_builtins(false);
        registry.register(def("z_tool"), None);
        registry.register(def("a_tool"), None);
        let schemas = registry.schemas();
        let tail: Vec<&str> = schemas
            .iter()
            .map(|d| d.function.name.as_str())
            .skip_while(|n| *n != "z_tool" && *n != "a_tool")
            .collect();
        assert_eq!(tail, ["a_tool", "z_tool"]);
    }
}
