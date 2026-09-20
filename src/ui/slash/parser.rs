/// One documented slash command. The single source of truth for the popup
/// (`compute_suggestions`) and for `/help` in both the local and remote entry
/// points, so neither list can drift. `command` is what the popup completes
/// to; `usage` is the form the `/help` line prints (empty to omit, e.g. the
/// `/mcp help` sub-form already covered by `/mcp`); `description` is the
/// popup row hint.
pub(crate) struct SlashCommandSpec {
    pub command: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

pub(crate) const COMMANDS: &[SlashCommandSpec] = &[
    SlashCommandSpec {
        command: "/quit",
        usage: "/quit",
        description: "Exit the REPL",
    },
    SlashCommandSpec {
        command: "/clear",
        usage: "/clear",
        description: "Clear conversation history",
    },
    SlashCommandSpec {
        command: "/new",
        usage: "/new",
        description: "Start a new session",
    },
    SlashCommandSpec {
        command: "/session",
        usage: "/session",
        description: "Show current session details",
    },
    SlashCommandSpec {
        command: "/resume",
        usage: "/resume [index|path]",
        description: "List or resume a session",
    },
    SlashCommandSpec {
        command: "/mode",
        usage: "/mode [plan|manual|auto]",
        description: "Show or set the agent mode",
    },
    SlashCommandSpec {
        command: "/mcp",
        usage: "/mcp",
        description: "Show MCP server status and auth",
    },
    SlashCommandSpec {
        command: "/extensions",
        usage: "/extensions",
        description: "Show loaded Lua extensions (`/extensions reload` rescans)",
    },
    SlashCommandSpec {
        command: "/mcp help",
        usage: "",
        description: "MCP OAuth help (login/logout run in the CLI)",
    },
    SlashCommandSpec {
        command: "/name",
        usage: "/name <n>",
        description: "Rename the current session",
    },
    SlashCommandSpec {
        command: "/model",
        usage: "/model [<m>]",
        description: "Show or switch the model",
    },
    SlashCommandSpec {
        command: "/provider",
        usage: "/provider [<name>]",
        description: "Show or switch the provider",
    },
    SlashCommandSpec {
        command: "/thinking",
        usage: "/thinking [<level>|clear]",
        description: "Show or set reasoning effort",
    },
    SlashCommandSpec {
        command: "/waive <reason>",
        usage: "/waive <reason>",
        description: "Waive verification with a reason",
    },
    SlashCommandSpec {
        command: "/undo",
        usage: "/undo",
        description: "Undo the last recorded file change",
    },
    SlashCommandSpec {
        command: "/help",
        usage: "/help",
        description: "Show available commands",
    },
];

/// Does the line name a registered extension command? `/name` or
/// `/name args...`.
pub(crate) fn is_extension_command(line: &str) -> bool {
    let word = line[1..].split_whitespace().next().unwrap_or_default();
    crate::extensions::command_list()
        .iter()
        .any(|(_, name, _)| name == word)
}

/// Split `/name args...` into (extension, name, rest).
pub(crate) fn split_extension_command(line: &str) -> (String, String, String) {
    let mut parts = line[1..].splitn(2, ' ');
    let name = parts.next().unwrap_or_default().to_string();
    let arg = parts.next().unwrap_or_default().trim().to_string();
    crate::extensions::command_list()
        .into_iter()
        .find(|(_, n, _)| *n == name)
        .map(|(ext, _, _)| (ext, name, arg))
        .unwrap_or_default()
}
