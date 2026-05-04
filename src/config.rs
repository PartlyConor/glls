use serde::Deserialize;
use std::path::PathBuf;

/// Top-level configuration for the LSP server.
#[derive(Debug, Clone)]
pub struct Config {
    pub gitlab_host: String,
    pub gitlab_token: String,
    pub mr_iid: Option<u64>,
    pub poll_interval_secs: u64,
}

/// Schema for `.helix/mr-lsp.toml`.
#[derive(Debug, Deserialize, Default)]
struct TomlConfig {
    gitlab_host: Option<String>,
    gitlab_token: Option<String>,
    mr_iid: Option<u64>,
    poll_interval_secs: Option<u64>,
}

impl Config {
    /// Load configuration using the priority chain:
    /// 1. `.helix/mr-lsp.toml` in the workspace root
    /// 2. Environment variables (`GITLAB_TOKEN`, `GITLAB_HOST`)
    /// 3. Host derived from the git remote origin URL
    /// 4. `glab auth status --show-token -h <host>` (shell out)
    pub async fn load(workspace_root: Option<&PathBuf>) -> anyhow::Result<Self> {
        let toml_cfg = load_toml_config(workspace_root);

        // --- host ---
        // Priority: toml → GITLAB_HOST env → git remote → gitlab.com fallback.
        // We resolve the remote-derived host before querying glab so we use the
        // right instance automatically.
        let gitlab_host = if let Some(h) = toml_cfg.gitlab_host.clone() {
            h
        } else if let Ok(h) = std::env::var("GITLAB_HOST") {
            h
        } else {
            crate::git::host_from_remote()
                .await
                .unwrap_or_else(|_| "https://gitlab.com".to_owned())
        };

        // --- token ---
        let gitlab_token = if let Some(t) = toml_cfg.gitlab_token.clone() {
            t
        } else if let Ok(t) = std::env::var("GITLAB_TOKEN") {
            t
        } else {
            // Fallback: ask glab, using the host we just resolved.
            match token_from_glab(&gitlab_host).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("Could not retrieve token from glab: {e}");
                    String::new()
                }
            }
        };

        Ok(Config {
            gitlab_host,
            gitlab_token,
            mr_iid: toml_cfg.mr_iid,
            poll_interval_secs: toml_cfg.poll_interval_secs.unwrap_or(60),
        })
    }
}

fn load_toml_config(workspace_root: Option<&PathBuf>) -> TomlConfig {
    let root = match workspace_root {
        Some(r) => r.clone(),
        None => return TomlConfig::default(),
    };
    let path = root.join(".helix").join("mr-lsp.toml");
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return TomlConfig::default(),
    };
    match toml::from_str(&contents) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("Failed to parse {}: {e}", path.display());
            TomlConfig::default()
        }
    }
}

async fn token_from_glab(host: &str) -> anyhow::Result<String> {
    // glab --hostname wants a bare hostname, not a URL scheme.
    let hostname = host
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');

    let output = tokio::process::Command::new("glab")
        .args(["auth", "status", "--show-token", "--hostname", hostname])
        .output()
        .await?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Strip ANSI escape sequences before parsing — glab may emit colour codes
    // depending on how it detects the terminal.
    let stderr_clean = strip_ansi(stderr.as_ref());
    let stdout_clean = strip_ansi(stdout.as_ref());

    // glab prints a line like: "  ✓ Token found: glpat-xxxx"
    for line in stderr_clean.lines().chain(stdout_clean.lines()) {
        if let Some(rest) = line.split("Token found:").nth(1) {
            let token = rest.trim().to_owned();
            if !token.is_empty() {
                return Ok(token);
            }
        }
    }
    anyhow::bail!("glab auth status did not produce a token")
}

/// Remove ANSI escape sequences (e.g. `\x1b[32m`) from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            // consume until we hit a letter (the final byte of the escape sequence)
            chars.next(); // consume '['
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}
