use std::process;

use clap::{Parser, ValueEnum};
use git2::Oid;

use git_pick::{PathMapping, SyncError, SyncMode, SyncOptions, sync_commit};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Synchronize a commit between repositories using file copies."
)]
struct Args {
    /// Source repository containing the commit to copy
    #[arg(long, value_name = "PATH")]
    source: std::path::PathBuf,

    /// Destination repository where the commit will be reproduced
    #[arg(long, value_name = "PATH", alias = "destination")]
    dest: std::path::PathBuf,

    /// Commit hash to replicate from the source repository
    #[arg(long, value_name = "OID")]
    commit: String,

    /// Map paths from the source repository to different destinations (format: src=dest)
    #[arg(long = "map", value_name = "SRC=DEST")]
    mappings: Vec<PathMapping>,

    /// Skip specified paths from being synchronized
    #[arg(long, value_name = "PATH")]
    skip: Vec<std::path::PathBuf>,

    /// Override the author name of the reproduced commit
    #[arg(long, value_name = "NAME")]
    author_name: Option<String>,

    /// Override the author email of the reproduced commit
    #[arg(long, value_name = "EMAIL")]
    author_email: Option<String>,

    /// Override the committer name of the reproduced commit
    #[arg(long, value_name = "NAME")]
    committer_name: Option<String>,

    /// Override the committer email of the reproduced commit
    #[arg(long, value_name = "EMAIL")]
    committer_email: Option<String>,

    /// Synchronization mode used to apply changes to the destination
    #[arg(long, value_name = "MODE", value_enum, default_value_t = Mode::Patch)]
    mode: Mode,

    /// Allow already-synchronized commits without treating them as errors
    #[arg(long)]
    allow_empty: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Mode {
    Patch,
    Copy,
}

#[cfg(not(test))]
fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        process::exit(1);
    }
}

#[cfg(not(test))]
fn run() -> Result<(), SyncError> {
    let args = Args::parse();
    run_with_args(args)
}

fn run_with_args(args: Args) -> Result<(), SyncError> {
    let options = build_options(args)?;
    sync_commit(options).map(|_| ())
}

fn build_options(args: Args) -> Result<SyncOptions, SyncError> {
    let Args {
        source,
        dest,
        commit,
        mappings,
        skip,
        author_name,
        author_email,
        committer_name,
        committer_email,
        mode,
        allow_empty,
    } = args;

    let oid = Oid::from_str(&commit).map_err(|source| SyncError::InvalidCommitId {
        input: commit,
        source,
    })?;

    let mut options = SyncOptions::new(source, dest, oid, mappings, skip)?;
    options.author_name = author_name;
    options.author_email = author_email;
    options.committer_name = committer_name;
    options.committer_email = committer_email;
    options.mode = match mode {
        Mode::Patch => SyncMode::Patch,
        Mode::Copy => SyncMode::Copy,
    };
    options.allow_empty = allow_empty;

    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{RepositoryInitOptions, Signature, Time};
    use tempfile::tempdir;

    fn parse_args<I, T>(iter: I) -> Args
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Args::try_parse_from(iter).expect("failed to parse arguments")
    }

    #[test]
    fn build_options_defaults_to_patch_mode() {
        let args = parse_args([
            "git-pick",
            "--source",
            "src",
            "--dest",
            "dest",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
        ]);
        let options = build_options(args).unwrap();
        assert_eq!(options.mode, SyncMode::Patch);
        assert!(options.author_name.is_none());
        assert!(options.committer_email.is_none());
        assert!(!options.allow_empty);
    }

    #[test]
    fn build_options_sets_copy_mode_and_overrides() {
        let args = parse_args([
            "git-pick",
            "--source",
            "src",
            "--dest",
            "dest",
            "--commit",
            "89abcdef0123456789abcdef0123456789abcdef",
            "--mode",
            "copy",
            "--author-name",
            "Author",
            "--author-email",
            "author@example.com",
            "--committer-name",
            "Committer",
            "--committer-email",
            "committer@example.com",
            "--allow-empty",
        ]);
        let options = build_options(args).unwrap();
        assert_eq!(options.mode, SyncMode::Copy);
        assert_eq!(options.author_name.as_deref(), Some("Author"));
        assert_eq!(options.author_email.as_deref(), Some("author@example.com"));
        assert_eq!(options.committer_name.as_deref(), Some("Committer"));
        assert_eq!(
            options.committer_email.as_deref(),
            Some("committer@example.com")
        );
        assert!(options.allow_empty);
    }

    #[test]
    fn build_options_rejects_invalid_commit() {
        let args = parse_args([
            "git-pick",
            "--source",
            "src",
            "--dest",
            "dest",
            "--commit",
            "not-a-hash",
        ]);
        let err = build_options(args).unwrap_err();
        assert!(matches!(err, SyncError::InvalidCommitId { .. }));
    }

    fn init_repo(path: &std::path::Path) -> git2::Repository {
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("main");
        git2::Repository::init_opts(path, &opts).unwrap()
    }

    fn test_signature(name: &str, time: i64) -> Signature<'static> {
        Signature::new(
            name,
            &format!("{}@example.com", name.to_lowercase()),
            &Time::new(time, 60),
        )
        .unwrap()
    }

    fn write_and_stage(repo: &git2::Repository, path: &std::path::Path, content: &str) {
        let full_path = repo.workdir().unwrap().join(path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full_path, content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
    }

    fn commit(repo: &git2::Repository, message: &str, sig: &Signature) -> Oid {
        let mut index = repo.index().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parents = match repo.head() {
            Ok(head) => vec![head.peel_to_commit().unwrap()],
            Err(_) => Vec::new(),
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        repo.commit(Some("HEAD"), sig, sig, message, &tree, &parent_refs)
            .unwrap()
    }

    #[test]
    fn run_with_args_executes_sync() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());

        let sig = test_signature("Dana", 1_700_000_000);
        write_and_stage(&source_repo, std::path::Path::new("file.txt"), "hello");
        let oid = commit(&source_repo, "initial", &sig);

        let args = parse_args([
            "git-pick",
            "--source",
            source_dir.path().to_str().unwrap(),
            "--dest",
            dest_dir.path().to_str().unwrap(),
            "--commit",
            &oid.to_string(),
        ]);

        run_with_args(args).unwrap();
        let contents = std::fs::read_to_string(dest_dir.path().join("file.txt")).unwrap();
        assert_eq!(contents, "hello");
    }
}
