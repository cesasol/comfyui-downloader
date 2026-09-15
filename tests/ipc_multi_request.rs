//! Integration test: a single connection carries several request/response
//! exchanges.  The CLI relies on this — `add` asks for version info before
//! enqueueing, `delete`/`cancel` resolve an ID prefix first, and the template
//! picker lists templates before queueing the selection.

use comfyui_downloader::ipc::protocol::{Request, Response};
use comfyui_downloader::ipc::{IpcClient, IpcServer};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

#[tokio::test]
async fn one_connection_serves_multiple_requests() {
    let tmp = TempDir::new().unwrap();
    let socket_path = tmp.path().join("multi.sock");
    let server = IpcServer::bind(&socket_path).unwrap();
    let handled = Arc::new(AtomicUsize::new(0));

    let counter = handled.clone();
    tokio::spawn(async move {
        let _ = server
            .serve(move |_req| {
                let counter = counter.clone();
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    Response::ok(serde_json::json!({ "handled": n }))
                }
            })
            .await;
    });

    let mut client = connect(&socket_path).await;

    for expected in 1..=3usize {
        let response = client
            .send(&Request::GetStatus)
            .await
            .unwrap_or_else(|e| panic!("request {expected} failed: {e:#}"));
        match response {
            Response::Ok(data) => assert_eq!(data["handled"].as_u64(), Some(expected as u64)),
            Response::Err { message } => panic!("request {expected} errored: {message}"),
        }
    }
    assert_eq!(handled.load(Ordering::SeqCst), 3);
}

/// Connect as soon as the server has bound the socket.
async fn connect(socket_path: &std::path::Path) -> IpcClient {
    for _ in 0..100 {
        match IpcClient::connect(socket_path).await {
            Ok(client) => return client,
            Err(_) => tokio::task::yield_now().await,
        }
    }
    panic!("server never accepted a connection at {socket_path:?}");
}
