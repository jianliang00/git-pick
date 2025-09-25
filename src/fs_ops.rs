use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::SyncError;
use tempfile::TempDir;

fn io_error(path: &Path, source: std::io::Error) -> SyncError {
    SyncError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn read_entry_for_patch(path: &Path, filemode: u32) -> Result<Vec<u8>, SyncError> {
    let metadata = fs::symlink_metadata(path).map_err(|err| io_error(path, err))?;
    if metadata.file_type().is_symlink() || filemode == 0o120000 {
        let target = fs::read_link(path).map_err(|err| io_error(path, err))?;
        Ok(os_str_to_bytes(target.as_os_str()))
    } else {
        fs::read(path).map_err(|err| io_error(path, err))
    }
}

pub(crate) fn os_str_to_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().into_owned().into_bytes()
    }
}

pub(crate) fn create_temp_dir_for(context: &Path) -> Result<TempDir, SyncError> {
    TempDir::new().map_err(|err| io_error(context, err))
}

pub(crate) fn create_dir_all(path: &Path) -> Result<(), SyncError> {
    fs::create_dir_all(path).map_err(|err| io_error(path, err))
}

pub(crate) fn remove_dir_all(path: &Path) -> Result<(), SyncError> {
    fs::remove_dir_all(path).map_err(|err| io_error(path, err))
}

pub(crate) fn remove_file(path: &Path) -> Result<(), SyncError> {
    fs::remove_file(path).map_err(|err| io_error(path, err))
}

pub(crate) fn copy_entry(
    source: &Path,
    dest: &Path,
    filemode: u32,
    lfs_store: Option<&Path>,
) -> Result<Option<PathBuf>, SyncError> {
    let metadata = fs::symlink_metadata(source).map_err(|err| io_error(source, err))?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source).map_err(|err| io_error(source, err))?;
        if let Ok(existing) = fs::symlink_metadata(dest) {
            if existing.file_type().is_dir() {
                fs::remove_dir_all(dest).map_err(|err| io_error(dest, err))?;
            } else {
                fs::remove_file(dest).map_err(|err| io_error(dest, err))?;
            }
        }
        create_symlink(&target, dest).map_err(|err| io_error(dest, err))?;
        Ok(None)
    } else {
        let resolved = resolve_lfs_pointer(source, lfs_store)?;
        fs::copy(source, dest).map_err(|err| io_error(dest, err))?;
        set_executable_if_needed(dest, filemode)?;
        Ok(resolved)
    }
}

pub(crate) fn materialize_lfs_object(
    object: &Path,
    dest: &Path,
    filemode: u32,
) -> Result<(), SyncError> {
    fs::copy(object, dest).map_err(|err| io_error(dest, err))?;
    set_executable_if_needed(dest, filemode)?;
    Ok(())
}

pub(crate) fn resolve_lfs_pointer(
    source: &Path,
    lfs_store: Option<&Path>,
) -> Result<Option<PathBuf>, SyncError> {
    let Some(lfs_root) = lfs_store else {
        return Ok(None);
    };
    let contents = match fs::read_to_string(source) {
        Ok(data) => data,
        Err(err) if err.kind() == ErrorKind::InvalidData => return Ok(None),
        Err(err) => {
            return Err(io_error(source, err));
        }
    };
    let mut lines = contents.lines();
    let Some(first_line) = lines.next() else {
        return Ok(None);
    };
    if first_line.trim_end() != "version https://git-lfs.github.com/spec/v1" {
        return Ok(None);
    }

    let mut oid: Option<String> = None;
    for line in lines.by_ref().take(8) {
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
    let object_path = lfs_root
        .join(&hash[0..2])
        .join(&hash[2..4])
        .join(hash.as_str());
    if !object_path.exists() {
        return Err(SyncError::MissingLfsObject {
            oid: hash,
            path: source.to_path_buf(),
        });
    }
    Ok(Some(object_path))
}

pub(crate) fn set_executable_if_needed(dest: &Path, filemode: u32) -> Result<(), SyncError> {
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
                fs::set_permissions(dest, permissions).map_err(|err| io_error(dest, err))?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (dest, filemode);
    Ok(())
}

#[cfg(unix)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub(crate) fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    if fs::metadata(target).map(|m| m.is_dir()).unwrap_or(false) {
        symlink_dir(target, link)
    } else {
        symlink_file(target, link)
    }
}
