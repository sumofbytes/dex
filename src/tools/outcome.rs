/// A tool result together with whether the call actually succeeded. Success
/// is decided where the exit status is known — never inferred from the
/// output text, which may legitimately contain markers like `[exit 1]`.
/// `diff` carries the display-only git diff for write/edit, captured before
/// the file was mutated; it never reaches the model.
/// Out-of-band shell facts a tool result carries beside its text: the
/// archive id `evidence_reducer::capture` stored for the raw output, and the
/// exit code it observed. Both travel outside the text on purpose — untrusted
/// tool output must not be able to point verification at a different
/// archive, or re-label a run's outcome. `None` for tools that ran no shell
/// command (or ran one without the reducer gate on, in the id's case).
#[derive(Clone, Debug)]
pub(crate) struct ShellEvidence {
    pub(crate) archive_id: Option<String>,
    pub(crate) exit_code: Option<i32>,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolOutcome {
    pub(crate) text: String,
    pub(crate) ok: bool,
    pub(crate) diff: Option<String>,
    pub(crate) shell: Option<ShellEvidence>,
}
