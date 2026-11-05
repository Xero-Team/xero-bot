use super::*;

fn command(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Bridge Test")
        .env("GIT_AUTHOR_EMAIL", "bridge@example.invalid")
        .env("GIT_COMMITTER_NAME", "Bridge Test")
        .env("GIT_COMMITTER_EMAIL", "bridge@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn repo() -> Scratch {
    let dir = Scratch::new().unwrap();
    command(&dir.0, &["init", "--bare", "--quiet"]);
    dir
}
fn commit(dir: &Path, branch: &str, parent: Option<&str>, message: &str) -> String {
    let tree = command(dir, &["mktree"]);
    let mut args = vec!["commit-tree", &tree, "-m", message];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    let sha = command(dir, &args);
    command(dir, &["update-ref", &format!("refs/heads/{branch}"), &sha]);
    sha
}
fn remote(dir: &Path) -> Remote {
    Remote::new(dir.to_string_lossy().into(), "test", "test-secret")
}

#[tokio::test]
async fn real_git_fast_forwards_and_preserves_destination_only_refs() {
    let source = repo();
    let dest = repo();
    let a = commit(&source.0, "main", None, "initial");
    command(&source.0, &["update-ref", "refs/tags/v1", &a]);
    transfer(&remote(&source.0), &remote(&dest.0), None)
        .await
        .unwrap();
    command(&dest.0, &["update-ref", "refs/pull/7/head", &a]);
    command(&dest.0, &["update-ref", "refs/heads/forgejo-only", &a]);
    let b = commit(&source.0, "main", Some(&a), "next");
    transfer(&remote(&source.0), &remote(&dest.0), None)
        .await
        .unwrap();
    assert_eq!(command(&dest.0, &["rev-parse", "main"]), b);
    assert_eq!(command(&dest.0, &["rev-parse", "refs/pull/7/head"]), a);
    assert_eq!(command(&dest.0, &["rev-parse", "forgejo-only"]), a);
    assert_eq!(command(&dest.0, &["rev-parse", "refs/tags/v1"]), a);
    // A new Forgejo branch can be imported without altering GitHub main.
    let c = commit(&dest.0, "forgejo-pr", Some(&b), "forgejo change");
    transfer(&remote(&dest.0), &remote(&source.0), Some("forgejo-pr"))
        .await
        .unwrap();
    assert_eq!(command(&source.0, &["rev-parse", "forgejo-pr"]), c);
    assert_eq!(command(&source.0, &["rev-parse", "main"]), b);
    transfer(&remote(&dest.0), &remote(&source.0), Some("forgejo-pr"))
        .await
        .unwrap();
}

#[tokio::test]
async fn real_git_rejects_divergence_moved_tags_and_missing_pr_heads() {
    let source = repo();
    let dest = repo();
    let a = commit(&source.0, "main", None, "initial");
    command(&source.0, &["update-ref", "refs/tags/v1", &a]);
    transfer(&remote(&source.0), &remote(&dest.0), None)
        .await
        .unwrap();
    let b = commit(&source.0, "main", Some(&a), "github change");
    let c = commit(&dest.0, "main", Some(&a), "forgejo change");
    command(&source.0, &["update-ref", "refs/tags/v1", &b]);
    let error = transfer(&remote(&source.0), &remote(&dest.0), None)
        .await
        .unwrap_err();
    assert!(error.contains("refs/heads/main") && error.contains("refs/tags/v1"));
    assert_eq!(command(&dest.0, &["rev-parse", "main"]), c);
    assert_eq!(command(&dest.0, &["rev-parse", "refs/tags/v1"]), a);
    assert!(
        transfer(&remote(&source.0), &remote(&dest.0), Some("missing"))
            .await
            .is_err()
    );
    assert!(
        transfer(&remote(&source.0), &remote(&dest.0), Some("../pull/1/head"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn destination_ahead_is_preserved_but_not_accepted_as_exact_pr_head() {
    let source = repo();
    let dest = repo();
    let a = commit(&source.0, "topic", None, "initial");
    transfer(&remote(&source.0), &remote(&dest.0), None)
        .await
        .unwrap();
    let b = commit(&dest.0, "topic", Some(&a), "ahead");
    transfer(&remote(&source.0), &remote(&dest.0), None)
        .await
        .unwrap();
    assert!(
        transfer(&remote(&source.0), &remote(&dest.0), Some("topic"))
            .await
            .is_err()
    );
    command(&source.0, &["update-ref", "-d", "refs/heads/topic"]);
    transfer(&remote(&source.0), &remote(&dest.0), None)
        .await
        .unwrap();
    assert_eq!(command(&dest.0, &["rev-parse", "topic"]), b);
}
