//! Thin re-export: file helpers live in `crate::workspace`.

pub(crate) use crate::workspace::{cached_parse, fnv_bytes, unique_tmp_path, xdg_path, FileCache};
