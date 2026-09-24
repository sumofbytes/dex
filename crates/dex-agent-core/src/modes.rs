/// The monotone tool gate: `read-only < ask < trusted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionMode {
    /// Permit reads, but reject all mutations and shell commands.
    ReadOnly,
    /// Prompt before writes, edits, and shell commands; reads are free.
    Ask,
    /// Permit every tool without prompting.
    Trusted,
}

impl PermissionMode {
    /// Parse the accepted spellings without performing host-specific logging.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "read-only" | "readonly" => Ok(Self::ReadOnly),
            "ask" | "ask-writes" | "ask-write" | "ask-shell" | "ask-commands" => Ok(Self::Ask),
            "trusted" | "non-interactive" => Ok(Self::Trusted),
            other => Err(format!(
                "invalid permission mode '{}'; use read-only, ask, or trusted",
                other
            )),
        }
    }

    /// Host warning key and message for accepted legacy spellings.
    pub fn deprecated_alias(value: &str) -> Option<(&'static str, &'static str)> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "ask-writes" | "ask-write" => Some((
                "permission-ask-writes",
                "permission mode 'ask-writes' is deprecated — use 'ask'; it now also prompts on shell commands",
            )),
            "ask-shell" | "ask-commands" => Some((
                "permission-ask-shell",
                "permission mode 'ask-shell' is deprecated — use 'ask'; it now also prompts on writes and edits",
            )),
            _ => None,
        }
    }

    pub fn permissiveness(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::Ask => 1,
            Self::Trusted => 2,
        }
    }

    /// Canonical wire spelling. Never emits a deprecated alias.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::Trusted => "trusted",
        }
    }
}

/// User-facing autonomy selector with three named permission levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentMode {
    Plan,
    Manual,
    Auto,
}

impl AgentMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "plan" | "planning" => Ok(Self::Plan),
            "manual" | "default" => Ok(Self::Manual),
            "auto" | "automatic" => Ok(Self::Auto),
            other => Err(format!(
                "invalid mode '{}'; use plan, manual, or auto",
                other
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }

    pub fn permission(self) -> PermissionMode {
        match self {
            Self::Plan => PermissionMode::ReadOnly,
            Self::Manual => PermissionMode::Ask,
            Self::Auto => PermissionMode::Trusted,
        }
    }

    pub fn from_permission(mode: PermissionMode) -> Self {
        match mode {
            PermissionMode::ReadOnly => Self::Plan,
            PermissionMode::Ask => Self::Manual,
            PermissionMode::Trusted => Self::Auto,
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Manual,
            Self::Manual => Self::Auto,
            Self::Auto => Self::Plan,
        }
    }

    pub fn label(self) -> &'static str {
        self.as_str()
    }

    pub fn is_plan(self) -> bool {
        matches!(self, Self::Plan)
    }
}
