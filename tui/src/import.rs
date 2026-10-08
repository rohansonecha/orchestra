// import.rs — Discover existing Claude Code and Codex sessions on disk.
//
//   Claude Code: ~/.claude/projects/<cwd-with-dashes>/<uuid>.jsonl
//     Top-level files only; subagent transcripts live in <uuid>/subagents/.
//     Title: last custom-title (/rename) > last ai-title > last-prompt >
//     first user message.
//   Codex:       ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
//     First line is session_meta (id, cwd). Threads with a parent_thread_id
//     are subagents (e.g. guardian reviews) and are skipped. Titles come
//     from ~/.codex/session_index.jsonl.
//
// Transcripts can be many MB, so only the head (for cwd + first prompt)
// and the tail (for the latest title) of each file are read.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::paths;
use crate::session::Backend;

#[derive(Debug, Clone)]
pub struct ExternalSession {
    pub backend: Backend,
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub git_branch: Option<String>,
    /// Transcript mtime (unix seconds) — "last active".
    pub modified: u64,
    pub path: PathBuf,
}

const HEAD_LINES: usize = 400;
const TAIL_BYTES: u64 = 256 * 1024;

/// All importable sessions, newest first. `under` restricts to sessions
/// whose cwd is inside that directory (the current repo, including its
/// .claude/ and .orchestra/ worktrees).
pub fn scan(under: Option<&Path>) -> Vec<ExternalSession> {
    let mut out = scan_claude(&paths::claude_projects_dir());
    out.extend(scan_codex(&paths::codex_dir()));
    out.extend(scan_pi(&paths::pi_sessions_dir(), &paths::home().join(".pi").join("agent").join("sessions")));
    if let Some(root) = under {
        out.retain(|s| Path::new(&s.cwd).starts_with(root));
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A file's size and modification time: when both are unchanged, what was
/// read from it last time still holds.
type Stamp = (u64, std::time::SystemTime);

fn stamp(p: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(p).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// Results of reading each transcript, reused while the file is unchanged.
/// A scan runs every 15 seconds over hundreds of transcripts, almost all of
/// them untouched since the last one.
type ReadCache = HashMap<PathBuf, (Stamp, Option<ExternalSession>)>;
static READ_CACHE: std::sync::Mutex<Option<ReadCache>> = std::sync::Mutex::new(None);

fn cached(path: &Path, read: impl FnOnce(&Path) -> Option<ExternalSession>) -> Option<ExternalSession> {
    let Some(st) = stamp(path) else { return read(path) };
    if let Some((s, v)) = READ_CACHE.lock().ok().and_then(|c| c.as_ref()?.get(path).cloned()) {
        if s == st {
            return v;
        }
    }
    let v = read(path);
    if let Ok(mut c) = READ_CACHE.lock() {
        c.get_or_insert_with(HashMap::new).insert(path.to_path_buf(), (st, v.clone()));
    }
    v
}

fn head_lines(p: &Path, n: usize) -> Vec<Value> {
    let Ok(f) = File::open(p) else { return Vec::new() };
    BufReader::new(f)
        .lines()
        .take(n)
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

fn tail_lines(p: &Path) -> Vec<Value> {
    let Ok(mut f) = File::open(p) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    let mut bytes = Vec::new();
    if f.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    buf.push_str(&String::from_utf8_lossy(&bytes));
    let mut lines = buf.lines();
    if start > 0 {
        lines.next(); // partial first line
    }
    lines.filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// First line of text, trimmed and capped, for list display.
pub fn one_line(s: &str, max: usize) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
}

/// Text of a Claude Code user message, or None for tool results and
/// harness-injected messages (slash-command echoes, reminders).
pub fn claude_user_text(entry: &Value) -> Option<String> {
    if entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let content = entry.pointer("/message/content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = strip_tagged(&text, "system-reminder");
    let t = text.trim();
    if t.is_empty()
        || t.starts_with("<command-name>")
        || t.starts_with("<local-command-")
        || t.starts_with("<command-message>")
        || t.starts_with("Caveat: The messages below")
    {
        return None;
    }
    Some(t.to_string())
}

/// Remove every `<tag>…</tag>` block.
pub fn strip_tagged(s: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(&open) {
        out.push_str(&rest[..i]);
        match rest[i..].find(&close) {
            Some(j) => rest = &rest[i + j + close.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn scan_claude(projects: &Path) -> Vec<ExternalSession> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(projects) else { return out };
    for dir in dirs.flatten().filter(|d| d.path().is_dir()) {
        let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            if let Some(s) = cached(&path, read_claude) {
                out.push(s);
            }
        }
    }
    out
}

fn read_claude(path: &Path) -> Option<ExternalSession> {
    let id = path.file_stem()?.to_string_lossy().to_string();
    // The launch directory: Claude files the transcript under it and
    // resolves `--resume <id>` from there. Later `cwd` values only track
    // the shell's `cd`s, and a session that entered a worktree re-enters it
    // on resume by itself.
    let mut cwd = None;
    let mut branch = None;
    let mut first_prompt = None;
    for e in head_lines(path, HEAD_LINES) {
        if cwd.is_none() {
            cwd = e.get("cwd").and_then(Value::as_str).map(str::to_string);
        }
        if branch.is_none() {
            branch = e.get("gitBranch").and_then(Value::as_str).filter(|b| !b.is_empty() && *b != "HEAD").map(str::to_string);
        }
        if first_prompt.is_none()
            && e.get("type").and_then(Value::as_str) == Some("user")
            && e.get("isSidechain").and_then(Value::as_bool) != Some(true)
        {
            first_prompt = claude_user_text(&e);
        }
        if cwd.is_some() && branch.is_some() && first_prompt.is_some() {
            break;
        }
    }
    // No real user turn means nothing worth resuming.
    let first_prompt = first_prompt?;
    // Sessions that entered a worktree are moved into its project folder
    // and record `relocated` entries. Resume from the directory the file
    // is filed under now — the one whose encoded name is the folder name.
    let folder = path.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string());
    let encode = |c: &str| c.chars().map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' }).collect::<String>();
    if let (Some(folder), Some(first)) = (&folder, &cwd) {
        if &encode(first) != folder {
            if let Some(moved) = relocated_cwds(path).into_iter().rev().find(|c| &encode(c) == folder) {
                cwd = Some(moved);
            }
        }
    }
    let (mut custom, mut ai, mut last_prompt) = (None, None, None);
    for e in tail_lines(path) {
        match e.get("type").and_then(Value::as_str) {
            Some("custom-title") => custom = e.get("customTitle").and_then(Value::as_str).map(str::to_string),
            Some("ai-title") => ai = e.get("aiTitle").and_then(Value::as_str).map(str::to_string),
            Some("last-prompt") => last_prompt = e.get("lastPrompt").and_then(Value::as_str).map(str::to_string),
            _ => {}
        }
    }
    let title = custom.or(ai).or(last_prompt).unwrap_or(first_prompt);
    Some(ExternalSession {
        backend: Backend::Claude,
        id,
        cwd: cwd?,
        title: one_line(&title, 80),
        git_branch: branch,
        modified: mtime(path),
        path: path.to_path_buf(),
    })
}

/// `relocatedCwd` values in a Claude transcript, in order. Only lines
/// mentioning it are parsed, so large transcripts stay cheap.
/// Per transcript: bytes read so far, and the relocations found in them.
type RelocatedCache = HashMap<PathBuf, (u64, Vec<String>)>;

fn relocated_cwds(path: &Path) -> Vec<String> {
    // Transcripts only grow, and some are hundreds of MB: remember how far
    // each was read and what it held, and read only what was added since.
    static SEEN: std::sync::Mutex<Option<RelocatedCache>> = std::sync::Mutex::new(None);
    let Ok(mut f) = File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let cache = seen.get_or_insert_with(HashMap::new);
    let (mut offset, mut found) = cache.get(path).cloned().unwrap_or_default();
    if len < offset {
        // Rewritten from scratch: start over.
        (offset, found) = (0, Vec::new());
    }
    if f.seek(SeekFrom::Start(offset)).is_ok() {
        let mut reader = BufReader::new(f);
        let mut line = Vec::new();
        // Stop before a partial last line; it is read next time, complete.
        while matches!(reader.read_until(b'\n', &mut line), Ok(n) if n > 0) && line.ends_with(b"\n") {
            offset += line.len() as u64;
            if line.windows(14).any(|w| w == b"\"relocatedCwd\"") {
                if let Some(c) = serde_json::from_slice::<Value>(&line).ok().and_then(|v| v.get("relocatedCwd")?.as_str().map(str::to_string)) {
                    found.push(c);
                }
            }
            line.clear();
        }
    }
    cache.insert(path.to_path_buf(), (offset, found.clone()));
    found
}

/// pi conversations: orchestra's own (`~/.orchestra/pi-sessions/<id>/`,
/// id = the directory, listed when no session state claims them — e.g.
/// after their state was lost) and ones started with plain `pi`
/// (`~/.pi/agent/sessions/<dir>/<file>.jsonl`, id = the file name).
pub fn scan_pi(orchestra_dir: &Path, pi_dir: &Path) -> Vec<ExternalSession> {
    let mut out = Vec::new();
    for d in std::fs::read_dir(orchestra_dir).into_iter().flatten().flatten() {
        let dir = d.path();
        let newest = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .max_by_key(|p| mtime(p));
        if let Some(p) = newest {
            if let Some(mut s) = cached(&p, read_pi) {
                s.id = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                out.push(s);
            }
        }
    }
    for d in std::fs::read_dir(pi_dir).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                if let Some(s) = cached(&p, read_pi) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Title and directory of a pi session file. The title is the first real
/// prompt (not orchestra's handoff note or a carried-over summary).
fn read_pi(path: &Path) -> Option<ExternalSession> {
    let head = head_lines(path, HEAD_LINES);
    let header = head.first()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let cwd = header.get("cwd")?.as_str()?.to_string();
    let prompt = head.iter().skip(1).find_map(|e| {
        let m = e.get("message")?;
        if m.get("role").and_then(Value::as_str) != Some("user") {
            return None;
        }
        let text = match m.get("content")? {
            Value::String(t) => t.clone(),
            Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(" "),
            _ => return None,
        };
        let t = text.trim();
        (!t.is_empty() && !t.starts_with("[orchestra]") && !t.starts_with("This session is being continued")).then(|| t.to_string())
    })?;
    Some(ExternalSession {
        backend: Backend::Pi,
        id: path.file_stem()?.to_string_lossy().to_string(),
        cwd,
        title: one_line(&prompt, 80),
        git_branch: None,
        modified: mtime(path),
        path: path.to_path_buf(),
    })
}

pub fn scan_codex(codex_home: &Path) -> Vec<ExternalSession> {
    let mut names: HashMap<String, String> = HashMap::new();
    if let Ok(f) = File::open(codex_home.join("session_index.jsonl")) {
        for l in BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&l) {
                if let (Some(id), Some(n)) = (
                    v.get("id").and_then(Value::as_str),
                    v.get("thread_name").and_then(Value::as_str),
                ) {
                    names.insert(id.to_string(), n.to_string());
                }
            }
        }
    }
    let mut files = Vec::new();
    collect_jsonl(&codex_home.join("sessions"), 4, &mut files);
    files
        .into_iter()
        .filter_map(|p| {
            // Cached without the thread name, which lives in another file.
            let mut s = cached(&p, read_codex)?;
            if let Some(n) = names.get(&s.id) {
                s.title = one_line(n, 80);
            }
            (!s.title.is_empty()).then_some(s)
        })
        .collect()
}

fn collect_jsonl(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() && depth > 0 {
            collect_jsonl(&p, depth - 1, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// A Codex thread; the title is its first prompt, or empty (the caller
/// applies the thread name, and drops threads with neither).
fn read_codex(path: &Path) -> Option<ExternalSession> {
    let head = head_lines(path, HEAD_LINES);
    let meta = head.first()?;
    if meta.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let p = meta.get("payload")?;
    if p.get("parent_thread_id").is_some_and(|v| !v.is_null()) || p.get("source").is_some_and(Value::is_object) {
        return None;
    }
    let id = p.get("id").or_else(|| p.get("session_id"))?.as_str()?.to_string();
    let cwd = p.get("cwd")?.as_str()?.to_string();
    let branch = p.pointer("/git/branch").and_then(Value::as_str).map(str::to_string);
    let first_prompt = head.iter().skip(1).find_map(codex_user_text);
    let title = first_prompt.unwrap_or_default();
    Some(ExternalSession {
        backend: Backend::Codex,
        id,
        cwd,
        title: one_line(&title, 80),
        git_branch: branch,
        modified: mtime(path),
        path: path.to_path_buf(),
    })
}

/// Text of a Codex user message, skipping the environment/instructions
/// preamble Codex injects as user-role messages.
pub fn codex_user_text(entry: &Value) -> Option<String> {
    let p = entry.get("payload")?;
    if entry.get("type").and_then(Value::as_str) != Some("response_item")
        || p.get("type").and_then(Value::as_str) != Some("message")
        || p.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let text = codex_content_text(p);
    let t = text.trim();
    if t.is_empty() || t.starts_with("# AGENTS.md") || is_injected_block(t) {
        return None;
    }
    Some(t.to_string())
}

/// Codex sends context (environment, instructions, plugin lists, ...) as
/// user-role messages made entirely of `<tag>…</tag>` blocks. A real prompt
/// doesn't both open with a bare tag line and close with a closing tag.
fn is_injected_block(t: &str) -> bool {
    let Some(rest) = t.strip_prefix('<') else { return false };
    let name_len = rest
        .find(|c: char| !(c.is_ascii_lowercase() || c == '_' || c == ' '))
        .unwrap_or(rest.len());
    let after = &rest[name_len..];
    if name_len == 0 {
        return false;
    }
    if after.starts_with("/>") {
        return true;
    }
    after.starts_with('>') && t.trim_end().ends_with('>') && t.contains("</")
}

pub fn codex_content_text(payload: &Value) -> String {
    payload
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, lines: &[Value]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let s: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn claude_title_priority_and_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("-r");
        write(
            &proj.join("aaa.jsonl"),
            &[
                serde_json::json!({"type":"user","cwd":"/r","gitBranch":"main","isMeta":true,
                    "message":{"role":"user","content":"<command-name>/clear</command-name>"}}),
                serde_json::json!({"type":"user","cwd":"/r","gitBranch":"main",
                    "message":{"role":"user","content":[{"type":"text","text":"<system-reminder>x</system-reminder>fix the bug\nmore"}]}}),
                serde_json::json!({"type":"ai-title","aiTitle":"Fix bug"}),
                serde_json::json!({"type":"custom-title","customTitle":"My name"}),
                serde_json::json!({"type":"user","cwd":"/r/sub","gitBranch":"HEAD","message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}),
            ],
        );
        // No real user message → skipped.
        write(&proj.join("bbb.jsonl"), &[serde_json::json!({"type":"mode","mode":"normal"})]);
        // Subagent transcript in a subdir → not scanned.
        write(
            &proj.join("aaa/subagents/agent-x.jsonl"),
            &[serde_json::json!({"type":"user","cwd":"/r","message":{"content":"sub"}})],
        );

        let got = scan_claude(tmp.path());
        assert_eq!(got.len(), 1);
        let s = &got[0];
        assert_eq!(s.id, "aaa");
        assert_eq!(s.title, "My name");
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.cwd, "/r", "launch dir, not later cd's");
    }

    #[test]
    fn claude_relocated_into_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        // Started in /home/u, entered /r/.claude/worktrees/w: Claude moved the
        // file into that worktree's project folder.
        write(
            &tmp.path().join("-r--claude-worktrees-w/ddd.jsonl"),
            &[
                serde_json::json!({"type":"user","cwd":"/home/u","message":{"content":"go"}}),
                serde_json::json!({"type":"relocated","relocatedCwd":"/r/.claude/worktrees/w"}),
            ],
        );
        assert_eq!(scan_claude(tmp.path())[0].cwd, "/r/.claude/worktrees/w");
    }

    #[test]
    fn claude_falls_back_to_first_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("-r/ccc.jsonl"),
            &[serde_json::json!({"type":"user","cwd":"/r","message":{"content":"  \n add tests please "}})],
        );
        assert_eq!(scan_claude(tmp.path())[0].title, "add tests please");
    }

    #[test]
    fn codex_skips_subagents_and_preamble() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path().join("sessions/2026/09/17");
        let msg = |role: &str, text: &str| {
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":role,
                "content":[{"type":"input_text","text":text}]}})
        };
        write(
            &day.join("rollout-a.jsonl"),
            &[
                serde_json::json!({"type":"session_meta","payload":{"id":"A","cwd":"/r","source":"cli"}}),
                msg("user", "<environment_context>x</environment_context>"),
                msg("user", "review my PR"),
            ],
        );
        write(
            &day.join("rollout-b.jsonl"),
            &[serde_json::json!({"type":"session_meta","payload":{"id":"B","cwd":"/r",
                "source":{"subagent":{"other":"guardian"}},"parent_thread_id":"A"}})],
        );
        write(
            &tmp.path().join("session_index.jsonl"),
            &[serde_json::json!({"id":"Z","thread_name":"unrelated"})],
        );
        let got = scan_codex(tmp.path());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "A");
        assert_eq!(got[0].title, "review my PR");

        write(
            &tmp.path().join("session_index.jsonl"),
            &[serde_json::json!({"id":"A","thread_name":"PR review"})],
        );
        assert_eq!(scan_codex(tmp.path())[0].title, "PR review");
    }

    #[test]
    fn injected_blocks() {
        assert!(is_injected_block("<recommended_plugins>\nx\n</recommended_plugins>\n<environment_context>\n</environment_context>"));
        assert!(is_injected_block("<permissions instructions>\n</permissions instructions>"));
        assert!(is_injected_block("<environment_context/>"));
        assert!(!is_injected_block("fix <b>this</b>"));
        assert!(!is_injected_block("<div> is rendered wrong\nsee it"));
    }

    #[test]
    fn pi_sessions_from_both_places() {
        let tmp = tempfile::tempdir().unwrap();
        let orch = tmp.path().join("orch");
        let pi = tmp.path().join("pi");
        let header = serde_json::json!({"type":"session","version":3,"id":"x","cwd":"/r"});
        let user = |t: &str| serde_json::json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":t}]}});
        write(&orch.join("dir-id-1/a.jsonl"), &[header.clone(), user("This session is being continued from a previous conversation"), user("check the ROI monitor")]);
        write(&pi.join("--r--/2026_s1.jsonl"), &[header.clone(), user("[orchestra] moved"), user("fix the build")]);
        write(&pi.join("--r--/empty.jsonl"), &[header]);
        let mut got = scan_pi(&orch, &pi);
        got.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<(&str, &str)> = got.iter().map(|s| (s.id.as_str(), s.title.as_str())).collect();
        assert_eq!(ids, vec![("2026_s1", "fix the build"), ("dir-id-1", "check the ROI monitor")]);
        assert!(got.iter().all(|s| s.backend == Backend::Pi && s.cwd == "/r"));
    }

    #[test]
    fn strip_and_one_line() {
        assert_eq!(strip_tagged("a<x>b</x>c<x>d</x>e", "x"), "ace");
        assert_eq!(strip_tagged("a<x>unterminated", "x"), "a");
        assert_eq!(one_line("\n  hello world  \nnext", 5), "hello…");
    }

    #[test]
    fn tail_handles_large_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("-r/big.jsonl");
        let mut lines = vec![serde_json::json!({"type":"user","cwd":"/r","message":{"content":"start"}})];
        let pad = "x".repeat(1000);
        for _ in 0..400 {
            lines.push(serde_json::json!({"type":"assistant","pad":pad}));
        }
        lines.push(serde_json::json!({"type":"ai-title","aiTitle":"Late title"}));
        write(&p, &lines);
        assert_eq!(scan_claude(tmp.path())[0].title, "Late title");
    }
}

