use anyhow::{Context, Result, ensure};
use orbit::{engine::Engine, model::id};
use sqlx::PgPool;
use std::path::Path;
use tempfile::TempDir;

pub struct DisposablePgTestContext {
    pub engine: Engine,
    pub schema: String,
    pub url: String,
    base_url: String,
    _artifacts: TempDir,
}

impl DisposablePgTestContext {
    pub async fn create(schema_prefix: &str, lease_seconds: i64) -> Result<Self> {
        ensure!(
            !schema_prefix.is_empty()
                && schema_prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
            "invalid disposable PostgreSQL schema prefix"
        );

        let base_url = required_test_database_url(std::env::var("ORBIT_TEST_DATABASE_URL").ok())?;
        let admin = PgPool::connect(&base_url)
            .await
            .context("failed to connect to disposable ORBIT_TEST_DATABASE_URL")?;
        let schema = format!("orbit_{schema_prefix}_{}", id().replace('-', ""));
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .context("failed to create disposable PostgreSQL test schema")?;
        admin.close().await;

        let separator = if base_url.contains('?') { '&' } else { '?' };
        let url = format!("{base_url}{separator}options=-csearch_path%3D{schema}");
        let artifacts = tempfile::tempdir().context("failed to create test artifact directory")?;
        let engine =
            match Engine::connect(&url, artifacts.path().join("artifacts"), lease_seconds).await {
                Ok(engine) => engine,
                Err(error) => {
                    if let Ok(admin) = PgPool::connect(&base_url).await {
                        let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
                            .execute(&admin)
                            .await;
                        admin.close().await;
                    }
                    return Err(error).context("failed to initialize disposable Orbit test schema");
                }
            };

        Ok(Self {
            engine,
            schema,
            url,
            base_url,
            _artifacts: artifacts,
        })
    }

    pub async fn teardown(self) -> Result<()> {
        self.engine.pool.close().await;
        let admin = PgPool::connect(&self.base_url)
            .await
            .context("failed to reconnect to disposable ORBIT_TEST_DATABASE_URL for cleanup")?;
        sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
            .execute(&admin)
            .await
            .context("failed to drop disposable PostgreSQL test schema")?;
        admin.close().await;
        Ok(())
    }
}

fn required_test_database_url(value: Option<String>) -> Result<String> {
    let value = value.context(
        "ORBIT_TEST_DATABASE_URL is required; configure the documented disposable PostgreSQL fixture",
    )?;
    let value = value.trim().to_owned();
    ensure!(
        !value.is_empty(),
        "ORBIT_TEST_DATABASE_URL must not be empty"
    );
    Ok(value)
}

pub fn init_git_repo(path: &Path) -> Result<()> {
    run_git(path, &["init", "--quiet"])?;
    run_git(path, &["config", "user.email", "orbit-test@example.com"])?;
    run_git(path, &["config", "user.name", "Orbit Tester"])?;
    Ok(())
}

pub struct TemporaryGitRepo {
    root: TempDir,
    baseline_revision: String,
}

impl TemporaryGitRepo {
    pub fn create() -> Result<Self> {
        let root = tempfile::tempdir().context("failed to create temporary Git repository")?;
        init_git_repo(root.path())?;
        std::fs::write(root.path().join("README.md"), "offline fixture baseline\n")?;
        run_git(root.path(), &["add", "README.md"])?;
        run_git(
            root.path(),
            &["commit", "--quiet", "-m", "fixture baseline"],
        )?;
        let baseline_revision = git_output(root.path(), &["rev-parse", "HEAD"])?;
        Ok(Self {
            root,
            baseline_revision,
        })
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn baseline_revision(&self) -> &str {
        &self.baseline_revision
    }
}

fn run_git(path: &Path, args: &[&str]) -> Result<()> {
    git_output(path, args).map(|_| ())
}

fn git_output(path: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .with_context(|| format!("failed to start git {args:?}"))?;
    ensure!(
        output.status.success(),
        "git {args:?} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::required_test_database_url;

    #[test]
    fn missing_or_blank_database_prerequisite_is_an_error() {
        assert!(required_test_database_url(None).is_err());
        assert!(required_test_database_url(Some(String::new())).is_err());
        assert!(required_test_database_url(Some("   ".into())).is_err());
        assert_eq!(
            required_test_database_url(Some("  postgres://localhost/orbit-test  ".into())).unwrap(),
            "postgres://localhost/orbit-test"
        );
    }
}
