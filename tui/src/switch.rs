// switch.rs — Move a running session to another agent or model, keeping
// its conversation.
//
//   Same agent, new model: restart the agent on its own transcript with
//   the new model (pi --continue --model, claude --resume --model,
//   codex resume -m). Nothing is converted; each agent already knows how
//   to replay its own history to a different model.
//
//   Different agent: read every segment of the conversation into the
//   neutral form (history.rs) and write it as the target agent's own
//   transcript. If the session already ran in the target agent earlier,
//   that agent's original transcript is copied and only what happened
//   since is appended — so switching Claude → pi → Claude gives Claude its
//   own untouched history back, plus the pi turns translated.

use std::path::{Path, PathBuf};

use crate::history::{self, Msg};
use crate::paths;
use crate::session::{Backend, Origin, Segment, Session};

/// What the user asked to switch to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub backend: Backend,
    pub model: Option<String>,
}

impl Target {
    /// `pi`, `pi:openrouter/x`, `claude`, `claude:opus`, `codex:gpt-6`.
    pub fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        let (b, m) = match spec.split_once(':') {
            Some((b, m)) => (b, Some(m.trim().to_string()).filter(|m| !m.is_empty())),
            None => (spec, None),
        };
        Some(Self { backend: Backend::parse(b)?, model: m })
    }

    pub fn label(&self) -> String {
        let name = match self.backend {
            Backend::Pi => "pi",
            Backend::Claude => "Claude Code",
            Backend::Codex => "Codex",
        };
        match &self.model {
            Some(m) => format!("{name} ({m})"),
            None => name.to_string(),
        }
    }
}

fn newest(files: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    files
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

fn jsonl_in(dir: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
}

/// The current agent's own transcript for this session, if it has one yet.
pub fn native_transcript(sess: &Session) -> Option<PathBuf> {
    match sess.backend {
        Backend::Pi => newest(jsonl_in(&sess.pi_session_dir())),
        Backend::Claude => {
            let id = sess.external_id.clone().unwrap_or_else(|| sess.id.clone());
            let direct = history::claude_project_dir(&sess.worktree_path).join(format!("{id}.jsonl"));
            if direct.exists() {
                return Some(direct);
            }
            std::fs::read_dir(paths::claude_projects_dir())
                .ok()?
                .flatten()
                .map(|d| d.path().join(format!("{id}.jsonl")))
                .find(|p| p.exists())
        }
        Backend::Codex => {
            let mut files = Vec::new();
            codex_rollouts(&paths::codex_dir().join("sessions"), 4, &mut files);
            match &sess.external_id {
                Some(id) => files.into_iter().find(|p| p.to_string_lossy().contains(id.as_str())),
                // Dispatched from orchestra: Codex picked the id. The
                // worktree is unique to the session, so the newest rollout
                // started in it is ours.
                None => newest(files.into_iter().filter(|p| {
                    codex_cwd(p).as_deref() == Some(sess.worktree_path.as_str())
                })),
            }
        }
    }
}

fn codex_rollouts(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() && depth > 0 {
            codex_rollouts(&p, depth - 1, out);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

fn codex_id(p: &Path) -> Option<String> {
    codex_meta(p, "/payload/id")
}

fn codex_cwd(p: &Path) -> Option<String> {
    codex_meta(p, "/payload/cwd")
}

fn codex_meta(p: &Path, ptr: &str) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(p).ok()?;
    let line = std::io::BufReader::new(f).lines().next()?.ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    v.pointer(ptr)?.as_str().map(str::to_string)
}

/// Rough character budget for a model's context: pi's model table gives
/// the window; leave room for the system prompt, tools and the reply.
fn budget_chars(t: &Target) -> usize {
    let tokens = match t.backend {
        Backend::Pi => t
            .model
            .as_deref()
            .and_then(pi_context_window)
            .unwrap_or(128_000),
        Backend::Claude => 200_000,
        Backend::Codex => 200_000,
    };
    tokens * 3 / 2 // ≈ 0.5 of the window at ~3 chars/token
}

/// Context window (tokens) of a pi model, from `pi --list-models`.
fn pi_context_window(model: &str) -> Option<usize> {
    let out = std::process::Command::new("pi")
        .args(["--offline", "--list-models", model])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let (provider, id) = model.split_once('/')?;
    text.lines().find_map(|l| {
        let cols: Vec<&str> = l.split_whitespace().collect();
        (cols.len() > 2 && cols[0] == provider && cols[1] == id).then(|| parse_tokens(cols[2])).flatten()
    })
}

/// "131.1K" / "1M" / "32768" → tokens.
fn parse_tokens(s: &str) -> Option<usize> {
    let (num, mul) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1_000.0),
        'M' | 'm' => (&s[..s.len() - 1], 1_000_000.0),
        _ => (s, 1.0),
    };
    num.parse::<f64>().ok().map(|n| (n * mul) as usize)
}

/// The whole conversation so far, in order, from every segment.
fn gather(segments: &[Segment]) -> std::io::Result<Vec<Msg>> {
    let mut all = Vec::new();
    for seg in segments {
        let path = Path::new(&seg.path);
        if path.exists() {
            all.extend(history::read(seg.backend, path, seg.seed)?);
        }
    }
    Ok(all)
}

/// Switch `sess` (state only — the caller restarts its tmux session).
/// Returns a one-line summary for the status bar.
pub fn switch(sess: &mut Session, target: &Target) -> Result<String, String> {
    let from = Target { backend: sess.backend, model: sess.model.clone() }.label();

    // Same agent: just a model change on the agent's own transcript.
    if target.backend == sess.backend {
        if let Some(p) = native_transcript(sess) {
            // Resume the transcript instead of starting a new one.
            sess.origin = Origin::Resumed;
            if sess.external_id.is_none() {
                sess.external_id = match sess.backend {
                    Backend::Claude => Some(sess.id.clone()),
                    Backend::Codex => codex_id(&p),
                    Backend::Pi => None,
                };
            }
        }
        sess.model = target.model.clone();
        return Ok(format!("{from} → {}: same conversation, new model", target.label()));
    }

    let current = native_transcript(sess);
    // Record where we are before leaving.
    if sess.segments.is_empty() {
        if let Some(p) = &current {
            sess.segments.push(Segment {
                backend: sess.backend,
                model: sess.model.clone(),
                path: p.to_string_lossy().to_string(),
                seed: 0,
            });
        }
    } else if let (Some(last), Some(p)) = (sess.segments.last_mut(), &current) {
        // pi may have started a new file in its dir; follow it.
        if last.backend == sess.backend {
            last.path = p.to_string_lossy().to_string();
        }
    }

    let cwd = sess.worktree_path.clone();
    // Reuse the target agent's own earlier transcript if there is one.
    let reuse = sess
        .segments
        .iter()
        .rposition(|s| s.backend == target.backend && Path::new(&s.path).exists());
    let (base, later): (Option<Segment>, &[Segment]) = match reuse {
        Some(i) => (Some(sess.segments[i].clone()), &sess.segments[i + 1..]),
        None => (None, &sess.segments[..]),
    };
    let mut msgs = gather(later).map_err(|e| format!("reading history: {e}"))?;
    if base.is_none() {
        msgs = history::fit_budget(history::normalize(msgs), budget_chars(target));
    }
    msgs.extend(history::handoff_note(&from, &target.label()));
    let msgs = history::normalize(msgs);
    let n_msgs = msgs.len();

    let new_seg = match target.backend {
        Backend::Pi => {
            let dir = sess.pi_session_dir();
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let ts = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ");
            let file = dir.join(format!("{ts}_{}.jsonl", uuid::Uuid::new_v4()));
            let parent = match &base {
                Some(b) => {
                    std::fs::copy(&b.path, &file).map_err(|e| e.to_string())?;
                    history::pi_leaf(&file)
                }
                None => None,
            };
            history::append_pi(&file, &msgs, &cwd, parent, target.model.as_deref()).map_err(|e| e.to_string())?;
            let seed = history::pi_entry_count(&file);
            sess.origin = Origin::Resumed;
            sess.external_id = None;
            Segment { backend: Backend::Pi, model: target.model.clone(), path: file.to_string_lossy().to_string(), seed }
        }
        Backend::Claude => {
            let id = uuid::Uuid::new_v4().to_string();
            let file = history::claude_project_dir(&cwd).join(format!("{id}.jsonl"));
            let parent = match &base {
                Some(b) => {
                    history::fork_claude(Path::new(&b.path), &file, &id).map_err(|e| e.to_string())?;
                    history::claude_leaf(&file)
                }
                None => None,
            };
            history::append_claude(&file, &history::claude_ready(msgs), &cwd, &id, parent, target.model.as_deref())
                .map_err(|e| e.to_string())?;
            let seed = history::line_count(&file);
            sess.origin = Origin::Resumed;
            sess.external_id = Some(id);
            Segment { backend: Backend::Claude, model: target.model.clone(), path: file.to_string_lossy().to_string(), seed }
        }
        Backend::Codex => {
            // Codex rollouts are rewritten in full each time (their
            // metadata ties a file to one thread id).
            let msgs = if base.is_some() {
                let mut all = gather(&sess.segments).map_err(|e| e.to_string())?;
                all = history::fit_budget(history::normalize(all), budget_chars(target));
                all.extend(history::handoff_note(&from, &target.label()));
                history::normalize(all)
            } else {
                msgs
            };
            let (id, file) = history::write_codex(&msgs, &cwd).map_err(|e| e.to_string())?;
            let seed = history::line_count(&file);
            sess.origin = Origin::Resumed;
            sess.external_id = Some(id);
            Segment { backend: Backend::Codex, model: target.model.clone(), path: file.to_string_lossy().to_string(), seed }
        }
    };
    let how = if base.is_some() { "its own earlier transcript + new turns" } else { "converted history" };
    sess.segments.push(new_seg);
    sess.backend = target.backend;
    sess.model = target.model.clone();
    Ok(format!("{from} → {}: {n_msgs} messages ({how})", target.label()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        assert_eq!(Target::parse("claude"), Some(Target { backend: Backend::Claude, model: None }));
        assert_eq!(
            Target::parse("pi:openrouter/qwen/qwen3"),
            Some(Target { backend: Backend::Pi, model: Some("openrouter/qwen/qwen3".into()) })
        );
        assert_eq!(Target::parse("codex: gpt-6 ").unwrap().model.as_deref(), Some("gpt-6"));
        assert_eq!(Target::parse("vim"), None);
    }

    #[test]
    fn token_parsing() {
        assert_eq!(parse_tokens("131.1K"), Some(131_100));
        assert_eq!(parse_tokens("1M"), Some(1_000_000));
        assert_eq!(parse_tokens("32768"), Some(32_768));
        assert_eq!(parse_tokens("x"), None);
    }
}
