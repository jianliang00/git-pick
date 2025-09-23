use std::fs;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::{Component, Path, PathBuf};

use git2::{
    DiffOptions, ErrorCode, Oid, Repository, Signature, StatusOptions, build::CheckoutBuilder,
};
use tempfile::TempDir;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("failed to open source repository at {path}")]
    SourceOpen {
        path: PathBuf,
        #[source]
        source: git2::Error,
    },
    #[error("failed to open destination repository at {path}")]
    DestOpen {
        path: PathBuf,
        #[source]
        source: git2::Error,
    },
    #[error("failed to parse commit id '{input}'")]
    InvalidCommitId {
        input: String,
        #[source]
        source: git2::Error,
    },
    #[error("commit {commit} not found in source repository")]
    CommitLookup {
        commit: String,
        #[source]
        source: git2::Error,
    },
    #[error("destination repository is bare")]
    BareDestination,
    #[error("destination repository has uncommitted changes")]
    DirtyDestination,
    #[error("merge commits are not supported for {commit}")]
    MergeCommit { commit: String },
    #[error("path mapping must be '<source>=<dest>' but got '{mapping}'")]
    InvalidMapping { mapping: String },
    #[error("invalid relative path '{path}'")]
    InvalidRelativePath { path: String },
    #[error("io error at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git lfs object {oid} referenced by {path} not found")]
    MissingLfsObject { oid: String, path: PathBuf },
    #[error(transparent)]
    Git(#[from] git2::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathMapping {
    source: PathBuf,
    dest: PathBuf,
    source_components: usize,
}

impl PathMapping {
    pub fn new(source: PathBuf, dest: PathBuf) -> Result<Self, SyncError> {
        let normalized_source = normalize_relative_path(&source)?;
        let normalized_dest = normalize_relative_path(&dest)?;
        let source_components = normalized_source.components().count();
        Ok(Self {
            source: normalized_source,
            dest: normalized_dest,
            source_components,
        })
    }

    pub fn parse(mapping: &str) -> Result<Self, SyncError> {
        let (source, dest) = mapping
            .split_once('=')
            .ok_or_else(|| SyncError::InvalidMapping {
                mapping: mapping.to_string(),
            })?;
        PathMapping::new(PathBuf::from(source), PathBuf::from(dest))
    }

    fn apply(&self, path: &Path) -> Option<PathBuf> {
        if self.source.as_os_str().is_empty() {
            let mut mapped = PathBuf::new();
            if !self.dest.as_os_str().is_empty() {
                mapped.push(&self.dest);
            }
            if !path.as_os_str().is_empty() {
                mapped.push(path);
            }
            return Some(mapped);
        }
        if !(path == self.source || path.starts_with(&self.source)) {
            return None;
        }
        let mut mapped = PathBuf::new();
        if !self.dest.as_os_str().is_empty() {
            mapped.push(&self.dest);
        }
        if let Ok(remainder) = path.strip_prefix(&self.source)
            && !remainder.as_os_str().is_empty()
        {
            mapped.push(remainder);
        }
        Some(mapped)
    }
}

impl std::str::FromStr for PathMapping {
    type Err = SyncError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PathMapping::parse(s)
    }
}

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub source_repo: PathBuf,
    pub dest_repo: PathBuf,
    pub commit: Oid,
    pub mappings: Vec<PathMapping>,
    pub skip: Vec<PathBuf>,
}

impl SyncOptions {
    pub fn new(
        source_repo: PathBuf,
        dest_repo: PathBuf,
        commit: Oid,
        mappings: Vec<PathMapping>,
        skip: Vec<PathBuf>,
    ) -> Result<Self, SyncError> {
        let normalized_skip = skip
            .into_iter()
            .map(|p| normalize_relative_path(&p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            source_repo,
            dest_repo,
            commit,
            mappings,
            skip: normalized_skip,
        })
    }
}

enum FileOp {
    Write {
        source: PathBuf,
        dest_relative: PathBuf,
        filemode: u32,
    },
    Delete {
        dest_relative: PathBuf,
    },
}

pub fn sync_commit(options: SyncOptions) -> Result<Oid, SyncError> {
    let source_repo =
        Repository::open(&options.source_repo).map_err(|source| SyncError::SourceOpen {
            path: options.source_repo.clone(),
            source,
        })?;
    let dest_repo = Repository::open(&options.dest_repo).map_err(|source| SyncError::DestOpen {
        path: options.dest_repo.clone(),
        source,
    })?;

    if dest_repo.is_bare() {
        return Err(SyncError::BareDestination);
    }

    ensure_destination_clean(&dest_repo)?;

    let commit =
        source_repo
            .find_commit(options.commit)
            .map_err(|source| SyncError::CommitLookup {
                commit: options.commit.to_string(),
                source,
            })?;

    if commit.parent_count() > 1 {
        return Err(SyncError::MergeCommit {
            commit: options.commit.to_string(),
        });
    }

    let commit_tree = commit.tree()?;
    let parent_tree = if commit.parent_count() == 1 {
        commit.parent(0)?.tree()?
    } else {
        let empty_tree_id = source_repo.treebuilder(None)?.write()?;
        source_repo.find_tree(empty_tree_id)?
    };

    let temp_dir = TempDir::new().map_err(|source| SyncError::Io {
        path: options.source_repo.clone(),
        source,
    })?;
    checkout_to_temp(&source_repo, &commit, temp_dir.path())?;

    let mut diff_options = DiffOptions::new();
    diff_options.include_typechange(true);
    diff_options.include_typechange_trees(true);
    let mut diff = source_repo.diff_tree_to_tree(
        Some(&parent_tree),
        Some(&commit_tree),
        Some(&mut diff_options),
    )?;
    diff.find_similar(None)?;

    let operations = collect_operations(&diff, temp_dir.path(), &options)?;
    apply_operations(&source_repo, &dest_repo, &operations)?;

    let mut index = dest_repo.index()?;
    let tree_id = index.write_tree()?;
    let tree = dest_repo.find_tree(tree_id)?;

    let author = commit.author();
    let committer = commit.committer();

    let parent_commit = match dest_repo.head() {
        Ok(head) => Some(head.peel_to_commit()?),
        Err(err) if err.code() == ErrorCode::UnbornBranch => None,
        Err(err) => return Err(err.into()),
    };
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();

    let author_sig = Signature::new(
        author.name().unwrap_or(""),
        author.email().unwrap_or(""),
        &author.when(),
    )?;
    let committer_sig = Signature::new(
        committer.name().unwrap_or(""),
        committer.email().unwrap_or(""),
        &committer.when(),
    )?;

    let oid = dest_repo.commit(
        Some("HEAD"),
        &author_sig,
        &committer_sig,
        commit.message().unwrap_or(""),
        &tree,
        &parents,
    )?;

    Ok(oid)
}

fn ensure_destination_clean(repo: &Repository) -> Result<(), SyncError> {
    let mut status_opts = StatusOptions::new();
    status_opts.include_untracked(true);
    let statuses = repo.statuses(Some(&mut status_opts))?;
    let dirty = statuses.iter().any(|entry| {
        let status = entry.status();
        status.intersects(
            git2::Status::INDEX_NEW
                | git2::Status::INDEX_MODIFIED
                | git2::Status::INDEX_DELETED
                | git2::Status::WT_NEW
                | git2::Status::WT_MODIFIED
                | git2::Status::WT_DELETED,
        )
    });
    if dirty {
        Err(SyncError::DirtyDestination)
    } else {
        Ok(())
    }
}

fn checkout_to_temp(
    repo: &Repository,
    commit: &git2::Commit,
    temp_path: &Path,
) -> Result<(), SyncError> {
    fs::create_dir_all(temp_path).map_err(|source| SyncError::Io {
        path: temp_path.to_path_buf(),
        source,
    })?;
    let tree = commit.tree()?;
    let mut checkout = CheckoutBuilder::new();
    checkout.force();
    checkout.target_dir(temp_path);
    repo.checkout_tree(tree.as_object(), Some(&mut checkout))?;
    Ok(())
}

fn collect_operations(
    diff: &git2::Diff,
    temp_root: &Path,
    options: &SyncOptions,
) -> Result<Vec<FileOp>, SyncError> {
    let mut operations = Vec::new();
    for delta in diff.deltas() {
        match delta.status() {
            git2::Delta::Deleted => {
                if let Some(old_path) = delta.old_file().path() {
                    let rel = normalize_relative_path(old_path)?;
                    if should_skip(&rel, &options.skip) {
                        continue;
                    }
                    let dest_rel = map_destination(&rel, &options.mappings);
                    operations.push(FileOp::Delete {
                        dest_relative: dest_rel,
                    });
                }
            }
            git2::Delta::Renamed => {
                let mut new_rel_opt = None;
                if let Some(new_path) = delta.new_file().path() {
                    let rel = normalize_relative_path(new_path)?;
                    if should_skip(&rel, &options.skip) {
                        continue;
                    }
                    new_rel_opt = Some(rel);
                }
                if let Some(new_rel) = new_rel_opt {
                    if let Some(old_path) = delta.old_file().path() {
                        let rel = normalize_relative_path(old_path)?;
                        if !should_skip(&rel, &options.skip) {
                            let dest_rel = map_destination(&rel, &options.mappings);
                            operations.push(FileOp::Delete {
                                dest_relative: dest_rel,
                            });
                        }
                    }
                    let dest_rel = map_destination(&new_rel, &options.mappings);
                    let source_path = temp_root.join(&new_rel);
                    operations.push(FileOp::Write {
                        source: source_path,
                        dest_relative: dest_rel,
                        filemode: delta.new_file().mode().into(),
                    });
                }
            }
            git2::Delta::Added
            | git2::Delta::Modified
            | git2::Delta::Copied
            | git2::Delta::Typechange => {
                if let Some(new_path) = delta.new_file().path() {
                    let rel = normalize_relative_path(new_path)?;
                    if should_skip(&rel, &options.skip) {
                        continue;
                    }
                    let dest_rel = map_destination(&rel, &options.mappings);
                    let source_path = temp_root.join(&rel);
                    operations.push(FileOp::Write {
                        source: source_path,
                        dest_relative: dest_rel,
                        filemode: delta.new_file().mode().into(),
                    });
                }
            }
            git2::Delta::Unmodified
            | git2::Delta::Ignored
            | git2::Delta::Unreadable
            | git2::Delta::Untracked => {}
            git2::Delta::Conflicted => {
                return Err(SyncError::Git(git2::Error::from_str(
                    "conflicted delta is not supported",
                )));
            }
        }
    }
    Ok(operations)
}

fn map_destination(path: &Path, mappings: &[PathMapping]) -> PathBuf {
    let mut best: Option<(PathBuf, usize)> = None;
    for mapping in mappings {
        if let Some(mapped) = mapping.apply(path) {
            let len = mapping.source_components;
            let replace = match best {
                Some((_, current_len)) => len >= current_len,
                None => true,
            };
            if replace {
                best = Some((mapped, len));
            }
        }
    }
    best.map(|(mapped, _)| mapped)
        .unwrap_or_else(|| path.to_path_buf())
}

fn should_skip(path: &Path, skip: &[PathBuf]) -> bool {
    skip.iter().any(|prefix| {
        if prefix.as_os_str().is_empty() {
            true
        } else {
            path == prefix || path.starts_with(prefix)
        }
    })
}

fn normalize_relative_path(path: &Path) -> Result<PathBuf, SyncError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(SyncError::InvalidRelativePath {
                    path: path.to_string_lossy().into_owned(),
                });
            }
        }
    }
    Ok(normalized)
}

fn apply_operations(
    source_repo: &Repository,
    dest_repo: &Repository,
    operations: &[FileOp],
) -> Result<(), SyncError> {
    let workdir = dest_repo
        .workdir()
        .ok_or(SyncError::BareDestination)?
        .to_path_buf();
    let lfs_store = source_repo.path().join("lfs").join("objects");
    let lfs_store = if lfs_store.exists() {
        Some(lfs_store)
    } else {
        None
    };
    let mut index = dest_repo.index()?;
    for operation in operations {
        match operation {
            FileOp::Write {
                source,
                dest_relative,
                filemode,
            } => {
                let dest_path = workdir.join(dest_relative);
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent).map_err(|source| SyncError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
                copy_entry(source, &dest_path, *filemode, lfs_store.as_deref())?;
                index.add_path(dest_relative)?;
            }
            FileOp::Delete { dest_relative } => {
                let dest_path = workdir.join(dest_relative);
                if let Ok(metadata) = fs::symlink_metadata(&dest_path) {
                    if metadata.file_type().is_dir() {
                        fs::remove_dir_all(&dest_path).map_err(|source| SyncError::Io {
                            path: dest_path.clone(),
                            source,
                        })?;
                    } else {
                        fs::remove_file(&dest_path).map_err(|source| SyncError::Io {
                            path: dest_path.clone(),
                            source,
                        })?;
                    }
                }
                index.remove_path(dest_relative)?;
            }
        }
    }
    index.write()?;
    Ok(())
}

fn copy_entry(
    source: &Path,
    dest: &Path,
    filemode: u32,
    lfs_store: Option<&Path>,
) -> Result<(), SyncError> {
    let metadata = fs::symlink_metadata(source).map_err(|source_err| SyncError::Io {
        path: source.to_path_buf(),
        source: source_err,
    })?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source).map_err(|source_err| SyncError::Io {
            path: source.to_path_buf(),
            source: source_err,
        })?;
        if let Ok(existing) = fs::symlink_metadata(dest) {
            if existing.file_type().is_dir() {
                fs::remove_dir_all(dest).map_err(|source_err| SyncError::Io {
                    path: dest.to_path_buf(),
                    source: source_err,
                })?;
            } else {
                fs::remove_file(dest).map_err(|source_err| SyncError::Io {
                    path: dest.to_path_buf(),
                    source: source_err,
                })?;
            }
        }
        create_symlink(&target, dest).map_err(|source_err| SyncError::Io {
            path: dest.to_path_buf(),
            source: source_err,
        })?;
    } else {
        let resolved = resolve_lfs_pointer(source, lfs_store)?;
        let copy_source = resolved.as_deref().unwrap_or(source);
        fs::copy(copy_source, dest).map_err(|source_err| SyncError::Io {
            path: dest.to_path_buf(),
            source: source_err,
        })?;
        set_executable_if_needed(dest, filemode)?;
    }
    Ok(())
}

fn resolve_lfs_pointer(
    source: &Path,
    lfs_store: Option<&Path>,
) -> Result<Option<PathBuf>, SyncError> {
    let Some(lfs_root) = lfs_store else {
        return Ok(None);
    };
    let file = match fs::File::open(source) {
        Ok(file) => file,
        Err(err) => {
            return Err(SyncError::Io {
                path: source.to_path_buf(),
                source: err,
            });
        }
    };
    let mut reader = BufReader::new(file);
    let mut first_line = String::new();
    match reader.read_line(&mut first_line) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::InvalidData => return Ok(None),
        Err(err) => {
            return Err(SyncError::Io {
                path: source.to_path_buf(),
                source: err,
            });
        }
    }
    if first_line.trim_end() != "version https://git-lfs.github.com/spec/v1" {
        return Ok(None);
    }

    let mut oid: Option<String> = None;
    for _ in 0..8 {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::InvalidData => return Ok(None),
            Err(err) => {
                return Err(SyncError::Io {
                    path: source.to_path_buf(),
                    source: err,
                });
            }
        }
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("oid ") {
            if let Some(hash) = value.strip_prefix("sha256:") {
                let hash = hash.trim();
                if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                    oid = Some(hash.to_owned());
                }
            }
            break;
        }
    }

    let Some(hash) = oid else {
        return Ok(None);
    };
    if hash.len() < 4 {
        return Ok(None);
    }
    let object_path = lfs_root.join(&hash[0..2]).join(&hash[2..4]).join(&hash);
    if !object_path.exists() {
        return Err(SyncError::MissingLfsObject {
            oid: hash,
            path: source.to_path_buf(),
        });
    }
    Ok(Some(object_path))
}

fn set_executable_if_needed(dest: &Path, filemode: u32) -> Result<(), SyncError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = filemode & 0o111 != 0;
        if let Ok(metadata) = fs::metadata(dest) {
            let mut permissions = metadata.permissions();
            let mut mode = permissions.mode();
            let current_exec = mode & 0o111 != 0;
            if executable != current_exec {
                mode = if executable {
                    mode | 0o111
                } else {
                    mode & !0o111
                };
                permissions.set_mode(mode);
                fs::set_permissions(dest, permissions).map_err(|source_err| SyncError::Io {
                    path: dest.to_path_buf(),
                    source: source_err,
                })?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (dest, filemode);
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    if fs::metadata(target).map(|m| m.is_dir()).unwrap_or(false) {
        symlink_dir(target, link)
    } else {
        symlink_file(target, link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{RepositoryInitOptions, Signature, Time};
    use tempfile::tempdir;

    fn init_repo(path: &Path) -> Repository {
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("main");
        Repository::init_opts(path, &opts).unwrap()
    }

    fn test_signature(name: &str, time: i64) -> Signature<'static> {
        Signature::new(
            name,
            &format!("{}@example.com", name.to_lowercase()),
            &Time::new(time, 60),
        )
        .unwrap()
    }

    fn write_and_stage(repo: &Repository, path: &Path, content: &str) {
        let full_path = repo.workdir().unwrap().join(path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full_path, content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(path).unwrap();
        index.write().unwrap();
    }

    fn commit(repo: &Repository, message: &str, sig: &Signature) -> Oid {
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
        repo.commit(Some("HEAD"), sig, sig, message, &tree, &parent_refs)
            .unwrap()
    }

    #[test]
    fn mapping_parse_and_apply() {
        let mapping = PathMapping::parse("aaa/bbb=ccc").unwrap();
        let mapped = mapping.apply(Path::new("aaa/bbb/file.txt")).unwrap();
        assert_eq!(mapped, PathBuf::from("ccc/file.txt"));
        assert!(mapping.apply(Path::new("other")).is_none());
    }

    #[test]
    fn sync_basic_commit() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());

        let sig = test_signature("Alice", 1_234_567_890);
        write_and_stage(&source_repo, Path::new("file.txt"), "hello");
        let oid = commit(&source_repo, "initial", &sig);

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![],
            vec![],
        )
        .unwrap();

        let new_oid = sync_commit(options).unwrap();
        let dest_commit = dest_repo.find_commit(new_oid).unwrap();
        assert_eq!(dest_commit.author().name().unwrap(), "Alice");
        assert_eq!(dest_commit.author().when().seconds(), 1_234_567_890);
        let contents = fs::read_to_string(dest_dir.path().join("file.txt")).unwrap();
        assert_eq!(contents, "hello");
    }

    #[test]
    fn sync_with_mapping() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());

        let sig = test_signature("Bob", 1_111_111_111);
        write_and_stage(&source_repo, Path::new("aaa/bbb/file.txt"), "value");
        let oid = commit(&source_repo, "mapped", &sig);

        let mapping = PathMapping::parse("aaa=dest").unwrap();
        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![mapping],
            vec![],
        )
        .unwrap();

        sync_commit(options).unwrap();
        let dest_contents = fs::read_to_string(dest_dir.path().join("dest/bbb/file.txt")).unwrap();
        assert_eq!(dest_contents, "value");
    }

    #[test]
    fn sync_with_skip_directory() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());

        let sig = test_signature("Carol", 1_400_000_000);
        write_and_stage(&source_repo, Path::new("include.txt"), "keep");
        write_and_stage(&source_repo, Path::new("skip/me.txt"), "skip");
        let oid = commit(&source_repo, "add", &sig);

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![],
            vec![PathBuf::from("skip")],
        )
        .unwrap();

        sync_commit(options).unwrap();
        assert!(dest_dir.path().join("include.txt").exists());
        assert!(!dest_dir.path().join("skip/me.txt").exists());
    }

    #[test]
    fn sync_handles_deletions() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());
        let sig = test_signature("Dan", 1_500_000_000);

        write_and_stage(&source_repo, Path::new("file.txt"), "one");
        let first = commit(&source_repo, "add", &sig);
        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                first,
                vec![],
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(dest_dir.path().join("file.txt").exists());

        let mut index = source_repo.index().unwrap();
        index.remove_path(Path::new("file.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = source_repo.find_tree(tree_id).unwrap();
        let parent = source_repo.find_commit(first).unwrap();
        let delete_oid = source_repo
            .commit(Some("HEAD"), &sig, &sig, "delete", &tree, &[&parent])
            .unwrap();

        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                delete_oid,
                vec![],
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!dest_dir.path().join("file.txt").exists());
        let dest_commit = dest_repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(dest_commit.summary().unwrap(), "delete");
    }

    #[test]
    fn sync_handles_renames() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());
        let sig = test_signature("Renamer", 1_650_000_000);

        write_and_stage(&source_repo, Path::new("old.txt"), "data");
        let base = commit(&source_repo, "base", &sig);
        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                base,
                vec![],
                vec![],
            )
            .unwrap(),
        )
        .unwrap();

        let old_path = source_repo.workdir().unwrap().join("old.txt");
        let new_path = source_repo.workdir().unwrap().join("renamed.txt");
        std::fs::rename(&old_path, &new_path).unwrap();
        let mut index = source_repo.index().unwrap();
        index.remove_path(Path::new("old.txt")).unwrap();
        index.add_path(Path::new("renamed.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = source_repo.find_tree(tree_id).unwrap();
        let parent = source_repo.find_commit(base).unwrap();
        let rename_commit = source_repo
            .commit(Some("HEAD"), &sig, &sig, "rename", &tree, &[&parent])
            .unwrap();

        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                rename_commit,
                vec![],
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!dest_dir.path().join("old.txt").exists());
        assert_eq!(
            fs::read_to_string(dest_dir.path().join("renamed.txt")).unwrap(),
            "data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sync_copies_symlinks() {
        use std::os::unix::fs as unix_fs;

        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());
        let sig = test_signature("Link", 1_660_000_000);

        let target_file = source_repo.workdir().unwrap().join("target.txt");
        fs::write(&target_file, "content").unwrap();
        unix_fs::symlink(
            "target.txt",
            source_repo.workdir().unwrap().join("link.txt"),
        )
        .unwrap();
        let mut index = source_repo.index().unwrap();
        index.add_path(Path::new("target.txt")).unwrap();
        index.add_path(Path::new("link.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = source_repo.find_tree(tree_id).unwrap();
        let commit = source_repo
            .commit(Some("HEAD"), &sig, &sig, "symlink", &tree, &[])
            .unwrap();

        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                commit,
                vec![],
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
        let dest_link = dest_dir.path().join("link.txt");
        let metadata = fs::symlink_metadata(&dest_link).unwrap();
        assert!(metadata.file_type().is_symlink());
        assert_eq!(
            fs::read_link(dest_link).unwrap(),
            PathBuf::from("target.txt")
        );
    }

    #[test]
    fn sync_copies_lfs_objects() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());
        let sig = test_signature("Lfs", 1_680_000_000);

        let actual_contents = b"actual file contents\n";
        let sha = "d396cf496e4d0318a52888cbb121fa38dee59224a60240a56320bac87f5fde8e";
        let pointer = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            sha,
            actual_contents.len()
        );

        write_and_stage(&source_repo, Path::new("large.bin"), &pointer);

        let lfs_object_path = source_repo
            .path()
            .join("lfs")
            .join("objects")
            .join(&sha[0..2])
            .join(&sha[2..4])
            .join(sha);
        fs::create_dir_all(lfs_object_path.parent().unwrap()).unwrap();
        fs::write(&lfs_object_path, actual_contents).unwrap();

        let lfs_commit = commit(&source_repo, "lfs", &sig);

        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                lfs_commit,
                vec![],
                vec![],
            )
            .unwrap(),
        )
        .unwrap();

        let dest_file = dest_dir.path().join("large.bin");
        assert_eq!(fs::read(&dest_file).unwrap(), actual_contents);

        let dest_commit = dest_repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(dest_commit.summary().unwrap(), "lfs");
    }

    #[test]
    fn sync_skips_everything_with_empty_prefix() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());
        let sig = test_signature("SkipAll", 1_670_000_000);

        write_and_stage(&source_repo, Path::new("file.txt"), "value");
        let base = commit(&source_repo, "base", &sig);
        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                base,
                vec![],
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
        write_and_stage(&source_repo, Path::new("file.txt"), "new value");
        let followup = commit(&source_repo, "update", &sig);

        let head_before_tree = dest_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree_id();
        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                followup,
                vec![],
                vec![PathBuf::new()],
            )
            .unwrap(),
        )
        .unwrap();
        let head_after_tree = dest_repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree_id();
        assert_eq!(head_before_tree, head_after_tree);
        assert_eq!(
            fs::read_to_string(dest_dir.path().join("file.txt")).unwrap(),
            "value"
        );
    }

    #[test]
    fn bare_destination_is_rejected() {
        let source_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let mut dest_opts = RepositoryInitOptions::new();
        dest_opts.bare(true);
        let dest_dir = tempdir().unwrap();
        Repository::init_opts(dest_dir.path(), &dest_opts).unwrap();
        let sig = test_signature("Bare", 1_680_000_000);
        write_and_stage(&source_repo, Path::new("file.txt"), "data");
        let commit = commit(&source_repo, "initial", &sig);

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            commit,
            vec![],
            vec![],
        )
        .unwrap();
        let err = sync_commit(options).unwrap_err();
        assert!(matches!(err, SyncError::BareDestination));
    }

    #[test]
    fn invalid_skip_path_is_rejected() {
        let oid = Oid::zero();
        let err = SyncOptions::new(
            PathBuf::from("a"),
            PathBuf::from("b"),
            oid,
            vec![],
            vec![PathBuf::from("../bad")],
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::InvalidRelativePath { .. }));
    }

    #[test]
    fn mapping_parse_errors() {
        let err = PathMapping::parse("invalid").unwrap_err();
        assert!(matches!(err, SyncError::InvalidMapping { .. }));
        let err = PathMapping::parse("../bad=dest").unwrap_err();
        assert!(matches!(err, SyncError::InvalidRelativePath { .. }));
    }

    #[test]
    fn missing_source_repository_returns_error() {
        let missing_repo = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        init_repo(dest_dir.path());
        let options = SyncOptions::new(
            missing_repo.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            Oid::zero(),
            vec![],
            vec![],
        )
        .unwrap();
        let err = sync_commit(options).unwrap_err();
        assert!(matches!(err, SyncError::SourceOpen { .. }));
    }

    #[test]
    fn missing_destination_repository_returns_error() {
        let source_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let sig = test_signature("DestMissing", 1_690_000_000);
        write_and_stage(&source_repo, Path::new("file.txt"), "value");
        let commit = commit(&source_repo, "initial", &sig);
        let missing_dest = tempdir().unwrap();

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            missing_dest.path().to_path_buf(),
            commit,
            vec![],
            vec![],
        )
        .unwrap();
        let err = sync_commit(options).unwrap_err();
        assert!(matches!(err, SyncError::DestOpen { .. }));
    }

    #[test]
    fn missing_commit_returns_error() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        init_repo(source_dir.path());
        init_repo(dest_dir.path());
        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            Oid::zero(),
            vec![],
            vec![],
        )
        .unwrap();
        let err = sync_commit(options).unwrap_err();
        assert!(matches!(err, SyncError::CommitLookup { .. }));
    }

    #[test]
    fn mapping_to_subdirectory() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());
        let sig = test_signature("Eve", 1_600_000_000);

        write_and_stage(&source_repo, Path::new("nested/path/file.txt"), "data");
        let oid = commit(&source_repo, "nested", &sig);

        let mapping = PathMapping::parse(".=mirror").unwrap();
        sync_commit(
            SyncOptions::new(
                source_dir.path().to_path_buf(),
                dest_dir.path().to_path_buf(),
                oid,
                vec![mapping],
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
        let mirrored = dest_dir.path().join("mirror/nested/path/file.txt");
        assert_eq!(fs::read_to_string(mirrored).unwrap(), "data");
    }

    #[test]
    fn merge_commit_not_supported() {
        let dir = tempdir().unwrap();
        let repo = init_repo(dir.path());
        let sig = test_signature("Merge", 1_700_000_000);

        write_and_stage(&repo, Path::new("base.txt"), "base");
        let base = commit(&repo, "base", &sig);

        // create two branches by committing twice from same parent
        write_and_stage(&repo, Path::new("left.txt"), "left");
        let left = commit(&repo, "left", &sig);
        repo.set_head_detached(base).unwrap();
        write_and_stage(&repo, Path::new("right.txt"), "right");
        let right = commit(&repo, "right", &sig);
        let left_commit = repo.find_commit(left).unwrap();
        let right_commit = repo.find_commit(right).unwrap();
        let mut index = repo.index().unwrap();
        index.read_tree(&left_commit.tree().unwrap()).unwrap();
        index.read_tree(&right_commit.tree().unwrap()).unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.set_head_detached(left).unwrap();
        let merge_oid = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "merge",
                &tree,
                &[&left_commit, &right_commit],
            )
            .unwrap();

        let dest_dir = tempdir().unwrap();
        init_repo(dest_dir.path());
        let options = SyncOptions::new(
            dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            merge_oid,
            vec![],
            vec![],
        )
        .unwrap();
        let err = sync_commit(options).unwrap_err();
        match err {
            SyncError::MergeCommit { .. } => {}
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn destination_dirty_is_rejected() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());
        let sig = test_signature("Dirty", 1_800_000_000);

        write_and_stage(&source_repo, Path::new("file.txt"), "content");
        let oid = commit(&source_repo, "commit", &sig);

        let dest_file = dest_repo.workdir().unwrap().join("dirty.txt");
        fs::write(dest_file, "dirty").unwrap();

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![],
            vec![],
        )
        .unwrap();
        let err = sync_commit(options).unwrap_err();
        match err {
            SyncError::DirtyDestination => {}
            other => panic!("unexpected error {other:?}"),
        }
    }
}
