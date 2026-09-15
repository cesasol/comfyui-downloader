use crate::ipc::protocol::{Request, Response};
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
    /// `Request → Response` exchanges; a connection carries as many as the
    /// client sends.
    pub async fn serve<F, Fut>(&self, request_handler: F) -> Result<()>
    where
        F: Fn(Request) -> Fut + Clone + Send + 'static,
        Fut: std::future::Future<Output = Response> + Send,
    {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let req_handler = request_handler.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, req_handler).await {
                    error!("IPC connection error: {e}");
                }
            });
        }
    }
}

async fn handle_connection<F, Fut>(stream: UnixStream, request_handler: F) -> Result<()>
where
    F: Fn(Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // One connection carries as many request/response exchanges as the client
    // sends: the CLI resolves version info or an ID prefix before acting.
    while let Some(line) = lines.next_line().await? {
        let req = match serde_json::from_str::<Request>(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(format!("bad request: {e}"));
                let mut encoded = serde_json::to_string(&resp)?;
                encoded.push('\n');
                writer.write_all(encoded.as_bytes()).await?;
                continue;
            }
        };

        let response = request_handler(req).await;
        let mut encoded = serde_json::to_string(&response)?;
        encoded.push('\n');
        writer.write_all(encoded.as_bytes()).await?;
    }
    Ok(())
}
