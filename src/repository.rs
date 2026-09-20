//! Worker-side materialization. Credential values never enter a plan or workspace.
use crate::{
    execution::credential_name,
    governance::{Scope, SecretRef},
    model::*,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Explicitly limited to numeric loopback addresses for disposable fixtures.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_http_loopback: bool,
}

pub fn endpoint(value: &str, allow_http_loopback: bool) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).context("invalid configured endpoint")?;
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.scheme() == "https" || (allow_http_loopback && loopback && url.scheme() == "http"),
        "endpoint requires HTTPS (HTTP is only permitted for explicit numeric loopback fixtures)"
    );
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && value.len() <= 2048,
        "endpoint must not contain credentials, query or fragment"
    );
    Ok(url)
}

impl Remote {
    pub fn validate(&self) -> Result<()> {
        endpoint(&self.url, self.allow_http_loopback)?;
        if let Some(name) = &self.credential {
            credential_name(name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Repository,
    Model,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub secret: SecretRef,
    pub purpose: Purpose,
    /// Repository ID or agent binding name, according to purpose.
    pub binding: String,
    /// Exact approved URL, not a user-supplied origin or redirect destination.
    pub audience: String,
    #[serde(default)]
    pub scopes: Vec<Scope>,
}

impl Credential {
    pub fn resolve(
        &self,
        purpose: Purpose,
        binding: &str,
        audience: &str,
        a: &Assignment,
    ) -> Result<String> {
        ensure!(
            self.purpose == purpose && self.binding == binding && self.audience == audience,
            "credential use denied for binding or audience"
        );
        ensure!(
            a.plan
                .scope
                .as_ref()
                .map_or(self.scopes.is_empty(), |scope| self.scopes.contains(scope)),
            "credential use denied for execution scope"
        );
        self.secret.resolve()
    }
}

/// Metadata is outside the mounted worktree, so shell tools cannot change host Git config.
pub struct Workspace {
    pub path: PathBuf,
    pub git_dir: PathBuf,
    pub home: PathBuf,
}

impl Workspace {
    pub async fn materialize(
        a: &Assignment,
        directory: &Path,
        credentials: &BTreeMap<String, Credential>,
    ) -> Result<Self> {
        let binding = &a.plan.repository;
        let workspace = Self {
            path: directory.join("repository"),
            git_dir: directory.join("git"),
            home: directory.join("git-home"),
        };
        tokio::fs::create_dir(&workspace.home).await?;
        let mut environment = BTreeMap::new();
        let source = if let Some(remote) = &binding.remote {
            remote.validate()?;
            if let Some(reference) = &remote.credential {
                let secret = credentials
                    .get(reference)
                    .context("repository credential unavailable")?
                    .resolve(
                        Purpose::Repository,
                        &a.plan.definition.inputs.repository_id,
                        &remote.url,
                        a,
                    )?;
                let helper = directory.join("git-credential.sh");
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::write(&helper, GIT_CREDENTIAL_HELPER).await?;
                tokio::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).await?;
                let url = endpoint(&remote.url, remote.allow_http_loopback)?;
                environment.insert("ORBIT_GIT_PROTOCOL".into(), url.scheme().into());
                let authority = remote
                    .url
                    .split_once("://")
                    .unwrap()
                    .1
                    .split('/')
                    .next()
                    .unwrap();
                environment.insert("ORBIT_GIT_HOST".into(), authority.into());
                environment.insert(
                    "ORBIT_GIT_PATH".into(),
                    url.path()
                        .trim_start_matches('/')
                        .trim_end_matches('/')
                        .into(),
                );
                environment.insert(
                    "ORBIT_GIT_HELPER".into(),
                    helper.to_string_lossy().into_owned(),
                );
                environment.insert("ORBIT_GIT_PASSWORD".into(), secret);
            }
            environment.insert(
                "GIT_ALLOW_PROTOCOL".into(),
                if remote.allow_http_loopback {
                    "https:http"
                } else {
                    "https"
                }
                .into(),
            );
            remote.url.as_str()
        } else {
            ensure!(
                Path::new(&binding.path).is_absolute(),
                "repository binding must be absolute"
            );
            environment.insert("GIT_ALLOW_PROTOCOL".into(), "file".into());
            binding.path.as_str()
        };
        let clone = vec![
            "clone".into(),
            "--no-checkout".into(),
            "--no-local".into(),
            "--no-hardlinks".into(),
            "--separate-git-dir".into(),
            workspace.git_dir.to_string_lossy().into_owned(),
            "--".into(),
            source.into(),
            workspace.path.to_string_lossy().into_owned(),
        ];
        git_output(&clone, directory, &workspace.home, &environment).await?;
        // The pointer is generated by our clone. Git operations below always pass
        // explicit metadata/worktree paths and never trust task-created .git files.
        tokio::fs::remove_file(workspace.path.join(".git")).await?;
        workspace
            .git(&[
                "checkout",
                "--detach",
                &a.plan.definition.inputs.base_revision,
            ])
            .await?;
        let revision = workspace.git(&["rev-parse", "HEAD"]).await?;
        ensure!(
            String::from_utf8(revision)?
                .trim()
                .eq_ignore_ascii_case(&a.plan.definition.inputs.base_revision),
            "base revision mismatch"
        );
        Ok(workspace)
    }

    pub async fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut argv = vec![
            format!("--git-dir={}", self.git_dir.display()),
            format!("--work-tree={}", self.path.display()),
        ];
        argv.extend(args.iter().map(|s| s.to_string()));
        git_output(&argv, &self.path, &self.home, &BTreeMap::new()).await
    }

    /// Non-destructively snapshot repository state and working tree status.
    ///
    /// Strictly read-only: observes git status, HEAD, and binary diff against baseline.
    /// Does NOT run reset, checkout, clean, or mutate files.
    pub async fn snapshot_workspace(
        &self,
        baseline_revision: &str,
    ) -> Result<(crate::continuation::WorkspaceSnapshot, Vec<u8>)> {
        let head_bytes = self.git(&["rev-parse", "HEAD"]).await?;
        let head_revision = String::from_utf8(head_bytes)?.trim().to_string();

        let status_bytes = self.git(&["status", "--porcelain=v1", "-z"]).await?;
        let (changed, added, deleted, untracked) = parse_porcelain_z(&status_bytes);

        let diff_bytes = self
            .git(&[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "--full-index",
                baseline_revision,
                "--",
            ])
            .await?;

        let diff_sha256 = crate::model::digest(&diff_bytes);

        let snapshot = crate::continuation::WorkspaceSnapshot {
            baseline_revision: baseline_revision.to_string(),
            head_revision,
            changed_files: changed,
            added_files: added,
            deleted_files: deleted,
            untracked_files: untracked,
            diff_sha256: Some(diff_sha256),
            diff_artifact_id: None,
        };

        Ok((snapshot, diff_bytes))
    }

    pub async fn patch(&self, a: &Assignment) -> Result<(Vec<u8>, Vec<u8>)> {
        self.git(&["add", "-A"]).await?;
        let base = &a.plan.definition.inputs.base_revision;
        let patch = self
            .git(&[
                "diff",
                "--cached",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--full-index",
                base,
                "--",
            ])
            .await?;
        let paths = self
            .git(&["diff", "--cached", "--name-only", "-z", base, "--"])
            .await?;
        let manifest = serde_json::to_vec(
            &serde_json::json!({"base_revision":base,"attempt_id":a.attempt_id,
            "checksum":digest(&patch), "changed_paths":paths.split(|b| *b == 0).filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).to_string()).collect::<Vec<_>>()}),
        )?;
        Ok((patch, manifest))
    }
}

async fn git_output(
    args: &[String],
    cwd: &Path,
    home: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "credential.helper=",
        "-c",
        "credential.useHttpPath=true",
        "-c",
        "http.followRedirects=false",
        "-c",
        "protocol.ext.allow=never",
    ]);
    if environment.contains_key("ORBIT_GIT_HELPER") {
        // The ! form is Git's shell helper protocol. Expansion inside double
        // quotes keeps every character of the operator-owned path literal.
        command.args(["-c", "credential.helper=!\"$ORBIT_GIT_HELPER\""]);
    }
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().context("Git unavailable")?;
    let _group = crate::worker::ProcessGroup(child.id().context("Git process missing")?);
    let mut stdout = child
        .stdout
        .take()
        .context("Git output missing")?
        .take(crate::artifacts::MAX_ARTIFACT_BYTES + 1);
    tokio::time::timeout(Duration::from_secs(120), async {
        let output = async {
            let mut bytes = vec![];
            stdout.read_to_end(&mut bytes).await?;
            ensure!(
                bytes.len() as u64 <= crate::artifacts::MAX_ARTIFACT_BYTES,
                "repository output too large"
            );
            Ok::<_, anyhow::Error>(bytes)
        };
        let (status, bytes) = tokio::try_join!(
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
            output
        )?;
        ensure!(
            status.success(),
            "repository materialization or Git operation failed"
        );
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .context("Git operation timed out")?
}

/// Discover the Git repository root directory starting from `start_dir` (or cwd if None).
pub fn find_repository_root(start_dir: Option<&Path>) -> Result<PathBuf> {
    let mut cmd = std::process::Command::new("git");
    let dir = match start_dir {
        Some(p) if p.is_dir() => Some(p),
        Some(p) => p.parent(),
        None => None,
    };
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    cmd.args(["rev-parse", "--show-toplevel"]);
    let output = cmd.output().context("failed to execute git command")?;
    ensure!(
        output.status.success(),
        "directory is not inside a Git repository: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let root = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(PathBuf::from(root))
}

/// Resolve a symbolic Git reference (such as HEAD, main, tags, or commit prefixes)
/// to a full, immutable 40-character or 64-character hexadecimal commit SHA.
/// If `start_dir` is provided, Git discovery begins from that path or its parent.
pub fn resolve_git_revision(git_ref: &str, start_dir: Option<&Path>) -> Result<String> {
    let git_ref = git_ref.trim();
    ensure!(!git_ref.is_empty(), "Git revision cannot be empty");

    let is_full_sha =
        [40, 64].contains(&git_ref.len()) && git_ref.bytes().all(|b| b.is_ascii_hexdigit());

    let mut cmd = std::process::Command::new("git");
    let dir = match start_dir {
        Some(p) if p.is_dir() => Some(p),
        Some(p) => p.parent(),
        None => None,
    };
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    cmd.args(["rev-parse", "--verify", &format!("{git_ref}^{{commit}}")]);

    match cmd.output() {
        Ok(output) if output.status.success() => {
            let sha = String::from_utf8(output.stdout)?.trim().to_string();
            ensure!(
                [40, 64].contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "resolved revision '{sha}' is not a valid full Git commit SHA"
            );
            Ok(sha)
        }
        Ok(output) => {
            if is_full_sha {
                Ok(git_ref.to_string())
            } else {
                let err = String::from_utf8_lossy(&output.stderr);
                let err_msg = err.trim();
                if err_msg.is_empty() {
                    anyhow::bail!("cannot resolve Git revision '{git_ref}'");
                } else {
                    anyhow::bail!("cannot resolve Git revision '{git_ref}': {err_msg}");
                }
            }
        }
        Err(e) => {
            if is_full_sha {
                Ok(git_ref.to_string())
            } else {
                anyhow::bail!("failed to invoke git to resolve revision '{git_ref}': {e:#}");
            }
        }
    }
}

const GIT_CREDENTIAL_HELPER: &str = "#!/bin/sh
[ \"$1\" = get ] || exit 0
protocol= host= path=
while IFS='=' read -r key value && [ -n \"$key\" ]; do
  case \"$key\" in
    protocol) protocol=$value;;
    host) host=$value;;
    path) path=$value;;
  esac
done
[ \"$protocol\" = \"$ORBIT_GIT_PROTOCOL\" ] &&
[ \"$host\" = \"$ORBIT_GIT_HOST\" ] &&
[ \"${path%/}\" = \"$ORBIT_GIT_PATH\" ] || exit 0
printf 'username=x-access-token\\npassword=%s\\n' \"$ORBIT_GIT_PASSWORD\"
";

pub fn parse_porcelain_z(output: &[u8]) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    let mut changed = std::collections::BTreeSet::new();
    let mut added = std::collections::BTreeSet::new();
    let mut deleted = std::collections::BTreeSet::new();
    let mut untracked = std::collections::BTreeSet::new();

    let entries: Vec<&[u8]> = output.split(|b| *b == 0).collect();
    let mut i = 0;
    while i < entries.len() {
        let entry = entries[i];
        if entry.is_empty() {
            i += 1;
            continue;
        }

        if entry.len() < 3 {
            i += 1;
            continue;
        }

        let x = entry[0] as char;
        let y = entry[1] as char;
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();

        // Renames in porcelain -z have the new path in entry, followed immediately
        // by the next NUL-delimited entry containing the old path.
        if x == 'R' || y == 'R' {
            let old_path = if i + 1 < entries.len() && !entries[i + 1].is_empty() {
                i += 1;
                String::from_utf8_lossy(entries[i]).into_owned()
            } else {
                String::new()
            };
            // Represent rename deterministically in added (new) and deleted (old),
            // and record changed.
            added.insert(path.clone());
            if !old_path.is_empty() {
                deleted.insert(old_path);
            }
            changed.insert(path);
            i += 1;
            continue;
        }

        // Untracked files
        if x == '?' && y == '?' {
            untracked.insert(path);
            i += 1;
            continue;
        }

        // Added files
        if x == 'A' || y == 'A' {
            added.insert(path.clone());
        }

        // Deleted files
        if x == 'D' || y == 'D' {
            deleted.insert(path.clone());
        }

        // Modified files (staged or unstaged)
        if x == 'M' || y == 'M' || x == 'T' || y == 'T' || x == 'U' || y == 'U' {
            changed.insert(path.clone());
        }

        // Any tracked change (including added/deleted) is also a changed file
        if (x != '?' && x != ' ') || (y != '?' && y != ' ') {
            changed.insert(path);
        }

        i += 1;
    }

    (
        changed.into_iter().collect(),
        added.into_iter().collect(),
        deleted.into_iter().collect(),
        untracked.into_iter().collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn git_helper_checks_exact_audience_and_never_stores_credentials() -> Result<()> {
        for (operation, protocol, host, path, allowed) in [
            (
                "get",
                "https",
                "git.example.invalid:8443",
                "owner/repo.git",
                true,
            ),
            (
                "get",
                "http",
                "git.example.invalid:8443",
                "owner/repo.git",
                false,
            ),
            ("get", "https", "elsewhere.invalid", "owner/repo.git", false),
            (
                "get",
                "https",
                "git.example.invalid:8443",
                "other/repo.git",
                false,
            ),
            (
                "store",
                "https",
                "git.example.invalid:8443",
                "owner/repo.git",
                false,
            ),
        ] {
            let mut child = std::process::Command::new("sh")
                .args(["-c", GIT_CREDENTIAL_HELPER, "fixture-helper", operation])
                .env_clear()
                .env("ORBIT_GIT_PROTOCOL", "https")
                .env("ORBIT_GIT_HOST", "git.example.invalid:8443")
                .env("ORBIT_GIT_PATH", "owner/repo.git")
                .env("ORBIT_GIT_PASSWORD", "fixture-only-private-token")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            if operation == "get" {
                child.stdin.take().unwrap().write_all(
                    format!("protocol={protocol}\nhost={host}\npath={path}\n\n").as_bytes(),
                )?;
            }
            let output = child.wait_with_output()?;
            assert!(output.status.success());
            assert!(output.stderr.is_empty());
            assert_eq!(!output.stdout.is_empty(), allowed);
        }
        Ok(())
    }
}
