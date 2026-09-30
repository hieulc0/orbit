//! Managed candidates stay separate from the developer checkout. Applying or
//! discarding is an explicit client action after workflow cleanup is confirmed.
use crate::workflow_coordinator::{compute_workspace_state, review_candidate_diff};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktree {
    pub repository: PathBuf,
    pub workspace: PathBuf,
    pub base_revision: String,
}

async fn git(repository: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = crate::execution::process::bounded_output(
        crate::tool_surface::safe_git_command(repository, args),
        8 * 1024 * 1024,
        Duration::from_secs(30),
    )
    .await?;
    ensure!(
        output.stdout.len() <= 8 * 1024 * 1024 && output.stderr.len() <= 65536,
        "Git output exceeds bounds"
    );
    ensure!(
        output.status.success(),
        "managed worktree Git operation failed"
    );
    Ok(output.stdout)
}

impl ManagedWorktree {
    pub async fn create(repository: &Path, workspace: &Path) -> Result<Self> {
        let repository = repository.canonicalize()?;
        ensure!(
            !repository.to_string_lossy().contains(['\n', '\r'])
                && workspace.is_absolute()
                && !workspace.exists()
                && !repository.starts_with(workspace),
            "invalid managed worktree path"
        );
        let parent = workspace
            .parent()
            .context("workspace parent missing")?
            .canonicalize()?;
        ensure!(
            parent.join(workspace.file_name().context("workspace name missing")?) == workspace,
            "workspace parent identity changed"
        );
        let root = String::from_utf8(git(&repository, &["rev-parse", "--show-toplevel"]).await?)?;
        ensure!(
            Path::new(root.trim()) == repository,
            "managed repository must be its canonical Git root"
        );
        Self::check_local_git_config(&repository).await?;
        let base_revision = String::from_utf8(
            git(&repository, &["rev-parse", "--verify", "HEAD^{commit}"]).await?,
        )?
        .trim()
        .to_owned();
        git(
            &repository,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "--detach",
                workspace.to_str().context("workspace path must be UTF-8")?,
                &base_revision,
            ],
        )
        .await?;
        let candidate = Self {
            repository,
            workspace: workspace.to_owned(),
            base_revision,
        };
        candidate.validate().await?;
        Ok(candidate)
    }

    async fn check_local_git_config(repository: &Path) -> Result<()> {
        let output = crate::execution::process::bounded_output(
            crate::tool_surface::safe_git_command(
                repository,
                &[
                    "config",
                    "--local",
                    "--includes",
                    "--name-only",
                    "--get-regexp",
                    "^filter\\.",
                ],
            ),
            65536,
            Duration::from_secs(30),
        )
        .await?;
        ensure!(
            output.status.code() == Some(1)
                || (output.status.success() && output.stdout.is_empty()),
            "managed local worktrees do not permit host Git filters"
        );
        Ok(())
    }

    pub async fn validate(&self) -> Result<()> {
        ensure!(
            self.repository.canonicalize()? == self.repository
                && self.workspace.canonicalize()? == self.workspace
                && self.repository != self.workspace,
            "worktree identity changed"
        );
        Self::check_local_git_config(&self.repository).await?;
        let inventory = git(&self.repository, &["worktree", "list", "--porcelain", "-z"]).await?;
        let expected = format!("worktree {}", self.workspace.display());
        ensure!(
            inventory
                .split(|byte| *byte == 0)
                .any(|entry| entry == expected.as_bytes()),
            "candidate is not a registered managed worktree"
        );
        let head = git(&self.workspace, &["rev-parse", "--verify", "HEAD"]).await?;
        ensure!(
            String::from_utf8(head)?.trim() == self.base_revision,
            "candidate HEAD changed"
        );
        let files = git(
            &self.workspace,
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
        )
        .await?;
        let mut count = 0;
        let mut total = 0u64;
        use std::os::unix::ffi::OsStrExt;
        for file in files
            .split(|byte| *byte == 0)
            .filter(|file| !file.is_empty())
        {
            count += 1;
            let relative = Path::new(std::ffi::OsStr::from_bytes(file));
            ensure!(
                relative
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
                "unconfined candidate path"
            );
            if let Ok(metadata) = std::fs::symlink_metadata(self.workspace.join(relative)) {
                ensure!(
                    metadata.is_file() || metadata.file_type().is_symlink(),
                    "unsupported candidate file"
                );
                ensure!(
                    metadata.len() <= 64 * 1024 * 1024,
                    "candidate file exceeds bounds"
                );
                total = total
                    .checked_add(metadata.len())
                    .context("candidate size overflow")?;
            }
            ensure!(
                count <= 8192 && total <= 256 * 1024 * 1024,
                "candidate exceeds file or byte bounds"
            );
        }
        Ok(())
    }

    pub async fn diff(&self) -> Result<String> {
        self.validate().await?;
        review_candidate_diff(&self.workspace, &self.base_revision).await
    }

    pub async fn require_state(&self, expected: &str) -> Result<()> {
        self.validate().await?;
        ensure!(
            compute_workspace_state(&self.workspace, &self.base_revision)
                .await?
                .state_id
                == expected,
            "STALE_CANDIDATE"
        );
        Ok(())
    }

    /// The session owner must first validate acceptance and claim the action.
    pub async fn admit_application(&self, expected: &str) -> Result<()> {
        self.require_state(expected).await?;
        ensure!(
            git(&self.workspace, &["diff", "--cached", "--name-only", "-z"])
                .await?
                .is_empty(),
            "candidate index must remain unchanged"
        );
        let main_head =
            String::from_utf8(git(&self.repository, &["rev-parse", "--verify", "HEAD"]).await?)?;
        ensure!(
            main_head.trim() == self.base_revision,
            "developer checkout HEAD changed"
        );
        ensure!(
            git(&self.repository, &["status", "--porcelain=v1", "-z"])
                .await?
                .is_empty(),
            "developer checkout must be clean before applying"
        );
        Ok(())
    }

    pub async fn apply(&self, expected: &str) -> Result<()> {
        self.admit_application(expected).await?;
        let patch = self.diff().await?;
        ensure!(!patch.is_empty(), "candidate has no patch to apply");
        let patch_file = tempfile::NamedTempFile::new()?;
        tokio::fs::write(patch_file.path(), patch.as_bytes()).await?;
        let patch_path = patch_file
            .path()
            .to_str()
            .context("patch path must be UTF-8")?;
        git(&self.repository, &["apply", "--check", "--", patch_path]).await?;
        self.require_state(expected).await?;
        ensure!(
            git(&self.repository, &["status", "--porcelain=v1", "-z"])
                .await?
                .is_empty(),
            "developer checkout changed during apply admission"
        );
        git(&self.repository, &["apply", "--", patch_path]).await?;
        ensure!(
            compute_workspace_state(&self.repository, &self.base_revision)
                .await?
                .state_id
                == expected,
            "APPLY_RECOVERY_REQUIRED: developer checkout differs from accepted candidate"
        );
        Ok(())
    }

    pub async fn discard(&self, expected: &str) -> Result<()> {
        self.require_state(expected).await?;
        git(
            &self.repository,
            &[
                "worktree",
                "remove",
                "--force",
                "--",
                self.workspace
                    .to_str()
                    .context("workspace path must be UTF-8")?,
            ],
        )
        .await?;
        ensure!(!self.workspace.exists(), "worktree cleanup unconfirmed");
        Ok(())
    }
}
