// commands.rs — Session commands that work the same on every agent
// (Claude Code, Codex, pi): prompts to send into a session, side
// questions that leave its conversation untouched, recurring prompts, and
// the GitHub issue draft for /bug.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::history;
use crate::paths;
use crate::session::{sq, Backend, Session};
use crate::switch;

pub const CODE_REVIEW: &str = "\
Review the changes in this worktree for real bugs. Find the diff under review: \
`git diff @{upstream}...HEAD` (or the branch's merge base with the default \
branch), plus `git diff HEAD` for uncommitted work. {TARGET}\
Read every hunk and the surrounding code as needed, and look for correctness \
problems: wrong or inverted conditions, off-by-one errors, missing error \
handling, removed checks, broken callers of changed functions, races. Prefer \
real failure modes over style; every finding needs a concrete scenario in \
which the code misbehaves. Do not edit any files. Finish with at most 15 \
findings, one line each: file:line — the problem — the scenario.";

pub const SIMPLIFY: &str = "\
Review the code you changed in this worktree (`git diff` against the base \
branch, plus uncommitted work) for reuse, simplification and efficiency: \
duplicated logic that existing helpers already cover, needless abstraction \
or indirection, dead code, wasted work in loops. Apply the cleanups that \
clearly improve the code without changing behavior, run the relevant tests, \
and list what you changed.";

pub const AUTOFIX_PR: &str = "\
Check the pull request for this branch{TARGET} with `gh pr view` and `gh pr \
checks`. For every failing check, read its log and fix the cause; for every \
unresolved review comment, address it or explain why not. Run the relevant \
tests locally, commit the fixes as new commits, push, and summarize what \
you fixed and what still needs a human.";

pub const RECAP_QUESTION: &str = "\
In one line of at most 25 words, recap this session: what has been done, \
its current state, and the next step. Reply with the line only.";

/// Text to type into a session for a command. Claude Code has its own
/// /simplify and /code-review, so those are passed through natively.
pub fn session_prompt(backend: Backend, command: &str, args: &str) -> String {
    let target = |s: &str| if s.trim().is_empty() { String::new() } else { format!(" ({})", s.trim()) };
    match (backend, command) {
        (Backend::Claude, "simplify") => format!("/simplify {args}").trim_end().to_string(),
        (_, "simplify") => format!("{SIMPLIFY}{}", if args.trim().is_empty() { String::new() } else { format!(" Focus: {}", args.trim()) }),
        (_, "autofix-pr") => AUTOFIX_PR.replace("{TARGET}", &target(args)),
        _ => args.to_string(),
    }
}

pub fn review_prompt(target: &str) -> String {
    let t = if target.trim().is_empty() {
        String::new()
    } else {
        format!("If \"{}\" names a PR, branch or path, review that instead. ", target.trim())
    };
    CODE_REVIEW.replace("{TARGET}", &t)
}

/// Type `text` into a session's agent and submit it.
pub fn send_to_session(name: &str, text: &str) -> Result<(), String> {
    let ok = Command::new("tmux").args(["send-keys", "-t", name, "-l", text]).status().map(|s| s.success());
    if !ok.unwrap_or(false) {
        return Err(format!("could not type into {name}"));
    }
    // A short pause so the agent's input box has taken the paste before
    // Enter arrives (some agents treat a fast Enter as part of a paste).
    std::thread::sleep(std::time::Duration::from_millis(300));
    Command::new("tmux").args(["send-keys", "-t", name, "Enter"]).status().map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// /loop
// ---------------------------------------------------------------------------

/// "30s" / "5m" / "2h" → seconds.
pub fn parse_interval(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    match unit {
        "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        _ => None,
    }
    .filter(|&v| v >= 10)
}

/// `/loop [interval] <prompt>` → (seconds, prompt). Default 10 minutes.
pub fn parse_loop(args: &str) -> Option<(u64, String)> {
    let args = args.trim();
    let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    match parse_interval(first) {
        Some(secs) if !rest.trim().is_empty() => Some((secs, rest.trim().to_string())),
        Some(_) => None,
        None if !args.is_empty() => Some((600, args.to_string())),
        None => None,
    }
}

pub fn loop_tmux_name(session: &str) -> String {
    format!("loop-{session}")
}

/// Start (or replace) the loop runner: its own tmux session that waits,
/// and types the prompt only when the agent is idle.
pub fn start_loop(session: &str, secs: u64, prompt: &str) -> Result<(), String> {
    let runner = loop_tmux_name(session);
    let _ = Command::new("tmux").args(["kill-session", "-t", &runner]).status();
    let bin = sq(&crate::session::orchestra_bin());
    let script = format!(
        "while tmux has-session -t {s} 2>/dev/null; do sleep {secs}; \
         {bin} tmux-idle {s} && {bin} send {s} {p}; done",
        s = sq(session),
        p = sq(prompt),
    );
    let ok = Command::new("tmux")
        .args(["new-session", "-d", "-s", &runner, "bash", "-c", &script])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok { Ok(()) } else { Err("could not start the loop runner".into()) }
}

pub fn stop_loop(session: &str) -> bool {
    Command::new("tmux")
        .args(["kill-session", "-t", &loop_tmux_name(session)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn loop_running(session: &str) -> bool {
    crate::session::tmux_alive(&loop_tmux_name(session))
}

// ---------------------------------------------------------------------------
// /btw and /recap: side questions
// ---------------------------------------------------------------------------

/// The pi model side questions use: the configured pi default, else pi's
/// own default, else the first model pi lists.
pub fn side_model(configured: Option<String>) -> Option<String> {
    configured
        .or_else(pi_settings_default)
        .or_else(|| crate::config::pi_models_table().into_iter().next().map(|(p, m, _)| format!("{p}/{m}")))
}

fn pi_settings_default() -> Option<String> {
    let dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home().join(".pi").join("agent"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).ok()?).ok()?;
    Some(format!("{}/{}", v.get("defaultProvider")?.as_str()?, v.get("defaultModel")?.as_str()?))
}

/// Ask `question` about a session's conversation without touching it: a
/// throwaway pi session gets a copy of the history, answers once with no
/// tools, and is deleted. Blocking; run it off the UI thread.
pub fn side_question(sess: &Session, question: &str, model: &str) -> Result<String, String> {
    let msgs = switch::conversation(sess).map_err(|e| format!("reading history: {e}"))?;
    if msgs.is_empty() {
        return Err("this session has no conversation yet".into());
    }
    let msgs = history::fit_budget(history::normalize(msgs), 300_000);
    let dir = paths::state_dir().join("tmp").join(format!("side-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("side.jsonl");
    let result = (|| {
        history::append_pi(&file, &msgs, &sess.worktree_path, None, Some(model)).map_err(|e| e.to_string())?;
        let out = Command::new("pi")
            .arg("--session-dir")
            .arg(&dir)
            .args(["--continue", "--tools", "read,grep,find,ls", "--no-extensions", "--no-skills", "-p", "--model", model, question])
            .current_dir(if Path::new(&sess.worktree_path).is_dir() { sess.worktree_path.as_str() } else { "/" })
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("pi: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || text.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("pi failed: {}", err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output")));
        }
        Ok(text)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

// ---------------------------------------------------------------------------
// /bug
// ---------------------------------------------------------------------------

pub const BUG_REPO: &str = "rohansonecha/orchestra";

fn version_of(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()
        .map(|o| {
            let t = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            t.lines().find(|l| !l.contains("WARNING") && !l.trim().is_empty()).unwrap_or("").trim().to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "not installed".into())
}

/// Issue title and body for `/bug <description>`. Contains no paths under
/// $HOME beyond the repo name and no environment values.
pub fn bug_draft(description: &str, selected: Option<&Session>, last_status: &str) -> (String, String) {
    let title: String = description.trim().lines().next().unwrap_or("").chars().take(80).collect();
    let session = match selected {
        Some(s) => format!(
            "- Selected session: {} on {}{} ({} segment(s))\n",
            s.name,
            s.backend.as_str(),
            s.model.as_ref().map(|m| format!(" · {m}")).unwrap_or_default(),
            s.segments.len().max(1)
        ),
        None => String::new(),
    };
    let body = format!(
        "{description}\n\n## Environment\n\n- orchestra {}\n- {}\n- pi: {}\n- claude: {}\n- codex: {}\n- tmux: {}\n{session}{}\n_Filed from orchestra with /bug._\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        version_of("pi", &["--version"]),
        version_of("claude", &["--version"]),
        version_of("codex", &["--version"]),
        version_of("tmux", &["-V"]),
        if last_status.is_empty() { String::new() } else { format!("- Last status line: {last_status}\n") },
        description = description.trim(),
    );
    (title, body)
}

/// File the issue with gh. Returns its URL.
pub fn file_bug(title: &str, body: &str) -> Result<String, String> {
    let dir = paths::state_dir().join("tmp");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let body_file = dir.join(format!("bug-{}.md", uuid::Uuid::new_v4()));
    std::fs::write(&body_file, body).map_err(|e| e.to_string())?;
    let out = Command::new("gh")
        .args(["issue", "create", "-R", BUG_REPO, "--title", title, "--body-file"])
        .arg(&body_file)
        .stdin(Stdio::null())
        .output();
    let _ = std::fs::remove_file(&body_file);
    let out = out.map_err(|e| format!("gh: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_and_loop_args() {
        assert_eq!(parse_interval("30s"), Some(30));
        assert_eq!(parse_interval("5m"), Some(300));
        assert_eq!(parse_interval("2h"), Some(7200));
        assert_eq!(parse_interval("5s"), None, "too fast");
        assert_eq!(parse_interval("five"), None);
        assert_eq!(parse_loop("5m check CI"), Some((300, "check CI".into())));
        assert_eq!(parse_loop("check CI"), Some((600, "check CI".into())));
        assert_eq!(parse_loop("5m"), None);
        assert_eq!(parse_loop(""), None);
    }

    #[test]
    fn prompts_per_agent() {
        assert_eq!(session_prompt(Backend::Claude, "simplify", ""), "/simplify");
        assert!(session_prompt(Backend::Pi, "simplify", "the parser").contains("Focus: the parser"));
        assert!(session_prompt(Backend::Codex, "autofix-pr", "#12").contains("branch (#12)"));
        assert!(review_prompt("").contains("Do not edit any files"));
        assert!(review_prompt("#42").contains("\"#42\""));
    }

    #[test]
    fn bug_draft_has_versions_and_no_home_paths() {
        let (title, body) = bug_draft("Left arrow does not detach from codex\nmore detail", None, "Switch failed: x");
        assert_eq!(title, "Left arrow does not detach from codex");
        assert!(body.contains("## Environment") && body.contains("- tmux:"));
        assert!(body.contains("Last status line: Switch failed: x"));
        assert!(!body.contains(&paths::home().display().to_string()));
    }
}
