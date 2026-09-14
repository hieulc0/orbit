//! Bounded JSON-RPC framing shared by the ACP client and the pinned Codex bridge.
//! No protocol payload is logged, and there is no unbounded reader task/queue.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

pub struct Wire {
    input: BufReader<Box<dyn AsyncRead + Unpin>>,
    output: Box<dyn AsyncWrite + Unpin>,
    next: u64,
    bytes: u64,
    messages: u64,
    limit: u64,
    codex: bool,
}
impl Wire {
    pub fn new(
        input: impl AsyncRead + Unpin + 'static,
        output: impl AsyncWrite + Unpin + 'static,
        limit: u64,
    ) -> Self {
        Self {
            input: BufReader::new(Box::new(input)),
            output: Box::new(output),
            next: 0,
            bytes: 0,
            messages: 0,
            limit,
            codex: false,
        }
    }
    pub fn codex(mut self) -> Self {
        self.codex = true;
        self
    }
    pub async fn read(&mut self) -> Result<Value> {
        let mut frame = Vec::new();
        loop {
            let data = self.input.fill_buf().await.context("agent stream failed")?;
            ensure!(!data.is_empty(), "agent stream closed");
            let end = data.iter().position(|b| *b == b'\n').map(|n| n + 1);
            let n = end.unwrap_or(data.len());
            self.bytes = self.bytes.saturating_add(n as u64);
            ensure!(
                self.bytes <= self.limit && frame.len() + n <= 1024 * 1024,
                "agent wire byte limit exceeded"
            );
            frame.extend_from_slice(&data[..n]);
            self.input.consume(n);
            if end.is_some() {
                break;
            }
        }
        self.messages += 1;
        ensure!(self.messages <= 16384, "agent wire message limit exceeded");
        let value: Value =
            serde_json::from_slice(&frame).map_err(|_| anyhow::anyhow!("malformed agent JSON"))?;
        ensure!(
            value.is_object()
                && (value["jsonrpc"] == "2.0" || (self.codex && value.get("jsonrpc").is_none())),
            "invalid agent JSON-RPC envelope"
        );
        ensure!(
            value.get("method").is_some()
                != (value.get("result").is_some() || value.get("error").is_some()),
            "ambiguous agent message"
        );
        Ok(value)
    }
    pub async fn send(&mut self, mut value: Value) -> Result<()> {
        if self.codex {
            value
                .as_object_mut()
                .context("invalid outgoing envelope")?
                .remove("jsonrpc");
        }
        let mut bytes = serde_json::to_vec(&value)?;
        ensure!(bytes.len() <= 1024 * 1024, "outgoing agent frame too large");
        bytes.push(b'\n');
        self.output
            .write_all(&bytes)
            .await
            .context("agent write failed")?;
        self.output.flush().await?;
        Ok(())
    }
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = json!(format!("orbit-{}", self.next));
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }
    pub async fn response(&mut self, id: Value, result: Result<Value>) -> Result<()> {
        let value = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(_) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"Orbit denied or could not confirm the operation"}})
            }
        };
        self.send(value).await
    }
    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }
    pub fn result(value: Value, id: &Value) -> Result<Value> {
        ensure!(
            &value["id"] == id && value.get("method").is_none(),
            "foreign agent response"
        );
        ensure!(value.get("error").is_none(), "agent request rejected");
        value
            .get("result")
            .cloned()
            .context("agent response missing result")
    }
}
