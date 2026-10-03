//! Matching PR file paths against repository path-trigger rules.
//!
//! The configuration deliberately supports a small, predictable glob language:
//! `*` and `?` match within one path segment, while a segment containing only
//! `**` matches zero or more complete directory segments.  The matcher works on
//! repository-relative, slash-separated paths and never reads file contents.

use serde_json::Value;

use crate::config::repository::{PathRule, Problem, ReasonCode};

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

fn path_matches(pattern: &str, path: &str) -> bool {
    let patterns: Vec<_> = pattern.split('/').collect();
    let paths: Vec<_> = path.split('/').collect();
    fn recurse(patterns: &[&str], paths: &[&str]) -> bool {
        if patterns.is_empty() {
            return paths.is_empty();
        }
        if patterns[0] == "**" {
            return recurse(&patterns[1..], paths)
                || (!paths.is_empty() && recurse(patterns, &paths[1..]));
        }
        !paths.is_empty()
            && glob::Pattern::new(patterns[0]).is_ok_and(|glob| glob.matches(paths[0]))
            && recurse(&patterns[1..], &paths[1..])
    }
    recurse(&patterns, &paths)
}

fn valid_repo_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "..")
}

fn rule_matches_path(rule: &PathRule, path: &str) -> bool {
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
        for rule in rules {
            if paths.iter().any(|path| rule_matches_path(rule, path)) {
                if let Some((_, labels)) = matched.iter_mut().find(|(id, _)| id == &rule.id) {
                    for label in &rule.labels {
                        if !labels.iter().any(|current| current == label) {
                            labels.push(label.clone());
                        }
                    }
                } else {
                    matched.push((rule.id.clone(), rule.labels.clone()));
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
        assert!(path_matches("src/**/*.rs", "src/lib.rs"));
        assert!(path_matches("src/**/*.rs", "src/a/b.rs"));
        assert!(!path_matches("src/*.rs", "src/a/b.rs"));
        assert!(!path_matches("src/main.rs", "src/MAIN.rs"));
        assert!(path_matches("文档/**", "文档/说明.md"));
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
