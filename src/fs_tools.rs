//! Secure, workspace-confined filesystem mutation tools for Orbit agent roles.
use anyhow::{Context, Result, bail, ensure};
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
    let full = confine_path(repo_path, path_str, false, false)?;
    if full.exists() {
        ensure!(full.is_dir(), "target exists and is not a directory");
        return Ok(full);
    }
    if recursive {
        std::fs::create_dir_all(&full)
            .with_context(|| format!("failed to create directory recursively: {path_str}"))?;
    } else {
        std::fs::create_dir(&full)
            .with_context(|| format!("failed to create directory: {path_str}"))?;
    }
    Ok(full)
}

/// Move or rename a file or directory within `repo_path`. Fails closed if destination exists.
pub fn move_path(
    repo_path: &Path,
    source_str: &str,
    destination_str: &str,
) -> Result<(PathBuf, PathBuf)> {
    let src = confine_path(repo_path, source_str, true, false)?;
    let dst = confine_path(repo_path, destination_str, false, false)?;

    ensure!(
        dst.symlink_metadata().is_err(),
        "destination already exists"
    );

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| "failed to create parent directories for destination".to_string())?;
    }

    std::fs::rename(&src, &dst)
        .with_context(|| format!("failed to move from {source_str} to {destination_str}"))?;
    Ok((src, dst))
}

/// Delete a single file within `repo_path`.
pub fn delete_file(repo_path: &Path, path_str: &str) -> Result<PathBuf> {
    let full = confine_path(repo_path, path_str, true, false)?;
    let meta = full
        .symlink_metadata()
        .with_context(|| format!("cannot inspect target: {path_str}"))?;
    ensure!(
        !meta.is_dir(),
        "target is a directory; use delete_directory"
    );

    std::fs::remove_file(&full).with_context(|| format!("failed to remove file: {path_str}"))?;
    Ok(full)
}

/// Delete a directory within `repo_path`.
pub fn delete_directory(repo_path: &Path, path_str: &str, recursive: bool) -> Result<PathBuf> {
    let full = confine_path(repo_path, path_str, true, false)?;
    let meta = full
        .symlink_metadata()
        .with_context(|| format!("cannot inspect target: {path_str}"))?;
    ensure!(meta.is_dir(), "target is not a directory; use delete_file");

    if recursive {
        std::fs::remove_dir_all(&full)
            .with_context(|| format!("failed to remove directory recursively: {path_str}"))?;
    } else {
        std::fs::remove_dir(&full)
            .with_context(|| format!("failed to remove directory: {path_str}"))?;
    }
    Ok(full)
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
}
