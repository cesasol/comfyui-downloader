use crate::ipc::protocol::{Frame, Request, Response};
use anyhow::Result;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info};

pub struct IpcServer {
    listener: UnixListener,
}

impl IpcServer {
    pub fn bind(path: &Path) -> Result<Self> {
        // Remove stale socket file if present.
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        info!("IPC socket bound at {}", path.display());
        Ok(Self { listener })
    }

    /// Accept connections in a loop. `request_handler` handles one-shot
    /// `Request → Response` exchanges. `subscribe_handler` is called when a
    /// connection's first request is `Request::Subscribe`; it owns the
    /// connection's writer and runs until the client disconnects.
    pub async fn serve<F, Fut, S, SFut>(
        &self,
        request_handler: F,
        subscribe_handler: S,
    ) -> Result<()>
    where
        F: Fn(Request) -> Fut + Clone + Send + 'static,
        Fut: std::future::Future<Output = Response> + Send,
        S: Fn(SubscribeWriter) -> SFut + Clone + Send + 'static,
        SFut: std::future::Future<Output = ()> + Send,
    {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let req_handler = request_handler.clone();
            let sub_handler = subscribe_handler.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, req_handler, sub_handler).await {
                    error!("IPC connection error: {e}");
                }
            });
        }
    }
}

/// Owned writer half of a subscribed connection. The subscribe handler uses
/// this to push `Frame` lines until the client disconnects.
pub struct SubscribeWriter {
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl SubscribeWriter {
    pub async fn send(&mut self, frame: &Frame) -> Result<()> {
        let mut line = serde_json::to_string(frame)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        Ok(())
    }
}

async fn handle_connection<F, Fut, S, SFut>(
    stream: UnixStream,
    request_handler: F,
    subscribe_handler: S,
) -> Result<()>
where
    F: Fn(Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
    S: Fn(SubscribeWriter) -> SFut,
    SFut: std::future::Future<Output = ()>,
{
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let line = match lines.next_line().await? {
        Some(l) => l,
        None => return Ok(()),
    };

    let req = match serde_json::from_str::<Request>(&line) {
        Ok(r) => r,
        Err(e) => {
            let resp = Response::err(format!("bad request: {e}"));
            let mut encoded = serde_json::to_string(&resp)?;
            encoded.push('\n');
            writer.write_all(encoded.as_bytes()).await?;
            return Ok(());
        }
    };

    if matches!(req, Request::Subscribe) {
        let sw = SubscribeWriter { writer };
        subscribe_handler(sw).await;
        return Ok(());
    }

    let response = request_handler(req).await;
    let mut encoded = serde_json::to_string(&response)?;
    encoded.push('\n');
    writer.write_all(encoded.as_bytes()).await?;
    Ok(())
}
