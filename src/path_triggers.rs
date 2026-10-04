//! Matching PR file paths against repository path-trigger rules.
//!
//! The configuration deliberately supports a small, predictable glob language:
//! `*` and `?` match within one path segment, while a segment containing only
//! `**` matches zero or more complete directory segments.  The matcher works on
//! repository-relative, slash-separated paths and never reads file contents.

use serde_json::Value;

use crate::config::repository::{PathRule, Problem, ReasonCode};

enum GlobSegment {
    AnyDepth,
    Pattern(Vec<GlobPiece>),
}

/// Fixed-width glob fragments separated by `*`. Keeping stars outside the
/// library avoids its recursive backtracking while retaining Unicode `?`.
struct GlobPiece {
    pattern: glob::Pattern,
    width: usize,
}

struct CompiledGlob {
    segments: Vec<GlobSegment>,
}

struct CompiledRule<'a> {
    rule: &'a PathRule,
    include: Vec<CompiledGlob>,
    exclude: Vec<CompiledGlob>,
}

/// Validate the path-glob subset accepted by the repository configuration.
pub(crate) fn valid_glob(pattern: &str) -> bool {
    if pattern.is_empty()
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.contains('\0')
        || pattern
            .split('/')
            .any(|segment| matches!(segment, "." | ".." | ""))
        || pattern
            .chars()
            .any(|ch| matches!(ch, '!' | '{' | '}' | '[' | ']'))
    {
        return false;
    }
    let segments: Vec<_> = pattern.split('/').collect();
    segments
        .iter()
        .all(|segment| *segment == "**" || !segment.contains("**"))
}

/// Compile each non-recursive segment once and collapse adjacent `**` segments.
fn compile_glob(pattern: &str) -> Result<CompiledGlob, Problem> {
    if !valid_glob(pattern) {
        return Err(Problem::new(
            ReasonCode::InvalidRule,
            "invalid path glob pattern",
        ));
    }
    let mut segments = Vec::new();
    for segment in pattern.split('/') {
        if segment == "**" {
            if !matches!(segments.last(), Some(GlobSegment::AnyDepth)) {
                segments.push(GlobSegment::AnyDepth);
            }
        } else {
            let pieces = segment
                .split('*')
                .map(|piece| {
                    Ok(GlobPiece {
                        pattern: glob::Pattern::new(piece).map_err(|_| {
                            Problem::new(ReasonCode::InvalidRule, "invalid path glob pattern")
                        })?,
                        width: piece.chars().count(),
                    })
                })
                .collect::<Result<_, Problem>>()?;
            segments.push(GlobSegment::Pattern(pieces));
        }
    }
    Ok(CompiledGlob { segments })
}

/// Match segment stars in polynomial time, at Unicode character boundaries.
fn segment_matches(pieces: &[GlobPiece], path: &str) -> bool {
    let offsets: Vec<_> = path
        .char_indices()
        .map(|(i, _)| i)
        .chain([path.len()])
        .collect();
    let count = offsets.len() - 1;
    let mut next = vec![false; count + 1];
    next[count] = true;
    for (index, piece) in pieces.iter().enumerate().rev() {
        let mut row = vec![false; count + 1];
        for start in (0..=count).rev() {
            let end = start + piece.width;
            row[start] = end <= count
                && next[end]
                && piece.pattern.matches(&path[offsets[start]..offsets[end]]);
            if index > 0 && start < count {
                row[start] |= row[start + 1];
            }
        }
        next = row;
    }
    next[0]
}

/// Match repository path segments with a rolling dynamic-programming row.
fn path_matches(pattern: &CompiledGlob, paths: &[&str]) -> bool {
    let path_count = paths.len();
    let mut next = vec![false; path_count + 1];
    next[path_count] = true;
    for segment in pattern.segments.iter().rev() {
        let mut row = vec![false; path_count + 1];
        for path_index in (0..=path_count).rev() {
            row[path_index] = match segment {
                GlobSegment::AnyDepth => {
                    next[path_index] || (path_index < path_count && row[path_index + 1])
                }
                GlobSegment::Pattern(segment) => {
                    path_index < path_count
                        && next[path_index + 1]
                        && segment_matches(segment, paths[path_index])
                }
            };
        }
        next = row;
    }
    next[0]
}

/// Reject paths that are not safe repository-relative slash-separated names.
fn valid_repo_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains('\0')
        && !path
            .split('/')
            .any(|segment| matches!(segment, "." | ".." | ""))
}

/// Apply include and exclude patterns to one repository-relative path.
fn rule_matches_path(rule: &CompiledRule<'_>, path: &[&str]) -> bool {
    rule.include
        .iter()
        .any(|pattern| path_matches(pattern, path))
        && !rule
            .exclude
            .iter()
            .any(|pattern| path_matches(pattern, path))
}

/// A rule match from the same validated complete diff used for path labels.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PathMatch {
    pub id: String,
    pub labels: Vec<String>,
    /// Bounded examples for notification display; never another file-list fetch.
    pub paths: Vec<String>,
}

pub fn matched_rules(
    rules: &[PathRule],
    files: &[Value],
) -> Result<Vec<(String, Vec<String>)>, Problem> {
    Ok(matched_rules_with_evidence(rules, files)?
        .into_iter()
        .map(|m| (m.id, m.labels))
        .collect())
}

/// Return the rule IDs and labels matched by a complete GitHub PR file list.
/// A renamed file contributes both its old and new path; a missing old path on
/// a rename is rejected because treating it as a normal modification would
/// silently skip a configured match.
pub fn matched_rules_with_evidence(
    rules: &[PathRule],
    files: &[Value],
) -> Result<Vec<PathMatch>, Problem> {
    let compiled: Vec<_> = rules
        .iter()
        .map(|rule| {
            Ok(CompiledRule {
                rule,
                include: rule
                    .include
                    .iter()
                    .map(|pattern| compile_glob(pattern))
                    .collect::<Result<_, _>>()?,
                exclude: rule
                    .exclude
                    .iter()
                    .map(|pattern| compile_glob(pattern))
                    .collect::<Result<_, _>>()?,
            })
        })
        .collect::<Result<_, Problem>>()?;
    let mut matched = std::collections::BTreeMap::<String, PathMatch>::new();
    for file in files {
        let filename = file["filename"].as_str().ok_or_else(|| {
            Problem::new(
                ReasonCode::InvalidResponse,
                "PR file list has a missing filename",
            )
        })?;
        if !valid_repo_path(filename) {
            return Err(Problem::new(
                ReasonCode::InvalidResponse,
                "PR file list contains an invalid repository path",
            ));
        }
        let mut paths = vec![filename];
        if file["status"].as_str() == Some("renamed") {
            let previous = file["previous_filename"].as_str().ok_or_else(|| {
                Problem::new(
                    ReasonCode::InvalidResponse,
                    "renamed PR file is missing previous_filename",
                )
            })?;
            if !valid_repo_path(previous) {
                return Err(Problem::new(
                    ReasonCode::InvalidResponse,
                    "renamed PR file contains an invalid previous path",
                ));
            }
            paths.push(previous);
        }
        for rule in &compiled {
            for path in &paths {
                if rule_matches_path(rule, &path.split('/').collect::<Vec<_>>()) {
                    let entry = matched
                        .entry(rule.rule.id.clone())
                        .or_insert_with(|| PathMatch {
                            id: rule.rule.id.clone(),
                            labels: rule.rule.labels.clone(),
                            paths: Vec::new(),
                        });
                    if !entry.paths.iter().any(|p| p == path) {
                        entry.paths.push((*path).to_string());
                        entry.paths.sort();
                        entry.paths.truncate(3);
                    }
                }
            }
        }
    }
    let matched = matched
        .into_values()
        .map(|mut entry| {
            entry.labels.sort();
            entry.labels.dedup();
            entry
        })
        .collect();
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(include: &[&str], exclude: &[&str]) -> PathRule {
        PathRule {
            id: "r".into(),
            include: include.iter().map(|s| (*s).into()).collect(),
            exclude: exclude.iter().map(|s| (*s).into()).collect(),
            labels: vec!["area/rust".into()],
            cc: vec![],
        }
    }

    #[test]
    fn glob_subset_matches_root_nested_and_case_sensitively() {
        let matches = |pattern: &str, path: &str| {
            path_matches(
                &compile_glob(pattern).unwrap(),
                &path.split('/').collect::<Vec<_>>(),
            )
        };
        assert!(matches("src/**/*.rs", "src/lib.rs"));
        assert!(matches("src/**/*.rs", "src/a/b.rs"));
        assert!(!matches("src/*.rs", "src/a/b.rs"));
        assert!(!matches("src/main.rs", "src/MAIN.rs"));
        assert!(matches("文档/**", "文档/说明.md"));
    }

    #[test]
    fn excludes_apply_per_path_and_renames_use_both_names() {
        let rules = vec![
            rule(&["src/**/*.rs"], &["src/generated/**"]),
            rule(&["docs/**"], &[]),
        ];
        let files = vec![json!({"filename":"src/generated/x.rs","status":"modified"})];
        assert!(matched_rules(&rules, &files).unwrap().is_empty());
        let files = vec![
            json!({"filename":"src/new.rs","previous_filename":"docs/old.md","status":"renamed"}),
        ];
        let ids = matched_rules(&rules, &files).unwrap();
        assert_eq!(
            ids.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["r"]
        );
    }

    #[test]
    fn malformed_rename_is_rejected() {
        let err = matched_rules(
            &[rule(&["**"], &[])],
            &[json!({"filename":"x","status":"renamed"})],
        )
        .unwrap_err();
        assert_eq!(err.code, ReasonCode::InvalidResponse);
    }

    #[test]
    fn segment_dp_preserves_glob_semantics_for_small_unicode_patterns() {
        let mut patterns = vec![String::new()];
        let mut paths = vec![String::new()];
        for _ in 0..4 {
            patterns.extend(
                patterns
                    .clone()
                    .into_iter()
                    .flat_map(|s| ['a', '文', '?', '*'].map(|ch| format!("{s}{ch}"))),
            );
            paths.extend(
                paths
                    .clone()
                    .into_iter()
                    .flat_map(|s| ['a', '文', '.'].map(|ch| format!("{s}{ch}"))),
            );
        }
        patterns.sort();
        patterns.dedup();
        paths.sort();
        paths.dedup();
        for pattern in patterns.into_iter().filter(|p| valid_glob(p)) {
            let compiled = compile_glob(&pattern).unwrap();
            let original = glob::Pattern::new(&pattern).unwrap();
            for path in &paths {
                assert_eq!(
                    path_matches(&compiled, &[path]),
                    original.matches(path),
                    "{pattern:?} / {path:?}"
                );
            }
        }
    }

    #[test]
    fn repeated_directory_and_segment_stars_have_bounded_cost() {
        let pattern = format!("{}x", "**/a/".repeat(32));
        assert!(!path_matches(
            &compile_glob(&pattern).unwrap(),
            &vec!["a"; 128]
        ));
        let pattern = format!("{}b", "*a".repeat(32));
        assert!(!path_matches(
            &compile_glob(&pattern).unwrap(),
            &[&"a".repeat(128)]
        ));
        assert!(path_matches(
            &compile_glob("文?/*明?.rs").unwrap(),
            &["文档", "说明文.rs"]
        ));
    }

    #[test]
    fn invalid_new_or_previous_paths_fail_the_entire_match() {
        for invalid in [
            "",
            "/src/lib.rs",
            "src/../lib.rs",
            "src/./lib.rs",
            "src//lib.rs",
            "src\\lib.rs",
            "src/\0.rs",
        ] {
            for old in [false, true] {
                let file = if old {
                    json!({"filename":"src/lib.rs","status":"renamed","previous_filename":invalid})
                } else {
                    json!({"filename":invalid,"status":"removed"})
                };
                assert!(matched_rules(&[rule(&["**"], &[])], &[file]).is_err());
            }
        }
    }
}
