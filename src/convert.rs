use std::collections::HashMap;
use std::path::Path;

use tower_lsp::lsp_types::*;

use crate::gitlab::types::{DiffRefs, Discussion, MrDiff};

// ──────────────────────────────────────────────────────────────────────────────
// Outdated detection
// ──────────────────────────────────────────────────────────────────────────────

/// A discussion is resolved if the discussion-level flag is set OR if any
/// resolvable note within it is marked resolved. The GitLab API sometimes
/// omits the discussion-level `resolved` field (defaults to false via serde),
/// so we check both places.
fn discussion_is_resolved(d: &Discussion) -> bool {
    d.resolved || d.notes.iter().any(|n| n.resolvable && n.resolved)
}

/// A note is outdated if its position SHAs don't match the MR's current
/// diff_refs. When `current_refs` is None (MR not yet loaded) we assume
/// up-to-date so we don't suppress everything on startup.
fn note_is_outdated(note: &crate::gitlab::types::Note, current_refs: Option<&DiffRefs>) -> bool {
    let refs = match current_refs {
        Some(r) => r,
        None => return false,
    };
    let pos = match &note.position {
        Some(p) => p,
        None => return false,
    };
    pos.head_sha != refs.head_sha
}

// ──────────────────────────────────────────────────────────────────────────────
// Diagnostics
// ──────────────────────────────────────────────────────────────────────────────

/// Convert discussions for `file_repo_path` into LSP diagnostics.
/// `current_refs` is the MR's current diff_refs — used to flag outdated notes.
pub fn discussions_to_diagnostics(
    discussions: &[Discussion],
    file_repo_path: &str,
    current_refs: Option<&DiffRefs>,
) -> Vec<Diagnostic> {
    discussions
        .iter()
        .filter(|d| !d.notes.is_empty())
        .filter_map(|d| {
            let first = &d.notes[0];
            let pos = first.position.as_ref()?;
            if pos.new_path != file_repo_path {
                return None;
            }
            let line = pos.new_line? as u32 - 1; // LSP is 0-indexed
            let range = Range {
                start: Position { line, character: 0 },
                end: Position {
                    line,
                    character: u32::MAX,
                },
            };

            let outdated = note_is_outdated(first, current_refs);
            let resolved = discussion_is_resolved(d);

            let severity = if outdated || resolved {
                DiagnosticSeverity::HINT
            } else {
                DiagnosticSeverity::INFORMATION
            };

            let first_line = first.body.lines().next().unwrap_or("").trim();
            let preview = if first_line.len() > 60 {
                format!("{}...", &first_line[..60])
            } else {
                first_line.to_owned()
            };
            let message = match (outdated, resolved) {
                (true, _) => format!("[outdated] @{} - {}", first.author.username, preview),
                (_, true) => format!("[resolved] @{} - {}", first.author.username, preview),
                _ => format!("@{} - {}", first.author.username, preview),
            };

            Some(Diagnostic {
                range,
                severity: Some(severity),
                source: Some("gitlab-mr".to_owned()),
                message,
                ..Default::default()
            })
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────────────
// Changed-file diagnostics (HINT on line 0 of every diff'd file)
// ──────────────────────────────────────────────────────────────────────────────

/// Produce a single HINT diagnostic at line 0 for each file touched by the MR.
/// This makes all changed files immediately visible in Helix's diagnostics
/// picker (`<space>d`) as soon as the MR data is loaded.
pub fn diffs_to_changed_file_diagnostics(mr_label: &str) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 0,
            },
        },
        severity: Some(DiagnosticSeverity::HINT),
        source: Some("gitlab-mr".to_owned()),
        message: format!("Changed in {mr_label}"),
        ..Default::default()
    }
}

// Workspace symbols (changed-file picker)
// ──────────────────────────────────────────────────────────────────────────────

pub fn diffs_to_symbols(
    diffs: &[MrDiff],
    repo_root: &Path,
    mr_label: &str,
    query: &str,
) -> Vec<SymbolInformation> {
    let query_lc = query.to_lowercase();
    diffs
        .iter()
        .filter(|d| query_lc.is_empty() || d.new_path.to_lowercase().contains(&query_lc))
        .filter_map(|d| {
            let path = repo_root.join(&d.new_path);
            let uri = Url::from_file_path(&path).ok()?;
            #[allow(deprecated)]
            Some(SymbolInformation {
                name: d.new_path.clone(),
                kind: SymbolKind::FILE,
                tags: None,
                deprecated: None,
                location: Location {
                    uri,
                    range: Range::default(),
                },
                container_name: Some(mr_label.to_owned()),
            })
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────────────
// Hover (full thread rendering)
// ──────────────────────────────────────────────────────────────────────────────

/// Render a discussion thread as Markdown for display in a hover popup.
/// `current_refs` is used to detect whether the thread is outdated.
pub fn render_thread(discussion: &Discussion, current_refs: Option<&DiffRefs>) -> String {
    let mut md = String::new();

    let outdated = discussion
        .notes
        .first()
        .map(|n| note_is_outdated(n, current_refs))
        .unwrap_or(false);

    if outdated {
        md.push_str("**[outdated — made on a previous version of this diff]**\n\n");
    }

    for (i, note) in discussion.notes.iter().enumerate() {
        let indent = if i == 0 { "" } else { "  " };
        // Format timestamp: take first 16 chars of ISO 8601 (YYYY-MM-DDTHH:MM)
        let ts = note
            .created_at
            .get(..16)
            .unwrap_or(&note.created_at)
            .replace('T', " ");
        md.push_str(&format!(
            "{}**@{}** · {}\n",
            indent, note.author.username, ts
        ));
        for line in note.body.lines() {
            md.push_str(&format!("{indent}> {line}\n"));
        }
        md.push('\n');
    }
    if discussion_is_resolved(discussion) {
        md.push_str("✓ *Resolved*\n");
    }
    md
}

// ──────────────────────────────────────────────────────────────────────────────
// Inlay hints (comment counts per line)
// ──────────────────────────────────────────────────────────────────────────────

/// `current_refs` is used to separate live vs outdated comment counts.
pub fn discussions_to_inlay_hints(
    discussions: &[Discussion],
    file_repo_path: &str,
    current_refs: Option<&DiffRefs>,
) -> Vec<InlayHint> {
    // Group discussions by new_line (0-indexed for LSP), split by outdated status
    let mut by_line: HashMap<u32, (usize, usize, usize, usize)> = HashMap::new();
    // value: (live_unresolved, live_resolved, outdated_unresolved, outdated_resolved)

    for d in discussions {
        let first = match d.notes.first() {
            Some(n) => n,
            None => continue,
        };
        let pos = match first
            .position
            .as_ref()
            .filter(|p| p.new_path == file_repo_path)
        {
            Some(p) => p,
            None => continue,
        };
        let line_1 = match pos.new_line {
            Some(l) => l,
            None => continue,
        };
        let line_0 = line_1 - 1;
        let outdated = note_is_outdated(first, current_refs);
        let resolved = discussion_is_resolved(d);
        let entry = by_line.entry(line_0).or_default();
        match (outdated, resolved) {
            (false, false) => entry.0 += 1,
            (false, true) => entry.1 += 1,
            (true, false) => entry.2 += 1,
            (true, true) => entry.3 += 1,
        }
    }

    by_line
        .into_iter()
        .map(
            |(line, (live_open, live_resolved, old_open, old_resolved))| {
                let mut parts: Vec<String> = Vec::new();

                let live_total = live_open + live_resolved;
                if live_total > 0 {
                    if live_open == 0 {
                        parts.push(format!(
                            "✓ {} comment{}",
                            live_total,
                            if live_total == 1 { "" } else { "s" }
                        ));
                    } else {
                        parts.push(format!("{live_open}/{live_total} unresolved"));
                    }
                }

                let old_total = old_open + old_resolved;
                if old_total > 0 {
                    parts.push(format!("{old_total} outdated"));
                }

                let label = parts.join(", ");

                InlayHint {
                    position: Position { line, character: 0 },
                    label: InlayHintLabel::String(label),
                    kind: Some(InlayHintKind::TYPE),
                    padding_left: None,
                    padding_right: Some(true),
                    text_edits: None,
                    tooltip: None,
                    data: None,
                }
            },
        )
        .collect()
}
