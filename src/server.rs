use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::backend::{BackendState, PendingAction, SharedState};
use crate::config::Config;
use crate::convert;
use crate::gitlab::GitLabClient;

pub struct Backend {
    pub client: Client,
    pub state: SharedState,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(RwLock::new(BackendState::default())),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helper: convert a file URI to a repo-relative path string
// ──────────────────────────────────────────────────────────────────────────────

fn uri_to_repo_path(uri: &Url, repo_root: &PathBuf) -> Option<String> {
    let file_path = uri.to_file_path().ok()?;
    file_path
        .strip_prefix(repo_root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

// ──────────────────────────────────────────────────────────────────────────────
// Background polling task
// ──────────────────────────────────────────────────────────────────────────────

async fn refresh_loop(client: Client, state: SharedState) {
    loop {
        let (config, project_path, poll_secs) = {
            let s = state.read().await;
            let cfg = match &s.config {
                Some(c) => c.clone(),
                None => return,
            };
            let poll = cfg.poll_interval_secs;
            (cfg, s.project_path.clone(), poll)
        };

        if config.gitlab_token.is_empty() {
            let mut s = state.write().await;
            if !s.notified_no_token {
                s.notified_no_token = true;
                client
                    .show_message(
                        MessageType::WARNING,
                        "gitlab-mr-lsp: no GitLab token found. Set GITLAB_TOKEN or run `glab auth login`.",
                    )
                    .await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
            continue;
        }

        let gl = GitLabClient::new(&config.gitlab_host, &config.gitlab_token);

        // Determine MR
        let mr_result = if let Some(iid) = config.mr_iid {
            gl.get_mr(&project_path, iid).await.map(Some)
        } else {
            let branch = crate::git::current_branch().await.unwrap_or_default();
            gl.find_mr(&project_path, &branch).await
        };

        let mr = match mr_result {
            Ok(Some(mr)) => mr,
            Ok(None) => {
                let mut s = state.write().await;
                if !s.notified_no_mr {
                    s.notified_no_mr = true;
                    let branch = crate::git::current_branch().await.unwrap_or_default();
                    client
                        .show_message(
                            MessageType::INFO,
                            format!(
                                "gitlab-mr-lsp: no open MR found for branch `{branch}`"
                            ),
                        )
                        .await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
                continue;
            }
            Err(e) => {
                tracing::warn!("Failed to fetch MR: {e:#}");
                tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
                continue;
            }
        };

        // Out-of-sync check: compare local HEAD to the SHA GitLab has indexed.
        // We warn once per unique local SHA that doesn't match, so the warning
        // re-fires if the user rebases and still hasn't pushed.
        if let Ok(local_head) = crate::git::local_head_sha().await {
            if local_head != mr.sha {
                let already_warned = {
                    let s = state.read().await;
                    s.last_out_of_sync_sha.as_deref() == Some(local_head.as_str())
                };
                if !already_warned {
                    state.write().await.last_out_of_sync_sha = Some(local_head.clone());
                    client
                        .show_message_request(
                            MessageType::WARNING,
                            format!(
                                "gitlab-mr-lsp: local HEAD ({}) does not match the MR head \
                                 on GitLab ({}). Comments may be misaligned until you push.",
                                &local_head[..8],
                                &mr.sha[..8],
                            ),
                            Some(vec![MessageActionItem {
                                title: "Dismiss".to_owned(),
                                properties: Default::default(),
                            }]),
                        )
                        .await
                        .ok();
                }
            } else {
                // Back in sync — clear the stored SHA so we warn again if it
                // drifts out of sync again later.
                state.write().await.last_out_of_sync_sha = None;
            }
        }

        // Fetch diffs + discussions in parallel
        let mr_iid = mr.iid;
        let (diffs_res, discussions_res) = tokio::join!(
            gl.list_diffs(&project_path, mr_iid),
            gl.list_discussions(&project_path, mr_iid),
        );

        let diffs = match diffs_res {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to fetch diffs: {e:#}");
                vec![]
            }
        };
        let discussions = match discussions_res {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("Failed to fetch discussions: {e:#}");
                vec![]
            }
        };

        // Previous files with diagnostics (to clear stale ones)
        let prev_diff_files: HashSet<String> = {
            let s = state.read().await;
            s.diffs.iter().map(|d| d.new_path.clone()).collect()
        };
        let prev_files: HashSet<String> = {
            let s = state.read().await;
            s.discussions_by_file.keys().cloned().collect()
        };

        // Update state
        {
            let mut s = state.write().await;
            s.mr = Some(mr);
            s.diffs = diffs;
            s.discussions = discussions;
            s.rebuild_derived();
        }

        // Push diagnostics
        let (repo_root, new_by_file, diff_refs, mr_label, new_diff_files) = {
            let s = state.read().await;
            let refs = s.mr.as_ref().and_then(|m| m.diff_refs.clone());
            let label = s.mr.as_ref()
                .map(|m| format!("MR !{}", m.iid))
                .unwrap_or_default();
            let diff_paths: HashSet<String> = s.diffs.iter().map(|d| d.new_path.clone()).collect();
            (s.repo_root.clone(), s.discussions_by_file.clone(), refs, label, diff_paths)
        };

        // For every changed file: publish the "Changed in MR !N" hint merged
        // with any discussion diagnostics for that file.
        let changed_hint = convert::diffs_to_changed_file_diagnostics(&mr_label);
        for file_path in &new_diff_files {
            let abs = repo_root.join(file_path);
            if let Ok(uri) = Url::from_file_path(&abs) {
                let mut diags = vec![changed_hint.clone()];
                if let Some(discussions) = new_by_file.get(file_path) {
                    diags.extend(convert::discussions_to_diagnostics(
                        discussions,
                        file_path,
                        diff_refs.as_ref(),
                    ));
                }
                client.publish_diagnostics(uri, diags, None).await;
            }
        }

        // For files with discussions but not in the diff list (shouldn't normally
        // happen, but be safe), publish discussion diagnostics alone.
        for (file_path, discussions) in &new_by_file {
            if new_diff_files.contains(file_path) {
                continue; // already handled above
            }
            let abs = repo_root.join(file_path);
            if let Ok(uri) = Url::from_file_path(&abs) {
                let diags = convert::discussions_to_diagnostics(
                    discussions,
                    file_path,
                    diff_refs.as_ref(),
                );
                client.publish_diagnostics(uri, diags, None).await;
            }
        }

        // Clear diagnostics for files no longer in the diff or discussion set
        let all_new: HashSet<&String> = new_diff_files.iter().chain(new_by_file.keys()).collect();
        for old_file in prev_diff_files.iter().chain(prev_files.iter()) {
            if !all_new.contains(old_file) {
                let abs = repo_root.join(old_file);
                if let Ok(uri) = Url::from_file_path(&abs) {
                    client.publish_diagnostics(uri, vec![], None).await;
                }
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Command handlers
// ──────────────────────────────────────────────────────────────────────────────

impl Backend {
    async fn handle_add_comment(&self, args: Vec<Value>) -> LspResult<Option<Value>> {
        let arg = args.first().cloned().unwrap_or(Value::Null);
        let file = arg.get("file").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        let line = arg.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

        self.open_input_buffer(
            "# Write your review comment below. Lines starting with # are ignored.\n# Save and run 'Submit MR input' (gitlab-mr.submitInput) when done.\n",
            PendingAction::AddComment { file, line },
        ).await
    }

    async fn handle_reply(&self, args: Vec<Value>) -> LspResult<Option<Value>> {
        let discussion_id = args
            .first()
            .and_then(|v| v.get("discussion_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        self.open_input_buffer(
            "# Write your reply below. Lines starting with # are ignored.\n# Save and run 'Submit MR input' (gitlab-mr.submitInput) when done.\n",
            PendingAction::Reply { discussion_id },
        ).await
    }

    /// Open a temporary markdown buffer in a Helix split for the user to write
    /// their comment/reply. Stores the pending action in state so that
    /// `handle_submit_input` knows what to do when the user is done.
    async fn open_input_buffer(&self, prompt: &str, action: PendingAction) -> LspResult<Option<Value>> {
        let tmp_path = std::env::temp_dir().join("gitlab-mr-input.md");

        // Guard: don't overwrite a pending input the user hasn't submitted yet.
        {
            let s = self.state.read().await;
            if s.pending_input.is_some() {
                self.client.show_message(
                    MessageType::WARNING,
                    "A comment is already in progress. Submit or cancel it first (delete the contents and run 'Submit MR input').",
                ).await;
                return Ok(None);
            }
        }

        if let Err(e) = tokio::fs::write(&tmp_path, prompt).await {
            tracing::warn!("Failed to write input buffer: {e:#}");
            self.client.show_message(MessageType::ERROR, format!("Failed to open input buffer: {e}")).await;
            return Ok(None);
        }

        {
            let mut s = self.state.write().await;
            s.pending_input = Some((tmp_path.clone(), action));
        }

        let uri = Url::from_file_path(&tmp_path).map_err(|_| tower_lsp::jsonrpc::Error::internal_error())?;
        match self.client.show_document(ShowDocumentParams {
            uri,
            external: Some(false),
            take_focus: Some(true),
            selection: None,
        }).await {
            Ok(true) => {
                self.client.show_message(
                    MessageType::INFO,
                    "Write your comment, save, then run 'Submit MR input' (gitlab-mr.submitInput).",
                ).await;
            }
            _ => {
                self.client.show_message(MessageType::ERROR, "Helix could not open the input buffer.").await;
                self.state.write().await.pending_input = None;
            }
        }
        Ok(None)
    }

    async fn handle_submit_input(&self) -> LspResult<Option<Value>> {
        let (tmp_path, action) = {
            let mut s = self.state.write().await;
            match s.pending_input.take() {
                Some(p) => p,
                None => {
                    self.client.show_message(MessageType::WARNING, "No pending MR input to submit.").await;
                    return Ok(None);
                }
            }
        };

        let contents = match tokio::fs::read_to_string(&tmp_path).await {
            Ok(c) => c,
            Err(e) => {
                self.client.show_message(MessageType::ERROR, format!("Could not read input file: {e}")).await;
                return Ok(None);
            }
        };

        let body: String = contents
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");

        if body.trim().is_empty() {
            self.client.show_message(MessageType::INFO, "Submission cancelled (empty body).").await;
            return Ok(None);
        }

        let (config, project_path, mr_iid) = snap_config_mr(&self.state).await?;
        let gl = GitLabClient::new(&config.gitlab_host, &config.gitlab_token);

        match action {
            PendingAction::AddComment { file, line } => {
                let diff_refs = self.state.read().await.mr.as_ref().and_then(|m| m.diff_refs.clone());
                let result = if let Some(refs) = diff_refs {
                    let position = crate::gitlab::types::NotePosition {
                        new_path: file.clone(),
                        new_line: Some(line),
                        old_path: Some(file.clone()),
                        old_line: None,
                        position_type: "text".to_owned(),
                        base_sha: refs.base_sha,
                        head_sha: refs.head_sha,
                        start_sha: refs.start_sha,
                    };
                    gl.create_diff_note(&project_path, mr_iid, &body, &position).await
                } else {
                    gl.create_note(&project_path, mr_iid, &body).await
                };
                match result {
                    Ok(_) => {
                        self.client.show_message(MessageType::INFO, "Comment posted to GitLab.").await;
                        trigger_refresh(self.client.clone(), Arc::clone(&self.state)).await;
                    }
                    Err(e) => {
                        tracing::warn!("create_note failed: {e:#}");
                        self.client.show_message(MessageType::ERROR, format!("Failed to post comment: {e}")).await;
                    }
                }
            }
            PendingAction::Reply { discussion_id } => {
                match gl.reply_to_discussion(&project_path, mr_iid, &discussion_id, &body).await {
                    Ok(_) => {
                        self.client.show_message(MessageType::INFO, "Reply posted to GitLab.").await;
                        trigger_refresh(self.client.clone(), Arc::clone(&self.state)).await;
                    }
                    Err(e) => {
                        tracing::warn!("reply_to_discussion failed: {e:#}");
                        self.client.show_message(MessageType::ERROR, format!("Failed to post reply: {e}")).await;
                    }
                }
            }
        }

        Ok(None)
    }

    async fn handle_resolve(&self, args: Vec<Value>) -> LspResult<Option<Value>> {
        let discussion_id = args
            .first()
            .and_then(|v| v.get("discussion_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let (config, project_path, mr_iid) = snap_config_mr(&self.state).await?;
        let gl = GitLabClient::new(&config.gitlab_host, &config.gitlab_token);

        match gl.resolve_discussion(&project_path, mr_iid, &discussion_id, true).await {
            Ok(_) => {
                self.client.show_message(MessageType::INFO, "Thread resolved.").await;
                trigger_refresh(self.client.clone(), Arc::clone(&self.state)).await;
            }
            Err(e) => {
                tracing::warn!("resolve_discussion failed: {e:#}");
                self.client.show_message(MessageType::ERROR, format!("Failed to resolve: {e}")).await;
            }
        }
        Ok(None)
    }

    async fn handle_show_thread(&self, args: Vec<Value>) -> LspResult<Option<Value>> {
        let discussion_id = args
            .first()
            .and_then(|v| v.get("discussion_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        // Look up the thread and render it
        let (rendered, already_resolved) = {
            let s = self.state.read().await;
            let diff_refs = s.mr.as_ref().and_then(|m| m.diff_refs.clone());
            let thread = s.discussions.iter().find(|d| d.id == discussion_id).cloned();
            match thread {
                Some(t) => {
                    let resolved = t.resolved || t.notes.iter().any(|n| n.resolvable && n.resolved);
                    (convert::render_thread(&t, diff_refs.as_ref()), resolved)
                }
                None => {
                    self.client
                        .show_message(MessageType::WARNING, "gitlab-mr-lsp: thread not found.")
                        .await;
                    return Ok(None);
                }
            }
        };

        let resolve_label = if already_resolved { "Unresolve" } else { "Resolve" };

        let chosen = self
            .client
            .show_message_request(
                MessageType::INFO,
                rendered,
                Some(vec![
                    MessageActionItem { title: "Reply".to_owned(), properties: Default::default() },
                    MessageActionItem { title: resolve_label.to_owned(), properties: Default::default() },
                    MessageActionItem { title: "Dismiss".to_owned(), properties: Default::default() },
                ]),
            )
            .await
            .unwrap_or(None);

        let action = chosen.as_ref().map(|a| a.title.as_str()).unwrap_or("Dismiss");
        let id_arg = vec![serde_json::json!({ "discussion_id": discussion_id })];

        match action {
            "Reply" => self.handle_reply(id_arg).await,
            "Resolve" | "Unresolve" => {
                let (config, project_path, mr_iid) = snap_config_mr(&self.state).await?;
                let gl = GitLabClient::new(&config.gitlab_host, &config.gitlab_token);
                let resolve = !already_resolved;
                match gl.resolve_discussion(&project_path, mr_iid, &discussion_id, resolve).await {
                    Ok(_) => {
                        let msg = if resolve { "Thread resolved." } else { "Thread unresolved." };
                        self.client.show_message(MessageType::INFO, msg).await;
                        trigger_refresh(self.client.clone(), Arc::clone(&self.state)).await;
                    }
                    Err(e) => {
                        tracing::warn!("resolve_discussion failed: {e:#}");
                        self.client
                            .show_message(MessageType::ERROR, format!("Failed to resolve: {e}"))
                            .await;
                    }
                }
                Ok(None)
            }
            _ => Ok(None), // Dismiss
        }
    }

    async fn handle_view_diff(&self, args: Vec<Value>) -> LspResult<Option<Value>> {
        let file_path = args
            .first()
            .and_then(|v| v.get("file"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let (diff_content, mr_iid) = {
            let s = self.state.read().await;
            let iid = s.mr.as_ref().map(|m| m.iid).unwrap_or(0);
            let content = s
                .diffs
                .iter()
                .find(|d| d.new_path == file_path)
                .map(|d| {
                    // Prepend a standard diff --git header so editors recognise
                    // the file type and the header shows the file name clearly.
                    format!(
                        "diff --git a/{} b/{}\n--- a/{}\n+++ b/{}\n{}",
                        d.old_path, d.new_path,
                        d.old_path, d.new_path,
                        d.diff,
                    )
                });
            (content, iid)
        };

        let diff_content = match diff_content {
            Some(c) => c,
            None => {
                self.client
                    .show_message(MessageType::WARNING, format!("No diff found for {file_path}"))
                    .await;
                return Ok(None);
            }
        };

        // Write to a stable temp path — filename encodes the MR iid and the
        // source file so re-running the command refreshes the same buffer.
        let safe_name = file_path.replace('/', "_").replace(std::path::MAIN_SEPARATOR, "_");
        let tmp_path = std::env::temp_dir().join(format!("gitlab-mr-{mr_iid}-{safe_name}.diff"));

        if let Err(e) = tokio::fs::write(&tmp_path, diff_content).await {
            tracing::warn!("Failed to write diff file: {e:#}");
            self.client
                .show_message(MessageType::ERROR, format!("Failed to write diff: {e}"))
                .await;
            return Ok(None);
        }

        let uri = match Url::from_file_path(&tmp_path) {
            Ok(u) => u,
            Err(_) => {
                self.client
                    .show_message(MessageType::ERROR, "Failed to build URI for diff file")
                    .await;
                return Ok(None);
            }
        };

        // Open in a vertical split without stealing focus
        match self.client.show_document(ShowDocumentParams {
            uri,
            external: Some(false),
            take_focus: Some(false),
            selection: None,
        }).await {
            Ok(true) => {}
            Ok(false) => {
                self.client
                    .show_message(MessageType::WARNING, "Helix could not open the diff pane")
                    .await;
            }
            Err(e) => {
                tracing::warn!("show_document failed: {e:#}");
            }
        }

        Ok(None)
    }

    async fn handle_approve(&self) -> LspResult<Option<Value>> {
        let (config, project_path, mr_iid) = snap_config_mr(&self.state).await?;
        let gl = GitLabClient::new(&config.gitlab_host, &config.gitlab_token);

        match gl.approve_mr(&project_path, mr_iid).await {
            Ok(_) => {
                self.client.show_message(MessageType::INFO, format!("MR !{mr_iid} approved.")).await;
                trigger_refresh(self.client.clone(), Arc::clone(&self.state)).await;
            }
            Err(e) => {
                tracing::warn!("approve_mr failed: {e:#}");
                self.client.show_message(MessageType::ERROR, format!("Failed to approve MR: {e}")).await;
            }
        }
        Ok(None)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Small helpers
// ──────────────────────────────────────────────────────────────────────────────

async fn state_snapshot(state: &SharedState) -> BackendState {
    state.read().await.clone()
}

async fn snap_config_mr(
    state: &SharedState,
) -> LspResult<(Config, String, u64)> {
    let s = state.read().await;
    let config = s.config.clone().ok_or_else(|| {
        tower_lsp::jsonrpc::Error::internal_error()
    })?;
    let mr_iid = s.mr.as_ref().map(|m| m.iid).ok_or_else(|| {
        tower_lsp::jsonrpc::Error::internal_error()
    })?;
    Ok((config, s.project_path.clone(), mr_iid))
}

async fn trigger_refresh(client: Client, state: SharedState) {
    // Run one refresh cycle immediately (outside the polling loop)
    tokio::spawn(async move {
        let (config, project_path) = {
            let s = state.read().await;
            let cfg = match &s.config { Some(c) => c.clone(), None => return };
            (cfg, s.project_path.clone())
        };
        let gl = GitLabClient::new(&config.gitlab_host, &config.gitlab_token);

        let branch = crate::git::current_branch().await.unwrap_or_default();
        let mr_opt = if let Some(iid) = config.mr_iid {
            gl.get_mr(&project_path, iid).await.ok()
        } else {
            gl.find_mr(&project_path, &branch).await.ok().flatten()
        };
        let mr = match mr_opt { Some(m) => m, None => return };
        let mr_iid = mr.iid;

        let (diffs, discussions) = tokio::join!(
            gl.list_diffs(&project_path, mr_iid),
            gl.list_discussions(&project_path, mr_iid),
        );

        let repo_root = {
            let mut s = state.write().await;
            s.mr = Some(mr);
            if let Ok(d) = diffs { s.diffs = d; }
            if let Ok(d) = discussions { s.discussions = d; }
            s.rebuild_derived();
            s.repo_root.clone()
        };

        let (by_file, diff_refs, mr_label, diff_files) = {
            let s = state.read().await;
            let refs = s.mr.as_ref().and_then(|m| m.diff_refs.clone());
            let label = s.mr.as_ref().map(|m| format!("MR !{}", m.iid)).unwrap_or_default();
            let diff_paths: HashSet<String> = s.diffs.iter().map(|d| d.new_path.clone()).collect();
            (s.discussions_by_file.clone(), refs, label, diff_paths)
        };
        let changed_hint = convert::diffs_to_changed_file_diagnostics(&mr_label);
        for fp in &diff_files {
            let abs = repo_root.join(fp);
            if let Ok(uri) = Url::from_file_path(&abs) {
                let mut diags = vec![changed_hint.clone()];
                if let Some(ds) = by_file.get(fp) {
                    diags.extend(convert::discussions_to_diagnostics(ds, fp, diff_refs.as_ref()));
                }
                client.publish_diagnostics(uri, diags, None).await;
            }
        }
        for (fp, ds) in &by_file {
            if diff_files.contains(fp) { continue; }
            let abs = repo_root.join(fp);
            if let Ok(uri) = Url::from_file_path(&abs) {
                let diags = convert::discussions_to_diagnostics(ds, fp, diff_refs.as_ref());
                client.publish_diagnostics(uri, diags, None).await;
            }
        }
    });
}

// ──────────────────────────────────────────────────────────────────────────────
// LanguageServer implementation
// ──────────────────────────────────────────────────────────────────────────────
#[async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        // Store workspace root for config loading
        let workspace_root: Option<PathBuf> = params
            .workspace_folders
            .as_deref()
            .and_then(|f| f.first())
            .and_then(|f| f.uri.to_file_path().ok())
            .or_else(|| {
                #[allow(deprecated)]
                params
                    .root_uri
                    .as_ref()
                    .and_then(|u| u.to_file_path().ok())
            });

        // Load config (async) — store in state for later use
        let config = Config::load(workspace_root.as_ref()).await;
        {
            let mut s = self.state.write().await;
            s.config = config.ok();
            if let Some(root) = workspace_root {
                s.repo_root = root;
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                workspace_symbol_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        "gitlab-mr.addComment".into(),
                        "gitlab-mr.showThread".into(),
                        "gitlab-mr.replyToThread".into(),
                        "gitlab-mr.resolveThread".into(),
                        "gitlab-mr.submitInput".into(),
                        "gitlab-mr.viewDiff".into(),
                        "gitlab-mr.approveMr".into(),
                    ],
                    ..Default::default()
                }),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::NONE),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        // Resolve git context
        let repo_root_result = crate::git::repo_root().await;
        let project_path_result = crate::git::project_path_from_remote().await;

        {
            let mut s = self.state.write().await;
            match repo_root_result {
                Ok(root) => {
                    tracing::info!("repo root: {}", root.display());
                    // Only update if not already set from initialize params
                    if s.repo_root == std::path::PathBuf::default() {
                        s.repo_root = root;
                    }
                }
                Err(e) => tracing::warn!("Could not determine repo root: {e}"),
            }
            match project_path_result {
                Ok(path) => {
                    tracing::info!("project path: {path}");
                    s.project_path = path;
                }
                Err(e) => tracing::warn!("Could not determine project path: {e}"),
            }
        }

        // Spawn background polling task
        let client = self.client.clone();
        let state = Arc::clone(&self.state);
        tokio::spawn(refresh_loop(client, state));

        tracing::info!("gitlab-mr-lsp initialized");
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    // ── workspace/symbol ──────────────────────────────────────────────────────

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> LspResult<Option<Vec<SymbolInformation>>> {
        let s = self.state.read().await;
        let label = s
            .mr
            .as_ref()
            .map(|m| format!("MR !{} — {}", m.iid, m.title))
            .unwrap_or_default();
        let symbols = convert::diffs_to_symbols(&s.diffs, &s.repo_root, &label, &params.query);
        Ok(Some(symbols))
    }

    // ── textDocument/hover ────────────────────────────────────────────────────

    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let pos = params.text_document_position_params;
        let uri = &pos.text_document.uri;
        let lsp_line = pos.position.line;
        let gitlab_line = lsp_line + 1; // GitLab is 1-indexed

        let s = self.state.read().await;
        let file_path = match uri_to_repo_path(uri, &s.repo_root) {
            Some(p) => p,
            None => return Ok(None),
        };
        let diff_refs = s.mr.as_ref().and_then(|m| m.diff_refs.clone());

        let threads: Vec<&crate::gitlab::types::Discussion> = s
            .discussions_by_file
            .get(&file_path)
            .map(|ds| {
                ds.iter()
                    .filter(|d| {
                        d.notes.iter().any(|n| {
                            n.position
                                .as_ref()
                                .and_then(|p| p.new_line)
                                .map(|l| l == gitlab_line)
                                .unwrap_or(false)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        if threads.is_empty() {
            return Ok(None);
        }

        let mut md = String::new();
        for thread in threads {
            md.push_str(&convert::render_thread(thread, diff_refs.as_ref()));
            md.push_str("\n\n---\n\n");
        }

        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: md,
            }),
            range: None,
        }))
    }

    // ── textDocument/codeAction ───────────────────────────────────────────────

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> LspResult<Option<CodeActionResponse>> {
        let s = self.state.read().await;
        let line = params.range.start.line + 1; // 1-indexed for GitLab
        let file_path = match uri_to_repo_path(&params.text_document.uri, &s.repo_root) {
            Some(p) => p,
            None => return Ok(None),
        };

        let threads_at_line: Vec<&crate::gitlab::types::Discussion> = s
            .discussions_by_file
            .get(&file_path)
            .map(|ds| {
                ds.iter()
                    .filter(|d| {
                        d.notes
                            .first()
                            .and_then(|n| n.position.as_ref())
                            .and_then(|p| p.new_line)
                            .map(|l| l == line)
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        // If there's a pending input buffer, surface the submit action prominently.
        if s.pending_input.is_some() {
            actions.push(make_command(
                "Submit MR input",
                "gitlab-mr.submitInput",
                serde_json::json!({}),
            ));
        }

        // Add comment on this line
        actions.push(make_command(
            "Add review comment",
            "gitlab-mr.addComment",
            serde_json::json!({ "file": file_path, "line": line }),
        ));

        // View MR diff for this file (if it's a changed file)
        if s.diffs.iter().any(|d| d.new_path == file_path) {
            actions.push(make_command(
                "View MR diff",
                "gitlab-mr.viewDiff",
                serde_json::json!({ "file": file_path }),
            ));
        }

        for thread in &threads_at_line {
            let id = &thread.id;
            let preview = first_note_preview(thread);
            actions.push(make_command(
                &format!("Show thread ({preview})"),
                "gitlab-mr.showThread",
                serde_json::json!({ "discussion_id": id }),
            ));
        }

        // Approve MR (always shown if MR is open)
        if let Some(mr) = &s.mr {
            if mr.state == "opened" {
                actions.push(make_command(
                    &format!("Approve MR !{}", mr.iid),
                    "gitlab-mr.approveMr",
                    serde_json::json!({}),
                ));
            }
        }

        Ok(Some(actions))
    }

    // ── workspace/executeCommand ──────────────────────────────────────────────

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> LspResult<Option<Value>> {
        match params.command.as_str() {
            "gitlab-mr.addComment" => self.handle_add_comment(params.arguments).await,
            "gitlab-mr.showThread" => self.handle_show_thread(params.arguments).await,
            "gitlab-mr.replyToThread" => self.handle_reply(params.arguments).await,
            "gitlab-mr.resolveThread" => self.handle_resolve(params.arguments).await,
            "gitlab-mr.submitInput" => self.handle_submit_input().await,
            "gitlab-mr.viewDiff" => self.handle_view_diff(params.arguments).await,
            "gitlab-mr.approveMr" => self.handle_approve().await,
            _ => Ok(None),
        }
    }

    // ── textDocument/inlayHint ────────────────────────────────────────────────

    async fn inlay_hint(&self, params: InlayHintParams) -> LspResult<Option<Vec<InlayHint>>> {
        let s = self.state.read().await;
        let file_path = match uri_to_repo_path(&params.text_document.uri, &s.repo_root) {
            Some(p) => p,
            None => return Ok(Some(vec![])),
        };

        let all_discussions: Vec<crate::gitlab::types::Discussion> = s.discussions.clone();
        let diff_refs = s.mr.as_ref().and_then(|m| m.diff_refs.clone());
        drop(s);

        let hints = convert::discussions_to_inlay_hints(&all_discussions, &file_path, diff_refs.as_ref());
        Ok(Some(hints))
    }

    // ── textDocument/didOpen ──────────────────────────────────────────────────

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let (file_path, discussions, repo_root, diff_refs, mr_label, in_diff) = {
            let s = self.state.read().await;
            let fp = match uri_to_repo_path(&uri, &s.repo_root) {
                Some(p) => p,
                None => return,
            };
            let ds = s.discussions_by_file.get(&fp).cloned().unwrap_or_default();
            let refs = s.mr.as_ref().and_then(|m| m.diff_refs.clone());
            let label = s.mr.as_ref().map(|m| format!("MR !{}", m.iid)).unwrap_or_default();
            let in_diff = s.diffs.iter().any(|d| d.new_path == fp);
            (fp, ds, s.repo_root.clone(), refs, label, in_diff)
        };

        let mut diags = Vec::new();
        if in_diff {
            diags.push(convert::diffs_to_changed_file_diagnostics(&mr_label));
        }
        diags.extend(convert::discussions_to_diagnostics(&discussions, &file_path, diff_refs.as_ref()));

        let abs = repo_root.join(&file_path);
        if let Ok(file_uri) = Url::from_file_path(&abs) {
            self.client.publish_diagnostics(file_uri, diags, None).await;
        }
    }

    // ── textDocument/didClose ─────────────────────────────────────────────────

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // If the closed file is our pending input buffer, clear the pending
        // action — the user discarded it by closing without submitting.
        let closed_path = match params.text_document.uri.to_file_path() {
            Ok(p) => p,
            Err(_) => return,
        };
        let is_pending = {
            let s = self.state.read().await;
            s.pending_input.as_ref().map(|(p, _)| *p == closed_path).unwrap_or(false)
        };
        if is_pending {
            self.state.write().await.pending_input = None;
            self.client.show_message(MessageType::INFO, "MR input cancelled (buffer closed).").await;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Code action helpers
// ──────────────────────────────────────────────────────────────────────────────

fn make_command(title: &str, command: &str, arg: serde_json::Value) -> CodeActionOrCommand {
    CodeActionOrCommand::Command(Command {
        title: title.to_owned(),
        command: command.to_owned(),
        arguments: Some(vec![arg]),
    })
}

fn first_note_preview(discussion: &crate::gitlab::types::Discussion) -> String {
    discussion
        .notes
        .first()
        .map(|n| {
            let s = n.body.lines().next().unwrap_or("");
            if s.len() > 40 {
                format!("{}…", &s[..40])
            } else {
                s.to_owned()
            }
        })
        .unwrap_or_default()
}
