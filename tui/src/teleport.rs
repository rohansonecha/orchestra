// teleport.rs — Move a session to a SkyPilot box, on request. Sessions are
// local by default; /teleport launches a cluster for one session, syncs its
// worktree (including uncommitted work) and its agent's own transcript and
// config, and resumes the same conversation there in tmux. The local tmux
// session becomes an ssh view of the remote one, so the list, Left and
// scrolling keep working. /teleport back brings the transcript and files
// home and resumes locally.

use std::path::{Path, PathBuf};

use crate::paths;
use crate::session::{sq, Backend, Session};
use crate::switch;

/// Where a teleported session runs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Remote {
    pub cluster: String,
    pub infra: String,
    /// Transcript path on the box (relative to its home), for /teleport back.
    pub transcript: String,
}

/// What /teleport is about to do, shown before anything is launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub cluster: String,
    pub infra: String,
    pub endpoint: String,
    pub yaml: PathBuf,
    pub launch: String,
    pub remote: Remote,
    /// Human-readable list of what gets sent.
    pub sends: Vec<String>,
    pub warnings: Vec<String>,
}

/// SkyPilot API server from ~/.sky/config.yaml (read locally; no network).
pub fn api_endpoint() -> Option<String> {
    let text = std::fs::read_to_string(paths::home().join(".sky").join("config.yaml")).ok()?;
    let mut in_api = false;
    for line in text.lines() {
        if !line.starts_with(' ') {
            in_api = line.trim_end() == "api_server:";
            continue;
        }
        if in_api {
            if let Some(v) = line.trim().strip_prefix("endpoint:") {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Infra: the argument, else SKY_INFRA from the environment or
/// ~/.orchestra/env.
fn infra(arg: &str) -> Option<String> {
    let arg = arg.trim();
    if !arg.is_empty() {
        return Some(arg.to_string());
    }
    std::env::var("SKY_INFRA").ok().filter(|v| !v.is_empty()).or_else(|| {
        let env = std::fs::read_to_string(paths::env_file()).ok()?;
        env.lines().rev().find_map(|l| {
            let l = l.trim().strip_prefix("export ").unwrap_or(l.trim());
            l.strip_prefix("SKY_INFRA=").map(|v| v.trim_matches('"').to_string()).filter(|v| !v.is_empty())
        })
    })
}

pub fn cluster_name(session: &str) -> String {
    let base: String = format!("orch-{session}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    base.chars().take(40).collect::<String>().trim_end_matches('-').to_string()
}

/// Secrets the box needs for this agent, by name. Values are passed from
/// the local environment with `sky launch --secret NAME`, never written to
/// the YAML.
fn secret_names(backend: Backend) -> Vec<&'static str> {
    let wanted: &[&str] = match backend {
        Backend::Pi => &["SKYPILOT_TOKENS_API_KEY", "ORCHESTRA_API_KEY", "OPENROUTER_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
        Backend::Claude => &["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        Backend::Codex => &["OPENAI_API_KEY"],
    };
    let mut names: Vec<&str> = wanted.iter().copied().filter(|n| std::env::var(n).is_ok_and(|v| !v.is_empty())).collect();
    if std::env::var("GITHUB_TOKEN").is_ok_and(|v| !v.is_empty()) {
        names.push("GITHUB_TOKEN");
    }
    names
}

fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// Build the plan and write the task YAML. Nothing is launched.
pub fn plan(sess: &Session, infra_arg: &str) -> Result<Plan, String> {
    let infra = infra(infra_arg).ok_or("say where: /teleport <infra> (e.g. k8s/my-context), or set SKY_INFRA in ~/.orchestra/env")?;
    let endpoint = api_endpoint().unwrap_or_else(|| "local SkyPilot (no API server configured)".into());
    let transcript = switch::native_transcript(sess).ok_or("this session has no conversation to move yet")?;
    let cluster = cluster_name(&sess.name);
    let dir = paths::state_dir().join("teleport").join(&sess.name);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let yaml = dir.join("task.yaml");
    let file_name = transcript.file_name().and_then(|f| f.to_str()).ok_or("bad transcript path")?.to_string();
    let home = paths::home();
    let mut warnings = Vec::new();
    let mut mounts: Vec<(String, PathBuf)> = vec![(format!("~/.orchestra-teleport/{file_name}"), transcript.clone())];
    let instructions = home.join(".claude").join("CLAUDE.md");
    let (install, place, start, remote_transcript) = match sess.backend {
        Backend::Pi => {
            for f in ["models.json", "settings.json", "themes"] {
                let p = home.join(".pi").join("agent").join(f);
                if p.exists() {
                    mounts.push((format!("~/.pi/agent/{f}"), p));
                }
            }
            if instructions.exists() {
                mounts.push(("~/.pi/agent/AGENTS.md".into(), instructions.clone()));
            }
            let model = sess.model.as_ref().map(|m| format!(" --model {}", sq(m))).unwrap_or_default();
            (
                "sudo npm install -g --ignore-scripts @earendil-works/pi-coding-agent".to_string(),
                format!("mkdir -p ~/.orchestra/pi-sessions/{id} && cp ~/.orchestra-teleport/{file_name} ~/.orchestra/pi-sessions/{id}/", id = sess.id),
                format!("pi --session-dir ~/.orchestra/pi-sessions/{} --continue{model}", sess.id),
                format!(".orchestra/pi-sessions/{}/{file_name}", sess.id),
            )
        }
        Backend::Claude => {
            if instructions.exists() {
                mounts.push(("~/.claude/CLAUDE.md".into(), instructions.clone()));
            }
            let id = file_name.trim_end_matches(".jsonl").to_string();
            if !secret_names(Backend::Claude).iter().any(|n| n.starts_with("CLAUDE") || n.starts_with("ANTHROPIC")) {
                warnings.push("No CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY here: Claude Code on the box will ask you to log in (run `claude setup-token` locally to avoid that).".into());
            }
            (
                "curl -fsSL https://claude.ai/install.sh | bash && export PATH=$HOME/.local/bin:$PATH".to_string(),
                format!("proj=~/.claude/projects/$(cd ~/sky_workdir && pwd | sed 's/[^A-Za-z0-9]/-/g'); mkdir -p $proj && cp ~/.orchestra-teleport/{file_name} $proj/"),
                format!("claude --resume {}", sq(&id)),
                format!(".claude/projects/<box workdir>/{file_name}"),
            )
        }
        Backend::Codex => {
            for f in ["auth.json", "config.toml"] {
                let p = home.join(".codex").join(f);
                if p.exists() {
                    mounts.push((format!("~/.codex/{f}"), p));
                }
            }
            if instructions.exists() {
                mounts.push(("~/.codex/AGENTS.md".into(), instructions.clone()));
            }
            let id = sess.external_id.clone().ok_or("this Codex session's thread id is not known yet")?;
            let rel = transcript
                .strip_prefix(paths::codex_dir())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| format!("sessions/{file_name}"));
            (
                "sudo npm install -g @openai/codex".to_string(),
                format!("mkdir -p ~/.codex/$(dirname {rel}) && cp ~/.orchestra-teleport/{file_name} ~/.codex/{rel}"),
                format!("codex resume {}", sq(&id)),
                format!(".codex/{rel}"),
            )
        }
    };
    let secrets = secret_names(sess.backend);
    let mut y = String::new();
    y.push_str(&format!("# Generated by orchestra /teleport for session {}.\nname: {cluster}\n\n", sess.name));
    y.push_str(&format!("resources:\n  infra: {}\n  cpus: 4+\n\n", yaml_str(&infra)));
    if Path::new(&sess.worktree_path).is_dir() {
        y.push_str(&format!("workdir: {}\n\n", yaml_str(&sess.worktree_path)));
    }
    y.push_str("file_mounts:\n");
    for (dst, src) in &mounts {
        y.push_str(&format!("  {dst}: {}\n", yaml_str(&src.to_string_lossy())));
    }
    if !secrets.is_empty() {
        y.push_str("\nsecrets:\n");
        for s in &secrets {
            y.push_str(&format!("  {s}: null\n"));
        }
    }
    let session = sq(&sess.name);
    y.push_str(&format!(
        "\nsetup: |\n  set -e\n  if ! command -v node >/dev/null; then curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - && sudo apt-get install -y nodejs; fi\n  command -v tmux >/dev/null || sudo apt-get install -y tmux\n  {install}\n\nrun: |\n  export PATH=$HOME/.local/bin:$PATH COLORTERM=truecolor\n  {place}\n  cd ~/sky_workdir 2>/dev/null || cd ~\n  tmux has-session -t {session} 2>/dev/null || tmux new-session -d -s {session} -x 200 -y 50 \"bash -lc '{start}; exec bash'\"\n  tmux set-option -t {session} status off\n  tmux set-option -t {session} mouse on\n  echo 'orchestra session {name} is running in tmux on this box.'\n  sleep infinity\n",
        start = start.replace('\'', "'\\''"),
        name = sess.name,
    ));
    std::fs::write(&yaml, y).map_err(|e| e.to_string())?;
    let secret_flags: String = secrets.iter().map(|s| format!(" --secret {s}")).collect();
    let launch = format!(
        "sky launch -y -d -c {cluster} {}{secret_flags}",
        sq(&yaml.to_string_lossy())
    );
    let mut sends = vec![format!("worktree {} (synced as ~/sky_workdir, uncommitted work included)", sess.worktree_path)];
    sends.extend(mounts.iter().map(|(d, s)| format!("{} → {d}", s.display())));
    if !secrets.is_empty() {
        sends.push(format!("secrets by name, from this environment: {}", secrets.join(", ")));
    }
    Ok(Plan {
        cluster: cluster.clone(),
        infra: infra.clone(),
        endpoint,
        yaml,
        launch,
        remote: Remote { cluster, infra, transcript: remote_transcript },
        sends,
        warnings,
    })
}

/// What the local tmux pane runs for a teleported session: launch once
/// (first time), then keep an ssh view of the remote tmux session open.
pub fn pane_command(sess: &Session, remote: &Remote, launch: Option<&str>) -> String {
    // `sky launch -d` returns before the run step has started the agent's
    // tmux session, so wait for it on the box before attaching.
    let name = sq(&sess.name);
    let remote_cmd = format!(
        "until tmux has-session -t {name} 2>/dev/null; do echo 'waiting for the agent to start on the box...'; sleep 3; done; tmux attach -t {name}"
    );
    let attach = format!("ssh -t {} {}", remote.cluster, sq(&remote_cmd));
    let launch = launch.map(|l| format!("{l} && ")).unwrap_or_default();
    format!("{launch}{attach}")
}

/// Commands that bring the session home: the transcript and changed files
/// (not .git) back into the local worktree.
pub fn back_commands(sess: &Session, remote: &Remote, local_transcript: &Path) -> Vec<String> {
    let mut cmds = vec![format!(
        "rsync -a --exclude .git {}:sky_workdir/ {}/",
        remote.cluster,
        sq(&sess.worktree_path)
    )];
    if !remote.transcript.contains('<') {
        cmds.push(format!("scp {}:{} {}", remote.cluster, sq(&remote.transcript), sq(&local_transcript.to_string_lossy())));
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_names_are_valid() {
        assert_eq!(cluster_name("Fix_the Bug-12"), "orch-fix-the-bug-12");
        assert!(cluster_name(&"x".repeat(80)).len() <= 40);
    }

    #[test]
    fn plan_writes_yaml_without_secret_values() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let mut sess = Session::new("tp-test".into(), "p".into(), wt.to_string_lossy().to_string(), Backend::Pi);
        sess.id = format!("teleport-test-{}", uuid::Uuid::new_v4());
        let dir = sess.pi_session_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.jsonl"), "{}\n").unwrap();
        std::env::set_var("ORCHESTRA_API_KEY", "sk-secret-value");
        let p = plan(&sess, "k8s/test-ctx").unwrap();
        let y = std::fs::read_to_string(&p.yaml).unwrap();
        assert!(y.contains("infra: \"k8s/test-ctx\""));
        assert!(y.contains("~/.orchestra-teleport/s.jsonl"));
        assert!(y.contains("ORCHESTRA_API_KEY: null"));
        assert!(!y.contains("sk-secret-value"), "secret values never in the YAML");
        assert!(y.contains("pi --session-dir ~/.orchestra/pi-sessions/"));
        assert!(p.launch.contains("--secret ORCHESTRA_API_KEY"));
        assert!(p.launch.starts_with("sky launch -y -d -c orch-tp-test "));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(p.yaml.parent().unwrap());
    }

    #[test]
    fn pane_waits_for_remote_session() {
        let sess = Session::new("s1".into(), "p".into(), "/w".into(), Backend::Pi);
        let r = Remote { cluster: "orch-s1".into(), infra: "k8s/x".into(), transcript: "t".into() };
        let c = pane_command(&sess, &r, Some("sky launch -y -d -c orch-s1 t.yaml"));
        assert!(c.starts_with("sky launch -y -d -c orch-s1 t.yaml && ssh -t orch-s1 "), "{c}");
        assert!(c.contains("until tmux has-session"));
    }

    #[test]
    fn needs_infra() {
        let tmp = tempfile::tempdir().unwrap();
        let sess = Session::new("n".into(), "p".into(), tmp.path().to_string_lossy().to_string(), Backend::Pi);
        std::env::remove_var("SKY_INFRA");
        if std::fs::read_to_string(paths::env_file()).map(|e| e.contains("SKY_INFRA=")).unwrap_or(false) {
            return; // configured on this machine
        }
        assert!(plan(&sess, "").unwrap_err().contains("/teleport <infra>"));
    }
}
