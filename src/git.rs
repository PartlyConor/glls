use anyhow::{Context, Result};
use std::path::PathBuf;

/// SHAs required by the GitLab diff position API.
#[derive(Debug, Clone)]
pub struct MrShas {
    pub base_sha: String,
    pub head_sha: String,
    pub start_sha: String,
}

/// Returns the local HEAD SHA via `git rev-parse HEAD`.
pub async fn local_head_sha() -> Result<String> {
    run("git", &["rev-parse", "HEAD"]).await
}

/// Walk up from CWD until a `.git` directory is found.
pub async fn repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("cannot determine CWD")?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".git").exists() {
            return Ok(dir.to_path_buf());
        }
        dir = dir.parent().context("reached filesystem root without finding .git")?;
    }
}

/// Returns the current branch name via `git branch --show-current`.
pub async fn current_branch() -> Result<String> {
    let out = run("git", &["branch", "--show-current"]).await?;
    if out.is_empty() {
        anyhow::bail!("git branch --show-current returned empty (detached HEAD?)");
    }
    Ok(out)
}

/// Returns the three SHAs needed for GitLab diff position API.
pub async fn mr_shas(base_branch: &str) -> Result<MrShas> {
    let origin_base = format!("origin/{base_branch}");
    let base_sha = run("git", &["merge-base", "HEAD", &origin_base]).await?;
    let head_sha = run("git", &["rev-parse", "HEAD"]).await?;
    let start_sha = run("git", &["rev-parse", &origin_base]).await?;
    Ok(MrShas { base_sha, head_sha, start_sha })
}

/// Derives `group/repo` from the `origin` remote URL.
///
/// Handles both SSH (`git@gitlab.com:group/repo.git`) and
/// HTTPS (`https://gitlab.com/group/repo.git`) remotes.
pub async fn project_path_from_remote() -> Result<String> {
    let url = run("git", &["remote", "get-url", "origin"]).await?;
    parse_project_path(&url)
}

/// Extracts the GitLab host (as an HTTPS URL) from the `origin` remote.
///
/// - `git@gitlab.example.com:group/repo.git`  → `https://gitlab.example.com`
/// - `https://gitlab.example.com/group/repo`  → `https://gitlab.example.com`
pub async fn host_from_remote() -> Result<String> {
    let url = run("git", &["remote", "get-url", "origin"]).await?;
    parse_host(&url)
}

fn parse_host(remote_url: &str) -> Result<String> {
    let url = remote_url.trim();

    // SSH: git@host:path
    if !url.starts_with("http") {
        if let Some((user_host, _)) = url.split_once(':') {
            // user_host = "git@gitlab.example.com"
            let host = user_host.split_once('@').map(|(_, h)| h).unwrap_or(user_host);
            return Ok(format!("https://{host}"));
        }
    }

    // HTTPS: https://host/path
    if url.starts_with("http://") || url.starts_with("https://") {
        let without_scheme = url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let host = without_scheme.split('/').next().context("empty URL")?;
        let scheme = if url.starts_with("https") { "https" } else { "http" };
        return Ok(format!("{scheme}://{host}"));
    }

    anyhow::bail!("unrecognised remote URL format: {url}")
}

fn parse_project_path(remote_url: &str) -> Result<String> {
    let url = remote_url.trim();

    // SSH: git@host:group/repo.git
    if let Some(after_colon) = url.split_once(':').map(|(_, r)| r) {
        // Make sure it's not an HTTPS port (contains //)
        if !url.starts_with("http") {
            let path = after_colon.trim_end_matches(".git");
            return Ok(path.to_owned());
        }
    }

    // HTTPS: https://host/group/repo.git  or  http://host/group/repo.git
    if url.starts_with("http://") || url.starts_with("https://") {
        // strip scheme + host
        let without_scheme = url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        let after_host = without_scheme
            .splitn(2, '/')
            .nth(1)
            .context("unexpected remote URL format")?;
        let path = after_host.trim_end_matches(".git");
        return Ok(path.to_owned());
    }

    anyhow::bail!("unrecognised remote URL format: {url}")
}

async fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn `{cmd}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`{cmd} {}` failed: {stderr}", args.join(" "));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_remote() {
        assert_eq!(
            parse_project_path("git@gitlab.com:mygroup/myrepo.git").unwrap(),
            "mygroup/myrepo"
        );
    }

    #[test]
    fn parse_https_remote() {
        assert_eq!(
            parse_project_path("https://gitlab.com/mygroup/myrepo.git").unwrap(),
            "mygroup/myrepo"
        );
    }

    #[test]
    fn parse_https_remote_no_dotgit() {
        assert_eq!(
            parse_project_path("https://gitlab.com/mygroup/myrepo").unwrap(),
            "mygroup/myrepo"
        );
    }

    #[test]
    fn parse_ssh_nested_group() {
        assert_eq!(
            parse_project_path("git@gitlab.com:a/b/c.git").unwrap(),
            "a/b/c"
        );
    }

    #[test]
    fn parse_host_ssh() {
        assert_eq!(
            parse_host("git@gitlab.example.com:group/repo.git").unwrap(),
            "https://gitlab.example.com"
        );
    }

    #[test]
    fn parse_host_https() {
        assert_eq!(
            parse_host("https://gitlab.example.com/group/repo.git").unwrap(),
            "https://gitlab.example.com"
        );
    }

    #[test]
    fn parse_host_gitlab_com() {
        assert_eq!(
            parse_host("git@gitlab.com:a/b.git").unwrap(),
            "https://gitlab.com"
        );
    }
}
