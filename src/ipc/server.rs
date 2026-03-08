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

    /// Accept connections in a loop, calling `handler` for each request.
    pub async fn serve<F, Fut>(&self, handler: F) -> Result<()>
    where
        F: Fn(Request) -> Fut + Clone + Send + 'static,
        Fut: std::future::Future<Output = Response> + Send,
    {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let handler = handler.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, handler).await {
                    error!("IPC connection error: {e}");
                }
            });
        }
    }
}

async fn handle_connection<F, Fut>(stream: UnixStream, handler: F) -> Result<()>
where
    F: Fn(Request) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handler(req).await,
            Err(e) => Response::err(format!("bad request: {e}")),
        };
        let mut encoded = serde_json::to_string(&response)?;
        encoded.push('\n');
        writer.write_all(encoded.as_bytes()).await?;
    }
    Ok(())
}
