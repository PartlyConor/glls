use std::collections::BTreeMap;

use crate::gitlab::types::{DiffRefs, NotePosition};

/// Parse a unified diff string and return a map of
/// `new_file_line_number → old_file_line_number` (None if the line was added).
/// Both line numbers are 1-indexed.
pub fn parse_diff_line_map(diff_text: &str) -> BTreeMap<u32, Option<u32>> {
    let mut map = BTreeMap::new();
    let mut new_line: u32 = 0;
    let mut old_line: u32 = 0;
    let mut in_hunk = false;

    for line in diff_text.lines() {
        // Hunk header: @@ -old_start,old_count +new_start,new_count @@
        if line.starts_with("@@") {
            if let Some((old_start, new_start)) = parse_hunk_header(line) {
                old_line = old_start;
                new_line = new_start;
                in_hunk = true;
            }
            continue;
        }

        // Skip file headers
        if line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            in_hunk = false;
            continue;
        }

        if !in_hunk {
            continue;
        }

        if let Some(rest) = line.strip_prefix(' ') {
            let _ = rest; // context line
            map.insert(new_line, Some(old_line));
            new_line += 1;
            old_line += 1;
        } else if line.starts_with('+') {
            // Added line — exists only in new file
            map.insert(new_line, None);
            new_line += 1;
        } else if line.starts_with('-') {
            // Removed line — only in old file
            old_line += 1;
        }
        // Lines starting with `\` (e.g. "\ No newline at end of file") are ignored
    }

    map
}

/// Parse `@@ -old_start[,old_count] +new_start[,new_count] @@` hunk header.
/// Returns `(old_start, new_start)` on success.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    // Example: "@@ -10,6 +10,8 @@ fn foo() {"
    let inner = line.strip_prefix("@@ ")?.splitn(3, ' ').nth(1)?;
    // inner = "+10,8"
    let new_part = inner.strip_prefix('+')?;
    let new_start: u32 = new_part.split(',').next()?.parse().ok()?;

    let old_part = line
        .strip_prefix("@@ ")?
        .splitn(3, ' ')
        .next()?
        .strip_prefix('-')?;
    let old_start: u32 = old_part.split(',').next()?.parse().ok()?;

    Some((old_start, new_start))
}

/// Convert an LSP 0-indexed line number to a `NotePosition` ready to POST.
pub fn lsp_line_to_position(
    lsp_line: u32, // 0-indexed (as LSP uses)
    new_path: &str,
    diff_refs: &DiffRefs,
) -> NotePosition {
    let new_line_1indexed = lsp_line + 1;
    NotePosition {
        new_path: new_path.to_owned(),
        new_line: Some(new_line_1indexed),
        old_path: Some(new_path.to_owned()),
        old_line: None,
        position_type: "text".to_owned(),
        base_sha: diff_refs.base_sha.clone(),
        head_sha: diff_refs.head_sha.clone(),
        start_sha: diff_refs.start_sha.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = r#"@@ -1,5 +1,7 @@
 line one
 line two
+added line A
+added line B
 line three
-removed line
 line five
"#;

    #[test]
    fn parse_context_lines_map_correctly() {
        let map = parse_diff_line_map(SAMPLE_DIFF);
        // "line one" is new_line 1 → old_line 1 (context)
        assert_eq!(map.get(&1), Some(&Some(1)));
        // "line two" → new 2, old 2
        assert_eq!(map.get(&2), Some(&Some(2)));
        // "added line A" → new 3, old None
        assert_eq!(map.get(&3), Some(&None));
        // "added line B" → new 4, old None
        assert_eq!(map.get(&4), Some(&None));
        // "line three" → new 5, old 3
        assert_eq!(map.get(&5), Some(&Some(3)));
        // removed line increments old only (old becomes 4)
        // "line five" → new 6, old 5
        assert_eq!(map.get(&6), Some(&Some(5)));
    }

    #[test]
    fn lsp_line_converts_to_1indexed() {
        let refs = DiffRefs {
            base_sha: "b".into(),
            head_sha: "h".into(),
            start_sha: "s".into(),
        };
        let pos = lsp_line_to_position(9, "src/foo.rs", &refs);
        assert_eq!(pos.new_line, Some(10));
        assert_eq!(pos.new_path, "src/foo.rs");
    }
}
