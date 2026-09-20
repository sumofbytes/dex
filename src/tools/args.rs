//! Shared argument extraction for the leaf tools: one string-arg reader
//! with uniform `ToolError`s. (Was in `then_run.rs`, which it predates and
//! does not belong to.)

use serde_json::{Map, Value};

use super::error::ToolError;

pub(super) fn arg_str(args: &Map<String, Value>, key: &'static str) -> Result<String, ToolError> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ToolError::NotString(key)),
        None => Err(ToolError::Missing(key)),
    }
}
