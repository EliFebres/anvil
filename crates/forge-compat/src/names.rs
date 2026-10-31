//! Tool names as Claude Code spells them versus forge's built-in names.
//!
//! Skills, agents and hooks written for Claude Code say `Bash` or `Read`;
//! forge's tools are `shell` and `read_file`. Both directions are needed:
//! allow lists are translated into forge names, and hook payloads carry the
//! Claude Code name so existing hook scripts match.

/// Pairs of (Claude Code name, forge name) for the tools both have.
const ALIASES: &[(&str, &str)] = &[
    ("Bash", "shell"),
    ("Read", "read_file"),
    ("Write", "write_file"),
    ("Edit", "edit_file"),
    ("MultiEdit", "edit_file"),
    ("Glob", "glob"),
    ("Grep", "grep"),
    ("LS", "list_dir"),
    ("Skill", "skill"),
    ("Task", "delegate"),
];

/// The forge name for a tool as a Claude Code file spells it.
///
/// A Claude Code permission pattern such as `Bash(git:*)` is reduced to its
/// tool name. Names that are not aliases (MCP tools, forge names, unknown
/// tools) come back unchanged.
///
/// # Arguments
///
/// * `name` - The name as written.
///
/// # Returns
///
/// The forge tool name.
pub fn to_forge_name(name: &str) -> String {
    let bare = name.split('(').next().unwrap_or(name).trim();
    ALIASES
        .iter()
        .find(|(claude, _)| *claude == bare)
        .map_or_else(|| bare.to_owned(), |(_, forge)| (*forge).to_owned())
}

/// The Claude Code name for a forge tool, for hook payloads and matchers.
///
/// # Arguments
///
/// * `name` - The forge tool name.
///
/// # Returns
///
/// The Claude Code name, or the input when there is no alias.
pub fn to_claude_name(name: &str) -> String {
    ALIASES
        .iter()
        .find(|(_, forge)| *forge == name)
        .map_or_else(|| name.to_owned(), |(claude, _)| (*claude).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_both_ways_and_passes_unknown_names_through() {
        assert_eq!(to_forge_name("Bash"), "shell");
        assert_eq!(to_forge_name("Bash(git:*)"), "shell");
        assert_eq!(
            to_forge_name("mcp__playwright__click"),
            "mcp__playwright__click"
        );
        assert_eq!(to_forge_name("shell"), "shell");
        assert_eq!(to_claude_name("read_file"), "Read");
        assert_eq!(to_claude_name("edit_file"), "Edit");
        assert_eq!(to_claude_name("mcp__fs__ls"), "mcp__fs__ls");
    }
}
