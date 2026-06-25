// command.rs — Parse slash-commands from the dispatch input.
//
// The Agent View dispatch input accepts either a plain prompt (dispatch a
// new session) or a `/`-command (Design §9). Currently only `/agent` is
// recognized; `/session`, `/model`, etc. can be added later.
//
// Parsing is intentionally strict: a `/agent` command must have a name,
// and the name must be a valid SkyPilot cluster suffix (alphanumeric +
// hyphens). Anything else falls through to `Other` so the caller can show
// an error or treat it as a plain prompt.

/// A parsed dispatch command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchCommand {
    /// `/agent <name>` — spawn a new sub-agent under the current agent.
    /// `name` is validated (lowercased, sanitized to a cluster suffix).
    SpawnAgent { name: String },
    /// `/rename <new name>` — rename the selected node (alternative to `n`).
    Rename { new_name: String },
    /// A `/`-prefixed command we don't recognize. The raw text is kept so
    /// the caller can surface "unknown command: /foo".
    Unknown { raw: String },
    /// Not a command — a plain prompt to dispatch as a new session.
    PlainPrompt { text: String },
}

/// Parse a line of dispatch input into a command.
pub fn parse(input: &str) -> DispatchCommand {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return DispatchCommand::PlainPrompt { text: String::new() };
    }
    if !trimmed.starts_with('/') {
        return DispatchCommand::PlainPrompt { text: trimmed.to_string() };
    }
    // Split into command word + rest.
    let (cmd, rest) = match trimmed[1..].split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (&trimmed[1..], ""),
    };
    match cmd {
        "agent" => {
            let name = sanitize_name(rest);
            if name.is_empty() {
                DispatchCommand::Unknown { raw: trimmed.to_string() }
            } else {
                DispatchCommand::SpawnAgent { name }
            }
        }
        "rename" => {
            if rest.is_empty() {
                DispatchCommand::Unknown { raw: trimmed.to_string() }
            } else {
                DispatchCommand::Rename { new_name: rest.to_string() }
            }
        }
        _ => DispatchCommand::Unknown { raw: trimmed.to_string() },
    }
}

/// Sanitize a candidate agent/session name into a valid cluster suffix:
/// lowercase, alphanumeric + hyphens only, no leading/trailing hyphens.
/// Returns empty if the input has no usable characters.
pub fn sanitize_name(raw: &str) -> String {
    let s: String = raw
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-');
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_prompt() {
        assert_eq!(
            parse("fix the login bug"),
            DispatchCommand::PlainPrompt { text: "fix the login bug".to_string() }
        );
    }

    #[test]
    fn empty_is_plain() {
        assert_eq!(parse(""), DispatchCommand::PlainPrompt { text: String::new() });
        assert_eq!(parse("   "), DispatchCommand::PlainPrompt { text: String::new() });
    }

    #[test]
    fn agent_command() {
        assert_eq!(
            parse("/agent research-box"),
            DispatchCommand::SpawnAgent { name: "research-box".to_string() }
        );
    }

    #[test]
    fn agent_command_sanitizes_name() {
        assert_eq!(
            parse("/agent Research Box!"),
            DispatchCommand::SpawnAgent { name: "research-box".to_string() }
        );
    }

    #[test]
    fn agent_command_without_name_is_unknown() {
        match parse("/agent") {
            DispatchCommand::Unknown { raw } => assert_eq!(raw, "/agent"),
            other => panic!("expected Unknown, got {other:?}"),
        }
        match parse("/agent   ") {
            DispatchCommand::Unknown { raw } => assert_eq!(raw, "/agent"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn rename_command() {
        assert_eq!(
            parse("/rename My Researcher"),
            DispatchCommand::Rename { new_name: "My Researcher".to_string() }
        );
    }

    #[test]
    fn unknown_command_keeps_raw() {
        match parse("/model glm") {
            DispatchCommand::Unknown { raw } => assert_eq!(raw, "/model glm"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_strips_leading_trailing_hyphens() {
        assert_eq!(sanitize_name("--foo--"), "foo");
        assert_eq!(sanitize_name("a b c"), "a-b-c");
        assert_eq!(sanitize_name("!!!"), "");
    }
}
