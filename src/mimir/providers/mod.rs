//! Mimir provider adapters: each translates a native wire format into the
//! provider-neutral `nexus::ChatProvider` streaming contract.

use std::path::Path;

pub(crate) mod anthropic_messages;
pub(crate) mod antigravity;
pub(crate) mod openai_codex_responses;
mod transport;

/// Build the shared Iris system-prompt body handed to every provider. The text
/// is provider-neutral (it describes the Iris tool surface); a provider may wrap
/// it in its own envelope (e.g. Anthropic prepends the required Claude Code
/// identity block as system block 0).
pub(super) fn build_iris_system_prompt(workspace: &Path) -> String {
    let prompt_cwd = workspace.display().to_string().replace('\\', "/");
    let tools = [
        "- read: Read the contents of a file by path, with optional line offset and limit.",
        "- bash: Execute a bash command in the current workspace.",
        "- edit: Edit an existing file by replacing text.",
        "- write: Create or overwrite a file, creating parent directories as needed.",
        "- grep: Search files for a regex or literal pattern.",
        "- find: Find files by glob pattern.",
        "- ls: List directory entries.",
    ]
    .join("\n");

    format!(
        "You are an expert coding assistant operating inside Iris, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\n\
Available tools:\n{tools}\n\n\
No other tools are available. Do not assume Codex CLI/native agent tools, multi_tool wrappers, subagents, or hidden parallel tool APIs exist.\n\n\
Guidelines:\n\
- Prefer read, grep, find, and ls for file inspection; use bash for shell commands and verification.\n\
- Be concise in your responses.\n\
- Show file paths clearly when working with files.\n\n\
Current working directory: {prompt_cwd}"
    )
}
