//! Secure, workspace-confined filesystem mutation tools for Orbit agent roles.
use anyhow::{Context, Result, bail, ensure};
use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

/// Validates and confines a requested path within `repo_path`.
///
/// Handles virtual ACP paths (`/orbit/home/workspace/...`), rejects `..` traversal,
/// rejects absolute host paths outside workspace, rejects symlink escapes,
/// and rejects workspace root modifications unless `allow_root` is true.
pub fn confine_path(
    repo_path: &Path,
    requested: &str,
    must_exist: bool,
    allow_root: bool,
) -> Result<PathBuf> {
    let clean = requested.trim();
    ensure!(!clean.is_empty(), "path cannot be empty");
    ensure!(!clean.contains('\0'), "path cannot contain null bytes");
    ensure!(!clean.starts_with('-'), "path cannot start with '-'");

    let canonical_repo = repo_path
        .canonicalize()
        .context("workspace repository missing or unreadable")?;

    // Determine the relative path portion
    let rel_path = if clean.starts_with('/') {
        if let Some(stripped) = clean.strip_prefix("/orbit/home/workspace/") {
            PathBuf::from(stripped)
        } else if clean == "/orbit/home/workspace" {
            PathBuf::new()
        } else if let Some(stripped) = clean.strip_prefix("/orbit/home/") {
            PathBuf::from(stripped)
        } else if clean == "/orbit/home" {
            PathBuf::new()
        } else {
            let req_path = Path::new(clean);
            if let Ok(stripped) = req_path.strip_prefix(&canonical_repo) {
                stripped.to_path_buf()
            } else if let Ok(stripped) = req_path.strip_prefix(repo_path) {
                stripped.to_path_buf()
            } else {
                bail!("absolute host path outside workspace rejected");
            }
        }
    } else {
        PathBuf::from(clean)
    };

    // Verify all components in rel_path are safe
    for component in rel_path.components() {
        match component {
            Component::ParentDir => bail!("path traversal rejected"),
            Component::RootDir | Component::Prefix(_) => bail!("absolute path rejected"),
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    let full_path = canonical_repo.join(&rel_path);

    if must_exist {
        let _meta = full_path
            .symlink_metadata()
            .context("path does not exist")?;
        let canonical_full = full_path
            .canonicalize()
            .context("failed to resolve target path")?;
        ensure!(
            canonical_full.starts_with(&canonical_repo),
            "symlink escapes workspace rejected"
        );
        if !allow_root && canonical_full == canonical_repo {
            bail!("workspace root operation rejected");
        }
        Ok(full_path)
    } else {
        // If it exists already, verify canonical confinement
        if full_path.symlink_metadata().is_ok() {
            let canonical_full = full_path
                .canonicalize()
                .context("failed to resolve target path")?;
            ensure!(
                canonical_full.starts_with(&canonical_repo),
                "symlink escapes workspace rejected"
            );
            if !allow_root && canonical_full == canonical_repo {
                bail!("workspace root operation rejected");
            }
            Ok(full_path)
        } else {
            // Target does not exist yet
            if !allow_root && (rel_path.as_os_str().is_empty() || rel_path == Path::new(".")) {
                bail!("workspace root operation rejected");
            }
            // Walk ancestor hierarchy to ensure all existing parents resolve inside repo
            let mut cur = full_path.parent();
            let mut found_existing = false;
            while let Some(parent) = cur {
                if parent.symlink_metadata().is_ok() {
                    let canonical_parent = parent
                        .canonicalize()
                        .context("failed to resolve ancestor directory")?;
                    ensure!(
                        canonical_parent.starts_with(&canonical_repo),
                        "parent path escapes workspace rejected"
                    );
                    found_existing = true;
                    break;
                }
                cur = parent.parent();
            }
            ensure!(
                found_existing,
                "path has no valid ancestor directory inside workspace"
            );
            Ok(full_path)
        }
    }
}

/// Create a directory within `repo_path`.
pub fn create_directory(repo_path: &Path, path_str: &str, recursive: bool) -> Result<PathBuf> {
    let root = RootedRepo::open(repo_path)?;
    let relative = root.relative(path_str)?;
    root.create_directory(&relative, recursive)?;
    Ok(root.path.join(relative))
}

/// Move or rename a file or directory within `repo_path`. Fails closed if destination exists.
pub fn move_path(
    repo_path: &Path,
    source_str: &str,
    destination_str: &str,
) -> Result<(PathBuf, PathBuf)> {
    let root = RootedRepo::open(repo_path)?;
    let src = root.relative(source_str)?;
    let dst = root.relative(destination_str)?;
    root.move_path(&src, &dst)?;
    Ok((root.path.join(src), root.path.join(dst)))
}

/// Delete a single file within `repo_path`.
pub fn delete_file(repo_path: &Path, path_str: &str) -> Result<PathBuf> {
    let root = RootedRepo::open(repo_path)?;
    let relative = root.relative(path_str)?;
    root.delete_file(&relative)?;
    Ok(root.path.join(relative))
}

/// Delete a directory within `repo_path`.
pub fn delete_directory(repo_path: &Path, path_str: &str, recursive: bool) -> Result<PathBuf> {
    let root = RootedRepo::open(repo_path)?;
    let relative = root.relative(path_str)?;
    root.delete_directory(&relative, recursive)?;
    Ok(root.path.join(relative))
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

struct RootedRepo {
    path: PathBuf,
    root: File,
}

impl RootedRepo {
    fn open(repo_path: &Path) -> Result<Self> {
        let path = repo_path
            .canonicalize()
            .context("repository root missing")?;
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        Ok(Self { path, root })
    }

    fn relative(&self, requested: &str) -> Result<PathBuf> {
        let full = confine_path(&self.path, requested, false, false)?;
        let relative = full.strip_prefix(&self.path)?.to_path_buf();
        ensure!(
            relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
                && !relative.as_os_str().is_empty(),
            "repository mutation path must have normal components"
        );
        Ok(relative)
    }

    fn open_child(parent: &File, name: &OsStr, flags: i32) -> Result<File> {
        let name = CString::new(name.as_bytes())?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: 0,
            resolve: 0x08 | 0x04 | 0x01, // BENEATH | NO_SYMLINKS | NO_XDEV
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                parent.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        } as i32;
        ensure!(
            fd >= 0,
            "confined repository open failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_parent(&self, relative: &Path, create: bool) -> Result<(File, CString)> {
        let leaf = relative
            .file_name()
            .context("mutation path has no filename")?;
        let leaf = CString::new(leaf.as_bytes())?;
        let mut parent = self.root.try_clone()?;
        if let Some(ancestors) = relative.parent() {
            for part in ancestors.components() {
                let Component::Normal(name) = part else {
                    bail!("invalid repository path component")
                };
                let name_c = CString::new(name.as_bytes())?;
                if create {
                    let created =
                        unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) };
                    if created != 0 {
                        ensure!(
                            std::io::Error::last_os_error().kind()
                                == std::io::ErrorKind::AlreadyExists,
                            "confined parent directory creation failed: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                }
                parent = Self::open_child(&parent, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
            }
        }
        Ok((parent, leaf))
    }

    fn stat_at(parent: &File, name: &CString) -> std::io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let status = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn create_directory(&self, relative: &Path, recursive: bool) -> Result<()> {
        let (parent, leaf) = self.open_parent(relative, recursive)?;
        let created = unsafe { libc::mkdirat(parent.as_raw_fd(), leaf.as_ptr(), 0o700) };
        if created != 0 {
            ensure!(
                std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists,
                "confined directory creation failed: {}",
                std::io::Error::last_os_error()
            );
            Self::open_child(
                &parent,
                OsStr::from_bytes(leaf.as_bytes()),
                libc::O_RDONLY | libc::O_DIRECTORY,
            )?;
        }
        Ok(())
    }

    fn read_file(&self, relative: &Path) -> Result<Vec<u8>> {
        let (parent, leaf) = self.open_parent(relative, false)?;
        let mut file =
            Self::open_child(&parent, OsStr::from_bytes(leaf.as_bytes()), libc::O_RDONLY)?;
        ensure!(
            file.metadata()?.is_file(),
            "repository target is not a regular file"
        );
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn write_file(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        let (parent, leaf) = self.open_parent(relative, true)?;
        let existing_mode = match Self::stat_at(&parent, &leaf) {
            Ok(stat) => {
                ensure!(
                    stat.st_mode & libc::S_IFMT == libc::S_IFREG,
                    "repository target is not a regular file"
                );
                stat.st_mode & 0o777
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0o600,
            Err(error) => return Err(error.into()),
        };
        let temporary = CString::new(format!(".orbit-write-{}", crate::model::id()))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        ensure!(fd >= 0, "confined temporary file creation failed");
        let result = (|| -> Result<()> {
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.write_all(bytes)?;
            ensure!(
                unsafe { libc::fchmod(file.as_raw_fd(), existing_mode) } == 0,
                "set candidate mode failed"
            );
            file.sync_all()?;
            ensure!(
                unsafe {
                    libc::renameat(
                        parent.as_raw_fd(),
                        temporary.as_ptr(),
                        parent.as_raw_fd(),
                        leaf.as_ptr(),
                    )
                } == 0,
                "confined candidate replacement failed: {}",
                std::io::Error::last_os_error()
            );
            parent.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0) };
        }
        result
    }

    fn move_path(&self, source: &Path, destination: &Path) -> Result<()> {
        let (source_parent, source_leaf) = self.open_parent(source, false)?;
        let (destination_parent, destination_leaf) = self.open_parent(destination, true)?;
        let stat = Self::stat_at(&source_parent, &source_leaf)?;
        ensure!(
            stat.st_mode & libc::S_IFMT != libc::S_IFLNK,
            "symlink source denied"
        );
        let status = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                source_parent.as_raw_fd(),
                source_leaf.as_ptr(),
                destination_parent.as_raw_fd(),
                destination_leaf.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                bail!("destination already exists");
            }
            bail!("confined move failed: {error}");
        }
        Ok(())
    }

    fn delete_file(&self, relative: &Path) -> Result<()> {
        let (parent, leaf) = self.open_parent(relative, false)?;
        let stat = Self::stat_at(&parent, &leaf)?;
        ensure!(
            stat.st_mode & libc::S_IFMT == libc::S_IFREG,
            "target is a directory or not a regular file"
        );
        ensure!(
            unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } == 0,
            "confined file deletion failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    fn delete_directory(&self, relative: &Path, recursive: bool) -> Result<()> {
        let (parent, leaf) = self.open_parent(relative, false)?;
        let dir = Self::open_child(
            &parent,
            OsStr::from_bytes(leaf.as_bytes()),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?;
        if recursive {
            let mut budget = 10_000usize;
            Self::remove_contents(&dir, 0, &mut budget)?;
        }
        ensure!(
            unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), libc::AT_REMOVEDIR) } == 0,
            "confined directory deletion failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    fn remove_contents(dir: &File, depth: usize, budget: &mut usize) -> Result<()> {
        ensure!(depth < 64, "repository delete depth limit exceeded");
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            ensure!(*budget > 0, "repository delete entry limit exceeded");
            *budget -= 1;
            let name = entry?.file_name();
            let name_c = CString::new(name.as_bytes())?;
            let stat = Self::stat_at(dir, &name_c)?;
            if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                let child = Self::open_child(dir, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
                Self::remove_contents(&child, depth + 1, budget)?;
                ensure!(
                    unsafe { libc::unlinkat(dir.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR) }
                        == 0,
                    "confined child directory deletion failed"
                );
            } else {
                ensure!(
                    unsafe { libc::unlinkat(dir.as_raw_fd(), name_c.as_ptr(), 0) } == 0,
                    "confined child file deletion failed"
                );
            }
        }
        Ok(())
    }

    fn copy_path(&self, source: &Path, destination: &Path, recursive: bool) -> Result<()> {
        ensure!(
            !destination.starts_with(source),
            "destination cannot be inside source"
        );
        let (source_parent, source_leaf) = self.open_parent(source, false)?;
        let stat = Self::stat_at(&source_parent, &source_leaf)?;
        let (destination_parent, destination_leaf) = self.open_parent(destination, true)?;
        match Self::stat_at(&destination_parent, &destination_leaf) {
            Ok(_) => bail!("ERR_DESTINATION_EXISTS: destination already exists"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            ensure!(
                recursive,
                "source is a directory; copy requires recursive=true"
            );
            self.create_directory(destination, true)?;
            self.copy_directory_contents(source, destination, 0, &mut 10_000usize)
        } else if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
            let bytes = self.read_file(source)?;
            let fd = unsafe {
                libc::openat(
                    destination_parent.as_raw_fd(),
                    destination_leaf.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            ensure!(
                fd >= 0,
                "confined copy destination creation failed: {}",
                std::io::Error::last_os_error()
            );
            let mut target = unsafe { File::from_raw_fd(fd) };
            target.write_all(&bytes)?;
            target.sync_all()?;
            Ok(())
        } else {
            bail!("unsupported or symlink source for copy")
        }
    }

    fn copy_directory_contents(
        &self,
        source: &Path,
        destination: &Path,
        depth: usize,
        budget: &mut usize,
    ) -> Result<()> {
        ensure!(depth < 64, "repository copy depth limit exceeded");
        let (source_parent, source_leaf) = self.open_parent(source, false)?;
        let dir = Self::open_child(
            &source_parent,
            OsStr::from_bytes(source_leaf.as_bytes()),
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?;
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            ensure!(*budget > 0, "repository copy entry limit exceeded");
            *budget -= 1;
            let name = entry?.file_name();
            let child_source = source.join(&name);
            let child_destination = destination.join(&name);
            let name_c = CString::new(name.as_bytes())?;
            let stat = Self::stat_at(&dir, &name_c)?;
            if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                self.create_directory(&child_destination, true)?;
                self.copy_directory_contents(&child_source, &child_destination, depth + 1, budget)?;
            } else if stat.st_mode & libc::S_IFMT == libc::S_IFREG {
                self.copy_path(&child_source, &child_destination, false)?;
            } else {
                bail!("unsupported or symlink source for copy")
            }
        }
        Ok(())
    }
}

pub fn read_text_confined(repo_path: &Path, path_str: &str) -> Result<String> {
    let root = RootedRepo::open(repo_path)?;
    let relative = root.relative(path_str)?;
    Ok(String::from_utf8(root.read_file(&relative)?)?)
}

pub fn write_text_confined(repo_path: &Path, path_str: &str, content: &str) -> Result<()> {
    let root = RootedRepo::open(repo_path)?;
    let relative = root.relative(path_str)?;
    root.write_file(&relative, content.as_bytes())
}

pub fn copy_path_confined(
    repo_path: &Path,
    source_str: &str,
    destination_str: &str,
    recursive: bool,
) -> Result<()> {
    let root = RootedRepo::open(repo_path)?;
    let source = root.relative(source_str)?;
    let destination = root.relative(destination_str)?;
    root.copy_path(&source, &destination, recursive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_delete_directory() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();

        let created = create_directory(repo, "nested/sub/dir", true)?;
        assert!(created.is_dir());

        // Idempotent
        let again = create_directory(repo, "nested/sub/dir", true)?;
        assert_eq!(created, again);

        // Delete empty sub dir
        delete_directory(repo, "nested/sub/dir", false)?;
        assert!(!created.exists());

        Ok(())
    }

    #[test]
    fn test_move_file_and_destination_collision() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();

        create_directory(repo, "docs/old", true)?;
        let src_file = repo.join("docs/old/file.txt");
        std::fs::write(&src_file, "hello world")?;

        let (src, dst) = move_path(repo, "docs/old/file.txt", "docs/new/file.txt")?;
        assert!(!src.exists());
        assert!(dst.exists());
        assert_eq!(std::fs::read_to_string(&dst)?, "hello world");

        // Collision fails closed
        std::fs::write(&src_file, "new content")?;
        let err = move_path(repo, "docs/old/file.txt", "docs/new/file.txt");
        assert!(err.is_err());
        assert!(
            err.unwrap_err()
                .to_string()
                .contains("destination already exists")
        );

        Ok(())
    }

    #[test]
    fn test_delete_file_safety() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();

        let file = repo.join("target.txt");
        std::fs::write(&file, "content")?;
        delete_file(repo, "target.txt")?;
        assert!(!file.exists());

        // Cannot delete directory with delete_file
        create_directory(repo, "some_dir", false)?;
        let err = delete_file(repo, "some_dir");
        assert!(err.is_err());
        assert!(
            err.unwrap_err()
                .to_string()
                .contains("target is a directory")
        );

        Ok(())
    }

    #[test]
    fn test_path_traversal_and_absolute_rejection() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();

        assert!(confine_path(repo, "../outside", false, false).is_err());
        assert!(confine_path(repo, "foo/../../outside", false, false).is_err());
        assert!(confine_path(repo, "/etc/passwd", false, false).is_err());
        assert!(confine_path(repo, "/tmp/somewhere", false, false).is_err());
        assert!(confine_path(repo, ".", false, false).is_err());
        assert!(confine_path(repo, "/orbit/home/workspace", false, false).is_err());

        Ok(())
    }

    #[test]
    fn test_symlink_escape_rejection() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();
        let outside = tempfile::tempdir()?;
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "confidential")?;

        let symlink_path = repo.join("symlink_to_outside");
        std::os::unix::fs::symlink(outside.path(), &symlink_path)?;

        let err = delete_file(repo, "symlink_to_outside/secret.txt");
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("escapes workspace"));

        let err_dir = create_directory(repo, "symlink_to_outside/new_dir", true);
        assert!(err_dir.is_err());
        assert!(
            err_dir
                .unwrap_err()
                .to_string()
                .contains("escapes workspace")
        );

        Ok(())
    }

    #[test]
    fn confined_mutations_reject_symlink_replacement() -> Result<()> {
        use std::os::unix::fs::symlink;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let repo = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        std::fs::create_dir(repo.path().join("inside"))?;
        let link = repo.path().join("switch");
        symlink(repo.path().join("inside"), &link)?;
        let outside_marker = outside.path().join("must-not-write.txt");
        let running = Arc::new(AtomicBool::new(true));
        let racing = Arc::clone(&running);
        let outside_path = outside.path().to_path_buf();
        let inside_path = repo.path().join("inside");
        let worker = std::thread::spawn(move || {
            while racing.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&link);
                let _ = symlink(&outside_path, &link);
                std::thread::yield_now();
                let _ = std::fs::remove_file(&link);
                let _ = symlink(&inside_path, &link);
            }
        });
        for _ in 0..200 {
            let _ = write_text_confined(repo.path(), "switch/must-not-write.txt", "bad");
            let _ = create_directory(repo.path(), "switch/must-not-create", true);
        }
        running.store(false, Ordering::Relaxed);
        worker.join().expect("symlink race worker panicked");
        assert!(!outside_marker.exists());
        assert!(!outside.path().join("must-not-create").exists());
        Ok(())
    }
}
