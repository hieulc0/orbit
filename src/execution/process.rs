//! Bounded capture for native coordinator processes.
use anyhow::{Context, Result, ensure};
use std::{process::Output, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub(crate) async fn bounded_output(
    mut command: Command,
    limit: usize,
    timeout: Duration,
) -> Result<Output> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("start bounded process")?;
    let stdout = child.stdout.take().context("stdout missing")?;
    let stderr = child.stderr.take().context("stderr missing")?;
    tokio::time::timeout(timeout, async {
        let read = async |stream: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                          maximum: usize|
               -> Result<Vec<u8>> {
            let mut bytes = Vec::new();
            stream
                .take(maximum as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            ensure!(bytes.len() <= maximum, "process output exceeds bounds");
            Ok(bytes)
        };
        let (stdout, stderr) =
            tokio::try_join!(read(Box::new(stdout), limit), read(Box::new(stderr), 65536))?;
        Ok(Output {
            status: child.wait().await?,
            stdout,
            stderr,
        })
    })
    .await
    .context("bounded process timed out")?
}
