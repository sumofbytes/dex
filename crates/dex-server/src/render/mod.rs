//! Headless terminal rendering: theme (palette, markdown line semantics,
//! tree-sitter highlighting) and format (tool previews, approval text,
//! clamping). Shared by the TUI (`ui`), the SSE stream printer (`llm`) and
//! tool-result shaping (`agent`, `session`, `tools`).
//
// With `--no-default-features` (no `tui`) this layer — and everything that
// only the TUI consumes from it — is compiled out via `#[cfg]`, so the
// headless build never links ratatui. Dead-code warnings for TUI-only items
// are therefore expected in that configuration and silenced in bulk here.
#![cfg_attr(not(feature = "tui"), allow(dead_code))]

pub mod format;
pub mod theme;
