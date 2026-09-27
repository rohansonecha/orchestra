// config.rs — Dispatch defaults set from the TUI (/backend, /model),
// persisted to ~/.orchestra/config.json.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::session::Backend;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub default_backend: Backend,
    /// Default model per backend (keyed by `pi` / `claude` / `codex`). A
    /// missing entry means the backend's own default (for pi, that is
    /// ORCHESTRA_PROVIDER / ORCHESTRA_MODEL or pi's settings).
    #[serde(default)]
    pub models: BTreeMap<String, String>,
}

impl Config {
    pub fn load() -> Self {
        std::fs::read_to_string(paths::config_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(paths::state_dir());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(paths::config_file(), s);
        }
    }

    pub fn model_for(&self, b: Backend) -> Option<String> {
        self.models.get(b.as_str()).cloned()
    }

    pub fn set_model(&mut self, b: Backend, model: Option<String>) {
        match model {
            Some(m) => self.models.insert(b.as_str().to_string(), m),
            None => self.models.remove(b.as_str()),
        };
    }
}

/// Check a pi model pattern against `pi --list-models` so a typo fails at
/// `/model` time instead of when a session starts. Returns the matching
/// `provider/id` lines (empty = unknown), or Err if pi couldn't be run.
pub fn pi_models_matching(pattern: &str) -> Result<Vec<String>, String> {
    let out = std::process::Command::new("pi")
        .args(["--offline", "--list-models", pattern])
        .output()
        .map_err(|e| format!("pi not runnable: {e}"))?;
    // Without a TTY on stdin pi runs in print mode, which redirects its
    // stdout to stderr — read both.
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok(parse_list_models(&text))
}

/// `pi --list-models` prints a table: provider, model, ... columns.
fn parse_list_models(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| !l.trim_start().starts_with("provider"))
        .skip(1)
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            Some(format!("{}/{}", cols.next()?, cols.next()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.default_backend, Backend::Pi);
        let mut c = c;
        c.default_backend = Backend::Codex;
        c.set_model(Backend::Pi, Some("openrouter/x".into()));
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"codex\""));
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.model_for(Backend::Pi).as_deref(), Some("openrouter/x"));
        let mut back = back;
        back.set_model(Backend::Pi, None);
        assert!(back.model_for(Backend::Pi).is_none());
    }

    #[test]
    fn parses_list_models_table() {
        let t = "provider    model              context  max-out\n\
                 anthropic   claude-opus-5-5    1M       128K\n\
                 orchestra   glm-5              256K     8K\n";
        assert_eq!(parse_list_models(t), vec!["anthropic/claude-opus-5-5", "orchestra/glm-5"]);
    }
}
