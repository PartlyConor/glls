use anyhow::{Context, Result};
use serde_json::json;

use super::types::*;

pub struct GitLabClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl GitLabClient {
    pub fn new(host: &str, token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            base_url: format!("{}/api/v4", host.trim_end_matches('/')),
            token: token.to_owned(),
        }
    }

    fn encode_path(&self, project_path: &str) -> String {
        project_path.replace('/', "%2F")
    }

    fn project_url(&self, project_path: &str) -> String {
        format!("{}/projects/{}", self.base_url, self.encode_path(project_path))
    }

    // ── MR lookups ────────────────────────────────────────────────────────────

    /// Find the open MR for `branch` in `project_path`.
    pub async fn find_mr(&self, project_path: &str, branch: &str) -> Result<Option<MergeRequest>> {
        let url = format!("{}/merge_requests", self.project_url(project_path));
        let resp = self
            .http
            .get(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .query(&[("source_branch", branch), ("state", "opened"), ("per_page", "1")])
            .send()
            .await
            .context("find_mr request failed")?
            .error_for_status()
            .context("find_mr returned non-2xx")?;

        let mut mrs: Vec<MergeRequest> = resp.json().await.context("find_mr deserialise failed")?;
        Ok(mrs.pop())
    }

    pub async fn get_mr(&self, project_path: &str, iid: u64) -> Result<MergeRequest> {
        let url = format!("{}/merge_requests/{iid}", self.project_url(project_path));
        let mr: MergeRequest = self
            .http
            .get(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .send()
            .await
            .context("get_mr request failed")?
            .error_for_status()
            .context("get_mr returned non-2xx")?
            .json()
            .await
            .context("get_mr deserialise failed")?;
        Ok(mr)
    }

    // ── Diffs ─────────────────────────────────────────────────────────────────

    pub async fn list_diffs(&self, project_path: &str, iid: u64) -> Result<Vec<MrDiff>> {
        let url = format!("{}/merge_requests/{iid}/diffs", self.project_url(project_path));
        self.paginate(&url).await
    }

    // ── Discussions ───────────────────────────────────────────────────────────

    pub async fn list_discussions(
        &self,
        project_path: &str,
        iid: u64,
    ) -> Result<Vec<Discussion>> {
        let url = format!(
            "{}/merge_requests/{iid}/discussions",
            self.project_url(project_path)
        );
        self.paginate(&url).await
    }

    // ── Write operations ──────────────────────────────────────────────────────

    pub async fn create_note(&self, project_path: &str, iid: u64, body: &str) -> Result<Note> {
        let url = format!("{}/merge_requests/{iid}/notes", self.project_url(project_path));
        let note: Note = self
            .http
            .post(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "body": body }))
            .send()
            .await
            .context("create_note request failed")?
            .error_for_status()
            .context("create_note returned non-2xx")?
            .json()
            .await
            .context("create_note deserialise failed")?;
        Ok(note)
    }

    pub async fn create_diff_note(
        &self,
        project_path: &str,
        iid: u64,
        body: &str,
        position: &NotePosition,
    ) -> Result<Note> {
        let url = format!(
            "{}/merge_requests/{iid}/discussions",
            self.project_url(project_path)
        );
        // The discussions endpoint returns a Discussion; we extract the first note.
        let discussion: Discussion = self
            .http
            .post(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "body": body, "position": position }))
            .send()
            .await
            .context("create_diff_note request failed")?
            .error_for_status()
            .context("create_diff_note returned non-2xx")?
            .json()
            .await
            .context("create_diff_note deserialise failed")?;
        discussion
            .notes
            .into_iter()
            .next()
            .context("create_diff_note: empty notes in response")
    }

    pub async fn reply_to_discussion(
        &self,
        project_path: &str,
        iid: u64,
        discussion_id: &str,
        body: &str,
    ) -> Result<Note> {
        let url = format!(
            "{}/merge_requests/{iid}/discussions/{discussion_id}/notes",
            self.project_url(project_path)
        );
        let note: Note = self
            .http
            .post(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "body": body }))
            .send()
            .await
            .context("reply_to_discussion request failed")?
            .error_for_status()
            .context("reply_to_discussion returned non-2xx")?
            .json()
            .await
            .context("reply_to_discussion deserialise failed")?;
        Ok(note)
    }

    pub async fn resolve_discussion(
        &self,
        project_path: &str,
        iid: u64,
        discussion_id: &str,
        resolved: bool,
    ) -> Result<()> {
        let url = format!(
            "{}/merge_requests/{iid}/discussions/{discussion_id}",
            self.project_url(project_path)
        );
        self.http
            .put(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .json(&json!({ "resolved": resolved }))
            .send()
            .await
            .context("resolve_discussion request failed")?
            .error_for_status()
            .context("resolve_discussion returned non-2xx")?;
        Ok(())
    }

    pub async fn approve_mr(&self, project_path: &str, iid: u64) -> Result<()> {
        let url = format!(
            "{}/merge_requests/{iid}/approve",
            self.project_url(project_path)
        );
        self.http
            .post(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .send()
            .await
            .context("approve_mr request failed")?
            .error_for_status()
            .context("approve_mr returned non-2xx")?;
        Ok(())
    }

    // ── Pagination helper ─────────────────────────────────────────────────────

    async fn paginate<T>(&self, base_url: &str) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut all: Vec<T> = Vec::new();
        let mut page = 1u32;

        loop {
            let resp = self
                .http
                .get(base_url)
                .header("PRIVATE-TOKEN", &self.token)
                .query(&[("per_page", "100"), ("page", &page.to_string())])
                .send()
                .await
                .with_context(|| format!("paginate GET {base_url} page {page} failed"))?
                .error_for_status()
                .with_context(|| format!("paginate GET {base_url} page {page} non-2xx"))?;

            let next_page = resp
                .headers()
                .get("x-next-page")
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_owned());

            let items: Vec<T> = resp
                .json()
                .await
                .with_context(|| format!("paginate deserialise page {page} failed"))?;
            all.extend(items);

            match next_page {
                Some(_) => page += 1,
                None => break,
            }
        }

        Ok(all)
    }
}
