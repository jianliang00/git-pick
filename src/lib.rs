use std::fs;
use std::path::{Component, Path, PathBuf};

use git2::{
    DiffOptions, ErrorCode, Oid, Repository, Signature, StatusOptions, build::CheckoutBuilder,
};
use thiserror::Error;

mod fs_ops;

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
    #[error("commit {commit} has already been synchronized")]
    AlreadySynced { commit: String },
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
    pub author_name: Option<String>,
    pub author_email: Option<String>,
    pub committer_name: Option<String>,
    pub committer_email: Option<String>,
    pub mode: SyncMode,
    pub allow_empty: bool,
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
            author_name: None,
            author_email: None,
            committer_name: None,
            committer_email: None,
            mode: SyncMode::Patch,
            allow_empty: false,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    Patch,
    Copy,
}

#[derive(Clone, Debug)]
struct BaseEntry {
    oid: Oid,
    filemode: u32,
}

enum FileOp {
    Write {
        source: PathBuf,
        dest_relative: PathBuf,
        filemode: u32,
        base: Option<BaseEntry>,
    },
    Delete {
        dest_relative: PathBuf,
        base: BaseEntry,
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

    let temp_dir = fs_ops::create_temp_dir_for(&options.source_repo)?;
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
    let lfs_store = source_repo.path().join("lfs").join("objects");
    let lfs_store = if lfs_store.exists() {
        Some(lfs_store)
    } else {
        None
    };
    if operations_already_applied(&operations, &dest_repo, lfs_store.as_deref())? {
        if options.allow_empty {
            eprintln!(
                "warning: commit {} already synchronized in destination",
                options.commit
            );
            let head = dest_repo.head()?;
            if let Some(oid) = head.target() {
                return Ok(oid);
            }
            return Err(git2::Error::from_str("destination HEAD missing").into());
        }
        return Err(SyncError::AlreadySynced {
            commit: options.commit.to_string(),
        });
    }
    match options.mode {
        SyncMode::Copy => {
            apply_operations_copy(&source_repo, &dest_repo, &operations)?;
        }
        SyncMode::Patch => {
            apply_operations_patch(&source_repo, &dest_repo, &operations)?;
        }
    }

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

    let author_sig = override_signature(
        &author,
        options.author_name.as_deref(),
        options.author_email.as_deref(),
    )?;
    let committer_sig = override_signature(
        &committer,
        options.committer_name.as_deref(),
        options.committer_email.as_deref(),
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
    fs_ops::create_dir_all(temp_path)?;
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
                    if !should_skip(&rel, &options.skip) {
                        let dest_rel = map_destination(&rel, &options.mappings);
                        if let Some(base) = base_entry_from_delta(&delta) {
                            operations.push(FileOp::Delete {
                                dest_relative: dest_rel,
                                base,
                            });
                        }
                    }
                }
            }
            git2::Delta::Renamed => {
                let mut new_rel_opt = None;
                if let Some(new_path) = delta.new_file().path() {
                    let rel = normalize_relative_path(new_path)?;
                    if !should_skip(&rel, &options.skip) {
                        new_rel_opt = Some(rel);
                    }
                }
                if let Some(new_rel) = new_rel_opt {
                    if let Some(old_path) = delta.old_file().path() {
                        let rel = normalize_relative_path(old_path)?;
                        if !should_skip(&rel, &options.skip) {
                            let dest_rel = map_destination(&rel, &options.mappings);
                            if let Some(base) = base_entry_from_delta(&delta) {
                                operations.push(FileOp::Delete {
                                    dest_relative: dest_rel,
                                    base,
                                });
                            }
                        }
                    }
                    let dest_rel = map_destination(&new_rel, &options.mappings);
                    let source_path = temp_root.join(&new_rel);
                    operations.push(FileOp::Write {
                        source: source_path,
                        dest_relative: dest_rel,
                        filemode: delta.new_file().mode().into(),
                        base: None,
                    });
                }
            }
            git2::Delta::Added
            | git2::Delta::Modified
            | git2::Delta::Copied
            | git2::Delta::Typechange => {
                if let Some(new_path) = delta.new_file().path() {
                    let rel = normalize_relative_path(new_path)?;
                    if !should_skip(&rel, &options.skip) {
                        let dest_rel = map_destination(&rel, &options.mappings);
                        let source_path = temp_root.join(&rel);
                        operations.push(FileOp::Write {
                            source: source_path,
                            dest_relative: dest_rel,
                            filemode: delta.new_file().mode().into(),
                            base: base_entry_from_delta(&delta),
                        });
                    }
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

fn base_entry_from_delta(delta: &git2::DiffDelta) -> Option<BaseEntry> {
    let oid = delta.old_file().id();
    if oid.is_zero() {
        None
    } else {
        Some(BaseEntry {
            oid,
            filemode: delta.old_file().mode().into(),
        })
    }
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

fn override_signature(
    base: &Signature,
    name_override: Option<&str>,
    email_override: Option<&str>,
) -> Result<Signature<'static>, git2::Error> {
    let name = name_override.unwrap_or_else(|| base.name().unwrap_or(""));
    let email = email_override.unwrap_or_else(|| base.email().unwrap_or(""));
    Signature::new(name, email, &base.when())
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

fn operations_already_applied(
    operations: &[FileOp],
    dest_repo: &Repository,
    lfs_store: Option<&Path>,
) -> Result<bool, SyncError> {
    if operations.is_empty() {
        return Ok(false);
    }
    let workdir = dest_repo
        .workdir()
        .ok_or(SyncError::BareDestination)?
        .to_path_buf();
    for operation in operations {
        match operation {
            FileOp::Write {
                source,
                dest_relative,
                filemode,
                ..
            } => {
                let dest_path = workdir.join(dest_relative);
                let metadata = match fs::symlink_metadata(&dest_path) {
                    Ok(metadata) => metadata,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(err) => {
                        return Err(SyncError::Io {
                            path: dest_path.clone(),
                            source: err,
                        });
                    }
                };
                if metadata.file_type().is_dir() {
                    return Ok(false);
                }
                let expected_symlink = *filemode == 0o120000;
                if metadata.file_type().is_symlink() != expected_symlink {
                    return Ok(false);
                }
                #[cfg(unix)]
                if !expected_symlink {
                    use std::os::unix::fs::PermissionsExt;
                    let dest_mode = metadata.permissions().mode();
                    let dest_exec = dest_mode & 0o111 != 0;
                    let expected_exec = filemode & 0o111 != 0;
                    if dest_exec != expected_exec {
                        return Ok(false);
                    }
                }
                let expected_path = if *filemode == 0o120000 {
                    source.clone()
                } else {
                    match fs_ops::resolve_lfs_pointer(source, lfs_store)? {
                        Some(object) => object,
                        None => source.clone(),
                    }
                };
                let expected_bytes = fs_ops::read_entry_bytes(&expected_path, *filemode)?;
                let dest_bytes = fs_ops::read_entry_bytes(&dest_path, *filemode)?;
                if expected_bytes != dest_bytes {
                    return Ok(false);
                }
            }
            FileOp::Delete { dest_relative, .. } => {
                let dest_path = workdir.join(dest_relative);
                match fs::symlink_metadata(&dest_path) {
                    Ok(_) => return Ok(false),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(SyncError::Io {
                            path: dest_path,
                            source: err,
                        });
                    }
                }
            }
        }
    }
    Ok(true)
}

fn apply_operations_copy(
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
                ..
            } => {
                let dest_path = workdir.join(dest_relative);
                if let Some(parent) = dest_path.parent() {
                    fs_ops::create_dir_all(parent)?;
                }
                let lfs_object =
                    fs_ops::copy_entry(source, &dest_path, *filemode, lfs_store.as_deref())?;
                index.add_path(dest_relative)?;
                if let Some(object) = lfs_object {
                    fs_ops::materialize_lfs_object(&object, &dest_path, *filemode)?;
                }
            }
            FileOp::Delete { dest_relative, .. } => {
                let dest_path = workdir.join(dest_relative);
                if let Ok(metadata) = fs::symlink_metadata(&dest_path) {
                    if metadata.file_type().is_dir() {
                        fs_ops::remove_dir_all(&dest_path)?;
                    } else {
                        fs_ops::remove_file(&dest_path)?;
                    }
                }
                index.remove_path(dest_relative)?;
            }
        }
    }
    index.write()?;
    Ok(())
}

fn apply_operations_patch(
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

    let head_commit = match dest_repo.head() {
        Ok(head) => Some(head.peel_to_commit()?),
        Err(err) if err.code() == ErrorCode::UnbornBranch => None,
        Err(err) => return Err(err.into()),
    };
    let head_tree = if let Some(commit) = head_commit.as_ref() {
        Some(commit.tree()?)
    } else {
        None
    };

    let mut index = dest_repo.index()?;
    let mut lfs_materializations = Vec::new();

    for operation in operations {
        match operation {
            FileOp::Write {
                source,
                dest_relative,
                filemode,
                base,
            } => {
                if let Some(base_entry) = base {
                    let _ = base_entry.filemode;
                    if let Some(tree) = head_tree.as_ref() {
                        if let Ok(existing) = tree.get_path(dest_relative) {
                            if existing.id() != base_entry.oid {
                                return Err(git2::Error::from_str(
                                    "conflicted delta is not supported",
                                )
                                .into());
                            }
                        } else {
                            return Err(
                                git2::Error::from_str("conflicted delta is not supported").into()
                            );
                        }
                    } else {
                        return Err(
                            git2::Error::from_str("conflicted delta is not supported").into()
                        );
                    }
                } else if let Some(tree) = head_tree.as_ref()
                    && tree.get_path(dest_relative).is_ok()
                {
                    return Err(git2::Error::from_str("conflicted delta is not supported").into());
                }

                let dest_path = workdir.join(dest_relative);
                if let Some(parent) = dest_path.parent() {
                    fs_ops::create_dir_all(parent)?;
                }
                let lfs_object =
                    fs_ops::copy_entry(source, &dest_path, *filemode, lfs_store.as_deref())?;
                index.add_path(dest_relative)?;
                if let Some(object) = lfs_object {
                    lfs_materializations.push(LfsMaterialization {
                        dest_relative: dest_relative.clone(),
                        object,
                        filemode: *filemode,
                    });
                }
            }
            FileOp::Delete {
                dest_relative,
                base,
            } => {
                if let Some(tree) = head_tree.as_ref() {
                    if let Ok(existing) = tree.get_path(dest_relative) {
                        if existing.id() != base.oid {
                            return Err(
                                git2::Error::from_str("conflicted delta is not supported").into()
                            );
                        }
                    } else {
                        return Err(
                            git2::Error::from_str("conflicted delta is not supported").into()
                        );
                    }
                } else {
                    return Err(git2::Error::from_str("conflicted delta is not supported").into());
                }

                let dest_path = workdir.join(dest_relative);
                if let Ok(metadata) = std::fs::symlink_metadata(&dest_path) {
                    if metadata.file_type().is_dir() {
                        fs_ops::remove_dir_all(&dest_path)?;
                    } else {
                        fs_ops::remove_file(&dest_path)?;
                    }
                }
                index.remove_path(dest_relative)?;
            }
        }
    }

    index.write()?;

    for materialization in lfs_materializations {
        let dest_path = workdir.join(&materialization.dest_relative);
        fs_ops::materialize_lfs_object(
            &materialization.object,
            &dest_path,
            materialization.filemode,
        )?;
    }

    Ok(())
}

struct LfsMaterialization {
    dest_relative: PathBuf,
    object: PathBuf,
    filemode: u32,
}

#[cfg(test)]
fn index_flags_for_len(len: usize) -> u16 {
    let capped = len.min(0x0FFF);
    capped as u16
}

#[cfg(test)]
fn path_to_repo_bytes(path: &Path) -> Vec<u8> {
    if path.as_os_str().is_empty() {
        return Vec::new();
    }
    let components: Vec<String> = path
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    components.join("/").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::fs_ops::{
        copy_entry, create_dir_all, materialize_lfs_object, os_str_to_bytes, read_entry_for_patch,
        remove_dir_all, remove_file, resolve_lfs_pointer, set_executable_if_needed,
    };
    use super::*;
    use git2::{Oid, RepositoryInitOptions, Signature, Time};
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
    fn sync_binary_add_patch_mode() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());

        let sig = test_signature("BinAdd", 1_920_000_000);

        let rel = Path::new("bin.dat");
        let full = source_repo.workdir().unwrap().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        let bytes = vec![0u8, 1, 2, 255, 128, 64, 10, 20, 30];
        fs::write(&full, &bytes).unwrap();
        let mut index = source_repo.index().unwrap();
        index.add_path(rel).unwrap();
        index.write().unwrap();
        let oid = commit(&source_repo, "bin-add", &sig);

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![],
            vec![],
        )
        .unwrap();
        sync_commit(options).unwrap();

        let dest_bytes = fs::read(dest_dir.path().join(rel)).unwrap();
        assert_eq!(dest_bytes, bytes);
    }

    #[test]
    fn sync_binary_modify_patch_mode() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());

        let sig = test_signature("BinMod", 1_930_000_000);

        let rel = Path::new("bin2.dat");
        let full = source_repo.workdir().unwrap().join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        let v1 = vec![1u8, 3, 5, 7, 9, 11, 13];
        fs::write(&full, &v1).unwrap();
        let mut index = source_repo.index().unwrap();
        index.add_path(rel).unwrap();
        index.write().unwrap();
        let base = commit(&source_repo, "bin-base", &sig);

        let options_base = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            base,
            vec![],
            vec![],
        )
        .unwrap();
        sync_commit(options_base).unwrap();

        let v2 = vec![2u8, 4, 6, 8, 10, 12, 14, 0];
        fs::write(&full, &v2).unwrap();
        let mut index2 = source_repo.index().unwrap();
        index2.add_path(rel).unwrap();
        index2.write().unwrap();
        let update = commit(&source_repo, "bin-update", &sig);

        let options_update = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            update,
            vec![],
            vec![],
        )
        .unwrap();
        sync_commit(options_update).unwrap();

        let dest_bytes = fs::read(dest_dir.path().join(rel)).unwrap();
        assert_eq!(dest_bytes, v2);
    }
    #[test]
    fn mapping_parse_and_apply() {
        let mapping = PathMapping::parse("aaa/bbb=ccc").unwrap();
        let mapped = mapping.apply(Path::new("aaa/bbb/file.txt")).unwrap();
        assert_eq!(mapped, PathBuf::from("ccc/file.txt"));
        assert!(mapping.apply(Path::new("other")).is_none());
    }

    #[test]
    fn mapping_apply_with_empty_source_prefix() {
        let mapping = PathMapping::new(PathBuf::new(), PathBuf::from("dest")).unwrap();
        let mapped = mapping.apply(Path::new("file.txt")).unwrap();
        assert_eq!(mapped, PathBuf::from("dest/file.txt"));
    }

    #[test]
    fn mapping_parse_rejects_invalid_format() {
        let err = PathMapping::parse("invalid").unwrap_err();
        assert!(matches!(err, SyncError::InvalidMapping { .. }));
    }

    #[test]
    fn normalize_relative_path_rejects_parent_dir() {
        let err = normalize_relative_path(Path::new("../bad")).unwrap_err();
        assert!(matches!(err, SyncError::InvalidRelativePath { .. }));
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
    fn sync_basic_commit_copy_mode() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());

        let sig = test_signature("Alice", 1_234_567_890);
        write_and_stage(&source_repo, Path::new("file.txt"), "hello");
        let oid = commit(&source_repo, "initial", &sig);

        let mut options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![],
            vec![],
        )
        .unwrap();
        options.mode = SyncMode::Copy;

        let new_oid = sync_commit(options).unwrap();
        let dest_commit = dest_repo.find_commit(new_oid).unwrap();
        assert_eq!(dest_commit.author().name().unwrap(), "Alice");
        assert_eq!(dest_commit.author().when().seconds(), 1_234_567_890);
        let contents = fs::read_to_string(dest_dir.path().join("file.txt")).unwrap();
        assert_eq!(contents, "hello");
    }

    #[test]
    fn sync_rejects_already_synced_commit_by_default() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());

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
        sync_commit(options).unwrap();

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![],
            vec![],
        )
        .unwrap();
        let err = sync_commit(options).unwrap_err();
        assert!(matches!(err, SyncError::AlreadySynced { .. }));
    }

    #[test]
    fn sync_allows_already_synced_commit_with_flag() {
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
        sync_commit(options).unwrap();

        let mut options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![],
            vec![],
        )
        .unwrap();
        options.allow_empty = true;
        let new_oid = sync_commit(options).unwrap();
        let head_oid = dest_repo.head().unwrap().target().unwrap();
        assert_eq!(new_oid, head_oid);
    }

    #[test]
    fn operations_already_applied_returns_true_for_matching_entries() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let dest_repo = init_repo(dest_dir.path());

        let source_path = source_dir.path().join("file.txt");
        fs::write(&source_path, "same").unwrap();
        let dest_path = dest_dir.path().join("file.txt");
        fs::write(&dest_path, "same").unwrap();

        let operations = vec![
            FileOp::Write {
                source: source_path,
                dest_relative: PathBuf::from("file.txt"),
                filemode: 0o100644,
                base: None,
            },
            FileOp::Delete {
                dest_relative: PathBuf::from("gone.txt"),
                base: BaseEntry {
                    oid: Oid::zero(),
                    filemode: 0o100644,
                },
            },
        ];

        assert!(operations_already_applied(&operations, &dest_repo, None).unwrap());
    }

    #[test]
    fn operations_already_applied_returns_false_when_delete_exists() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = init_repo(dest_dir.path());

        let existing_path = dest_dir.path().join("exists.txt");
        fs::write(&existing_path, "keep").unwrap();

        let operations = vec![FileOp::Delete {
            dest_relative: PathBuf::from("exists.txt"),
            base: BaseEntry {
                oid: Oid::zero(),
                filemode: 0o100644,
            },
        }];

        assert!(!operations_already_applied(&operations, &dest_repo, None).unwrap());
    }

    #[test]
    fn operations_already_applied_returns_false_when_dest_missing() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let dest_repo = init_repo(dest_dir.path());

        let source_path = source_dir.path().join("file.txt");
        fs::write(&source_path, "content").unwrap();

        let operations = vec![FileOp::Write {
            source: source_path,
            dest_relative: PathBuf::from("missing.txt"),
            filemode: 0o100644,
            base: None,
        }];

        assert!(!operations_already_applied(&operations, &dest_repo, None).unwrap());
    }

    #[test]
    fn operations_already_applied_returns_false_for_directory_dest() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let dest_repo = init_repo(dest_dir.path());

        let source_path = source_dir.path().join("file.txt");
        fs::write(&source_path, "content").unwrap();
        fs::create_dir_all(dest_dir.path().join("file.txt")).unwrap();

        let operations = vec![FileOp::Write {
            source: source_path,
            dest_relative: PathBuf::from("file.txt"),
            filemode: 0o100644,
            base: None,
        }];

        assert!(!operations_already_applied(&operations, &dest_repo, None).unwrap());
    }

    #[test]
    fn operations_already_applied_returns_false_for_symlink_mismatch() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let dest_repo = init_repo(dest_dir.path());

        let source_path = source_dir.path().join("link.txt");
        fs::write(&source_path, "content").unwrap();
        fs::write(dest_dir.path().join("link.txt"), "content").unwrap();

        let operations = vec![FileOp::Write {
            source: source_path,
            dest_relative: PathBuf::from("link.txt"),
            filemode: 0o120000,
            base: None,
        }];

        assert!(!operations_already_applied(&operations, &dest_repo, None).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn operations_already_applied_returns_false_for_exec_mismatch() {
        use std::os::unix::fs::PermissionsExt;

        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let dest_repo = init_repo(dest_dir.path());

        let source_path = source_dir.path().join("file.txt");
        fs::write(&source_path, "content").unwrap();
        let dest_path = dest_dir.path().join("file.txt");
        fs::write(&dest_path, "content").unwrap();
        let mut permissions = fs::metadata(&dest_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&dest_path, permissions).unwrap();

        let operations = vec![FileOp::Write {
            source: source_path,
            dest_relative: PathBuf::from("file.txt"),
            filemode: 0o100644,
            base: None,
        }];

        assert!(!operations_already_applied(&operations, &dest_repo, None).unwrap());
    }

    #[test]
    fn operations_already_applied_returns_false_for_content_mismatch() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let dest_repo = init_repo(dest_dir.path());

        let source_path = source_dir.path().join("file.txt");
        fs::write(&source_path, "source").unwrap();
        let dest_path = dest_dir.path().join("file.txt");
        fs::write(&dest_path, "dest").unwrap();

        let operations = vec![FileOp::Write {
            source: source_path,
            dest_relative: PathBuf::from("file.txt"),
            filemode: 0o100644,
            base: None,
        }];

        assert!(!operations_already_applied(&operations, &dest_repo, None).unwrap());
    }

    #[test]
    fn operations_already_applied_returns_false_for_missing_write_source() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = init_repo(dest_dir.path());
        let source_path = dest_dir.path().join("missing.txt");
        fs::write(dest_dir.path().join("file.txt"), "dest").unwrap();

        let operations = vec![FileOp::Write {
            source: source_path,
            dest_relative: PathBuf::from("file.txt"),
            filemode: 0o100644,
            base: None,
        }];

        let err = operations_already_applied(&operations, &dest_repo, None).unwrap_err();
        assert!(matches!(err, SyncError::Io { .. }));
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
    fn sync_fails_when_destination_is_dirty() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        init_repo(dest_dir.path());

        let sig = test_signature("Dirty", 1_500_000_001);
        write_and_stage(&source_repo, Path::new("file.txt"), "hello");
        let oid = commit(&source_repo, "initial", &sig);

        fs::write(dest_dir.path().join("untracked.txt"), "dirty").unwrap();

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            oid,
            vec![],
            vec![],
        )
        .unwrap();

        let err = sync_commit(options).unwrap_err();
        assert!(matches!(err, SyncError::DirtyDestination));
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

    #[test]
    fn patch_mode_errors_on_conflict() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());
        let sig = test_signature("Conflicted", 1_750_000_000);

        write_and_stage(&source_repo, Path::new("file.txt"), "base");
        commit(&source_repo, "base", &sig);
        write_and_stage(&source_repo, Path::new("file.txt"), "source update");
        let source_commit = commit(&source_repo, "update", &sig);

        write_and_stage(&dest_repo, Path::new("file.txt"), "dest change");
        commit(&dest_repo, "dest", &sig);

        let options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            source_commit,
            vec![],
            vec![],
        )
        .unwrap();

        let err = sync_commit(options).unwrap_err();
        assert!(matches!(err, SyncError::Git(_)));
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
        let tree = dest_commit.tree().unwrap();
        let entry = tree.get_path(Path::new("large.bin")).unwrap();
        let blob = dest_repo.find_blob(entry.id()).unwrap();
        assert_eq!(blob.content(), pointer.as_bytes());
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

    #[test]
    fn override_signature_applies_overrides() {
        let base = test_signature("Base", 1_900_000_000);
        let overridden =
            override_signature(&base, Some("Other"), Some("other@example.com")).unwrap();
        assert_eq!(overridden.name(), Some("Other"));
        assert_eq!(overridden.email(), Some("other@example.com"));

        let fallback = override_signature(&base, None, None).unwrap();
        assert_eq!(fallback.name(), base.name());
        assert_eq!(fallback.email(), base.email());
    }

    #[test]
    fn index_and_path_helpers_cover_branches() {
        assert_eq!(index_flags_for_len(0), 0);
        assert_eq!(index_flags_for_len(0x2000), 0x0FFF);

        assert!(path_to_repo_bytes(Path::new("")).is_empty());
        assert_eq!(
            path_to_repo_bytes(Path::new("dir/name")),
            b"dir/name".to_vec()
        );
    }

    #[test]
    fn read_entry_for_patch_handles_regular_and_symlink() {
        let dir = tempdir().unwrap();
        let regular = dir.path().join("regular.txt");
        fs::write(&regular, "contents").unwrap();
        let data = read_entry_for_patch(&regular, 0o100644).unwrap();
        assert_eq!(data, b"contents");

        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;

            let target = dir.path().join("target.txt");
            fs::write(&target, "symlink-target").unwrap();
            let link = dir.path().join("link");
            unix_fs::symlink(&target, &link).unwrap();

            let link_data = read_entry_for_patch(&link, 0o120000).unwrap();
            assert_eq!(link_data, os_str_to_bytes(target.as_os_str()));
        }
    }

    #[test]
    fn set_executable_if_needed_toggles_permissions() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("script.sh");
        fs::write(&file, "echo hi").unwrap();

        set_executable_if_needed(&file, 0o100755).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&file).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0);
        }

        set_executable_if_needed(&file, 0o100644).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0);
        }
    }

    #[test]
    fn materialize_lfs_object_copies_and_sets_mode() {
        let dir = tempdir().unwrap();
        let object = dir.path().join("object.dat");
        let dest = dir.path().join("dest.dat");
        fs::write(&object, b"payload").unwrap();

        materialize_lfs_object(&object, &dest, 0o100755).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"payload");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dest).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0);
        }
    }

    #[test]
    fn fs_helper_wrappers_manage_dirs_and_files() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("parent/child");
        create_dir_all(&nested).unwrap();
        assert!(nested.exists());

        let file_path = dir.path().join("file.txt");
        fs::write(&file_path, "temp").unwrap();
        remove_file(&file_path).unwrap();
        assert!(!file_path.exists());

        let dir_path = dir.path().join("to_remove");
        fs::create_dir_all(&dir_path).unwrap();
        remove_dir_all(&dir_path).unwrap();
        assert!(!dir_path.exists());
    }

    #[test]
    fn copy_entry_covers_regular_and_symlink_and_lfs() {
        let dir = tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let dest_dir = dir.path().join("dest");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();

        let regular_src = src_dir.join("script.sh");
        fs::write(&regular_src, "#!/bin/sh\necho hi\n").unwrap();
        let regular_dest = dest_dir.join("script.sh");
        let resolved = copy_entry(&regular_src, &regular_dest, 0o100755, None).unwrap();
        assert!(resolved.is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&regular_dest).unwrap().permissions().mode();
            assert_ne!(mode & 0o111, 0);
        }

        let lfs_root = src_dir.join("lfs").join("objects");
        fs::create_dir_all(&lfs_root).unwrap();
        let hash = "9d0b5e1423f8a0a2f4f2b8d74d07d377c6e6f4c4f8b4f6a8d0c2e4b6a0c8d2e4";
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hash}\nsize 4\n");
        let pointer_src = src_dir.join("pointer.bin");
        fs::write(&pointer_src, pointer).unwrap();
        let object_path = lfs_root.join(&hash[0..2]).join(&hash[2..4]).join(hash);
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        fs::write(&object_path, b"blob").unwrap();
        let pointer_dest = dest_dir.join("pointer.bin");
        let resolved = copy_entry(&pointer_src, &pointer_dest, 0o100644, Some(&lfs_root)).unwrap();
        assert_eq!(resolved.unwrap(), object_path);

        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;
            let target = src_dir.join("target.txt");
            fs::write(&target, "symlink").unwrap();
            let link_src = src_dir.join("link.txt");
            unix_fs::symlink(&target, &link_src).unwrap();
            let link_dest = dest_dir.join("link.txt");
            copy_entry(&link_src, &link_dest, 0o120000, None).unwrap();
            assert!(
                fs::symlink_metadata(&link_dest)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );

            let dir_dest = dest_dir.join("link_dir");
            fs::create_dir_all(&dir_dest).unwrap();
            copy_entry(&link_src, &dir_dest, 0o120000, None).unwrap();
            assert!(
                fs::symlink_metadata(&dir_dest)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );

            let file_dest = dest_dir.join("link_file");
            fs::write(&file_dest, "old").unwrap();
            copy_entry(&link_src, &file_dest, 0o120000, None).unwrap();
            assert!(
                fs::symlink_metadata(&file_dest)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn resolve_lfs_pointer_handles_various_cases() {
        let dir = tempdir().unwrap();
        let pointer = dir.path().join("pointer");
        fs::write(&pointer, "version https://git-lfs.github.com/spec/v1\n").unwrap();

        // No store configured returns None.
        assert!(resolve_lfs_pointer(&pointer, None).unwrap().is_none());

        let lfs_root = dir.path().join("objects");
        fs::create_dir_all(&lfs_root).unwrap();

        // Short hash should return None.
        fs::write(
            &pointer,
            "version https://git-lfs.github.com/spec/v1\noid sha256:abc\n",
        )
        .unwrap();
        assert!(
            resolve_lfs_pointer(&pointer, Some(&lfs_root))
                .unwrap()
                .is_none()
        );

        // Missing object should produce an error.
        let hash = "2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f70819";
        let pointer_body =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hash}\nsize 12\n");
        fs::write(&pointer, pointer_body.clone()).unwrap();
        let err = resolve_lfs_pointer(&pointer, Some(&lfs_root)).unwrap_err();
        match err {
            SyncError::MissingLfsObject { oid, .. } => assert_eq!(oid, hash),
            other => panic!("unexpected error {other:?}"),
        }

        // Create the object so the pointer resolves successfully.
        let object_path = lfs_root.join(&hash[0..2]).join(&hash[2..4]).join(hash);
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        fs::write(&object_path, b"real-content").unwrap();
        let resolved = resolve_lfs_pointer(&pointer, Some(&lfs_root)).unwrap();
        assert_eq!(resolved.unwrap(), object_path);

        // Ensure additional lines without oid keep scanning.
        let extra_pointer = dir.path().join("extra.pointer");
        fs::write(
            &extra_pointer,
            format!(
                "version https://git-lfs.github.com/spec/v1\ncomment ignored\noid sha256:{hash}\n"
            ),
        )
        .unwrap();
        let resolved = resolve_lfs_pointer(&extra_pointer, Some(&lfs_root)).unwrap();
        assert_eq!(resolved.unwrap(), object_path);
    }

    #[test]
    fn apply_operations_patch_handles_base_delete_and_lfs() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let temp_checkout = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());

        // Prepare base commit with two files.
        write_and_stage(&source_repo, Path::new("file.txt"), "old");
        write_and_stage(&source_repo, Path::new("remove.txt"), "gone");
        let base_sig = test_signature("Pat", 1_910_000_000);
        let base_commit = commit(&source_repo, "base", &base_sig);

        // Mirror base state into destination repository.
        write_and_stage(&dest_repo, Path::new("file.txt"), "old");
        write_and_stage(&dest_repo, Path::new("remove.txt"), "gone");
        commit(&dest_repo, "base", &base_sig);

        // Prepare new file contents in checkout directory.
        let new_file_path = temp_checkout.path().join("file.txt");
        fs::create_dir_all(new_file_path.parent().unwrap()).unwrap();
        fs::write(&new_file_path, "new").unwrap();

        let pointer_hash = "4b5c6d7e8f9012a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b";
        let pointer_contents = format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{pointer_hash}\nsize 5\n"
        );
        let pointer_path = temp_checkout.path().join("pointer.dat");
        fs::write(&pointer_path, &pointer_contents).unwrap();
        let lfs_store = source_repo.path().join("lfs").join("objects");
        fs::create_dir_all(&lfs_store).unwrap();
        let pointer_object = lfs_store
            .join(&pointer_hash[0..2])
            .join(&pointer_hash[2..4])
            .join(pointer_hash);
        fs::create_dir_all(pointer_object.parent().unwrap()).unwrap();
        fs::write(&pointer_object, b"lfs\n").unwrap();

        // Build base entries from the base commit tree.
        let base_tree = source_repo
            .find_commit(base_commit)
            .unwrap()
            .tree()
            .unwrap();
        let file_entry = base_tree.get_path(Path::new("file.txt")).unwrap();
        let delete_entry = base_tree.get_path(Path::new("remove.txt")).unwrap();

        let operations = vec![
            FileOp::Write {
                source: new_file_path,
                dest_relative: PathBuf::from("file.txt"),
                filemode: 0o100644,
                base: Some(BaseEntry {
                    oid: file_entry.id(),
                    filemode: file_entry.filemode() as u32,
                }),
            },
            FileOp::Delete {
                dest_relative: PathBuf::from("remove.txt"),
                base: BaseEntry {
                    oid: delete_entry.id(),
                    filemode: delete_entry.filemode() as u32,
                },
            },
            FileOp::Write {
                source: pointer_path,
                dest_relative: PathBuf::from("pointer.dat"),
                filemode: 0o100644,
                base: None,
            },
        ];

        apply_operations_patch(&source_repo, &dest_repo, &operations).unwrap();

        assert_eq!(
            fs::read_to_string(dest_dir.path().join("file.txt")).unwrap(),
            "new"
        );
        assert!(!dest_dir.path().join("remove.txt").exists());
        assert_eq!(
            fs::read(dest_dir.path().join("pointer.dat")).unwrap(),
            b"lfs\n"
        );
    }

    #[test]
    fn apply_operations_patch_no_changes_is_noop() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());

        apply_operations_patch(&source_repo, &dest_repo, &[]).unwrap();
    }

    #[test]
    fn path_mapping_new_normalizes_inputs() {
        let mapping =
            PathMapping::new(PathBuf::from("./foo/./bar"), PathBuf::from("dest/.")).unwrap();
        let applied = mapping.apply(Path::new("foo/bar/file.txt")).unwrap();
        assert_eq!(applied, PathBuf::from("dest/file.txt"));
    }

    #[test]
    fn path_mapping_from_str_works() {
        let mapping: PathMapping = "src=dst".parse().unwrap();
        assert!(mapping.apply(Path::new("src/name.txt")).is_some());
    }

    #[test]
    fn sync_options_normalizes_skip_paths() {
        let options = SyncOptions::new(
            PathBuf::from("/tmp/source"),
            PathBuf::from("/tmp/dest"),
            Oid::zero(),
            vec![],
            vec![PathBuf::from("./skip/./dir")],
        )
        .unwrap();
        assert_eq!(options.skip, vec![PathBuf::from("skip/dir")]);
    }

    #[test]
    fn ensure_destination_clean_detects_changes_directly() {
        let dir = tempdir().unwrap();
        let repo = init_repo(dir.path());
        fs::write(repo.workdir().unwrap().join("dirty.txt"), "dirty").unwrap();
        let err = ensure_destination_clean(&repo).unwrap_err();
        assert!(matches!(err, SyncError::DirtyDestination));
    }

    #[test]
    fn checkout_to_temp_exports_tree() {
        let dir = tempdir().unwrap();
        let repo = init_repo(dir.path());
        let sig = test_signature("Checkout", 1_920_000_000);
        write_and_stage(&repo, Path::new("file.txt"), "data");
        let oid = commit(&repo, "commit", &sig);
        let commit = repo.find_commit(oid).unwrap();
        let temp = tempdir().unwrap();
        checkout_to_temp(&repo, &commit, temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    fn collect_operations_covers_multiple_statuses() {
        let dir = tempdir().unwrap();
        let repo = init_repo(dir.path());
        let sig = test_signature("Ops", 1_930_000_000);
        write_and_stage(&repo, Path::new("delete.txt"), "one");
        write_and_stage(&repo, Path::new("rename.txt"), "two");
        write_and_stage(&repo, Path::new("modify.txt"), "three");
        let base_oid = commit(&repo, "base", &sig);

        let delete_path = repo.workdir().unwrap().join("delete.txt");
        fs::remove_file(&delete_path).unwrap();
        let rename_old = repo.workdir().unwrap().join("rename.txt");
        let rename_new = repo.workdir().unwrap().join("renamed.txt");
        std::fs::rename(&rename_old, &rename_new).unwrap();
        fs::write(repo.workdir().unwrap().join("modify.txt"), "updated").unwrap();
        fs::write(repo.workdir().unwrap().join("add.txt"), "added").unwrap();

        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("delete.txt")).unwrap();
        index.remove_path(Path::new("rename.txt")).unwrap();
        index.add_path(Path::new("renamed.txt")).unwrap();
        index.add_path(Path::new("modify.txt")).unwrap();
        index.add_path(Path::new("add.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let new_tree = repo.find_tree(tree_id).unwrap();
        let base_commit = repo.find_commit(base_oid).unwrap();
        let mut diff_opts = DiffOptions::new();
        diff_opts.include_typechange(true);
        diff_opts.include_typechange_trees(true);
        let mut diff = repo
            .diff_tree_to_tree(
                Some(&base_commit.tree().unwrap()),
                Some(&new_tree),
                Some(&mut diff_opts),
            )
            .unwrap();
        diff.find_similar(None).unwrap();

        let options = SyncOptions::new(
            dir.path().to_path_buf(),
            tempdir().unwrap().path().to_path_buf(),
            base_oid,
            vec![],
            vec![],
        )
        .unwrap();

        let operations = collect_operations(&diff, repo.workdir().unwrap(), &options).unwrap();
        assert!(operations.iter().any(|op| matches!(
            op,
            FileOp::Delete { dest_relative, .. } if dest_relative == Path::new("delete.txt")
        )));
        assert!(operations.iter().any(|op| matches!(
            op,
            FileOp::Write { dest_relative, .. } if dest_relative == Path::new("renamed.txt")
        )));
        assert!(operations.iter().any(|op| matches!(
            op,
            FileOp::Write { dest_relative, .. } if dest_relative == Path::new("add.txt")
        )));
    }

    #[test]
    fn collect_operations_respects_skip_paths() {
        let dir = tempdir().unwrap();
        let repo = init_repo(dir.path());
        let sig = test_signature("Skip", 1_940_000_000);
        write_and_stage(&repo, Path::new("delete.txt"), "one");
        write_and_stage(&repo, Path::new("rename.txt"), "two");
        write_and_stage(&repo, Path::new("modify.txt"), "three");
        let base_oid = commit(&repo, "base", &sig);

        let delete_path = repo.workdir().unwrap().join("delete.txt");
        fs::remove_file(&delete_path).unwrap();
        let rename_old = repo.workdir().unwrap().join("rename.txt");
        let rename_new = repo.workdir().unwrap().join("renamed.txt");
        std::fs::rename(&rename_old, &rename_new).unwrap();
        fs::write(repo.workdir().unwrap().join("modify.txt"), "updated").unwrap();
        fs::write(repo.workdir().unwrap().join("add.txt"), "added").unwrap();

        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("delete.txt")).unwrap();
        index.remove_path(Path::new("rename.txt")).unwrap();
        index.add_path(Path::new("renamed.txt")).unwrap();
        index.add_path(Path::new("modify.txt")).unwrap();
        index.add_path(Path::new("add.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let new_tree = repo.find_tree(tree_id).unwrap();
        let base_commit = repo.find_commit(base_oid).unwrap();
        let mut diff_opts = DiffOptions::new();
        diff_opts.include_typechange(true);
        diff_opts.include_typechange_trees(true);
        let mut diff = repo
            .diff_tree_to_tree(
                Some(&base_commit.tree().unwrap()),
                Some(&new_tree),
                Some(&mut diff_opts),
            )
            .unwrap();
        diff.find_similar(None).unwrap();

        let options = SyncOptions::new(
            dir.path().to_path_buf(),
            tempdir().unwrap().path().to_path_buf(),
            base_oid,
            vec![],
            vec![
                PathBuf::from("delete.txt"),
                PathBuf::from("renamed.txt"),
                PathBuf::from("modify.txt"),
                PathBuf::from("add.txt"),
            ],
        )
        .unwrap();

        let operations = collect_operations(&diff, repo.workdir().unwrap(), &options).unwrap();
        assert!(operations.is_empty());
    }

    #[test]
    fn apply_operations_copy_handles_lfs_and_delete() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());

        let regular_src = source_dir.path().join("regular.txt");
        fs::write(&regular_src, "regular").unwrap();

        let hash = "5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c";
        let pointer_src = source_dir.path().join("pointer.dat");
        fs::write(
            &pointer_src,
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hash}\nsize 6\n"),
        )
        .unwrap();
        let lfs_store = source_repo.path().join("lfs").join("objects");
        fs::create_dir_all(&lfs_store).unwrap();
        let object_path = lfs_store.join(&hash[0..2]).join(&hash[2..4]).join(hash);
        fs::create_dir_all(object_path.parent().unwrap()).unwrap();
        fs::write(&object_path, b"object").unwrap();

        let dest_remove = dest_repo.workdir().unwrap().join("remove.txt");
        fs::write(&dest_remove, "remove").unwrap();

        let operations = vec![
            FileOp::Write {
                source: regular_src,
                dest_relative: PathBuf::from("regular.txt"),
                filemode: 0o100644,
                base: None,
            },
            FileOp::Write {
                source: pointer_src,
                dest_relative: PathBuf::from("pointer.dat"),
                filemode: 0o100755,
                base: None,
            },
            FileOp::Delete {
                dest_relative: PathBuf::from("remove.txt"),
                base: BaseEntry {
                    oid: Oid::zero(),
                    filemode: 0o100644,
                },
            },
        ];

        apply_operations_copy(&source_repo, &dest_repo, &operations).unwrap();
        assert_eq!(
            fs::read_to_string(dest_dir.path().join("regular.txt")).unwrap(),
            "regular"
        );
        assert_eq!(
            fs::read(dest_dir.path().join("pointer.dat")).unwrap(),
            b"object"
        );
        assert!(!dest_dir.path().join("remove.txt").exists());
    }

    #[test]
    fn copy_entry_errors_when_lfs_missing() {
        let dir = tempdir().unwrap();
        let pointer = dir.path().join("pointer");
        let lfs_root = dir.path().join("lfs").join("objects");
        fs::create_dir_all(&lfs_root).unwrap();
        let hash = "abcde12345abcde12345abcde12345abcde12345abcde12345abcde12345abcd";
        fs::write(
            &pointer,
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{hash}\nsize 4\n"),
        )
        .unwrap();
        let err = copy_entry(
            &pointer,
            &dir.path().join("dest"),
            0o100644,
            Some(&lfs_root),
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::MissingLfsObject { .. }));
    }

    #[test]
    fn resolve_lfs_pointer_handles_invalid_data() {
        let dir = tempdir().unwrap();
        let pointer = dir.path().join("invalid.pointer");
        fs::write(&pointer, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(
            resolve_lfs_pointer(&pointer, Some(dir.path()))
                .unwrap()
                .is_none()
        );

        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let pointer_two = dir.path().join("invalid_loop.pointer");
        let mut data = Vec::new();
        data.extend_from_slice(b"version https://git-lfs.github.com/spec/v1\n");
        data.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        fs::write(&pointer_two, data).unwrap();
        let store = dir.path().join("objects");
        fs::create_dir_all(&store).unwrap();
        assert!(
            resolve_lfs_pointer(&pointer_two, Some(&store))
                .unwrap()
                .is_none()
        );

        fs::write(
            &pointer_two,
            format!(
                "version https://git-lfs.github.com/spec/v1\noid sha256:{}\n",
                &hash[0..10]
            ),
        )
        .unwrap();
        assert!(
            resolve_lfs_pointer(&pointer_two, Some(&store))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_lfs_pointer_reports_io_error_for_missing_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing.pointer");
        let err = resolve_lfs_pointer(&missing, Some(dir.path()))
            .expect_err("expected io error for missing pointer");
        assert!(matches!(err, SyncError::Io { .. }));
    }

    #[test]
    fn resolve_lfs_pointer_handles_completely_empty_file() {
        let dir = tempdir().unwrap();
        let pointer = dir.path().join("empty.pointer");
        fs::File::create(&pointer).unwrap();
        assert!(
            resolve_lfs_pointer(&pointer, Some(dir.path()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_lfs_pointer_returns_none_for_empty_file() {
        let dir = tempdir().unwrap();
        let pointer = dir.path().join("empty.pointer");
        fs::write(&pointer, "version https://git-lfs.github.com/spec/v1").unwrap();
        let store = dir.path().join("objects");
        fs::create_dir_all(&store).unwrap();
        assert!(
            resolve_lfs_pointer(&pointer, Some(&store))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn read_entry_for_patch_reports_error() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        let err = read_entry_for_patch(&missing, 0o100644).unwrap_err();
        match err {
            SyncError::Io { path, .. } => assert_eq!(path, missing),
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn map_destination_prefers_longest_prefix() {
        let mappings = vec![
            PathMapping::parse("a=b").unwrap(),
            PathMapping::parse("a/b=c").unwrap(),
        ];
        let mapped = map_destination(Path::new("a/b/file.txt"), &mappings);
        assert_eq!(mapped, PathBuf::from("c/file.txt"));
    }

    #[test]
    fn sync_commit_honors_signature_overrides() {
        let source_dir = tempdir().unwrap();
        let dest_dir = tempdir().unwrap();
        let source_repo = init_repo(source_dir.path());
        let dest_repo = init_repo(dest_dir.path());

        let sig = test_signature("Orig", 1_940_000_000);
        write_and_stage(&source_repo, Path::new("file.txt"), "data");
        let commit_oid = commit(&source_repo, "commit", &sig);

        let mut options = SyncOptions::new(
            source_dir.path().to_path_buf(),
            dest_dir.path().to_path_buf(),
            commit_oid,
            vec![],
            vec![],
        )
        .unwrap();
        options.author_name = Some("Override".to_string());
        options.author_email = Some("override@example.com".to_string());
        options.committer_name = Some("Committer".to_string());
        options.committer_email = Some("committer@example.com".to_string());

        let new_oid = sync_commit(options).unwrap();
        let commit = dest_repo.find_commit(new_oid).unwrap();
        assert_eq!(commit.author().name().unwrap(), "Override");
        assert_eq!(commit.committer().name().unwrap(), "Committer");
    }
}
