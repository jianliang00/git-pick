use std::process;

use clap::Parser;
use git2::Oid;

use git_pick::{PathMapping, SyncError, SyncOptions, sync_commit};

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
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        process::exit(1);
    }
}

fn run() -> Result<(), SyncError> {
    let args = Args::parse();
    let oid = Oid::from_str(&args.commit).map_err(|source| SyncError::InvalidCommitId {
        input: args.commit.clone(),
        source,
    })?;

    let mut options = SyncOptions::new(args.source, args.dest, oid, args.mappings, args.skip)?;
    options.author_name = args.author_name;
    options.author_email = args.author_email;
    options.committer_name = args.committer_name;
    options.committer_email = args.committer_email;

    sync_commit(options).map(|_| ())
}
