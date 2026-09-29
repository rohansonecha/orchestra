// history.rs — Move a conversation between agents (Claude Code, Codex, pi)
// without losing what happened in it.
//
// Every transcript is read into one neutral form (`Msg` / `Block`), and
// written back out in the target agent's own session format, so the target
// resumes it natively (`claude --resume`, `codex resume`, `pi --continue`).
//
// Tool calls are the hard part (see README "Switching agents"):
//   - The four tools that make up nearly all real use — run a shell
//     command, read, write, and edit a file — are mapped to the target's
//     own tool names and argument shapes (`Tool`), so the target model sees
//     calls it could have made itself.
//   - Anything else (web fetch, MCP tools, Codex's JavaScript `exec` cells)
//     has no safe equivalent and is kept as text describing the call.
//   - Every call is paired with a result, and every result with a call:
//     APIs reject orphans, and transcripts often contain them (interrupted
//     turns, compaction, a crash between call and result).
//   - Tool output is capped; reasoning is dropped (it is encrypted or
//     signed per provider and can't be replayed elsewhere).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::import::{claude_user_text, codex_content_text, codex_user_text, strip_tagged};
use crate::session::Backend;

/// Per tool-result text cap when translating.
const TOOL_OUTPUT_MAX: usize = 4000;
/// Per tool-call argument cap when a call is kept as text.
const TOOL_INPUT_MAX: usize = 1500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// The tools every coding agent has, in neutral form.
#[derive(Debug, Clone, PartialEq)]
pub enum Tool {
    Shell { command: String },
    Read { path: String, offset: Option<u64>, limit: Option<u64> },
    Write { path: String, content: String },
    Edit { path: String, edits: Vec<(String, String)> },
    /// No neutral equivalent; `name`/`args` are the source agent's own.
    Other { name: String, args: Value },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    Call { id: String, tool: Tool },
    Result { id: String, output: String, is_error: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub role: Role,
    pub blocks: Vec<Block>,
}

// ---------------------------------------------------------------------------
// Tool mapping
// ---------------------------------------------------------------------------

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}
fn n(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(Value::as_u64)
}

pub fn from_claude_tool(name: &str, input: &Value) -> Tool {
    let other = || Tool::Other { name: name.to_string(), args: input.clone() };
    match name {
        "Bash" => s(input, "command").map(|command| Tool::Shell { command }).unwrap_or_else(other),
        "Read" => match s(input, "file_path") {
            Some(path) => Tool::Read { path, offset: n(input, "offset"), limit: n(input, "limit") },
            None => other(),
        },
        "Write" => match (s(input, "file_path"), s(input, "content")) {
            (Some(path), Some(content)) => Tool::Write { path, content },
            _ => other(),
        },
        "Edit" => match (s(input, "file_path"), s(input, "old_string"), s(input, "new_string")) {
            // replace_all has no equivalent elsewhere; keep it as-is.
            (Some(path), Some(o), Some(nw)) if input.get("replace_all").and_then(Value::as_bool) != Some(true) => {
                Tool::Edit { path, edits: vec![(o, nw)] }
            }
            _ => other(),
        },
        _ => other(),
    }
}

pub fn to_claude_tool(t: &Tool) -> (String, Value) {
    match t {
        Tool::Shell { command } => ("Bash".into(), json!({"command": command})),
        Tool::Read { path, offset, limit } => {
            let mut v = json!({"file_path": path});
            if let Some(o) = offset { v["offset"] = json!(o); }
            if let Some(l) = limit { v["limit"] = json!(l); }
            ("Read".into(), v)
        }
        Tool::Write { path, content } => ("Write".into(), json!({"file_path": path, "content": content})),
        // Multi-edit calls are split into one Edit per change by the writer.
        Tool::Edit { path, edits } => (
            "Edit".into(),
            json!({"file_path": path, "old_string": edits[0].0, "new_string": edits[0].1}),
        ),
        Tool::Other { name, args } => (name.clone(), args.clone()),
    }
}

pub fn from_pi_tool(name: &str, args: &Value) -> Tool {
    let other = || Tool::Other { name: name.to_string(), args: args.clone() };
    match name {
        "bash" => s(args, "command").map(|command| Tool::Shell { command }).unwrap_or_else(other),
        "read" => match s(args, "path") {
            Some(path) => Tool::Read { path, offset: n(args, "offset"), limit: n(args, "limit") },
            None => other(),
        },
        "write" => match (s(args, "path"), s(args, "content")) {
            (Some(path), Some(content)) => Tool::Write { path, content },
            _ => other(),
        },
        "edit" => {
            let edits: Option<Vec<(String, String)>> = args.get("edits").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(|e| Some((s(e, "oldText")?, s(e, "newText")?)))
                    .collect()
            });
            match (s(args, "path"), edits) {
                (Some(path), Some(edits)) if !edits.is_empty() => Tool::Edit { path, edits },
                _ => other(),
            }
        }
        _ => other(),
    }
}

pub fn to_pi_tool(t: &Tool) -> (String, Value) {
    match t {
        Tool::Shell { command } => ("bash".into(), json!({"command": command})),
        Tool::Read { path, offset, limit } => {
            let mut v = json!({"path": path});
            if let Some(o) = offset { v["offset"] = json!(o); }
            if let Some(l) = limit { v["limit"] = json!(l); }
            ("read".into(), v)
        }
        Tool::Write { path, content } => ("write".into(), json!({"path": path, "content": content})),
        Tool::Edit { path, edits } => (
            "edit".into(),
            json!({"path": path, "edits": edits.iter().map(|(o, n)| json!({"oldText": o, "newText": n})).collect::<Vec<_>>()}),
        ),
        Tool::Other { name, args } => (name.clone(), args.clone()),
    }
}

/// Codex's `exec` tool takes JavaScript (`text(await tools.exec_command({cmd:
/// "ls"}))`). A cell that is just one exec_command call is a shell command;
/// anything else stays as code.
pub fn from_codex_call(name: &str, raw: &str) -> Tool {
    if name == "exec" {
        if let Some(cmd) = single_exec_command(raw) {
            return Tool::Shell { command: cmd };
        }
        return Tool::Other { name: "codex_exec".into(), args: json!({"code": raw}) };
    }
    let args: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!({"input": raw}));
    match name {
        "shell" | "exec_command" | "container.exec" => {
            let cmd = match args.get("command").or_else(|| args.get("cmd")) {
                Some(Value::String(c)) => Some(c.clone()),
                // ["bash", "-lc", "ls"] → the script part.
                Some(Value::Array(a)) => {
                    let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
                    match parts.as_slice() {
                        [sh, flag, script] if sh.ends_with("sh") && flag.starts_with('-') => Some(script.to_string()),
                        _ => Some(parts.join(" ")),
                    }
                }
                _ => None,
            };
            cmd.map(|command| Tool::Shell { command })
                .unwrap_or(Tool::Other { name: name.to_string(), args })
        }
        _ => Tool::Other { name: name.to_string(), args },
    }
}

/// `cmd` of a JS cell that contains exactly one `tools.exec_command({...})`.
fn single_exec_command(code: &str) -> Option<String> {
    if code.matches("tools.").count() != 1 {
        return None;
    }
    let rest = &code[code.find("tools.exec_command(")?..];
    // `cmd:` or `"cmd":`, then the string literal.
    let after = &rest[rest.find("cmd")? + 3..];
    let after = after.strip_prefix('"').unwrap_or(after).trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    parse_js_string(after)
}

/// Parse a leading JS/JSON string literal ("..", '..' or `..`).
fn parse_js_string(s: &str) -> Option<String> {
    let mut chars = s.chars();
    let quote = chars.next()?;
    if !matches!(quote, '"' | '\'' | '`') {
        return None;
    }
    let mut out = String::new();
    let mut esc = false;
    for c in chars {
        if esc {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == quote {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

/// A call rendered as text, for targets that can't take it as a real call.
fn call_as_text(t: &Tool, source_name: impl Fn(&Tool) -> String) -> String {
    let (name, args) = match t {
        Tool::Other { name, args } => (name.clone(), args.clone()),
        other => (source_name(other), to_claude_tool(other).1),
    };
    let args = match &args {
        Value::Object(m) if m.len() == 1 && m.contains_key("code") => m["code"].as_str().unwrap_or("").to_string(),
        Value::Object(m) if m.len() == 1 && m.contains_key("command") => m["command"].as_str().unwrap_or("").to_string(),
        v => v.to_string(),
    };
    format!("[called {name}] {}", cap(&args, TOOL_INPUT_MAX))
}

fn neutral_name(t: &Tool) -> String {
    match t {
        Tool::Shell { .. } => "shell".into(),
        Tool::Read { .. } => "read_file".into(),
        Tool::Write { .. } => "write_file".into(),
        Tool::Edit { .. } => "edit_file".into(),
        Tool::Other { name, .. } => name.clone(),
    }
}

fn cap(s: &str, max: usize) -> String {
    let s = s.trim_end();
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Keep head and tail: errors and summaries tend to be at the end.
    let head: String = s.chars().take(max * 2 / 3).collect();
    let tail: String = s.chars().rev().take(max / 3).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{head}\n… [{} chars omitted] …\n{tail}", s.chars().count() - head.chars().count() - tail.chars().count())
}

// ---------------------------------------------------------------------------
// Building + cleaning a history
// ---------------------------------------------------------------------------

fn push(msgs: &mut Vec<Msg>, role: Role, block: Block) {
    if let Block::Text(t) = &block {
        if t.trim().is_empty() {
            return;
        }
    }
    match msgs.last_mut() {
        Some(m) if m.role == role => m.blocks.push(block),
        _ => msgs.push(Msg { role, blocks: vec![block] }),
    }
}

/// Make the history valid for any API: every call answered, every result
/// preceded by its call, results directly after the assistant turn that
/// made the calls, and strict user/assistant alternation starting with a
/// user turn.
pub fn normalize(msgs: Vec<Msg>) -> Vec<Msg> {
    let mut out: Vec<Msg> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut known: HashSet<String> = HashSet::new();
    for m in msgs {
        match m.role {
            Role::Assistant => {
                // Calls left unanswered by the previous assistant turn.
                if !pending.is_empty() {
                    for id in pending.drain(..) {
                        push(&mut out, Role::User, Block::Result {
                            id, output: "[no result recorded]".into(), is_error: true,
                        });
                    }
                }
                for b in m.blocks {
                    if let Block::Call { id, .. } = &b {
                        known.insert(id.clone());
                        pending.push(id.clone());
                    }
                    if matches!(b, Block::Result { .. }) {
                        continue;
                    }
                    push(&mut out, Role::Assistant, b);
                }
            }
            Role::User => {
                let (results, rest): (Vec<Block>, Vec<Block>) =
                    m.blocks.into_iter().partition(|b| matches!(b, Block::Result { .. }));
                let mut answered = Vec::new();
                for r in results {
                    let Block::Result { id, output, is_error } = r else { continue };
                    if pending.contains(&id) {
                        answered.push(id.clone());
                        push(&mut out, Role::User, Block::Result { id, output, is_error });
                    } else if !known.contains(&id) {
                        // A result whose call was lost (compaction cut):
                        // keep what it said, as text.
                        push(&mut out, Role::User, Block::Text(format!("[tool result] {}", cap(&output, TOOL_OUTPUT_MAX))));
                    }
                }
                pending.retain(|p| !answered.contains(p));
                if !rest.is_empty() {
                    for id in pending.drain(..) {
                        push(&mut out, Role::User, Block::Result {
                            id, output: "[no result recorded]".into(), is_error: true,
                        });
                    }
                    for b in rest {
                        push(&mut out, Role::User, b);
                    }
                }
            }
        }
    }
    for id in pending.drain(..) {
        push(&mut out, Role::User, Block::Result { id, output: "[no result recorded]".into(), is_error: true });
    }
    // Must start on a user turn with text.
    while out.first().is_some_and(|m| m.role != Role::User) {
        out.remove(0);
    }
    out
}

fn msg_chars(m: &Msg) -> usize {
    m.blocks
        .iter()
        .map(|b| match b {
            Block::Text(t) => t.len(),
            Block::Result { output, .. } => output.len().min(TOOL_OUTPUT_MAX),
            Block::Call { tool, .. } => to_claude_tool(tool).1.to_string().len().min(TOOL_INPUT_MAX * 4),
        })
        .sum()
}

fn is_prompt(m: &Msg) -> bool {
    m.role == Role::User && m.blocks.iter().any(|b| matches!(b, Block::Text(_)))
}

/// Keep the first prompt plus the latest turns that fit `max_chars`,
/// cutting only at a user prompt so no call loses its result.
pub fn fit_budget(msgs: Vec<Msg>, max_chars: usize) -> Vec<Msg> {
    let total: usize = msgs.iter().map(msg_chars).sum();
    if total <= max_chars || msgs.len() < 4 {
        return msgs;
    }
    let first_len = msg_chars(&msgs[0]);
    let mut budget = max_chars.saturating_sub(first_len);
    let mut start = msgs.len();
    let mut i = msgs.len();
    while i > 1 {
        i -= 1;
        let c = msg_chars(&msgs[i]);
        if c > budget {
            break;
        }
        budget -= c;
        if is_prompt(&msgs[i]) {
            start = i;
        }
    }
    if start >= msgs.len() {
        // Even the last exchange doesn't fit; keep the last prompt onward.
        start = msgs.iter().rposition(is_prompt).unwrap_or(msgs.len() - 1).max(1);
    }
    let dropped = start - 1;
    let mut out = vec![msgs[0].clone()];
    out.push(Msg {
        role: Role::Assistant,
        blocks: vec![Block::Text(format!("[… {dropped} earlier messages omitted to fit this model's context]"))],
    });
    out.extend(msgs.into_iter().skip(start));
    out
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

fn read_lines(path: &Path) -> std::io::Result<Vec<Value>> {
    let f = std::fs::File::open(path)?;
    Ok(BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .map(|l| serde_json::from_str(&l).unwrap_or(Value::Null))
        .collect())
}

/// Number of lines, counting newline bytes (no per-line allocation; fast
/// on the 100 MB transcripts long Claude Code sessions reach).
pub fn line_count(path: &Path) -> usize {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return 0 };
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0;
    while let Ok(k) = f.read(&mut buf) {
        if k == 0 {
            break;
        }
        n += buf[..k].iter().filter(|&&b| b == b'\n').count();
    }
    n
}

/// The last lines of a file as JSON, reading only its final `bytes`.
fn tail_values(path: &Path, bytes: u64) -> Vec<Value> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut data = Vec::new();
    let _ = f.read_to_end(&mut data);
    let text = String::from_utf8_lossy(&data);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // partial first line
    }
    lines.iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p.get("type").and_then(Value::as_str) {
                Some("image") | Some("input_image") => "[image omitted]".to_string(),
                _ => p.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Claude Code transcript, from line `from` on (0 = everything; earlier
/// lines are a segment that was already translated in).
pub fn read_claude(path: &Path, from: usize) -> std::io::Result<Vec<Msg>> {
    let lines = read_lines(path)?;
    let main: Vec<&Value> = lines
        .iter()
        .skip(from)
        .filter(|e| matches!(e.get("type").and_then(Value::as_str), Some("user" | "assistant")))
        .filter(|e| e.get("isSidechain").and_then(Value::as_bool) != Some(true))
        .collect();
    let start = main
        .iter()
        .rposition(|e| e.get("isCompactSummary").and_then(Value::as_bool) == Some(true))
        .unwrap_or(0);
    let mut msgs = Vec::new();
    for e in &main[start..] {
        let content = e.pointer("/message/content").unwrap_or(&Value::Null);
        match e.get("type").and_then(Value::as_str) {
            Some("user") => {
                if e.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
                    push(&mut msgs, Role::User, Block::Text(result_text(content)));
                    continue;
                }
                if let Some(t) = claude_user_text(e) {
                    push(&mut msgs, Role::User, Block::Text(t));
                }
                for p in content.as_array().into_iter().flatten() {
                    match p.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            let output = strip_tagged(&result_text(p.get("content").unwrap_or(&Value::Null)), "system-reminder");
                            push(&mut msgs, Role::User, Block::Result {
                                id: s(p, "tool_use_id").unwrap_or_default(),
                                output,
                                is_error: p.get("is_error").and_then(Value::as_bool) == Some(true),
                            });
                        }
                        Some("image") => push(&mut msgs, Role::User, Block::Text("[image omitted]".into())),
                        _ => {}
                    }
                }
            }
            Some("assistant") => {
                for p in content.as_array().into_iter().flatten() {
                    match p.get("type").and_then(Value::as_str) {
                        Some("text") => push(&mut msgs, Role::Assistant, Block::Text(s(p, "text").unwrap_or_default())),
                        Some("tool_use") => {
                            let name = s(p, "name").unwrap_or_default();
                            let tool = from_claude_tool(&name, p.get("input").unwrap_or(&Value::Null));
                            push(&mut msgs, Role::Assistant, Block::Call { id: s(p, "id").unwrap_or_default(), tool });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(msgs)
}

pub fn read_codex(path: &Path, from: usize) -> std::io::Result<Vec<Msg>> {
    let lines = read_lines(path)?;
    let mut msgs = Vec::new();
    let item = |msgs: &mut Vec<Msg>, p: &Value| {
        let wrapper = json!({"type": "response_item", "payload": p});
        match p.get("type").and_then(Value::as_str) {
            Some("message") => match p.get("role").and_then(Value::as_str) {
                Some("user") => {
                    if let Some(t) = codex_user_text(&wrapper) {
                        push(msgs, Role::User, Block::Text(t));
                    }
                }
                Some("assistant") => push(msgs, Role::Assistant, Block::Text(codex_content_text(p))),
                _ => {}
            },
            Some("function_call") => {
                let raw = s(p, "arguments").unwrap_or_default();
                let tool = from_codex_call(&s(p, "name").unwrap_or_default(), &raw);
                push(msgs, Role::Assistant, Block::Call { id: s(p, "call_id").unwrap_or_default(), tool });
            }
            Some("custom_tool_call") => {
                let raw = s(p, "input").unwrap_or_default();
                let tool = from_codex_call(&s(p, "name").unwrap_or_default(), &raw);
                push(msgs, Role::Assistant, Block::Call { id: s(p, "call_id").unwrap_or_default(), tool });
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                let out = result_text(p.get("output").unwrap_or(&Value::Null));
                push(msgs, Role::User, Block::Result { id: s(p, "call_id").unwrap_or_default(), output: out, is_error: false });
            }
            _ => {}
        }
    };
    for e in lines.iter().skip(from) {
        match e.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                if let Some(p) = e.get("payload") {
                    item(&mut msgs, p);
                }
            }
            Some("compacted") => {
                msgs.clear();
                push(&mut msgs, Role::User, Block::Text(
                    "[Earlier conversation was compacted by Codex; its summary is not readable outside Codex.]".into(),
                ));
                for p in e.pointer("/payload/replacement_history").and_then(Value::as_array).into_iter().flatten() {
                    item(&mut msgs, p);
                }
            }
            _ => {}
        }
    }
    Ok(msgs)
}

/// The active branch of a pi session (pi sessions are trees: /tree can
/// branch), as its message entries in order.
fn pi_path(lines: &[Value]) -> Vec<&Value> {
    let by_id: HashMap<&str, &Value> = lines
        .iter()
        .filter_map(|e| Some((e.get("id")?.as_str()?, e)))
        .filter(|(_, e)| e.get("type").and_then(Value::as_str) != Some("session"))
        .collect();
    let Some(mut cur) = lines.iter().rev().find(|e| {
        e.get("type").and_then(Value::as_str) != Some("session") && e.get("id").is_some()
    }) else {
        return Vec::new();
    };
    let mut path = vec![cur];
    while let Some(p) = cur.get("parentId").and_then(Value::as_str).and_then(|p| by_id.get(p)) {
        cur = p;
        path.push(cur);
    }
    path.reverse();
    path.into_iter().filter(|e| e.get("type").and_then(Value::as_str) == Some("message")).collect()
}

pub fn pi_entry_count(path: &Path) -> usize {
    read_lines(path).map(|l| pi_path(&l).len()).unwrap_or(0)
}

/// pi session file, skipping the first `from` messages of the active branch.
pub fn read_pi(path: &Path, from: usize) -> std::io::Result<Vec<Msg>> {
    let lines = read_lines(path)?;
    let mut msgs = Vec::new();
    for e in pi_path(&lines).into_iter().skip(from) {
        let m = &e["message"];
        match m.get("role").and_then(Value::as_str) {
            Some("user") => push(&mut msgs, Role::User, Block::Text(result_text(m.get("content").unwrap_or(&Value::Null)))),
            Some("assistant") => {
                for b in m.get("content").and_then(Value::as_array).into_iter().flatten() {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => push(&mut msgs, Role::Assistant, Block::Text(s(b, "text").unwrap_or_default())),
                        Some("toolCall") => {
                            let tool = from_pi_tool(&s(b, "name").unwrap_or_default(), b.get("arguments").unwrap_or(&Value::Null));
                            push(&mut msgs, Role::Assistant, Block::Call { id: s(b, "id").unwrap_or_default(), tool });
                        }
                        _ => {}
                    }
                }
            }
            Some("toolResult") => push(&mut msgs, Role::User, Block::Result {
                id: s(m, "toolCallId").unwrap_or_default(),
                output: result_text(m.get("content").unwrap_or(&Value::Null)),
                is_error: m.get("isError").and_then(Value::as_bool) == Some(true),
            }),
            _ => {}
        }
    }
    Ok(msgs)
}

/// The latest assistant text in a transcript, reading only its tail, for
/// the one-line summary in the session list.
pub fn last_assistant_text(backend: Backend, path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(256 * 1024);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    for l in lines.iter().rev() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let found = match backend {
            Backend::Claude => (v.get("type").and_then(Value::as_str) == Some("assistant")
                && v.get("isSidechain").and_then(Value::as_bool) != Some(true))
            .then(|| {
                v.pointer("/message/content")?.as_array()?.iter().rev().find_map(|b| {
                    (b.get("type").and_then(Value::as_str) == Some("text")).then(|| s(b, "text")).flatten()
                })
            })
            .flatten(),
            Backend::Codex => (v.pointer("/payload/type").and_then(Value::as_str) == Some("message")
                && v.pointer("/payload/role").and_then(Value::as_str) == Some("assistant"))
            .then(|| codex_content_text(&v["payload"]))
            .filter(|t| !t.trim().is_empty()),
            Backend::Pi => (v.pointer("/message/role").and_then(Value::as_str) == Some("assistant"))
                .then(|| {
                    v.pointer("/message/content")?.as_array()?.iter().rev().find_map(|b| {
                        (b.get("type").and_then(Value::as_str) == Some("text")).then(|| s(b, "text")).flatten()
                    })
                })
                .flatten(),
        };
        if let Some(t) = found.filter(|t| !t.trim().is_empty()) {
            // First non-empty line, without markdown emphasis noise.
            let line = t.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("```")).unwrap_or("");
            return Some(line.trim_start_matches(['#', '*', '-', ' ']).to_string());
        }
    }
    None
}

pub fn read(backend: Backend, path: &Path, from: usize) -> std::io::Result<Vec<Msg>> {
    match backend {
        Backend::Claude => read_claude(path, from),
        Backend::Codex => read_codex(path, from),
        Backend::Pi => read_pi(path, from),
    }
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

fn now_ts() -> (String, i64) {
    let now = chrono::Utc::now();
    (now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), now.timestamp_millis())
}

/// Tool call ids must be `[A-Za-z0-9_-]` for the Anthropic API.
fn clean_id(id: &str, i: usize) -> String {
    let c: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    if c.is_empty() { format!("call_{i}") } else { c }
}

/// Append `msgs` to a pi session file (creating it with a header when
/// `parent` is None). Returns the new leaf id.
pub fn append_pi(
    file: &Path,
    msgs: &[Msg],
    cwd: &str,
    parent: Option<String>,
    model: Option<&str>,
) -> std::io::Result<Option<String>> {
    let (ts, ms) = now_ts();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    if parent.is_none() {
        let id = uuid::Uuid::new_v4().to_string();
        writeln!(f, "{}", json!({"type": "session", "version": 3, "id": id, "timestamp": ts, "cwd": cwd}))?;
    }
    let mut parent = parent;
    let mut emit = |f: &mut std::fs::File, message: Value| -> std::io::Result<()> {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        writeln!(f, "{}", json!({"type": "message", "id": id, "parentId": parent, "timestamp": ts, "message": message}))?;
        parent = Some(id);
        Ok(())
    };
    let usage = json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                       "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}});
    let mut names: HashMap<String, String> = HashMap::new();
    for (i, m) in msgs.iter().enumerate() {
        match m.role {
            Role::Assistant => {
                let mut content = Vec::new();
                for b in &m.blocks {
                    match b {
                        Block::Text(t) => content.push(json!({"type": "text", "text": t})),
                        Block::Call { id, tool } => {
                            let (name, args) = to_pi_tool(tool);
                            names.insert(id.clone(), name.clone());
                            content.push(json!({"type": "toolCall", "id": clean_id(id, i), "name": name, "arguments": args}));
                        }
                        Block::Result { .. } => {}
                    }
                }
                let has_calls = content.iter().any(|c| c["type"] == "toolCall");
                // provider "imported": pi then treats these as another
                // model's turns and re-normalizes call ids per provider.
                emit(&mut f, json!({"role": "assistant", "content": content, "api": "openai-completions",
                    "provider": "imported", "model": "imported", "usage": usage,
                    "stopReason": if has_calls { "toolUse" } else { "stop" }, "timestamp": ms}))?;
            }
            Role::User => {
                let mut text = Vec::new();
                for b in &m.blocks {
                    match b {
                        Block::Result { id, output, is_error } => emit(&mut f, json!({"role": "toolResult",
                            "toolCallId": clean_id(id, i), "toolName": names.get(id).cloned().unwrap_or_default(),
                            "content": [{"type": "text", "text": cap(output, TOOL_OUTPUT_MAX)}],
                            "isError": is_error, "timestamp": ms}))?,
                        Block::Text(t) => text.push(t.clone()),
                        Block::Call { .. } => {}
                    }
                }
                if !text.is_empty() {
                    emit(&mut f, json!({"role": "user", "content": [{"type": "text", "text": text.join("\n\n")}], "timestamp": ms}))?;
                }
            }
        }
    }
    if let Some((provider, model_id)) = model.and_then(|m| m.split_once('/')) {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        writeln!(f, "{}", json!({"type": "model_change", "id": id, "parentId": parent, "timestamp": ts,
            "provider": provider, "modelId": model_id}))?;
        parent = Some(id);
    }
    Ok(parent)
}

/// Last entry id of a pi session file (to append after it).
pub fn pi_leaf(file: &Path) -> Option<String> {
    let lines = read_lines(file).ok()?;
    lines.iter().rev().find_map(|e| {
        (e.get("type").and_then(Value::as_str) != Some("session")).then(|| s(e, "id")).flatten()
    })
}

/// ~/.claude/projects/<cwd with every non-alphanumeric char as '-'>/
pub fn claude_project_dir(cwd: &str) -> PathBuf {
    let enc: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    crate::paths::claude_projects_dir().join(enc)
}

/// Append `msgs` to a Claude Code transcript for `session_id`. Returns the
/// last entry uuid.
/// `model` goes on the written assistant turns: Claude Code restores the
/// session's model from them, and warns on a name it doesn't know.
pub fn append_claude(
    file: &Path,
    msgs: &[Msg],
    cwd: &str,
    session_id: &str,
    parent: Option<String>,
    model: Option<&str>,
) -> std::io::Result<Option<String>> {
    let model = model.map(str::to_string).or_else(|| claude_last_model(file));
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    let (ts, _) = now_ts();
    let branch = crate::repo::git(Path::new(cwd), &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "HEAD".into());
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    let mut parent = parent;
    for (i, m) in msgs.iter().enumerate() {
        let uuid = uuid::Uuid::new_v4().to_string();
        let mut content = Vec::new();
        for b in &m.blocks {
            match (m.role, b) {
                (_, Block::Text(t)) => content.push(json!({"type": "text", "text": t})),
                (Role::Assistant, Block::Call { id, tool }) => match tool {
                    // One Edit per change: Claude's Edit takes a single pair.
                    Tool::Edit { path, edits } if edits.len() > 1 => {
                        for (k, (o, nw)) in edits.iter().enumerate() {
                            content.push(json!({"type": "tool_use", "id": format!("{}_{k}", clean_id(id, i)), "name": "Edit",
                                "input": {"file_path": path, "old_string": o, "new_string": nw}}));
                        }
                    }
                    Tool::Other { .. } => content.push(json!({"type": "text", "text": call_as_text(tool, neutral_name)})),
                    _ => {
                        let (name, input) = to_claude_tool(tool);
                        content.push(json!({"type": "tool_use", "id": clean_id(id, i), "name": name, "input": input}));
                    }
                },
                (Role::User, Block::Result { id, output, is_error }) => {
                    content.push(json!({"type": "tool_result", "tool_use_id": clean_id(id, i),
                        "content": cap(output, TOOL_OUTPUT_MAX), "is_error": is_error}));
                }
                _ => {}
            }
        }
        if content.is_empty() {
            continue;
        }
        let entry = match m.role {
            Role::User => json!({"parentUuid": parent, "isSidechain": false, "type": "user",
                "message": {"role": "user", "content": content}, "uuid": uuid, "timestamp": ts,
                "userType": "external", "entrypoint": "cli", "cwd": cwd, "sessionId": session_id, "gitBranch": branch}),
            Role::Assistant => {
                let calls = content.iter().any(|c| c["type"] == "tool_use");
                json!({"parentUuid": parent, "isSidechain": false, "type": "assistant",
                    "message": {"id": format!("msg_orchestra_{i}"), "type": "message", "role": "assistant",
                        "model": model, "content": content,
                        "stop_reason": if calls { "tool_use" } else { "end_turn" }, "stop_sequence": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}},
                    "uuid": uuid, "timestamp": ts, "userType": "external", "entrypoint": "cli",
                    "cwd": cwd, "sessionId": session_id, "gitBranch": branch})
            }
        };
        writeln!(f, "{entry}")?;
        parent = Some(uuid);
    }
    Ok(parent)
}

/// Fixup for a Claude Code call-pairing rule: foreign (`Other`) calls are
/// written as text, so their results must be text too. Apply before
/// `append_claude`.
pub fn claude_ready(msgs: Vec<Msg>) -> Vec<Msg> {
    let mut other_ids = HashSet::new();
    let mut out = Vec::new();
    for m in msgs {
        let blocks = m
            .blocks
            .into_iter()
            .map(|b| match b {
                Block::Call { id, tool: t @ Tool::Other { .. } } => {
                    other_ids.insert(id);
                    Block::Text(call_as_text(&t, neutral_name))
                }
                Block::Result { id, output, .. } if other_ids.contains(&id) => {
                    Block::Text(format!("[result]\n{}", cap(&output, TOOL_OUTPUT_MAX)))
                }
                other => other,
            })
            .collect();
        out.push(Msg { role: m.role, blocks });
    }
    out
}

/// Model of the last assistant turn in a Claude transcript.
pub fn claude_last_model(file: &Path) -> Option<String> {
    let lines = tail_values(file, 4 << 20);
    lines.iter().rev().find_map(|e| {
        (e.get("type").and_then(Value::as_str) == Some("assistant"))
            .then(|| e.pointer("/message/model").and_then(Value::as_str).map(str::to_string))
            .flatten()
            .filter(|m| m != "imported" && m != "<synthetic>")
    })
}

/// Last uuid of a Claude transcript.
pub fn claude_leaf(file: &Path) -> Option<String> {
    let lines = tail_values(file, 4 << 20);
    lines.iter().rev().find_map(|e| s(e, "uuid"))
}

/// Copy a Claude transcript under a new session id (a fork), so appending
/// never touches the original (it may still be open somewhere). Streams
/// the file once, replacing the old session id (the file name) byte for
/// byte — no JSON re-encoding, so an 83 MB transcript copies in about a
/// second instead of minutes.
pub fn fork_claude(src: &Path, dst: &Path, new_id: &str) -> std::io::Result<()> {
    use std::io::{BufRead, Write};
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d)?;
    }
    let old_id = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let (old, new) = (old_id.as_bytes(), new_id.as_bytes());
    let mut input = BufReader::with_capacity(1 << 20, std::fs::File::open(src)?);
    let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(dst)?);
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if old.is_empty() || !line.windows(old.len()).any(|w| w == old) {
            out.write_all(&line)?;
            continue;
        }
        let mut i = 0;
        while i < line.len() {
            if line[i..].starts_with(old) {
                out.write_all(new)?;
                i += old.len();
            } else {
                out.write_all(&line[i..i + 1])?;
                i += 1;
            }
        }
    }
    if !line.ends_with(b"\n") && !line.is_empty() {
        out.write_all(b"\n")?;
    }
    out.flush()
}

/// Path for building a transcript before it appears under its real name:
/// `x.jsonl` → `x.jsonl.part` (not a `.jsonl`, so nothing lists it).
pub fn part_path(file: &Path) -> PathBuf {
    let mut p = file.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

/// Codex rollouts can't take foreign tool calls (its tools are JavaScript
/// cells run in its own harness), so everything becomes text. Returns the
/// thread id; Codex indexes the file on its next start.
pub fn write_codex(msgs: &[Msg], cwd: &str) -> std::io::Result<(String, PathBuf)> {
    let now = chrono::Utc::now();
    let id = uuid::Uuid::now_v7().to_string();
    let dir = crate::paths::codex_dir().join("sessions").join(now.format("%Y/%m/%d").to_string());
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("rollout-{}-{id}.jsonl", now.format("%Y-%m-%dT%H-%M-%S")));
    let ts = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut f = std::fs::File::create(&file)?;
    writeln!(f, "{}", json!({"timestamp": ts, "type": "session_meta", "payload": {
        "id": id, "session_id": id, "timestamp": ts, "cwd": cwd, "originator": "orchestra",
        "cli_version": "0.0.0", "source": "cli", "thread_source": "user", "model_provider": "openai"}}))?;
    for m in msgs {
        let (role, kind) = match m.role {
            Role::User => ("user", "input_text"),
            Role::Assistant => ("assistant", "output_text"),
        };
        let text: Vec<String> = m
            .blocks
            .iter()
            .map(|b| match b {
                Block::Text(t) => t.clone(),
                Block::Call { tool, .. } => call_as_text(tool, neutral_name),
                Block::Result { output, is_error, .. } => {
                    format!("[result{}]\n{}", if *is_error { " (error)" } else { "" }, cap(output, TOOL_OUTPUT_MAX))
                }
            })
            .collect();
        writeln!(f, "{}", json!({"timestamp": ts, "type": "response_item", "payload": {
            "type": "message", "role": role, "content": [{"type": kind, "text": text.join("\n\n")}]}}))?;
    }
    Ok((id, file))
}

/// A note telling the model what just happened, as a user turn plus a
/// short acknowledgement (keeps alternation, and keeps the model from
/// answering the note on its first real turn).
pub fn handoff_note(from: &str, to: &str) -> Vec<Msg> {
    vec![
        Msg {
            role: Role::User,
            blocks: vec![Block::Text(format!(
                "[orchestra] This conversation was moved from {from} to {to}. The history above came \
                 from {from}: its shell, read, write and edit tool calls were converted to your tools; \
                 other tool calls are shown as text, and long tool output was shortened. The files on \
                 disk are the current state — re-read a file before editing it."
            ))],
        },
        Msg { role: Role::Assistant, blocks: vec![Block::Text("Understood — continuing from here.".into())] },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(role: Role, t: &str) -> Msg {
        Msg { role, blocks: vec![Block::Text(t.into())] }
    }
    fn write_jsonl(p: &Path, lines: &[Value]) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
    }

    #[test]
    fn tool_mapping_roundtrips() {
        let cases = [
            ("Bash", json!({"command": "ls -la", "description": "list"})),
            ("Read", json!({"file_path": "/a.rs", "offset": 10, "limit": 5})),
            ("Write", json!({"file_path": "/a.rs", "content": "x"})),
            ("Edit", json!({"file_path": "/a.rs", "old_string": "a", "new_string": "b"})),
        ];
        for (name, input) in cases {
            let t = from_claude_tool(name, &input);
            assert!(!matches!(t, Tool::Other { .. }), "{name} should map");
            let (pn, pa) = to_pi_tool(&t);
            assert_eq!(from_pi_tool(&pn, &pa), t, "pi roundtrip for {name}");
            let (cn, _) = to_claude_tool(&t);
            assert_eq!(cn, name);
        }
        let t = from_claude_tool("Edit", &json!({"file_path": "/a", "old_string": "a", "new_string": "b", "replace_all": true}));
        assert!(matches!(t, Tool::Other { .. }), "replace_all has no equivalent");
        assert!(matches!(from_claude_tool("WebFetch", &json!({"url": "x"})), Tool::Other { .. }));
    }

    #[test]
    fn codex_exec_cells() {
        assert_eq!(
            from_codex_call("exec", r#"text(await tools.exec_command({cmd:"pwd && rg --files -g \"AGENTS.md\"","max_output_tokens":2000}));"#),
            Tool::Shell { command: "pwd && rg --files -g \"AGENTS.md\"".into() }
        );
        assert!(matches!(
            from_codex_call("exec", "const a = await tools.exec_command({cmd:'ls'}); const b = await tools.exec_command({cmd:'pwd'});"),
            Tool::Other { .. }
        ));
        assert_eq!(
            from_codex_call("shell", r#"{"command":["bash","-lc","cargo test"],"workdir":"/r"}"#),
            Tool::Shell { command: "cargo test".into() }
        );
        assert!(matches!(from_codex_call("wait", r#"{"cell_id":"1"}"#), Tool::Other { .. }));
    }

    #[test]
    fn normalize_pairs_calls_and_results() {
        let call = |id: &str| Block::Call { id: id.into(), tool: Tool::Shell { command: "ls".into() } };
        let res = |id: &str| Block::Result { id: id.into(), output: "ok".into(), is_error: false };
        let msgs = vec![
            text(Role::Assistant, "leading assistant turn is dropped"),
            text(Role::User, "go"),
            Msg { role: Role::Assistant, blocks: vec![call("a"), call("b")] },
            Msg { role: Role::User, blocks: vec![res("a"), res("zzz")] }, // b orphaned, zzz unknown
            Msg { role: Role::Assistant, blocks: vec![Block::Text("done".into())] },
            Msg { role: Role::Assistant, blocks: vec![call("c")] }, // merged, never answered
        ];
        let out = normalize(msgs);
        assert_eq!(out[0].role, Role::User);
        for w in out.windows(2) {
            assert_ne!(w[0].role, w[1].role, "strict alternation");
        }
        let results: Vec<&str> = out
            .iter()
            .flat_map(|m| &m.blocks)
            .filter_map(|b| if let Block::Result { id, .. } = b { Some(id.as_str()) } else { None })
            .collect();
        assert_eq!(results, vec!["a", "b", "c"], "b and c get synthesized results; zzz becomes text");
        assert!(out.iter().flat_map(|m| &m.blocks).any(|b| matches!(b, Block::Text(t) if t.starts_with("[tool result]"))));
    }

    #[test]
    fn budget_cuts_at_prompts_only() {
        let mut msgs = vec![text(Role::User, "first")];
        for i in 0..6 {
            msgs.push(Msg { role: Role::Assistant, blocks: vec![Block::Call { id: format!("c{i}"), tool: Tool::Shell { command: "x".repeat(50) } }] });
            msgs.push(Msg { role: Role::User, blocks: vec![Block::Result { id: format!("c{i}"), output: "y".repeat(100), is_error: false }, Block::Text(format!("prompt {i}"))] });
        }
        let out = normalize(fit_budget(normalize(msgs), 500));
        assert!(matches!(&out[0].blocks[0], Block::Text(t) if t == "first"));
        assert!(matches!(&out[1].blocks[0], Block::Text(t) if t.contains("omitted")));
        // Every call still has its result.
        let calls = out.iter().flat_map(|m| &m.blocks).filter(|b| matches!(b, Block::Call { .. })).count();
        let results = out.iter().flat_map(|m| &m.blocks).filter(|b| matches!(b, Block::Result { .. })).count();
        assert_eq!(calls, results);
    }

    #[test]
    fn claude_to_pi_to_claude_keeps_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc.jsonl");
        write_jsonl(&cc, &[
            json!({"type":"user","message":{"content":"fix it"}}),
            json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":"","signature":"s"},
                {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test"}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"1 failed"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"mcp__x__y","input":{"q":1}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"mcp out"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"fixed"}]}}),
        ]);
        let msgs = normalize(read_claude(&cc, 0).unwrap());
        assert_eq!(msgs.len(), 6);

        // → pi: real toolCall with pi's name, toolResult entries.
        let pi = tmp.path().join("pi.jsonl");
        append_pi(&pi, &msgs, "/w", None, Some("openrouter/qwen/qwen3")).unwrap();
        let lines: Vec<Value> = read_lines(&pi).unwrap();
        let call = lines.iter().find(|l| l["message"]["content"][0]["type"] == "toolCall").unwrap();
        assert_eq!(call["message"]["content"][0]["name"], "bash");
        assert_eq!(call["message"]["content"][0]["arguments"]["command"], "cargo test");
        assert!(lines.iter().any(|l| l["message"]["role"] == "toolResult" && l["message"]["toolName"] == "bash"));
        assert_eq!(lines.last().unwrap()["type"], "model_change");

        // pi → neutral again: same calls.
        let back = normalize(read_pi(&pi, 0).unwrap());
        assert_eq!(back, msgs);

        // → Claude: Bash stays a tool_use, the MCP call becomes text.
        let out = tmp.path().join("out.jsonl");
        append_claude(&out, &claude_ready(msgs), "/w", "SID", None, None).unwrap();
        let lines = read_lines(&out).unwrap();
        let kinds: Vec<String> = lines
            .iter()
            .flat_map(|l| l["message"]["content"].as_array().cloned().unwrap_or_default())
            .map(|c| c["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, vec!["text", "tool_use", "tool_result", "text", "text", "text"]);
        assert!(lines.iter().all(|l| l["sessionId"] == "SID"));
        assert_eq!(lines[1]["parentUuid"], lines[0]["uuid"]);
        // And it reads back to the same text-for-foreign-tools history.
        let reread = normalize(read_claude(&out, 0).unwrap());
        assert_eq!(reread.len(), 6);
    }

    #[test]
    fn read_from_offset_skips_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc.jsonl");
        write_jsonl(&cc, &[
            json!({"type":"user","message":{"content":"seeded"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"seeded reply"}]}}),
            json!({"type":"user","message":{"content":"new"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"new reply"}]}}),
        ]);
        let delta = read_claude(&cc, 2).unwrap();
        assert_eq!(delta, vec![text(Role::User, "new"), text(Role::Assistant, "new reply")]);
    }

    #[test]
    fn pi_follows_active_branch_and_multi_edit_splits() {
        let tmp = tempfile::tempdir().unwrap();
        let pi = tmp.path().join("pi.jsonl");
        write_jsonl(&pi, &[
            json!({"type":"session","id":"S"}),
            json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"q"}}),
            json!({"type":"message","id":"b","parentId":"a","message":{"role":"assistant","content":[{"type":"text","text":"abandoned"}]}}),
            json!({"type":"message","id":"c","parentId":"a","message":{"role":"assistant","content":[
                {"type":"toolCall","id":"t1","name":"edit","arguments":{"path":"/f","edits":[{"oldText":"1","newText":"2"},{"oldText":"3","newText":"4"}]}}]}}),
            json!({"type":"message","id":"d","parentId":"c","message":{"role":"toolResult","toolCallId":"t1","toolName":"edit","content":[{"type":"text","text":"ok"}]}}),
        ]);
        let msgs = normalize(read_pi(&pi, 0).unwrap());
        assert!(!format!("{msgs:?}").contains("abandoned"), "off-branch entry skipped");
        let out = tmp.path().join("cc.jsonl");
        append_claude(&out, &claude_ready(msgs), "/w", "SID", None, None).unwrap();
        let s = std::fs::read_to_string(out).unwrap();
        assert_eq!(s.matches("\"name\":\"Edit\"").count(), 2);
    }

    #[test]
    fn codex_writer_is_text_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CODEX_HOME", tmp.path());
        let msgs = normalize(vec![
            text(Role::User, "go"),
            Msg { role: Role::Assistant, blocks: vec![Block::Call { id: "1".into(), tool: Tool::Shell { command: "ls".into() } }] },
            Msg { role: Role::User, blocks: vec![Block::Result { id: "1".into(), output: "a.rs".into(), is_error: false }] },
        ]);
        let (id, file) = write_codex(&msgs, "/w").unwrap();
        std::env::remove_var("CODEX_HOME");
        let lines = read_lines(&file).unwrap();
        assert_eq!(lines[0]["payload"]["id"], id);
        assert!(lines[2]["payload"]["content"][0]["text"].as_str().unwrap().contains("[called shell] ls"));
        let back = read_codex(&file, 0).unwrap();
        assert_eq!(back.len(), 3);
    }

    #[test]
    fn last_assistant_text_per_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let cc = tmp.path().join("cc.jsonl");
        write_jsonl(&cc, &[
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"old"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"## Fixed the bug\nmore"}]}}),
            json!({"type":"user","message":{"content":"thanks"}}),
        ]);
        assert_eq!(last_assistant_text(Backend::Claude, &cc).as_deref(), Some("Fixed the bug"));
        let pi = tmp.path().join("pi.jsonl");
        write_jsonl(&pi, &[json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"pi says"}]}})]);
        assert_eq!(last_assistant_text(Backend::Pi, &pi).as_deref(), Some("pi says"));
        let cx = tmp.path().join("cx.jsonl");
        write_jsonl(&cx, &[json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"codex says"}]}})]);
        assert_eq!(last_assistant_text(Backend::Codex, &cx).as_deref(), Some("codex says"));
    }

    #[test]
    fn fork_replaces_id_and_keeps_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("11111111-aaaa.jsonl");
        std::fs::write(&src, "{\"sessionId\":\"11111111-aaaa\",\"uuid\":\"u1\",\"type\":\"user\"}\n{\"type\":\"assistant\",\"uuid\":\"u2\",\"sessionId\":\"11111111-aaaa\",\"message\":{\"model\":\"claude-x\"}}\n").unwrap();
        let dst = tmp.path().join("new.jsonl");
        fork_claude(&src, &dst, "22222222-bbbb").unwrap();
        let out = std::fs::read_to_string(&dst).unwrap();
        assert!(!out.contains("11111111-aaaa") && out.matches("22222222-bbbb").count() == 2);
        assert_eq!(line_count(&dst), 2);
        assert_eq!(claude_leaf(&dst).as_deref(), Some("u2"));
        assert_eq!(claude_last_model(&dst).as_deref(), Some("claude-x"));
        assert!(part_path(&dst).to_string_lossy().ends_with("new.jsonl.part"));
    }

    #[test]
    fn cap_keeps_head_and_tail() {
        let s = format!("{}END", "a".repeat(100));
        let c = cap(&s, 30);
        assert!(c.starts_with("aaaa") && c.ends_with("END") && c.contains("omitted"));
    }

    #[test]
    fn claude_project_dir_encoding() {
        let d = claude_project_dir("/home/sky/sky_workdir/feature-plugin/.claude/worktrees/x");
        assert!(d.ends_with("-home-sky-sky-workdir-feature-plugin--claude-worktrees-x"));
    }
}

