use crate::ipc::protocol::{Request, Response};
use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    pub async fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting to daemon socket {}", path.display()))?;
        Ok(Self { stream })
    }

    pub async fn send(&mut self, req: &Request) -> Result<Response> {
        let (reader, mut writer) = self.stream.split();

        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;

        let mut response_line = String::new();
        BufReader::new(reader).read_line(&mut response_line).await?;

        serde_json::from_str(&response_line).context("parsing daemon response")
    }
}
