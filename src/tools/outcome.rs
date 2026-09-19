/// A tool result together with whether the call actually succeeded. Success
/// is decided where the exit status is known — never inferred from the
/// output text, which may legitimately contain markers like `[exit 1]`.
/// `diff` carries the display-only git diff for write/edit, captured before
/// the file was mutated; it never reaches the model.
#[derive(Clone, Debug)]
pub(crate) struct ToolOutcome {
    pub(crate) text: String,
    pub(crate) ok: bool,
    pub(crate) diff: Option<String>,
}
