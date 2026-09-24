use crate::protocol::PermissionMode;

use std::env;

pub(crate) fn permission_from_env() -> Result<PermissionMode, Box<dyn std::error::Error>> {
    let value = env::var("DEX_PERMISSION")
        .ok()
        .unwrap_or_else(|| "trusted".to_string());
    crate::protocol::parse_permission_mode(&value).map_err(Into::into)
}
