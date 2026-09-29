// command.rs — Parse slash-commands from the dispatch input.
//
// The Agent View dispatch input accepts either a plain prompt (dispatch a
// new session with the default backend) or a `/`-command (Design §9):
//
//   /pi|/claude|/codex <prompt>   dispatch with that backend, once
//   /backend <pi|claude|codex>    set the default backend
//   /model [<model>]              set (or show) the default model for the
//                                 default backend; `/model -` clears it
//   /import                       browse Claude Code / Codex sessions
//   /switch <target>              move the selected session to another
//                                 agent/model, keeping the conversation
//   /agent <name>, /rename <name>
//
// Parsing is intentionally strict: a `/agent` command must have a name,
// and the name must be a valid SkyPilot cluster suffix (alphanumeric +
// hyphens). Anything else falls through to `Other` so the caller can show
// an error or treat it as a plain prompt.

use crate::session::Backend;

/// A parsed dispatch command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchCommand {
    /// `/agent <name>` — spawn a new sub-agent under the current agent.
    /// `name` is validated (lowercased, sanitized to a cluster suffix).
    SpawnAgent { name: String },
    /// `/rename <new name>` — rename the selected node (alternative to `n`).
    Rename { new_name: String },
    /// `/pi|/claude|/codex <prompt>` — dispatch with a specific backend.
    DispatchWith { backend: Backend, text: String },
    /// `/backend <name>` — change the default backend.
    SetBackend { backend: Backend },
    /// `/model [<model>]` — None shows the current default.
    Model { model: Option<String> },
    /// `/import` — open the session importer.
    Import,
    /// `/switch <target>` — move the selected session to another agent or
    /// model, keeping its conversation (`claude`, `codex:gpt-6`,
    /// `pi:provider/model`, or a bare pi model name).
    Switch { target: String },
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
        "pi" | "claude" | "codex" if !rest.is_empty() => DispatchCommand::DispatchWith {
            backend: Backend::parse(cmd).unwrap_or_default(),
            text: rest.to_string(),
        },
        "backend" => match Backend::parse(rest) {
            Some(backend) => DispatchCommand::SetBackend { backend },
            None => DispatchCommand::Unknown { raw: trimmed.to_string() },
        },
        "model" => DispatchCommand::Model {
            model: (!rest.is_empty()).then(|| rest.to_string()),
        },
        "import" => DispatchCommand::Import,
        "switch" if !rest.is_empty() => DispatchCommand::Switch { target: rest.to_string() },
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
        match parse("/frobnicate foo") {
            DispatchCommand::Unknown { raw } => assert_eq!(raw, "/frobnicate foo"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn backend_commands() {
        assert_eq!(
            parse("/claude fix the bug"),
            DispatchCommand::DispatchWith { backend: Backend::Claude, text: "fix the bug".into() }
        );
        assert_eq!(
            parse("/codex  review"),
            DispatchCommand::DispatchWith { backend: Backend::Codex, text: "review".into() }
        );
        assert!(matches!(parse("/claude"), DispatchCommand::Unknown { .. }));
        assert_eq!(parse("/backend cc"), DispatchCommand::SetBackend { backend: Backend::Claude });
        assert!(matches!(parse("/backend vim"), DispatchCommand::Unknown { .. }));
    }

    #[test]
    fn model_and_import() {
        assert_eq!(parse("/model"), DispatchCommand::Model { model: None });
        assert_eq!(
            parse("/model openrouter/qwen/qwen3-coder"),
            DispatchCommand::Model { model: Some("openrouter/qwen/qwen3-coder".into()) }
        );
        assert_eq!(parse("/import"), DispatchCommand::Import);
        assert_eq!(parse("/switch claude:opus"), DispatchCommand::Switch { target: "claude:opus".into() });
        assert!(matches!(parse("/switch"), DispatchCommand::Unknown { .. }));
    }

    #[test]
    fn sanitize_strips_leading_trailing_hyphens() {
        assert_eq!(sanitize_name("--foo--"), "foo");
        assert_eq!(sanitize_name("a b c"), "a-b-c");
        assert_eq!(sanitize_name("!!!"), "");
    }
}
