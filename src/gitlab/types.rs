use serde::{Deserialize, Serialize};

// ──────────────────────────────────────────────────────────────────────────────
// Top-level MR
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct MergeRequest {
    pub iid: u64,
    pub title: String,
    pub state: String,
    pub source_branch: String,
    pub target_branch: String,
    pub sha: String,
    pub diff_refs: Option<DiffRefs>,
    pub author: User,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiffRefs {
    pub base_sha: String,
    pub head_sha: String,
    pub start_sha: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// Diffs
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct MrDiff {
    pub old_path: String,
    pub new_path: String,
    pub diff: String,
    #[serde(default)]
    pub new_file: bool,
    #[serde(default)]
    pub deleted_file: bool,
    #[serde(default)]
    pub renamed_file: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// Discussions & Notes
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Discussion {
    pub id: String,
    #[serde(default)]
    pub resolved: bool,
    pub notes: Vec<Note>,
}

impl Discussion {
    /// True if at least one note in this discussion is resolvable.
    pub fn resolvable(&self) -> bool {
        self.notes.iter().any(|n| n.resolvable)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Note {
    pub id: u64,
    #[serde(default)]
    pub body: String, // null on some system/bot notes
    pub author: User,
    #[serde(default)]
    pub created_at: String,
    pub position: Option<NotePosition>,
    #[serde(default)]
    pub resolvable: bool,
    #[serde(default)]
    pub resolved: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NotePosition {
    pub new_path: String,
    pub new_line: Option<u32>,
    pub old_path: Option<String>, // null for comments on new files
    pub old_line: Option<u32>,
    #[serde(default)]
    pub position_type: String,
    #[serde(default)]
    pub base_sha: String,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub start_sha: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// Users
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: u64,
    pub username: String,
    pub name: String,
}

// ──────────────────────────────────────────────────────────────────────────────
// Request bodies (for POST/PUT)
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CreateNoteBody<'a> {
    pub body: &'a str,
}

#[derive(Debug, Serialize)]
pub struct CreateDiffNoteBody<'a> {
    pub body: &'a str,
    pub position: &'a NotePosition,
}

#[derive(Debug, Serialize)]
pub struct ResolveDiscussionBody {
    pub resolved: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialise_merge_request() {
        let json = r#"{
            "iid": 42,
            "title": "Fix the bug",
            "state": "opened",
            "source_branch": "fix/my-bug",
            "target_branch": "main",
            "sha": "abc123",
            "diff_refs": {
                "base_sha": "base",
                "head_sha": "head",
                "start_sha": "start"
            },
            "author": { "id": 1, "username": "alice", "name": "Alice" }
        }"#;
        let mr: MergeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(mr.iid, 42);
        assert_eq!(mr.title, "Fix the bug");
        assert!(mr.diff_refs.is_some());
    }

    #[test]
    fn deserialise_discussion() {
        let json = r#"{
            "id": "abc",
            "resolved": false,
            "notes": [{
                "id": 1,
                "body": "please fix this",
                "author": { "id": 2, "username": "bob", "name": "Bob" },
                "created_at": "2026-01-01T00:00:00Z",
                "position": {
                    "new_path": "src/main.rs",
                    "new_line": 10,
                    "old_path": "src/main.rs",
                    "old_line": null,
                    "position_type": "text",
                    "base_sha": "base",
                    "head_sha": "head",
                    "start_sha": "start"
                },
                "resolvable": true,
                "resolved": false
            }]
        }"#;
        let d: Discussion = serde_json::from_str(json).unwrap();
        assert_eq!(d.notes.len(), 1);
        assert_eq!(d.notes[0].position.as_ref().unwrap().new_line, Some(10));
    }
}
