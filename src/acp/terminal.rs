//! Asynchronous ACP terminal handle over the existing workspace supervisor.
use anyhow::{Context, Result, ensure};
use std::{cell::RefCell, process::Stdio, rc::Rc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    sync::{oneshot, watch},
    task::AbortHandle,
};

#[derive(Clone, Default)]
pub struct Output {
    pub bytes: Vec<u8>,
    pub total: u64,
    pub truncated: bool,
    pub exit_code: Option<i32>,
    pub complete: bool,
    pub cleanup_confirmed: bool,
    pub overflow: bool,
}
impl Output {
    pub fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        let excess = text.len().saturating_sub(self.bytes.len());
        let start = (excess..=text.len())
            .find(|n| text.is_char_boundary(*n))
            .unwrap();
        text[start..].into()
    }
}
pub struct Terminal {
    output: Rc<RefCell<Output>>,
    stop: RefCell<Option<oneshot::Sender<()>>>,
    done: watch::Receiver<bool>,
    tasks: Vec<AbortHandle>,
}
impl Drop for Terminal {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Terminal {
    pub async fn start(
        spec: &crate::model::CommandSpec,
        directory: &std::path::Path,
        output_limit: usize,
        total_limit: u64,
    ) -> Result<Self> {
        ensure!(
            output_limit <= 65536 && total_limit <= 8 * 1024 * 1024,
            "invalid ACP terminal output bounds"
        );
        let mut command = tokio::process::Command::new(&spec.argv[0]);
        command
            .args(&spec.argv[1..])
            .current_dir(directory)
            .env_clear()
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .env(
                "HOME",
                std::env::var_os("HOME").context("rootless runtime HOME unavailable")?,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        if let Some(value) = std::env::var_os("XDG_RUNTIME_DIR") {
            command.env("XDG_RUNTIME_DIR", value);
        }
        command.process_group(0);
        let mut child = command
            .spawn()
            .context("ACP workspace supervisor launch failed")?;
        let mut lifeline = child.stdin.take();
        let stdout = child.stdout.take().context("terminal output missing")?;
        let stderr = child
            .stderr
            .take()
            .context("terminal diagnostics missing")?;
        let output = Rc::new(RefCell::new(Output::default()));
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let (done_tx, done_rx) = watch::channel(false);
        let (overflow_tx, mut overflow_rx) = watch::channel(false);
        let capture = |mut stream: Box<dyn tokio::io::AsyncRead + Unpin>,
                       output: Rc<RefCell<Output>>,
                       overflow: watch::Sender<bool>| {
            tokio::task::spawn_local(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    let n = match stream.read(&mut buffer).await {
                        Ok(n) => n,
                        Err(_) => {
                            output.borrow_mut().overflow = true;
                            let _ = overflow.send(true);
                            break;
                        }
                    };
                    if n == 0 {
                        break;
                    }
                    let mut state = output.borrow_mut();
                    state.total = state.total.saturating_add(n as u64);
                    state.bytes.extend_from_slice(&buffer[..n]);
                    let discarded = state.bytes.len().saturating_sub(output_limit);
                    state.bytes.drain(..discarded);
                    state.truncated |= discarded > 0;
                    if state.total > total_limit {
                        state.overflow = true;
                        let _ = overflow.send(true);
                    }
                }
            })
        };
        let out_task = capture(Box::new(stdout), output.clone(), overflow_tx.clone());
        let err_task = capture(Box::new(stderr), output.clone(), overflow_tx);
        let out_abort = out_task.abort_handle();
        let err_abort = err_task.abort_handle();
        let state = output.clone();
        let timeout = spec.timeout_seconds;
        let request = std::path::PathBuf::from(&spec.argv[3]);
        let captures = vec![out_abort.clone(), err_abort.clone()];
        let driver = tokio::task::spawn_local(async move {
            let code = tokio::select! {
                status=child.wait()=>status.ok().and_then(|s|s.code()),
                _=&mut stop_rx=>{lifeline.take();tokio::time::timeout(Duration::from_secs(60),child.wait()).await.ok().and_then(Result::ok).and_then(|s|s.code())},
                _=overflow_rx.changed()=>{lifeline.take();tokio::time::timeout(Duration::from_secs(60),child.wait()).await.ok().and_then(Result::ok).and_then(|s|s.code())},
                _=tokio::time::sleep(Duration::from_secs(timeout))=>{lifeline.take();tokio::time::timeout(Duration::from_secs(60),child.wait()).await.ok().and_then(Result::ok).and_then(|s|s.code())},
            };
            lifeline.take();
            if tokio::time::timeout(Duration::from_secs(3), async {
                let _ = tokio::join!(out_task, err_task);
            })
            .await
            .is_err()
            {
                out_abort.abort();
                err_abort.abort();
                state.borrow_mut().overflow = true;
            }
            let mut output = state.borrow_mut();
            output.exit_code = code;
            output.cleanup_confirmed = code.is_some_and(|code| {
                crate::acp_process::read_cleanup(&request, None).ok() == Some(code)
            });
            output.complete = true;
            let _ = done_tx.send(true);
        });
        Ok(Self {
            output,
            stop: RefCell::new(Some(stop_tx)),
            done: done_rx,
            tasks: captures
                .into_iter()
                .chain([driver.abort_handle()])
                .collect(),
        })
    }
    pub fn output(&self) -> Output {
        self.output.borrow().clone()
    }
    pub async fn wait(&self) -> Result<Output> {
        let mut done = self.done.clone();
        while !*done.borrow_and_update() {
            done.changed()
                .await
                .context("ACP terminal supervisor lost")?;
        }
        let output = self.output();
        ensure!(output.cleanup_confirmed, "ACP terminal cleanup unconfirmed");
        ensure!(!output.overflow, "ACP terminal output limit exceeded");
        Ok(output)
    }
    pub async fn kill(&self) -> Result<Output> {
        if let Some(stop) = self.stop.borrow_mut().take() {
            let _ = stop.send(());
        }
        self.wait().await
    }
}
