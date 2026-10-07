//! Thin re-export: tool path resolution lives in `crate::workspace`.

pub(crate) use crate::workspace::{
    normalize_conflict_path, resolve_path, resolve_workspace_path, tool_path, workspace_root,
};
