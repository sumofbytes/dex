//! Thin re-export: workspace confinement lives in `crate::workspace`.

pub(crate) use crate::workspace::{
    normalize_conflict_path, readable_granted_path, resolve_workspace_path, workspace_path,
    workspace_root,
};
