//! AI code review — builtin engine (faithful port of review.py) plus the
//! shared publication pipeline used by every engine.
//!
//! Flow: fetch diff → parse added lines → build prompt → call AI → parse
//! verdict → post review (with inline comments, degrading gracefully).

// ---------------------------------------------------------------------------
// Diff parsing — per-file added line numbers (RIGHT side)
// ---------------------------------------------------------------------------

/// For each file, collect line numbers on the new side that were added.
/// These are the only lines GitHub lets inline review comments attach to.
pub fn parse_added_lines(
    diff: &str,
) -> std::collections::HashMap<String, std::collections::HashSet<i64>> {
    use regex::Regex;
    use std::collections::{HashMap, HashSet};

    let mut added: HashMap<String, HashSet<i64>> = HashMap::new();
    let file_re = Regex::new(r"^\+\+\+ b/(.+)$").unwrap();
    let hunk_re = Regex::new(r"\+(\d+)(?:,(\d+))?").unwrap();

    let mut current_file: Option<String> = None;
    let mut new_line: i64 = 0;

    for line in diff.lines() {
        if let Some(m) = file_re.captures(line) {
            let name = m.get(1).unwrap().as_str().to_string();
            added.entry(name.clone()).or_default();
            current_file = Some(name);
            continue;
        }
        if line.starts_with("+++ ") {
            current_file = None;
            continue;
        }
        if line.starts_with("@@") {
            if let Some(mm) = hunk_re.captures(line) {
                new_line = mm.get(1).and_then(|d| d.as_str().parse().ok()).unwrap_or(0) - 1;
            }
            continue;
        }
        let Some(file) = &current_file else {
            continue;
        };
        if line.starts_with('+') {
            new_line += 1;
            added.get_mut(file).unwrap().insert(new_line);
        } else if line.starts_with('-') {
            // removed line: new-side numbering unchanged
        } else {
            new_line += 1;
        }
    }
    added
}

pub fn truncate(diff: &str, max_chars: usize) -> (String, bool) {
    if diff.len() <= max_chars {
        return (diff.to_string(), false);
    }
    (diff.chars().take(max_chars).collect(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,6 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
+    let x = 1;
+    let y = 2;
 }
@@ -10,3 +12,4 @@
 fn other() {
+    helper();
 }
";

    #[test]
    fn test_parse_added_lines() {
        let added = parse_added_lines(SAMPLE_DIFF);
        let set = added.get("src/main.rs").unwrap();
        // lines 2,3,4 added in first hunk (start=1, +3 lines → 2,3,4);
        // hunk 2 starts at 12: line 13 added
        assert!(set.contains(&2) && set.contains(&3) && set.contains(&4));
        assert!(set.contains(&13));
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn test_truncate() {
        let (d, t) = truncate("hello", 10);
        assert_eq!(d, "hello");
        assert!(!t);
        let (d, t) = truncate("hello world", 5);
        assert_eq!(d, "hello");
        assert!(t);
    }
}
