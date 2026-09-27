// convert.rs — Fork a Claude Code or Codex transcript into a pi session so
// the conversation can continue in pi with any provider/model.
//
// Output is a pi v3 session file (header + `message` entries chained by
// parentId), written to the new session's --session-dir so
// `pi --session-dir <dir> --continue` picks it up.
//
// Tool calls and results are flattened into text. Replaying them as real
// tool-call blocks would tie the history to the source agent's tool names
// and ids, which other providers reject; as text, any model can read what
// happened. Thinking blocks are dropped (their signatures are
// provider-specific). Where the source compacted, only the post-compaction
// history is kept — the same context the source agent itself would send.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::import::{claude_user_text, codex_content_text, codex_user_text, strip_tagged};
use crate::session::Backend;

/// Per tool call / tool result text cap.
const TOOL_INPUT_MAX: usize = 600;
const TOOL_OUTPUT_MAX: usize = 1500;
/// Whole-history cap (~150k tokens). Older turns beyond this are dropped.
const HISTORY_MAX_CHARS: usize = 600_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

fn cap(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… [truncated, {} chars total]", s.chars().count())
}

/// Append text to the history, merging consecutive same-role turns.
fn push(turns: &mut Vec<Turn>, role: Role, text: String) {
    if text.trim().is_empty() {
        return;
    }
    match turns.last_mut() {
        Some(last) if last.role == role => {
            last.text.push_str("\n\n");
            last.text.push_str(&text);
        }
        _ => turns.push(Turn { role, text }),
    }
}

fn read_jsonl(path: &Path) -> std::io::Result<Vec<Value>> {
    let f = std::fs::File::open(path)?;
    Ok(BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect())
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

pub fn claude_turns(entries: &[Value]) -> Vec<Turn> {
    let main: Vec<&Value> = entries
        .iter()
        .filter(|e| matches!(e.get("type").and_then(Value::as_str), Some("user" | "assistant")))
        .filter(|e| e.get("isSidechain").and_then(Value::as_bool) != Some(true))
        .collect();
    let start = main
        .iter()
        .rposition(|e| e.get("isCompactSummary").and_then(Value::as_bool) == Some(true))
        .unwrap_or(0);

    let mut turns = Vec::new();
    let mut tool_names = std::collections::HashMap::new();
    for e in &main[start..] {
        let is_summary = e.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
        match e.get("type").and_then(Value::as_str) {
            Some("user") => {
                if is_summary {
                    let text = tool_result_text(e.pointer("/message/content").unwrap_or(&Value::Null));
                    push(&mut turns, Role::User, text);
                    continue;
                }
                if let Some(t) = claude_user_text(e) {
                    push(&mut turns, Role::User, t);
                }
                for part in e.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    if part.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let id = part.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    let name = tool_names.get(id).cloned().unwrap_or_else(|| "tool".to_string());
                    let body = strip_tagged(&tool_result_text(part.get("content").unwrap_or(&Value::Null)), "system-reminder");
                    let err = if part.get("is_error").and_then(Value::as_bool) == Some(true) { " (error)" } else { "" };
                    push(&mut turns, Role::User, format!("[{name} result{err}]\n{}", cap(&body, TOOL_OUTPUT_MAX)));
                }
            }
            Some("assistant") => {
                for part in e.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = part.get("text").and_then(Value::as_str).unwrap_or("");
                            push(&mut turns, Role::Assistant, t.to_string());
                        }
                        Some("tool_use") => {
                            let name = part.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
                            if let Some(id) = part.get("id").and_then(Value::as_str) {
                                tool_names.insert(id.to_string(), name.clone());
                            }
                            let input = part.get("input").map(Value::to_string).unwrap_or_default();
                            push(&mut turns, Role::Assistant, format!("[called {name}] {}", cap(&input, TOOL_INPUT_MAX)));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    turns
}

pub fn codex_turns(entries: &[Value]) -> Vec<Turn> {
    let mut turns = Vec::new();
    let mut names = std::collections::HashMap::new();
    let mut item = |turns: &mut Vec<Turn>, p: &Value, wrapper: &Value| {
        match p.get("type").and_then(Value::as_str) {
            Some("message") => match p.get("role").and_then(Value::as_str) {
                Some("user") => {
                    if let Some(t) = codex_user_text(wrapper) {
                        push(turns, Role::User, t);
                    }
                }
                Some("assistant") => push(turns, Role::Assistant, codex_content_text(p)),
                _ => {}
            },
            Some(kind @ ("function_call" | "custom_tool_call")) => {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
                if let Some(id) = p.get("call_id").and_then(Value::as_str) {
                    names.insert(id.to_string(), name.clone());
                }
                let key = if kind == "function_call" { "arguments" } else { "input" };
                let input = match p.get(key) {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                push(turns, Role::Assistant, format!("[called {name}] {}", cap(&input, TOOL_INPUT_MAX)));
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                let id = p.get("call_id").and_then(Value::as_str).unwrap_or("");
                let name = names.get(id).cloned().unwrap_or_else(|| "tool".to_string());
                let out = tool_result_text(p.get("output").unwrap_or(&Value::Null));
                push(turns, Role::User, format!("[{name} result]\n{}", cap(&out, TOOL_OUTPUT_MAX)));
            }
            _ => {}
        }
    };
    for e in entries {
        match e.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                if let Some(p) = e.get("payload") {
                    item(&mut turns, p, e);
                }
            }
            Some("compacted") => {
                // Codex replaces the history with the retained items plus
                // an encrypted summary we can't read.
                turns.clear();
                push(&mut turns, Role::User, "[Earlier conversation was compacted by Codex; its summary is not available.]".to_string());
                for p in e.pointer("/payload/replacement_history").and_then(Value::as_array).into_iter().flatten() {
                    let wrapper = json!({"type": "response_item", "payload": p});
                    item(&mut turns, p, &wrapper);
                }
            }
            _ => {}
        }
    }
    turns
}

/// Keep the first user turn plus as many of the latest turns as fit.
pub fn fit_budget(turns: Vec<Turn>, max_chars: usize) -> Vec<Turn> {
    let total: usize = turns.iter().map(|t| t.text.len()).sum();
    if total <= max_chars || turns.len() < 3 {
        return turns;
    }
    let first = turns[0].clone();
    let mut budget = max_chars.saturating_sub(first.text.len());
    let mut tail = Vec::new();
    for t in turns[1..].iter().rev() {
        if t.text.len() > budget {
            break;
        }
        budget -= t.text.len();
        tail.push(t.clone());
    }
    tail.reverse();
    // pi expects alternation after the first user turn; start the kept
    // tail on an assistant turn.
    while tail.first().is_some_and(|t| t.role == Role::User) {
        tail.remove(0);
    }
    let dropped = turns.len() - 1 - tail.len();
    let mut out = vec![first];
    out[0].text.push_str(&format!("\n\n[… {dropped} earlier messages omitted when importing into pi]"));
    out.extend(tail);
    out
}

/// Write `turns` as a pi session file in `session_dir`. Returns the path.
/// `model` (`provider/id`) is recorded as the session's model; pi restores
/// the last model_change / assistant model on --continue, and without it
/// would try to restore the placeholder model on the imported messages.
pub fn write_pi_session(
    turns: &[Turn],
    session_dir: &Path,
    cwd: &str,
    source: &str,
    model: Option<&str>,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(session_dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let ts = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let ms = now.timestamp_millis();
    let file = session_dir.join(format!("{}_{id}.jsonl", ts.replace([':', '.'], "-")));
    let mut f = std::fs::File::create(&file)?;
    writeln!(f, "{}", json!({"type": "session", "version": 3, "id": id, "timestamp": ts, "cwd": cwd}))?;

    let mut parent: Option<String> = None;
    for (i, t) in turns.iter().enumerate() {
        // 8-hex ids like pi's own; multiply-by-odd + xor is a bijection on
        // u32, so ids never collide within a file.
        let entry_id = format!("{:08x}", (i as u32).wrapping_mul(2654435761) ^ 0x5eed_0000);
        let message = match t.role {
            Role::User => json!({
                "role": "user",
                "content": [{"type": "text", "text": t.text}],
                "timestamp": ms,
            }),
            Role::Assistant => json!({
                "role": "assistant",
                "content": [{"type": "text", "text": t.text}],
                "api": "openai-completions",
                "provider": "imported",
                "model": source,
                "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                          "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
                "stopReason": "stop",
                "timestamp": ms,
            }),
        };
        writeln!(
            f,
            "{}",
            json!({"type": "message", "id": entry_id, "parentId": parent, "timestamp": ts, "message": message})
        )?;
        parent = Some(entry_id);
    }
    if let Some((provider, model_id)) = model.and_then(|m| m.split_once('/')) {
        writeln!(
            f,
            "{}",
            json!({"type": "model_change", "id": "ffffffff", "parentId": parent, "timestamp": ts,
                   "provider": provider, "modelId": model_id})
        )?;
    }
    Ok(file)
}

/// pi's configured default model (`provider/id`) from its settings.json.
pub fn pi_default_model() -> Option<String> {
    let dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::home().join(".pi").join("agent"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).ok()?).ok()?;
    Some(format!(
        "{}/{}",
        v.get("defaultProvider")?.as_str()?,
        v.get("defaultModel")?.as_str()?
    ))
}

/// Convert a transcript into a pi session. Returns (file, turn count).
pub fn fork_into_pi(
    backend: Backend,
    transcript: &Path,
    session_dir: &Path,
    cwd: &str,
    model: Option<&str>,
) -> std::io::Result<(PathBuf, usize)> {
    let entries = read_jsonl(transcript)?;
    let (turns, source) = match backend {
        Backend::Claude => (claude_turns(&entries), "claude-code"),
        Backend::Codex => (codex_turns(&entries), "codex"),
        Backend::Pi => return Err(std::io::Error::other("already a pi session")),
    };
    let mut turns = fit_budget(turns, HISTORY_MAX_CHARS);
    if turns.is_empty() {
        return Err(std::io::Error::other("transcript has no messages"));
    }
    // A conversation that ends on the user's turn would make pi's first
    // request answer it immediately; end on an assistant turn instead.
    if turns.last().is_some_and(|t| t.role == Role::User) {
        push(&mut turns, Role::Assistant, "[Imported into pi. Continuing from here.]".to_string());
    }
    let model = model.map(str::to_string).or_else(pi_default_model);
    let file = write_pi_session(&turns, session_dir, cwd, source, model.as_deref())?;
    Ok((file, turns.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(t: &[Turn]) -> Vec<Role> {
        t.iter().map(|t| t.role).collect()
    }

    #[test]
    fn claude_flattens_tools_and_skips_noise() {
        let entries = vec![
            json!({"type":"user","message":{"content":"fix the bug"}}),
            json!({"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"hmm","signature":"s"},
                {"type":"text","text":"Looking."},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}),
            json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"t1","content":"a.rs\n<system-reminder>r</system-reminder>"}]}}),
            json!({"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}]}}),
            json!({"type":"attachment"}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Fixed."}]}}),
        ];
        let t = claude_turns(&entries);
        assert_eq!(roles(&t), vec![Role::User, Role::Assistant, Role::User, Role::Assistant]);
        assert_eq!(t[1].text, "Looking.\n\n[called Bash] {\"command\":\"ls\"}");
        assert_eq!(t[2].text, "[Bash result]\na.rs");
        assert!(!t.iter().any(|x| x.text.contains("hmm") || x.text.contains("sub")));
    }

    #[test]
    fn claude_starts_at_last_compaction() {
        let entries = vec![
            json!({"type":"user","message":{"content":"old"}}),
            json!({"type":"user","isCompactSummary":true,"isMeta":true,"message":{"content":"SUMMARY"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}),
        ];
        let t = claude_turns(&entries);
        assert_eq!(t[0].text, "SUMMARY");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn codex_turns_and_compaction() {
        let ri = |p: Value| json!({"type":"response_item","payload":p});
        let entries = vec![
            ri(json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"dev"}]})),
            ri(json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context/>"}]})),
            ri(json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]})),
            ri(json!({"type":"function_call","name":"shell","call_id":"c1","arguments":"{\"cmd\":\"ls\"}"})),
            ri(json!({"type":"function_call_output","call_id":"c1","output":[{"type":"input_text","text":"out"}]})),
            ri(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]})),
        ];
        let t = codex_turns(&entries);
        assert_eq!(t[0].text, "hi");
        assert_eq!(t[1].text, "[called shell] {\"cmd\":\"ls\"}");
        assert_eq!(t[2].text, "[shell result]\nout");
        assert_eq!(t[3].text, "done");

        let mut with_compact = entries.clone();
        with_compact.push(json!({"type":"compacted","payload":{"replacement_history":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"kept"}]},
            {"type":"compaction","encrypted_content":"x"}]}}));
        let t = codex_turns(&with_compact);
        assert_eq!(t.len(), 1);
        assert!(t[0].text.contains("compacted") && t[0].text.ends_with("kept"));
    }

    #[test]
    fn budget_keeps_first_and_latest() {
        let mut turns = vec![Turn { role: Role::User, text: "first".into() }];
        for i in 0..10 {
            let role = if i % 2 == 0 { Role::Assistant } else { Role::User };
            turns.push(Turn { role, text: format!("{i}{}", "x".repeat(99)) });
        }
        let out = fit_budget(turns, 350);
        assert!(out[0].text.starts_with("first") && out[0].text.contains("omitted"));
        assert_eq!(out[1].role, Role::Assistant);
        assert!(out.last().unwrap().text.starts_with('9'));
        assert!(out.len() < 11);
    }

    #[test]
    fn writes_pi_session_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({"type":"user","message":{"content":"q"}}),
                json!({"type":"assistant","message":{"content":[{"type":"text","text":"a"}]}})
            ),
        )
        .unwrap();
        let dir = tmp.path().join("pi");
        let (file, n) = fork_into_pi(Backend::Claude, &transcript, &dir, "/w", Some("openrouter/qwen/qwen3")).unwrap();
        assert_eq!(n, 2);
        let lines: Vec<Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["version"], 3);
        assert_eq!(lines[0]["cwd"], "/w");
        assert!(lines[1]["parentId"].is_null());
        assert_eq!(lines[2]["parentId"], lines[1]["id"]);
        assert_eq!(lines[2]["message"]["role"], "assistant");
        assert_eq!(lines[2]["message"]["stopReason"], "stop");
        // Model ids may contain '/': only the first splits off the provider.
        assert_eq!(lines[3]["type"], "model_change");
        assert_eq!(lines[3]["provider"], "openrouter");
        assert_eq!(lines[3]["modelId"], "qwen/qwen3");
        assert_eq!(lines[3]["parentId"], lines[2]["id"]);
    }

    #[test]
    fn ends_on_assistant_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(&transcript, format!("{}\n", json!({"type":"user","message":{"content":"q"}}))).unwrap();
        let (file, n) = fork_into_pi(Backend::Claude, &transcript, &tmp.path().join("pi"), "/w", None).unwrap();
        assert_eq!(n, 2);
        assert!(std::fs::read_to_string(file).unwrap().contains("Imported into pi"));
    }
}
