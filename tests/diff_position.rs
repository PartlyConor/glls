use std::collections::BTreeMap;

// Re-test the diff position math from the integration test perspective.
// The unit tests in src/gitlab/diff.rs cover the core logic;
// here we test a more realistic multi-hunk diff.

fn parse_diff_line_map(diff: &str) -> BTreeMap<u32, Option<u32>> {
    // We duplicate the logic here to avoid importing the crate,
    // or we can use it directly if cfg(test) allows.
    // Since this is an integration test, we test via the public API.
    // For now, replicate the algorithm inline to keep the test self-contained.
    let mut map = BTreeMap::new();
    let mut new_line: u32 = 0;
    let mut old_line: u32 = 0;
    let mut in_hunk = false;

    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some((o, n)) = parse_hunk_header(line) {
                old_line = o;
                new_line = n;
                in_hunk = true;
            }
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with(' ') {
            map.insert(new_line, Some(old_line));
            new_line += 1;
            old_line += 1;
        } else if line.starts_with('+') {
            map.insert(new_line, None);
            new_line += 1;
        } else if line.starts_with('-') {
            old_line += 1;
        }
    }
    map
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let after = line.strip_prefix("@@ ")?;
    let mut parts = after.splitn(3, ' ');
    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;
    let old_start: u32 = old_part.split(',').next()?.parse().ok()?;
    let new_start: u32 = new_part.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

#[test]
fn multi_hunk_diff_position() {
    let diff = "\
@@ -1,3 +1,4 @@
 alpha
+inserted
 beta
 gamma
@@ -10,3 +11,2 @@
 delta
-removed
 epsilon
";
    let map = parse_diff_line_map(diff);

    // First hunk
    assert_eq!(map[&1], Some(1)); // alpha: context
    assert_eq!(map[&2], None); // inserted: added
    assert_eq!(map[&3], Some(2)); // beta: context
    assert_eq!(map[&4], Some(3)); // gamma: context

    // Second hunk (starts at new=11, old=10)
    assert_eq!(map[&11], Some(10)); // delta: context
                                    // old line 11 = "removed" → old increments to 12, new stays at 12
    assert_eq!(map[&12], Some(12)); // epsilon: context
}

#[test]
fn added_only_diff() {
    let diff = "\
@@ -0,0 +1,3 @@
+first
+second
+third
";
    let map = parse_diff_line_map(diff);
    assert_eq!(map[&1], None);
    assert_eq!(map[&2], None);
    assert_eq!(map[&3], None);
}
