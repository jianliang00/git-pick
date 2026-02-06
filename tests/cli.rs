use std::fs;
use std::path::Path;

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
    let second = commit(&source_repo, "second");
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
