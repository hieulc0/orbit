//! Bounded JSON-RPC framing shared by the ACP client and the pinned Codex bridge.
//! No protocol payload is logged, and there is no unbounded reader task/queue.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// A correlated peer rejection. Never retain peer message/data in durable errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestRejected {
    pub code: Option<i64>,
}

impl std::fmt::Display for RequestRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent request rejected (code={:?})", self.code)
    }
}

impl std::error::Error for RequestRejected {}

#[derive(Debug, Clone, Copy)]
pub struct StreamClosed;

impl std::fmt::Display for StreamClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("agent stream closed")
    }
}

impl std::error::Error for StreamClosed {}

/// Safe, explicit correlation metadata used only by the pinned Codex bridge.
/// Values are identifiers, never tool arguments or provider payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrbitToolInvocationMeta {
    pub invocation_id: String,
    pub provider_tool_call_id: String,
}

impl OrbitToolInvocationMeta {
    pub fn new(invocation_id: &str, provider_tool_call_id: &str) -> Result<Self> {
        ensure!(
            safe_correlator(invocation_id, 128),
            "invalid Orbit tool invocation id"
        );
        ensure!(
            safe_correlator(provider_tool_call_id, 256),
            "invalid provider tool call id"
        );
        Ok(Self {
            invocation_id: invocation_id.to_owned(),
            provider_tool_call_id: provider_tool_call_id.to_owned(),
        })
    }

    pub fn envelope_metadata(&self) -> Value {
        json!({
            "orbit": {
                "toolInvocationId": self.invocation_id,
                "providerToolCallId": self.provider_tool_call_id,
            }
        })
    }
}

/// Parse only Orbit's bounded correlation extension. `None` means that the peer
/// did not advertise it; malformed or partial metadata is an error.
pub fn orbit_tool_invocation_meta(envelope: &Value) -> Result<Option<OrbitToolInvocationMeta>> {
    let Some(orbit) = envelope.get("_meta").and_then(|meta| meta.get("orbit")) else {
        return Ok(None);
    };
    let invocation_id = orbit
        .get("toolInvocationId")
        .and_then(Value::as_str)
        .context("invalid Orbit tool invocation metadata")?;
    let provider_tool_call_id = orbit
        .get("providerToolCallId")
        .and_then(Value::as_str)
        .context("invalid Orbit tool invocation metadata")?;
    Ok(Some(OrbitToolInvocationMeta::new(
        invocation_id,
        provider_tool_call_id,
    )?))
}

fn safe_correlator(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

pub struct Wire {
    input: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    output: Box<dyn AsyncWrite + Send + Unpin>,
    next: u64,
    bytes: u64,
    messages: u64,
    limit: u64,
    codex: bool,
    response_limit: Option<usize>,
    response_limit_hit: bool,
    response_payload_bytes: u64,
}
impl Wire {
    pub fn new(
        input: impl AsyncRead + Send + Unpin + 'static,
        output: impl AsyncWrite + Send + Unpin + 'static,
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
            response_limit: None,
            response_limit_hit: false,
            response_payload_bytes: 0,
        }
    }
    pub fn codex(mut self) -> Self {
        self.codex = true;
        self
    }
    /// Apply a per-tool result bound to server callback responses on this wire.
    pub fn set_response_limit(&mut self, limit: usize) {
        self.response_limit = Some(limit);
        self.response_limit_hit = false;
    }
    pub fn take_response_limit_hit(&mut self) -> bool {
        std::mem::take(&mut self.response_limit_hit)
    }
    pub fn response_payload_bytes(&self) -> u64 {
        self.response_payload_bytes
    }
    pub async fn read(&mut self) -> Result<Value> {
        let mut frame = Vec::new();
        loop {
            let data = self.input.fill_buf().await.context("agent stream failed")?;
            if data.is_empty() {
                return Err(StreamClosed.into());
            }
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
                != (value.get("result").is_some() || value.get("error").is_some())
                && !(value.get("result").is_some() && value.get("error").is_some()),
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
    pub async fn request_with_tool_invocation(
        &mut self,
        method: &str,
        params: Value,
        invocation: &OrbitToolInvocationMeta,
    ) -> Result<Value> {
        self.next += 1;
        let id = json!(format!("orbit-{}", self.next));
        self.send(json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":method,
            "params":params,
            "_meta":invocation.envelope_metadata(),
        }))
        .await?;
        Ok(id)
    }
    pub async fn response_ok(&mut self, id: Value, result: Value) -> Result<()> {
        if self.response_limit.is_some_and(|limit| {
            serde_json::to_vec(&result).map_or(true, |bytes| bytes.len() > limit)
        }) {
            self.response_limit_hit = true;
            return self.response_error(id, -32603, "OUTPUT_LIMIT").await;
        }
        self.response_payload_bytes = self
            .response_payload_bytes
            .saturating_add(serde_json::to_vec(&result)?.len() as u64);
        self.send(json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await
    }
    pub async fn response_error(&mut self, id: Value, code: i64, message: &str) -> Result<()> {
        let max = self.response_limit.unwrap_or(65536).min(65536);
        let mut end = max.min(message.len());
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        while serde_json::to_vec(&json!({"code":code,"message":&message[..end]}))?.len() > max
            && end > 0
        {
            end -= 1;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
        }
        let message = &message[..end];
        self.response_payload_bytes = self.response_payload_bytes.saturating_add(
            serde_json::to_vec(&json!({"code":code,"message":message}))?.len() as u64,
        );
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            }
        }))
        .await
    }
    pub async fn response(&mut self, id: Value, result: Result<Value>) -> Result<()> {
        let value = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(err) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":format!("{:#}", err)}})
            }
        };
        self.send(value).await
    }
    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }
    pub async fn notify_with_tool_invocation(
        &mut self,
        method: &str,
        params: Value,
        invocation: &OrbitToolInvocationMeta,
    ) -> Result<()> {
        self.send(json!({
            "jsonrpc":"2.0",
            "method":method,
            "params":params,
            "_meta":invocation.envelope_metadata(),
        }))
        .await
    }
    pub fn result(value: Value, id: &Value) -> Result<Value> {
        ensure!(
            &value["id"] == id && value.get("method").is_none(),
            "foreign agent response"
        );
        if let Some(err) = value.get("error") {
            // Peer messages/data can contain credentials, prompt text or arbitrary
            // control characters. Only the bounded numeric protocol code is safe.
            return Err(RequestRejected {
                code: err["code"].as_i64(),
            }
            .into());
        }
        value
            .get("result")
            .cloned()
            .context("agent response missing result")
    }
}

#[cfg(test)]
mod tests {
    use super::{OrbitToolInvocationMeta, Wire, orbit_tool_invocation_meta};
    use anyhow::Result;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, BufReader, duplex};

    #[tokio::test]
    async fn invocation_metadata_is_carried_on_update_and_callback_envelopes() -> Result<()> {
        let invocation = OrbitToolInvocationMeta::new("oti-test-1", "codex-call-9")?;
        let (mut peer, writer) = duplex(8192);
        let mut wire = Wire::new(tokio::io::empty(), writer, 8192);

        wire.notify_with_tool_invocation(
            "session/update",
            json!({"update":{"sessionUpdate":"tool_call","toolCallId":"codex-call-9"}}),
            &invocation,
        )
        .await?;
        let mut reader = BufReader::new(&mut peer);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let notification: serde_json::Value = serde_json::from_str(&line)?;
        assert_eq!(
            orbit_tool_invocation_meta(&notification)?,
            Some(invocation.clone())
        );
        assert_eq!(
            notification["params"]["update"]["toolCallId"],
            "codex-call-9"
        );

        line.clear();
        drop(reader);
        let callback_id = wire
            .request_with_tool_invocation(
                "fs/read_text_file",
                json!({"path":"README.md"}),
                &invocation,
            )
            .await?;
        let mut reader = BufReader::new(&mut peer);
        reader.read_line(&mut line).await?;
        let callback: serde_json::Value = serde_json::from_str(&line)?;
        assert_eq!(callback["id"], callback_id);
        assert_eq!(orbit_tool_invocation_meta(&callback)?, Some(invocation));
        Ok(())
    }

    #[test]
    fn invocation_metadata_rejects_partial_or_unsafe_identifiers() {
        assert!(OrbitToolInvocationMeta::new("oti\nsecret", "call-1").is_err());
        assert!(
            orbit_tool_invocation_meta(&json!({
                "_meta":{"orbit":{"toolInvocationId":"oti-1"}}
            }))
            .is_err()
        );
    }
}
