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
    Pattern(glob::Pattern),
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
        || pattern
            .split('/')
            .any(|segment| segment == ".." || segment.is_empty())
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
    let mut segments = Vec::new();
    for segment in pattern.split('/') {
        if segment == "**" {
            if !matches!(segments.last(), Some(GlobSegment::AnyDepth)) {
                segments.push(GlobSegment::AnyDepth);
            }
        } else {
            segments.push(GlobSegment::Pattern(glob::Pattern::new(segment).map_err(
                |_| Problem::new(ReasonCode::InvalidRule, "invalid path glob pattern"),
            )?));
        }
    }
    Ok(CompiledGlob { segments })
}

/// Match repository path segments with a bounded dynamic-programming table.
fn path_matches(pattern: &CompiledGlob, paths: &[&str]) -> bool {
    let pattern_count = pattern.segments.len();
    let path_count = paths.len();
    let mut dp = vec![vec![false; path_count + 1]; pattern_count + 1];
    dp[pattern_count][path_count] = true;
    for pattern_index in (0..pattern_count).rev() {
        for path_index in (0..=path_count).rev() {
            dp[pattern_index][path_index] = match &pattern.segments[pattern_index] {
                GlobSegment::AnyDepth => {
                    dp[pattern_index + 1][path_index]
                        || (path_index < path_count && dp[pattern_index][path_index + 1])
                }
                GlobSegment::Pattern(segment) => {
                    path_index < path_count
                        && segment.matches(paths[path_index])
                        && dp[pattern_index + 1][path_index + 1]
                }
            };
        }
    }
    dp[0][0]
}

/// Reject paths that are not safe repository-relative slash-separated names.
fn valid_repo_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "..")
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

/// Return the rule IDs and labels matched by a complete GitHub PR file list.
/// A renamed file contributes both its old and new path; a missing old path on
/// a rename is rejected because treating it as a normal modification would
/// silently skip a configured match.
pub fn matched_rules(
    rules: &[PathRule],
    files: &[Value],
) -> Result<Vec<(String, Vec<String>)>, Problem> {
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
    let mut matched: Vec<(String, Vec<String>)> = Vec::new();
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
        let path_segments: Vec<Vec<_>> =
            paths.iter().map(|path| path.split('/').collect()).collect();
        for rule in &compiled {
            if path_segments
                .iter()
                .any(|segments| rule_matches_path(rule, segments))
            {
                if let Some((_, labels)) = matched.iter_mut().find(|(id, _)| id == &rule.rule.id) {
                    for label in &rule.rule.labels {
                        if !labels
                            .iter()
                            .any(|current| current.eq_ignore_ascii_case(label))
                        {
                            labels.push(label.clone());
                        }
                    }
                } else {
                    matched.push((rule.rule.id.clone(), rule.rule.labels.clone()));
                }
            }
        }
    }
    matched.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, labels) in &mut matched {
        labels.sort();
        labels.dedup();
    }
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
}
