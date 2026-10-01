// coverage.rs — `orchestra roi`: how many tool calls a switch
// between agents carries over as real tool calls (mapped to the target's
// own shell/read/write/edit tools) versus text describing the call.
//
// Uses the same mapping functions as history.rs, so the numbers are what a
// switch actually does. Only lines that contain a tool call are parsed,
// which keeps it fast on large transcripts.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::history::{from_claude_tool, from_codex_call, from_pi_tool, Tool};
use crate::paths;
use crate::session::{Backend, Session};

#[derive(Default, Clone, Copy)]
struct Count {
    calls: u64,
    mapped: u64,
}

impl Count {
    fn add(&mut self, mapped: bool) {
        self.calls += 1;
        self.mapped += mapped as u64;
    }
    fn pct(&self) -> String {
        if self.calls == 0 { "-".into() } else { format!("{:.1}%", 100.0 * self.mapped as f64 / self.calls as f64) }
    }
}

/// One tool call found in a transcript.
struct Call {
    tool: Tool,
    /// Original tool name, for the "kept as text" list.
    name: String,
}

fn calls_in(backend: Backend, path: &Path) -> Vec<Call> {
    calls_from(backend, path, 0)
}

/// Calls after the first `skip` lines (a segment's own turns start after
/// the history copied in from earlier segments; for pi the seed counts
/// messages, which is at most one line each, plus the header).
fn calls_from(backend: Backend, path: &Path, skip: usize) -> Vec<Call> {
    let Ok(f) = std::fs::File::open(path) else { return Vec::new() };
    let needle = match backend {
        Backend::Claude => "\"tool_use\"",
        Backend::Codex => "_call\"",
        Backend::Pi => "\"toolCall\"",
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok).skip(skip) {
        if !line.contains(needle) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match backend {
            Backend::Claude => {
                if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                for b in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        let tool = from_claude_tool(&name, b.get("input").unwrap_or(&Value::Null));
                        out.push(Call { tool, name });
                    }
                }
            }
            Backend::Codex => {
                let Some(p) = v.get("payload") else { continue };
                let raw = match p.get("type").and_then(Value::as_str) {
                    Some("function_call") => p.get("arguments"),
                    Some("custom_tool_call") => p.get("input"),
                    _ => continue,
                };
                let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let tool = from_codex_call(&name, raw.and_then(Value::as_str).unwrap_or(""));
                out.push(Call { tool, name });
            }
            Backend::Pi => {
                for b in v.pointer("/message/content").and_then(Value::as_array).into_iter().flatten() {
                    if b.get("type").and_then(Value::as_str) == Some("toolCall") {
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        let tool = from_pi_tool(&name, b.get("arguments").unwrap_or(&Value::Null));
                        out.push(Call { tool, name });
                    }
                }
            }
        }
    }
    out
}

fn mapped(t: &Tool) -> bool {
    !matches!(t, Tool::Other { .. })
}

fn jsonl_files(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() && depth > 0 {
            jsonl_files(&p, depth - 1, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// Monday of the week a file was last written, as YYYY-MM-DD.
fn week_of(p: &Path) -> String {
    let t = std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let dt: chrono::DateTime<chrono::Utc> = t.map(Into::into).unwrap_or_else(chrono::Utc::now);
    use chrono::Datelike;
    let monday = dt.date_naive() - chrono::Duration::days(dt.weekday().num_days_from_monday() as i64);
    monday.format("%Y-%m-%d").to_string()
}

fn label(b: Backend) -> &'static str {
    match b {
        Backend::Claude => "Claude Code",
        Backend::Codex => "Codex",
        Backend::Pi => "pi",
    }
}

pub fn run() {
    // Every transcript, by agent.
    let mut sources: Vec<(Backend, PathBuf)> = Vec::new();
    for d in std::fs::read_dir(paths::claude_projects_dir()).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
            if f.path().extension().is_some_and(|x| x == "jsonl") {
                sources.push((Backend::Claude, f.path()));
            }
        }
    }
    let mut v = Vec::new();
    jsonl_files(&paths::codex_dir().join("sessions"), 4, &mut v);
    sources.extend(v.drain(..).map(|p| (Backend::Codex, p)));
    jsonl_files(&paths::pi_sessions_dir(), 1, &mut v);
    jsonl_files(&paths::home().join(".pi").join("agent").join("sessions"), 1, &mut v);
    sources.extend(v.drain(..).map(|p| (Backend::Pi, p)));
    eprintln!("reading {} transcripts…", sources.len());

    let mut by_week: BTreeMap<String, HashMap<&'static str, Count>> = BTreeMap::new();
    let mut by_agent: BTreeMap<&'static str, Count> = BTreeMap::new();
    let mut unmapped: HashMap<String, u64> = HashMap::new();
    for (b, p) in &sources {
        let calls = calls_in(*b, p);
        let week = week_of(p);
        for c in &calls {
            let m = mapped(&c.tool);
            by_week.entry(week.clone()).or_default().entry(label(*b)).or_default().add(m);
            by_agent.entry(label(*b)).or_default().add(m);
            if !m {
                *unmapped.entry(format!("{} {}", label(*b), c.name)).or_default() += 1;
            }
        }
    }

    println!("\nTool calls a switch would keep as real tool calls (vs. text), by week");
    println!("{:<12} {:>8} {:>8} {:>8}   by agent", "week of", "calls", "kept", "share");
    for (week, agents) in &by_week {
        let mut t = Count::default();
        for c in agents.values() {
            t.calls += c.calls;
            t.mapped += c.mapped;
        }
        let parts: Vec<String> = agents.iter().map(|(a, c)| format!("{a} {} of {}", c.pct(), c.calls)).collect();
        println!("{:<12} {:>8} {:>8} {:>8}   {}", week, t.calls, t.mapped, t.pct(), parts.join(" · "));
    }
    println!("\nBy source agent (all time)");
    for (a, c) in &by_agent {
        println!("  {:<12} {:>8} calls  {:>7} kept as tool calls", a, c.calls, c.pct());
    }

    // Actual orchestra switches: the calls each switch carried over, and
    // how many stayed real tool calls in the target (Codex: none).
    println!("\nOrchestra switches (calls carried into each new agent)");
    let mut any = false;
    let mut total = Count::default();
    for e in std::fs::read_dir(paths::sessions_dir()).into_iter().flatten().flatten() {
        let Ok(text) = std::fs::read_to_string(e.path().join("state.json")) else { continue };
        let Ok(sess) = serde_json::from_str::<Session>(&text) else { continue };
        if sess.segments.len() < 2 {
            continue;
        }
        any = true;
        let mut carried: Vec<bool> = Vec::new();
        for (i, seg) in sess.segments.iter().enumerate() {
            if i > 0 {
                let mut c = Count::default();
                for &m in &carried {
                    c.add(m && seg.backend != Backend::Codex);
                }
                total.calls += c.calls;
                total.mapped += c.mapped;
                println!("  {:<40} → {:<12} {:>6} calls carried, {:>7} as tool calls",
                    sess.display_title().chars().take(40).collect::<String>(), label(seg.backend), c.calls, c.pct());
            }
            // This segment's own calls, made in its agent after the copied-in
            // history, become part of what the next switch carries.
            let skip = if seg.backend == Backend::Pi && seg.seed > 0 { seg.seed + 1 } else { seg.seed };
            carried.extend(calls_from(seg.backend, Path::new(&seg.path), skip).iter().map(|c| mapped(&c.tool)));
        }
    }
    if any {
        println!("  {:<40}   {:<12} {:>6} calls carried, {:>7} as tool calls", "all switches", "", total.calls, total.pct());
    } else {
        println!("  (no orchestra session has switched agents yet)");
    }

    let mut top: Vec<(String, u64)> = unmapped.into_iter().collect();
    top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("\nMost common tool calls kept as text (candidates to map next)");
    for (name, n) in top.into_iter().take(12) {
        println!("  {:>7}  {name}", n);
    }
}
