use std::fs;
use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use git2::{Repository, RepositoryInitOptions, Signature, Time};
use tempfile::tempdir;

fn init_repo(path: &Path) -> Repository {
    let mut opts = RepositoryInitOptions::new();
    opts.initial_head("main");
    Repository::init_opts(path, &opts).unwrap()
}

fn signature() -> Signature<'static> {
    Signature::new(
        "Tester",
        "tester@example.com",
        &Time::new(1_900_000_000, 120),
    )
    .unwrap()
}

fn write_and_stage(repo: &Repository, path: &Path, content: &str) {
    let full = repo.workdir().unwrap().join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, content).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(path).unwrap();
    index.write().unwrap();
}

fn run_git(repo: &Path, args: &[&str]) -> String {
    let output = StdCommand::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git command failed: git -C {} {}\nstdout: {}\nstderr: {}",
        repo.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(repo: &Repository, message: &str) -> String {
    let sig = signature();
    let mut index = repo.index().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let parents = match repo.head() {
        Ok(head) => head
            .peel_to_commit()
            .ok()
            .map(|c| vec![c])
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .unwrap()
        .to_string()
}

#[test]
fn cli_syncs_commit() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    let source_repo = init_repo(source_dir.path());
    init_repo(dest_dir.path());

    write_and_stage(&source_repo, Path::new("a/b.txt"), "data");
    let oid = commit(&source_repo, "cli");

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &oid,
            "--map",
            "a=dest",
        ])
        .assert()
        .success();

    let dest_repo = Repository::open(dest_dir.path()).unwrap();
    let head = dest_repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.summary().unwrap(), "cli");
    let file = dest_dir.path().join("dest/b.txt");
    assert_eq!(fs::read_to_string(file).unwrap(), "data");
}

#[test]
fn cli_syncs_commit_copy_mode() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    let source_repo = init_repo(source_dir.path());
    init_repo(dest_dir.path());

    write_and_stage(&source_repo, Path::new("a/b.txt"), "data");
    let oid = commit(&source_repo, "cli-copy");

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &oid,
            "--mode",
            "copy",
        ])
        .assert()
        .success();

    let dest_repo = Repository::open(dest_dir.path()).unwrap();
    let head = dest_repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.summary().unwrap(), "cli-copy");
    let file = dest_dir.path().join("a/b.txt");
    assert_eq!(fs::read_to_string(file).unwrap(), "data");
}

#[test]
fn cli_overrides_signatures() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    let source_repo = init_repo(source_dir.path());
    init_repo(dest_dir.path());

    write_and_stage(&source_repo, Path::new("file.txt"), "content");
    let oid = commit(&source_repo, "override");

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &oid,
            "--author-name",
            "New Author",
            "--author-email",
            "author@example.com",
            "--committer-name",
            "New Committer",
            "--committer-email",
            "committer@example.com",
        ])
        .assert()
        .success();

    let dest_repo = Repository::open(dest_dir.path()).unwrap();
    let head = dest_repo.head().unwrap().peel_to_commit().unwrap();
    let author = head.author();
    assert_eq!(author.name(), Some("New Author"));
    assert_eq!(author.email(), Some("author@example.com"));
    let committer = head.committer();
    assert_eq!(committer.name(), Some("New Committer"));
    assert_eq!(committer.email(), Some("committer@example.com"));
}

#[test]
fn cli_rejects_invalid_commit() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    init_repo(source_dir.path());
    init_repo(dest_dir.path());

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            "not-a-hash",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("failed to parse commit"));
}

#[test]
fn cli_syncs_unpicked_ancestors_with_all_flag() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    let source_repo = init_repo(source_dir.path());
    init_repo(dest_dir.path());

    write_and_stage(&source_repo, Path::new("file.txt"), "one\n");
    let first = commit(&source_repo, "first");
    write_and_stage(&source_repo, Path::new("file.txt"), "two\n");
    commit(&source_repo, "second");
    write_and_stage(&source_repo, Path::new("file.txt"), "three\n");
    let third = commit(&source_repo, "third");

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &first,
        ])
        .assert()
        .success();

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &third,
            "--all",
        ])
        .assert()
        .success();

    let dest_repo = Repository::open(dest_dir.path()).unwrap();
    let head = dest_repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.summary(), Some("third"));
    assert_eq!(
        fs::read_to_string(dest_dir.path().join("file.txt")).unwrap(),
        "three\n"
    );

    let parent = head.parent(0).unwrap();
    assert_eq!(parent.summary(), Some("second"));
}

#[test]
fn cli_all_stops_at_already_synced_merge_boundary() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    init_repo(source_dir.path());
    init_repo(dest_dir.path());

    run_git(source_dir.path(), &["config", "user.name", "Tester"]);
    run_git(
        source_dir.path(),
        &["config", "user.email", "tester@example.com"],
    );
    run_git(dest_dir.path(), &["config", "user.name", "Tester"]);
    run_git(
        dest_dir.path(),
        &["config", "user.email", "tester@example.com"],
    );

    fs::write(source_dir.path().join("main.txt"), "base\n").unwrap();
    run_git(source_dir.path(), &["add", "."]);
    run_git(source_dir.path(), &["commit", "-m", "base"]);

    run_git(source_dir.path(), &["checkout", "-b", "feature"]);
    fs::write(source_dir.path().join("feature.txt"), "feature\n").unwrap();
    run_git(source_dir.path(), &["add", "."]);
    run_git(source_dir.path(), &["commit", "-m", "feature"]);

    run_git(source_dir.path(), &["checkout", "main"]);
    fs::write(source_dir.path().join("main.txt"), "main2\n").unwrap();
    run_git(source_dir.path(), &["add", "."]);
    run_git(source_dir.path(), &["commit", "-m", "main2"]);
    run_git(
        source_dir.path(),
        &["merge", "--no-ff", "feature", "-m", "merge feature"],
    );

    fs::write(source_dir.path().join("post.txt"), "post\n").unwrap();
    run_git(source_dir.path(), &["add", "."]);
    run_git(source_dir.path(), &["commit", "-m", "post"]);

    let post_oid = run_git(source_dir.path(), &["rev-parse", "HEAD"]);
    let _merge_oid = run_git(source_dir.path(), &["rev-parse", "HEAD~1"]);
    let main2_oid = run_git(source_dir.path(), &["rev-parse", "HEAD~2"]);
    let base_oid = run_git(source_dir.path(), &["rev-parse", "HEAD~3"]);

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &base_oid,
        ])
        .assert()
        .success();

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &main2_oid,
        ])
        .assert()
        .success();

    fs::write(dest_dir.path().join("feature.txt"), "feature\n").unwrap();
    run_git(dest_dir.path(), &["add", "."]);
    run_git(dest_dir.path(), &["commit", "-m", "seed merge boundary"]);

    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &post_oid,
            "--all",
        ])
        .assert()
        .success();

    let dest_repo = Repository::open(dest_dir.path()).unwrap();
    let head = dest_repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.summary(), Some("post"));
    assert_eq!(
        head.parent(0).unwrap().summary(),
        Some("seed merge boundary")
    );
    assert_eq!(
        fs::read_to_string(dest_dir.path().join("post.txt")).unwrap(),
        "post\n"
    );
}
