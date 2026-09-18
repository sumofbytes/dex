use std::io;

#[derive(Debug)]
pub(crate) enum ToolError {
    Missing(&'static str),
    NotString(&'static str),
    InvalidArgument(String),
    Io(io::Error),
    EditNotUnique(usize),
    OutsideWorkspace(String),
    /// A write/edit supplied an `expected_hash` that no longer matches the
    /// file on disk (someone else changed it since the model's last read).
    /// The caller must re-read and retry — the write is not applied.
    StaleFile {
        path: String,
        expected: String,
        actual: String,
    },
    /// A shell command ran but signalled failure (non-zero exit, killed, or
    /// timed out). `code` is `None` when the process never exited on its own.
    /// Carries the combined output so partial results still reach the model.
    Shell {
        output: String,
        code: Option<i32>,
    },
    /// An internal tool-engine failure (not a bad invocation, not a shell
    /// exit): e.g. the fff index failed to initialize.
    Internal(String),
    /// The permission policy refused the call before it ran: a read-only
    /// rejection, a user deny, an unanswered approval (approver gone), or
    /// approval needed with no channel to ask on. Never inferred from tool
    /// output — decided by the Phase 0 gate in `execute`.
    Denied(String),
    Unknown(String),
}

impl From<crate::workspace::WorkspaceError> for ToolError {
    fn from(e: crate::workspace::WorkspaceError) -> Self {
        match e {
            crate::workspace::WorkspaceError::Io(io) => Self::Io(io),
            crate::workspace::WorkspaceError::OutsideWorkspace(p) => Self::OutsideWorkspace(p),
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(k) => write!(f, "missing argument '{}'", k),
            Self::NotString(k) => write!(f, "argument '{}' must be a string", k),
            Self::InvalidArgument(message) => write!(f, "{}", message),
            Self::Io(e) => write!(f, "io error: {}", e),
            Self::EditNotUnique(n) => write!(
                f,
                "oldText matches {n} locations; include more surrounding lines to make it unique, or pass replaceAll: true"
            ),
            Self::OutsideWorkspace(path) => write!(f, "path is outside the workspace: {}", path),
            Self::StaleFile {
                path,
                expected,
                actual,
            } => write!(
                f,
                "file changed since it was read (expected_hash mismatch: expected {expected}, file is {actual}) — re-read {} and retry; concurrent edit wins, your write was not applied",
                path
            ),
            Self::Shell { output, code } => match code {
                Some(code) => write!(f, "{output}\n[exit {code}]"),
                None => write!(f, "{output}"),
            },
            Self::Internal(e) => write!(f, "{e}"),
            Self::Denied(message) => write!(f, "permission denied: {message}"),
            Self::Unknown(t) => write!(f, "unknown tool '{}'", t),
        }
    }
}
