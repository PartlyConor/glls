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

impl BackendState {
    /// Rebuild the derived maps from the raw discussions and diffs.
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
