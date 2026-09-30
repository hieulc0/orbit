//! A developer-selected Linux terminal boundary. Repository callbacks remain
//! subject to Orbit's role policy and durable mutation ownership.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "profile", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoleExecutionProfile {
    #[default]
    Trusted,
    DevLocal {
        bubblewrap: PathBuf,
    },
}

impl RoleExecutionProfile {
    pub fn validate(&self) -> Result<()> {
        if let Self::DevLocal { bubblewrap } = self {
            ensure!(
                bubblewrap.is_absolute(),
                "bubblewrap must be operator-pinned"
            );
            ensure!(
                bubblewrap.canonicalize()? == *bubblewrap && bubblewrap.is_file(),
                "invalid bubblewrap executable"
            );
        }
        Ok(())
    }

    /// Construct a fresh namespace without the host home, network, sockets or
    /// repository-external Git metadata. Never fall back to a host command.
    pub fn terminal_command(
        &self,
        repository: &Path,
        cwd: &Path,
        command: &str,
        args: &[String],
    ) -> Result<tokio::process::Command> {
        self.validate()?;
        let Self::DevLocal { bubblewrap } = self else {
            anyhow::bail!("CLI_WORKFLOW_TERMINAL_DISABLED");
        };
        let repository = repository.canonicalize()?;
        ensure!(repository != Path::new("/"), "invalid local workspace root");
        let cwd = cwd.canonicalize()?;
        let relative = cwd
            .strip_prefix(&repository)
            .context("terminal cwd outside workspace")?;
        ensure!(
            !command.is_empty()
                && !command.contains('\0')
                && args.len() <= 128
                && args
                    .iter()
                    .all(|arg| arg.len() <= 65536 && !arg.contains('\0')),
            "invalid terminal command"
        );
        let mut process = tokio::process::Command::new(bubblewrap);
        process.args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--cap-drop",
            "ALL",
        ]);
        for directory in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
            if Path::new(directory).exists() {
                process.args(["--ro-bind", directory, directory]);
            }
        }
        process.args(["--dir", "/etc"]);
        for file in ["/etc/ld.so.cache", "/etc/localtime"] {
            if Path::new(file).is_file() {
                process.args(["--ro-bind", file, file]);
            }
        }
        process.args([
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/home",
            "--dir",
            "/home/orbit",
        ]);
        process.arg("--bind").arg(&repository).arg("/workspace");
        let metadata = repository.join(".git");
        if metadata.exists() {
            ensure!(
                !std::fs::symlink_metadata(&metadata)?
                    .file_type()
                    .is_symlink(),
                "symlink Git metadata is unsupported"
            );
            process
                .arg("--ro-bind")
                .arg(metadata)
                .arg("/workspace/.git");
        }
        process.args([
            "--setenv",
            "HOME",
            "/home/orbit",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "GIT_CONFIG_NOSYSTEM",
            "1",
            "--setenv",
            "GIT_CONFIG_GLOBAL",
            "/dev/null",
        ]);
        process
            .arg("--chdir")
            .arg(Path::new("/workspace").join(relative))
            .arg("--")
            .arg(command)
            .args(args);
        process.current_dir(&repository).env_clear();
        unsafe {
            process.pre_exec(|| {
                for (resource, limit) in [
                    (libc::RLIMIT_FSIZE, 64 * 1024 * 1024),
                    (libc::RLIMIT_NOFILE, 256),
                    (libc::RLIMIT_CPU, 300),
                ] {
                    let bound = libc::rlimit {
                        rlim_cur: limit,
                        rlim_max: limit,
                    };
                    if libc::setrlimit(resource, &bound) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_never_constructs_host_terminal() {
        assert!(
            RoleExecutionProfile::Trusted
                .terminal_command(Path::new("/tmp"), Path::new("/tmp"), "sh", &[])
                .is_err()
        );
    }

    #[test]
    fn local_mounts_only_workspace_and_system_runtime() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let profile = RoleExecutionProfile::DevLocal {
            bubblewrap: PathBuf::from("/usr/bin/bwrap"),
        };
        if !Path::new("/usr/bin/bwrap").exists() {
            return Ok(());
        }
        let process = profile.terminal_command(
            directory.path(),
            directory.path(),
            "sh",
            &["-c".into(), "echo ready".into()],
        )?;
        let args: Vec<_> = process
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--unshare-all".into()));
        assert!(args.contains(&"--clearenv".into()));
        assert!(!args.iter().any(|arg| arg.contains(".orbit/private")
            || arg.contains(".ssh")
            || arg.contains("podman.sock")));
        assert!(
            profile
                .terminal_command(directory.path(), Path::new("/"), "sh", &[])
                .is_err()
        );
        Ok(())
    }
}
