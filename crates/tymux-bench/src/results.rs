//! The JSON that `bench/` (Go) writes. Scenarios a backend can't run are absent.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Results {
    pub env: Env,
    pub backends: Vec<Backend>,
}

#[derive(Debug, Deserialize)]
pub struct Env {
    pub tymux_git_sha: String,
    pub tmux_version: String,
    pub kernel: String,
    pub cpu: String,
    pub cores: u32,
    pub sessions: u32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Stats {
    pub n: u32,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

#[derive(Debug, Deserialize)]
pub struct Bucket {
    pub live_from: u32,
    pub live_to: u32,
    #[serde(flatten)]
    pub stats: Stats,
}

#[derive(Debug, Deserialize)]
pub struct Reattach {
    #[serde(flatten)]
    pub stats: Stats,
    pub gap_free_resumes: u32,
}

#[derive(Debug, Deserialize)]
pub struct Backend {
    pub name: String,
    pub create: Vec<Bucket>,
    pub kill: Stats,
    #[serde(rename = "list_at_max_sessions")]
    pub list: Stats,
    #[serde(rename = "capture_at_max_sessions")]
    pub capture: Option<Stats>,
    #[serde(rename = "keystroke_echo")]
    pub echo: Option<Stats>,
    pub cold_start: Option<Stats>,
    pub reattach: Option<Reattach>,
    #[serde(rename = "server_rss_kib_per_session")]
    pub rss_per_session_kib: Option<f64>,
}

impl Results {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))
    }

    pub fn backend(&self, name: &str) -> Option<&Backend> {
        self.backends.iter().find(|b| b.name == name)
    }
}
