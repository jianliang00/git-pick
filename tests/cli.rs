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
fn cli_syncs_commit_chain_with_all_flag() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    let source_repo = init_repo(source_dir.path());
    let dest_repo = init_repo(dest_dir.path());

    // Create a chain of commits in the source repo
    write_and_stage(&source_repo, Path::new("file1.txt"), "content1");
    let _commit1 = commit(&source_repo, "commit 1");

    write_and_stage(&source_repo, Path::new("file2.txt"), "content2");
    let _commit2 = commit(&source_repo, "commit 2");

    write_and_stage(&source_repo, Path::new("file3.txt"), "content3");
    let commit3 = commit(&source_repo, "commit 3");

    // Use --all flag to sync all three commits
    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &commit3,
            "--all",
        ])
        .assert()
        .success();

    // Verify all three files were synced
    assert!(dest_dir.path().join("file1.txt").exists());
    assert!(dest_dir.path().join("file2.txt").exists());
    assert!(dest_dir.path().join("file3.txt").exists());

    // Verify we have 3 commits in destination
    let head = dest_repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.summary().unwrap(), "commit 3");
    let parent1 = head.parent(0).unwrap();
    assert_eq!(parent1.summary().unwrap(), "commit 2");
    let parent2 = parent1.parent(0).unwrap();
    assert_eq!(parent2.summary().unwrap(), "commit 1");
}

#[test]
fn cli_all_flag_stops_at_empty_diff() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    let source_repo = init_repo(source_dir.path());
    let dest_repo = init_repo(dest_dir.path());

    // Create initial commit in source
    write_and_stage(&source_repo, Path::new("file1.txt"), "content1");
    let commit1 = commit(&source_repo, "base commit");

    // Sync the first commit manually to destination using copy mode
    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &commit1,
            "--mode",
            "copy",
        ])
        .assert()
        .success();

    // Add more commits in source
    write_and_stage(&source_repo, Path::new("file2.txt"), "content2");
    let _commit2 = commit(&source_repo, "new commit 1");

    write_and_stage(&source_repo, Path::new("file3.txt"), "content3");
    let commit3 = commit(&source_repo, "new commit 2");

    // Use --all flag - should sync only the new commits
    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &commit3,
            "--all",
            "--mode",
            "copy",
        ])
        .assert()
        .success();

    // Verify the new files were synced
    assert!(dest_dir.path().join("file2.txt").exists());
    assert!(dest_dir.path().join("file3.txt").exists());

    // Verify we have 3 commits total in destination
    let head = dest_repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.summary().unwrap(), "new commit 2");
}

#[test]
fn cli_all_flag_with_skip_produces_empty_diff() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    let source_repo = init_repo(source_dir.path());
    init_repo(dest_dir.path());

    // Create commits that will be skipped
    write_and_stage(&source_repo, Path::new("skip/file1.txt"), "content1");
    let _commit1 = commit(&source_repo, "skipped commit");

    // Create a non-skipped commit
    write_and_stage(&source_repo, Path::new("keep/file2.txt"), "content2");
    let commit2 = commit(&source_repo, "kept commit");

    // Use --all with --skip flag
    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &commit2,
            "--all",
            "--skip",
            "skip",
        ])
        .assert()
        .success();

    // Verify only the non-skipped file was synced
    assert!(!dest_dir.path().join("skip/file1.txt").exists());
    assert!(dest_dir.path().join("keep/file2.txt").exists());
}

#[test]
fn cli_all_flag_when_specified_commit_is_empty() {
    let source_dir = tempdir().unwrap();
    let dest_dir = tempdir().unwrap();
    let source_repo = init_repo(source_dir.path());
    init_repo(dest_dir.path());

    // Create a commit with only skipped files
    write_and_stage(&source_repo, Path::new("skip/file.txt"), "content");
    let commit1 = commit(&source_repo, "skipped");

    // Use --all flag with skip - the commit produces empty diff
    Command::cargo_bin("git-pick")
        .unwrap()
        .args([
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &commit1,
            "--all",
            "--skip",
            "skip",
        ])
        .assert()
        .success();

    // Verify no files were synced
    assert!(!dest_dir.path().join("skip/file.txt").exists());
}
