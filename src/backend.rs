use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::gitlab::types::{Discussion, MergeRequest, MrDiff};

/// What action should be taken when the user runs `gitlab-mr.submitInput`.
#[derive(Debug, Clone)]
pub enum PendingAction {
    /// Post a new top-level diff note on a specific file+line.
    AddComment { file: String, line: u32 },
    /// Reply to an existing discussion thread.
    Reply { discussion_id: String },
}

#[derive(Debug, Default, Clone)]
pub struct BackendState {
    pub config: Option<Config>,
    pub repo_root: PathBuf,
    pub project_path: String,
    pub mr: Option<MergeRequest>,
    pub diffs: Vec<MrDiff>,
    pub discussions: Vec<Discussion>,
    /// Map from repo-relative file path → discussions touching that file.
    pub discussions_by_file: HashMap<String, Vec<Discussion>>,
    /// Diff line maps keyed by new_path (new_line_1indexed → old_line or None).
    pub line_maps: HashMap<String, BTreeMap<u32, Option<u32>>>,
    /// Track whether we've already notified the user about "no MR found".
    pub notified_no_mr: bool,
    /// The local HEAD SHA for which we last emitted an out-of-sync warning.
    pub last_out_of_sync_sha: Option<String>,
    /// Temp-file paths for which we have already called window/showDocument.
    pub shown_diff_paths: HashSet<PathBuf>,
    /// Pending input: path of the temp buffer the user is editing + what to do on submit.
    pub pending_input: Option<(PathBuf, PendingAction)>,
    /// Files the user has marked as "seen", keyed by repo-relative path.
    /// Value is the MR head SHA at the time of marking; if the MR head advances
    /// the entry is stale and the file is treated as unseen again.
    pub seen_files: HashMap<String, String>,
}

/// Why review features (code actions, hover, inlay hints) are unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    /// No GitLab auth — token is empty or config failed to load.
    NoAuth,
    /// No upstream — couldn't resolve a `group/repo` project path from the remote.
    NoUpstream,
    /// Configured correctly, but there's no open MR for the current branch.
    NoMr,
}

impl NotReady {
    /// A human-readable explanation, suitable for logs and user notifications.
    pub fn message(self) -> &'static str {
        match self {
            NotReady::NoAuth => {
                "no GitLab token found — set GITLAB_TOKEN or run `glab auth login`"
            }
            NotReady::NoUpstream => {
                "no GitLab upstream — could not derive a project path from the git remote `origin`"
            }
            NotReady::NoMr => "no open MR found for the current branch",
        }
    }
}

impl BackendState {
    /// Returns `None` when review features have everything they need (auth,
    /// upstream, and an MR loaded), or `Some(reason)` describing the first
    /// missing prerequisite. Used to suppress code actions, hover, and inlay
    /// hints so the workspace isn't cluttered with actions that can't succeed
    /// (no config) or have nothing to target (no open MR).
    pub fn review_blocker(&self) -> Option<NotReady> {
        if self.config.as_ref().map_or(true, |c| c.gitlab_token.is_empty()) {
            Some(NotReady::NoAuth)
        } else if self.project_path.is_empty() {
            Some(NotReady::NoUpstream)
        } else if self.mr.is_none() {
            Some(NotReady::NoMr)
        } else {
            None
        }
    }

    /// Auto-mark any diff files whose path matches a configured ignore pattern
    /// as seen at the given `head_sha`. Existing seen entries for ignored files
    /// are refreshed to the new SHA so they stay suppressed after a rebase/push.
    pub fn apply_ignore_patterns(&mut self, head_sha: &str) {
        let patterns: Vec<glob::Pattern> = self
            .config
            .as_ref()
            .map(|c| c.ignore_patterns.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();

        if patterns.is_empty() {
            return;
        }

        for diff in &self.diffs {
            let path = &diff.new_path;
            if patterns.iter().any(|pat| pat.matches(path)) {
                self.seen_files.insert(path.clone(), head_sha.to_owned());
            }
        }
    }
    pub fn rebuild_derived(&mut self) {
        // discussions_by_file
        let mut by_file: HashMap<String, Vec<Discussion>> = HashMap::new();
        for d in &self.discussions {
            if let Some(first) = d.notes.first() {
                if let Some(pos) = &first.position {
                    by_file
                        .entry(pos.new_path.clone())
                        .or_default()
                        .push(d.clone());
                }
            }
        }
        self.discussions_by_file = by_file;

        // line_maps
        let mut maps = HashMap::new();
        for diff in &self.diffs {
            let map = crate::gitlab::diff::parse_diff_line_map(&diff.diff);
            maps.insert(diff.new_path.clone(), map);
        }
        self.line_maps = maps;
    }
}

pub type SharedState = Arc<RwLock<BackendState>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_token(token: &str) -> Config {
        Config {
            gitlab_host: "https://gitlab.com".to_owned(),
            gitlab_token: token.to_owned(),
            mr_iid: None,
            poll_interval_secs: 60,
            ignore_patterns: vec![],
        }
    }

    fn dummy_mr() -> MergeRequest {
        MergeRequest {
            iid: 1,
            title: "Test MR".to_owned(),
            state: "opened".to_owned(),
            source_branch: "feature".to_owned(),
            target_branch: "main".to_owned(),
            sha: "abc123".to_owned(),
            diff_refs: None,
            author: crate::gitlab::types::User {
                id: 1,
                username: "alice".to_owned(),
                name: "Alice".to_owned(),
            },
        }
    }

    fn state(config: Option<Config>, project_path: &str, mr: Option<MergeRequest>) -> BackendState {
        BackendState {
            config,
            project_path: project_path.to_owned(),
            mr,
            ..Default::default()
        }
    }

    #[test]
    fn ready_when_token_project_path_and_mr_present() {
        let s = state(Some(config_with_token("glpat-xxx")), "group/repo", Some(dummy_mr()));
        assert_eq!(s.review_blocker(), None);
    }

    #[test]
    fn no_auth_when_config_missing() {
        let s = state(None, "group/repo", Some(dummy_mr()));
        assert_eq!(s.review_blocker(), Some(NotReady::NoAuth));
    }

    #[test]
    fn no_auth_when_token_empty() {
        let s = state(Some(config_with_token("")), "group/repo", Some(dummy_mr()));
        assert_eq!(s.review_blocker(), Some(NotReady::NoAuth));
    }

    #[test]
    fn no_upstream_when_project_path_empty() {
        let s = state(Some(config_with_token("glpat-xxx")), "", Some(dummy_mr()));
        assert_eq!(s.review_blocker(), Some(NotReady::NoUpstream));
    }

    #[test]
    fn no_mr_when_mr_absent() {
        let s = state(Some(config_with_token("glpat-xxx")), "group/repo", None);
        assert_eq!(s.review_blocker(), Some(NotReady::NoMr));
    }

    #[test]
    fn auth_takes_priority_over_upstream_and_mr() {
        // With nothing configured, the first missing prerequisite is reported.
        let s = state(None, "", None);
        assert_eq!(s.review_blocker(), Some(NotReady::NoAuth));
    }
}
